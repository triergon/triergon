//! Text subtitles, the operations of the formats reference's `subtitle-text-`
//! modes (`harness/reference_subtitle_text.h`).
//!
//! - `packets`: a session capsule (`TRSTXT01`) of decoder opens, packets and
//!   flushes. tx3g samples go through `compat::mov_text::Decoder`, folding
//!   each call's return value and the complete subtitle; ASS packets through
//!   `compat::ass::Decoder`, with the capsule's charset label if it has one,
//!   folding the published text and its clock.
//! - `srt` and `vtt`: one SubRip or WebVTT document through
//!   `compat::document::Reader`, folding every cue's start and duration in
//!   milliseconds, its text and, for WebVTT, its identifier and settings.
//!
//! The digest is the ordered fold of a little-endian byte transcript.

use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, SourceId, TimeBase, WorkBudget};
use triergon_harness_protocol::digest::fold_bytes;
use triergon_harness_runner::{Invocation, Result, Totals, measure};
use triergon_subtitles::compat::{ass, document, mov_text};
use triergon_subtitles::text::{Encoding, Format, Input, Timing};

pub const PREFIX: &str = "subtitle-text-";
const OPERATION: &str = "subtitle-text-cues";
const MAX_INPUT: u64 = 16 * 1024 * 1024;
const MAX_POLLS: usize = 4_000_000;
/// The work each decoder call gets when measured.
const QUANTUM: u64 = 1 << 20;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Codec {
    MovText,
    Ass,
}

struct Open {
    timebase: Option<TimeBase>,
    width: u32,
    height: u32,
    extradata: Option<Bytes>,
}

struct Packet {
    clock: mov_text::Clock,
    payload: Bytes,
}

enum Op {
    Open(Open),
    Packet(Packet),
    Flush,
}

struct Session {
    codec: Codec,
    label: Option<Bytes>,
    ops: Vec<Op>,
}

enum Prepared {
    Session(Session),
    Document(Bytes, document::Syntax),
}

/// The transcript of one operation.
struct Work {
    digest: u64,
    packets: u64,
    bytes: u64,
    /// The work each decoder call gets.
    quantum: u64,
}

impl Work {
    fn new(quantum: u64) -> Self {
        Self {
            digest: 0,
            packets: 0,
            bytes: 0,
            quantum,
        }
    }
    fn data(&mut self, bytes: &[u8]) {
        fold_bytes(&mut self.digest, bytes);
    }
    fn byte(&mut self, value: u8) {
        self.data(&[value]);
    }
    fn u32(&mut self, value: u32) {
        self.data(&value.to_le_bytes());
    }
    fn i64(&mut self, value: i64) {
        self.data(&value.to_le_bytes());
    }
    fn framed(&mut self, bytes: &[u8]) {
        self.u32(bytes.len() as u32);
        self.data(bytes);
    }
    fn text(&mut self, bytes: &[u8]) {
        self.framed(bytes);
        self.packets += 1;
        self.bytes += bytes.len() as u64;
    }
    fn budget(&self) -> WorkBudget {
        WorkBudget::new(self.quantum)
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or("capsule offset overflow")?;
        let value = self.bytes.get(self.at..end).ok_or("truncated capsule")?;
        self.at = end;
        Ok(value)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }
    fn known(&mut self) -> Result<Option<i64>> {
        let known = self.byte()?;
        let value = i64::from_be_bytes(self.take(8)?.try_into()?);
        match known {
            0 => Ok(None),
            1 => Ok(Some(value)),
            _ => Err("capsule clock flag".into()),
        }
    }
}

/// Parses a session capsule, loading every byte string outside the measured
/// operation.
fn parse(pool: &Budget, raw: &[u8]) -> Result<Session> {
    let mut c = Cursor { bytes: raw, at: 0 };
    if c.take(8)? != b"TRSTXT01" {
        return Err("not a subtitle text capsule".into());
    }
    let codec = match c.byte()? {
        1 => Codec::MovText,
        2 => Codec::Ass,
        _ => return Err("capsule codec".into()),
    };
    let label = match c.u16()? {
        0 => None,
        size => Some(Bytes::copy_from_slice(pool, c.take(size.into())?)?),
    };
    if codec == Codec::MovText && label.is_some() {
        return Err("mov_text takes no charset label".into());
    }
    let count = c.u32()?;
    if count > 4096 {
        return Err("capsule operation limit".into());
    }
    let mut ops = Vec::with_capacity(count as usize);
    for _ in 0..count {
        ops.push(match c.byte()? {
            b'O' => {
                let numerator = c.u32()?;
                let denominator = c.u32()?;
                let width = c.u32()?;
                let height = c.u32()?;
                let present = c.byte()?;
                let size = c.u32()? as usize;
                if present > 1 || (present == 0 && size != 0) {
                    return Err("capsule extradata".into());
                }
                let extradata = match present {
                    1 => Some(Bytes::copy_from_slice(pool, c.take(size)?)?),
                    _ => None,
                };
                Op::Open(Open {
                    timebase: (numerator != 0)
                        .then(|| TimeBase::new(u64::from(numerator), u64::from(denominator)))
                        .transpose()?,
                    width,
                    height,
                    extradata,
                })
            }
            b'P' => {
                let pts = c.known()?;
                let duration = c.known()?;
                let size = c.u32()? as usize;
                Op::Packet(Packet {
                    clock: mov_text::Clock {
                        pts,
                        dts: None,
                        duration,
                    },
                    payload: Bytes::copy_from_slice(pool, c.take(size)?)?,
                })
            }
            b'F' => Op::Flush,
            _ => return Err("capsule operation".into()),
        });
    }
    if c.at != raw.len() || !matches!(ops.first(), Some(Op::Open(_))) {
        return Err("capsule must start with an open and end with its last operation".into());
    }
    Ok(Session { codec, label, ops })
}

/// A decoder of the session's codec, open with one configuration.
// One lives at a time, on the stack: boxing it would add a heap allocation to
// the measured operation.
#[allow(clippy::large_enum_variant)]
enum Decoder {
    MovText(mov_text::Decoder),
    Ass(ass::Decoder),
}

impl Decoder {
    /// The decoder `open` configures, or `None` when the open is refused.
    fn open(
        pool: &Budget,
        session: &Session,
        open: &Open,
        work: &Work,
        cancel: &AtomicBool,
    ) -> Option<Self> {
        match session.codec {
            Codec::MovText => {
                let options = mov_text::Options {
                    packet_timebase: open.timebase,
                    frame_width: open.width,
                    frame_height: open.height,
                };
                let mut decoder = mov_text::Decoder::new(
                    pool,
                    open.extradata.as_ref(),
                    options,
                    mov_text::Limits::default(),
                    0,
                )
                .ok()?;
                for _ in 0..MAX_POLLS {
                    match decoder.poll(&mut work.budget(), cancel).ok()? {
                        mov_text::Status::Pending => {}
                        mov_text::Status::NeedInput => return Some(Self::MovText(decoder)),
                        _ => return None,
                    }
                }
                None
            }
            Codec::Ass => {
                let options = ass::Options {
                    packet_timebase: open.timebase,
                };
                let decoder = ass::Decoder::new(
                    pool,
                    open.extradata.as_ref(),
                    session.label.as_ref(),
                    options,
                    ass::Limits::default(),
                )
                .ok()?;
                Some(Self::Ass(decoder))
            }
        }
    }

    /// Folds the open: for mov_text whether it has a header, then the header.
    fn fold_open(&self, work: &mut Work) {
        match self {
            Self::MovText(decoder) => {
                let header = decoder.subtitle_header();
                work.byte(u8::from(header.is_some()));
                work.framed(header.map_or(&[][..], Bytes::as_ref));
            }
            Self::Ass(decoder) => work.framed(decoder.header()),
        }
    }

    /// Decodes and folds one packet; `Err` when the decoder fails.
    fn packet(
        &mut self,
        open: &Open,
        packet: &Packet,
        cancel: &AtomicBool,
        work: &mut Work,
    ) -> std::result::Result<(), ()> {
        match self {
            Self::MovText(decoder) => mov_text_packet(decoder, open, packet, cancel, work),
            Self::Ass(decoder) => ass_packet(decoder, packet, cancel, work),
        }
    }
}

fn mov_text_packet(
    decoder: &mut mov_text::Decoder,
    open: &Open,
    packet: &Packet,
    cancel: &AtomicBool,
    work: &mut Work,
) -> std::result::Result<(), ()> {
    let sample = Input {
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: packet.payload.clone(),
        codec_id: None,
        codec_private: open.extradata.clone(),
        charset_label: None,
        addition: None,
        timing: Timing::unknown(),
        source_id: SourceId(1),
        generation: 0,
        sequence: 0,
        configuration_epoch: 0,
        container_position: None,
    };
    decoder.provide(sample, packet.clock).map_err(drop)?;
    for _ in 0..MAX_POLLS {
        match decoder.poll(&mut work.budget(), cancel).map_err(drop)? {
            mov_text::Status::Pending => {}
            mov_text::Status::OutputReady => {
                let output = decoder.take_output(cancel).map_err(drop)?;
                work.data(&output.returned.to_le_bytes());
                let Some(subtitle) = output.subtitle else {
                    work.byte(0);
                    return Ok(());
                };
                work.byte(1);
                work.u32(u32::from(subtitle.format()));
                fold_clock(work, &subtitle);
                work.u32(u32::from(subtitle.rectangle_type()));
                for value in subtitle.geometry() {
                    work.data(&value.to_le_bytes());
                }
                work.u32(subtitle.flags());
                work.u32(subtitle.palette_entries());
                work.text(subtitle.ass.as_ref());
                return Ok(());
            }
            _ => return Err(()),
        }
    }
    Err(())
}

fn ass_packet(
    decoder: &mut ass::Decoder,
    packet: &Packet,
    cancel: &AtomicBool,
    work: &mut Work,
) -> std::result::Result<(), ()> {
    decoder
        .provide(&packet.payload, packet.clock)
        .map_err(drop)?;
    for _ in 0..MAX_POLLS {
        match decoder.poll(&mut work.budget(), cancel).map_err(drop)? {
            ass::Status::Pending => {}
            ass::Status::NeedInput => return Err(()),
            ass::Status::Decoded(None) => {
                work.byte(0);
                return Ok(());
            }
            ass::Status::Decoded(Some(subtitle)) => {
                work.byte(1);
                fold_clock(work, &subtitle);
                work.text(subtitle.ass.as_ref());
                return Ok(());
            }
        }
    }
    Err(())
}

/// A subtitle's PTS, display times and rectangle count.
fn fold_clock(work: &mut Work, subtitle: &mov_text::Subtitle) {
    work.byte(u8::from(subtitle.pts_microseconds.is_some()));
    work.i64(subtitle.pts_microseconds.unwrap_or(0));
    work.u32(subtitle.start_display_ms);
    work.u32(subtitle.end_display_ms);
    work.u32(1);
}

/// One measured session: every open, packet and flush in order.
fn session(pool: &Budget, session: &Session, cancel: &AtomicBool, quantum: u64) -> Work {
    let mut work = Work::new(quantum);
    work.data(b"TXT1");
    work.byte(match session.codec {
        Codec::MovText => 1,
        Codec::Ass => 2,
    });
    let mut current: Option<(&Open, Decoder)> = None;
    let mut failed = false;
    for op in &session.ops {
        match op {
            Op::Open(open) => {
                drop(current.take());
                failed = false;
                work.byte(b'O');
                current = Decoder::open(pool, session, open, &work, cancel).map(|d| (open, d));
                work.byte(u8::from(current.is_some()));
                if let Some((_, decoder)) = &current {
                    decoder.fold_open(&mut work);
                }
            }
            Op::Flush => {
                work.byte(b'F');
                if let Some((_, Decoder::MovText(decoder))) = &mut current {
                    failed |= decoder.reset(0, cancel).is_err();
                }
            }
            Op::Packet(packet) => {
                let Some((open, decoder)) = current.as_mut().filter(|_| !failed) else {
                    continue;
                };
                work.byte(b'P');
                if decoder.packet(open, packet, cancel, &mut work).is_err() {
                    work.byte(b'X');
                    failed = true;
                }
            }
        }
    }
    if let Some((_, Decoder::MovText(decoder))) = &mut current {
        let _ = decoder.finish_input(cancel);
    }
    work.byte(b'Z');
    work
}

/// One measured document: every cue the reader publishes, then the outcome.
fn document(
    pool: &Budget,
    raw: &Bytes,
    syntax: document::Syntax,
    cancel: &AtomicBool,
    quantum: u64,
) -> Work {
    let mut work = Work::new(quantum);
    let read = document::Reader::new(pool, raw.clone(), syntax, document::Limits::default())
        .and_then(|mut reader| {
            loop {
                match reader.poll(&mut work.budget(), cancel)? {
                    document::Status::Yield => {}
                    document::Status::Cue(cue) => fold_cue(&mut work, &cue, syntax),
                    document::Status::Finished => return Ok(()),
                }
            }
        });
    work.byte(if read.is_ok() { b'Z' } else { b'F' });
    work
}

fn fold_cue(work: &mut Work, cue: &document::Cue, syntax: document::Syntax) {
    work.byte(b'C');
    work.i64(cue.start_ms);
    work.i64(cue.duration_ms);
    work.text(cue.text.as_ref());
    if syntax == document::Syntax::WebVtt {
        for field in [&cue.identifier, &cue.settings] {
            work.framed(field.as_ref().map_or(&[][..], Bytes::as_ref));
        }
    }
}

/// The input of `KIND`, outside the measured operation.
fn prepare(pool: &Budget, raw: &[u8], kind: &str) -> Result<Prepared> {
    Ok(match kind {
        "packets" => Prepared::Session(parse(pool, raw)?),
        "srt" => Prepared::Document(Bytes::copy_from_slice(pool, raw)?, document::Syntax::SubRip),
        "vtt" => Prepared::Document(Bytes::copy_from_slice(pool, raw)?, document::Syntax::WebVtt),
        _ => return Err("expected KIND packets, srt or vtt".into()),
    })
}

/// One measured operation.
fn operation(pool: &Budget, prepared: &Prepared, cancel: &AtomicBool, quantum: u64) -> Work {
    match prepared {
        Prepared::Session(capsule) => session(pool, capsule, cancel, quantum),
        Prepared::Document(raw, syntax) => document(pool, raw, *syntax, cancel, quantum),
    }
}

/// `subtitle-text-fold FILE KIND`, `subtitle-text-bench FILE N KIND` or
/// `subtitle-text-memory FILE KIND`; KIND is `packets`, `srt` or `vtt`.
pub fn run(args: &[String]) -> Result<()> {
    let invocation = Invocation::parse(args, PREFIX, " KIND", 1..=1, false)?;
    let raw = invocation.input(MAX_INPUT)?;
    let pool = Budget::new(256 * 1024 * 1024)?;
    let prepared = prepare(&pool, &raw, &invocation.arguments[0])?;
    drop(raw);
    let cancel = AtomicBool::new(false);
    measure(&invocation, OPERATION, Some(&pool), || {
        let work = operation(&pool, std::hint::black_box(&prepared), &cancel, QUANTUM);
        Ok(Totals {
            packets: work.packets,
            bytes: work.bytes,
            digest: work.digest,
            // A refusal is folded into the transcript, as the reference folds it.
            refusal: None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::{AtomicBool, Budget, QUANTUM, operation, prepare};
    use crate::json;
    use std::path::Path;

    fn number(value: &json::Value, key: &str) -> u64 {
        let json::Value::Number(value) = value.get(key).unwrap() else {
            panic!("integer required")
        };
        u64::try_from(value.as_i128().unwrap()).unwrap()
    }

    /// Every case of the `subtitle-text` corpus folds as the reference recorded
    /// it, in work slices of any size, and leaves its pool as it found it.
    #[test]
    fn every_case_folds_as_the_reference_recorded_it() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let corpus = json::parse_strict(include_str!(
            "../../../harness/features/subtitle-text/corpus.json"
        ))
        .unwrap();
        let cases = corpus.get("cases").unwrap().as_array().unwrap();
        for case in cases {
            let text = |key: &str| case.get(key).unwrap().as_str().unwrap();
            let expected = case.get("expected").unwrap();
            let kind = case.get("arguments").unwrap().as_array().unwrap()[0]
                .as_str()
                .unwrap();
            let raw = std::fs::read(root.join(text("file"))).unwrap();
            let pool = Budget::new(256 * 1024 * 1024).unwrap();
            let prepared = prepare(&pool, &raw, kind).unwrap();
            let expected = (
                number(expected, "packets"),
                number(expected, "bytes"),
                number(expected, "digest"),
            );
            for quantum in [1, 7, 4096, QUANTUM] {
                let held = pool.used();
                let work = operation(&pool, &prepared, &AtomicBool::new(false), quantum);
                assert_eq!(pool.used(), held, "{} at {quantum}", text("id"));
                let folded = (work.packets, work.bytes, work.digest);
                assert_eq!(folded, expected, "{} at {quantum}", text("id"));
            }
        }
    }
}
