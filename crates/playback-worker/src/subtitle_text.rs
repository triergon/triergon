//! Text subtitles, the same operations as the reference worker's subtitle-text modes.
//!
//! - `packets`: a session capsule (`TRSTXT01`) of decoder opens, packets and
//!   flushes. tx3g samples go through `compat::mov_text::Decoder`, folding each
//!   call's return value and the complete ASS subtitle. ASS packets go through the
//!   opaque ASS extractor, or, with a charset label, through
//!   `compat::charset::AssPacketJob` with the glibc receiver repertoire, folding
//!   the published text and its clock.
//! - `srt` and `vtt`: one SubRip or WebVTT document through `text::Extractor` with
//!   the stated encoding, folding every cue's start and duration in milliseconds,
//!   its text and, for WebVTT, its identifier and settings.
//!
//! The digest is the ordered fold of a little-endian byte transcript; a refused
//! document ends with a refusal marker.

use crate::runner::{Invocation, Result, Totals, measure};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Rounding, SourceId, Time, TimeBase, WorkBudget};
use triergon_harness_protocol::digest::fold_bytes;
use triergon_subtitles::compat::charset::receiver_charset_with_glibc_euc_kr;
use triergon_subtitles::compat::charset::{AssPacketJob, Outcome, PacketStatus};
use triergon_subtitles::compat::mov_text::{
    Clock, Decoder, Limits as MovTextLimits, Options, Status as MovTextStatus,
};
use triergon_subtitles::text::{
    Cue, Encoding, Event, Extractor, Format, Input, Limits, RawClock, Status, Timing,
};

pub const PREFIX: &str = "subtitle-text-";
const OPERATION: &str = "subtitle-text-cues";
const MAX_INPUT: u64 = 16 * 1024 * 1024;
const MAX_STEPS: usize = 4_000_000;
const QUANTUM: u64 = 1 << 20;
const MILLISECONDS: u64 = 1000;
const MICROSECONDS: u64 = 1_000_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Codec {
    MovText,
    Ass,
}

struct Open {
    timebase: Option<TimeBase>,
    raw_timebase: (u32, u32),
    width: u32,
    height: u32,
    extradata: Option<Bytes>,
}

struct Packet {
    pts: Option<i64>,
    duration: Option<i64>,
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
    Document(Bytes, Document),
}

#[derive(Clone, Copy)]
enum Document {
    SubRip(Encoding),
    WebVtt(Encoding),
}

#[derive(Default)]
struct Work {
    digest: u64,
    packets: u64,
    bytes: u64,
}

impl Work {
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
    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into()?))
    }
    fn known(&mut self) -> Result<Option<i64>> {
        let known = self.byte()?;
        let value = self.i64()?;
        match known {
            0 => Ok(None),
            1 => Ok(Some(value)),
            _ => Err("capsule clock flag".into()),
        }
    }
}

/// Parse a session capsule and load every byte string outside the measured operation.
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
    let label = c.u16()? as usize;
    let label = if label == 0 {
        None
    } else {
        Some(Bytes::copy_from_slice(pool, c.take(label)?)?)
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
                let extradata = if present == 1 {
                    Some(Bytes::copy_from_slice(pool, c.take(size)?)?)
                } else {
                    None
                };
                Op::Open(Open {
                    timebase: if numerator == 0 {
                        None
                    } else {
                        Some(TimeBase::new(u64::from(numerator), u64::from(denominator))?)
                    },
                    raw_timebase: (numerator, denominator),
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
                    pts,
                    duration,
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

fn input(format: Format, encoding: Encoding, payload: &Bytes, timing: Timing) -> Input {
    Input {
        format,
        encoding,
        payload: payload.clone(),
        codec_id: None,
        codec_private: None,
        charset_label: None,
        addition: None,
        timing,
        source_id: SourceId(1),
        generation: 0,
        sequence: 0,
        configuration_epoch: 0,
        container_position: None,
    }
}

/// An exact time as whole `1/per_second` units, or `None` when it is not whole.
fn exact(time: Time, per_second: u64) -> Option<i64> {
    let base = TimeBase::new(1, per_second).ok()?;
    let floor = time.to_i64_ticks(base, Rounding::Floor).ok()?;
    (time.to_i64_ticks(base, Rounding::Ceil).ok()? == floor).then_some(floor)
}

/// The mov_text decoder of the current open, or the reason it did not open.
fn open_mov_text(pool: &Budget, open: &Open, cancel: &AtomicBool) -> Option<Decoder> {
    let options = Options {
        packet_timebase: open.timebase,
        frame_width: open.width,
        frame_height: open.height,
    };
    let mut decoder = Decoder::new(
        pool,
        open.extradata.as_ref(),
        options,
        MovTextLimits::default(),
        0,
    )
    .ok()?;
    for _ in 0..MAX_STEPS {
        match decoder.poll(&mut WorkBudget::new(QUANTUM), cancel).ok()? {
            MovTextStatus::Pending => {}
            MovTextStatus::NeedInput => return Some(decoder),
            _ => return None,
        }
    }
    None
}

fn mov_text_packet(
    decoder: &mut Decoder,
    open: &Open,
    packet: &Packet,
    cancel: &AtomicBool,
    work: &mut Work,
) -> std::result::Result<(), ()> {
    let mut sample = input(
        Format::Tx3g,
        Encoding::Tx3gInBand,
        &packet.payload,
        Timing::unknown(),
    );
    sample.codec_private.clone_from(&open.extradata);
    let clock = Clock {
        pts: packet.pts,
        dts: None,
        duration: packet.duration,
    };
    decoder.provide(sample, clock).map_err(drop)?;
    for _ in 0..MAX_STEPS {
        match decoder
            .poll(&mut WorkBudget::new(QUANTUM), cancel)
            .map_err(drop)?
        {
            MovTextStatus::Pending => {}
            MovTextStatus::OutputReady => {
                let output = decoder.take_output(cancel).map_err(drop)?;
                work.data(&output.returned.to_le_bytes());
                let Some(subtitle) = output.subtitle else {
                    work.byte(0);
                    return Ok(());
                };
                work.byte(1);
                work.u32(u32::from(subtitle.format()));
                work.byte(u8::from(subtitle.pts_microseconds.is_some()));
                work.i64(subtitle.pts_microseconds.unwrap_or(0));
                work.u32(subtitle.start_display_ms);
                work.u32(subtitle.end_display_ms);
                work.u32(1);
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

/// The published text and clock of one ASS packet, folded as the decoder's
/// subtitle: PTS in microseconds, start 0 and the duration in milliseconds.
fn ass_subtitle(work: &mut Work, cue: &Cue, text: Option<&[u8]>) -> std::result::Result<(), ()> {
    let Some(text) = text else {
        work.byte(0);
        return Ok(());
    };
    let pts = cue.start.map(|start| exact(start, MICROSECONDS).ok_or(()));
    let end = cue.duration.map_or(Ok(0), |duration| {
        exact(duration, MILLISECONDS)
            .and_then(|ms| u32::try_from(ms).ok())
            .ok_or(())
    })?;
    work.byte(1);
    work.byte(u8::from(pts.is_some()));
    work.i64(pts.transpose()?.unwrap_or(0));
    work.u32(0);
    work.u32(end);
    work.u32(1);
    work.text(text);
    Ok(())
}

fn ass_packet(
    pool: &Budget,
    open: &Open,
    label: Option<&Bytes>,
    packet: &Packet,
    cancel: &AtomicBool,
    work: &mut Work,
) -> std::result::Result<(), ()> {
    let timing = match open.timebase {
        Some(_) if packet.pts.is_some() || packet.duration.is_some() => {
            Timing::from_ticks(RawClock {
                numerator: u64::from(open.raw_timebase.0),
                denominator: u64::from(open.raw_timebase.1),
                pts_ticks: packet.pts.map(i128::from),
                dts_ticks: None,
                duration_ticks: packet.duration.map(i128::from),
            })
            .map_err(drop)?
        }
        _ => Timing::unknown(),
    };
    let mut source = input(Format::OpaqueAss, Encoding::Opaque, &packet.payload, timing);
    source.codec_private.clone_from(&open.extradata);
    if let Some(label) = label {
        source.charset_label = Some(label.clone());
        let mut job = AssPacketJob::new_with_glibc_euc_kr(source, pool, Limits::default(), 0)
            .map_err(drop)?;
        for _ in 0..MAX_STEPS {
            if job
                .poll(&mut WorkBudget::new(QUANTUM), cancel)
                .map_err(drop)?
                == PacketStatus::Ready
            {
                let view = job.take_output(cancel).map_err(drop)?;
                let text = match view.outcome() {
                    Outcome::Converted => view.visible(),
                    Outcome::UnsupportedLabel | Outcome::ConversionRefused(_) => None,
                };
                return ass_subtitle(work, view.source(), text);
            }
        }
        return Err(());
    }
    let mut extractor = Extractor::new(pool, Limits::default(), 0);
    extractor.provide(source).map_err(drop)?;
    extractor.finish_input();
    let mut cue = None;
    for _ in 0..MAX_STEPS {
        match extractor
            .step(&mut WorkBudget::new(QUANTUM), cancel)
            .map_err(drop)?
        {
            Status::Event(Event::Cue(published)) => cue = Some(published),
            Status::Event(Event::Metadata(_)) | Status::Yield => {}
            Status::NeedInput | Status::Finished => {
                let cue = cue.ok_or(())?;
                let text = &cue.input.payload.as_ref()[cue.text_range.clone()];
                return ass_subtitle(work, &cue, Some(text));
            }
            Status::NeedOutput => return Err(()),
        }
    }
    Err(())
}

/// One measured session: every open, packet and flush in order.
fn session(pool: &Budget, session: &Session, cancel: &AtomicBool) -> Work {
    let mut work = Work::default();
    work.data(b"TXT1");
    work.byte(match session.codec {
        Codec::MovText => 1,
        Codec::Ass => 2,
    });
    let mut open: Option<&Open> = None;
    let mut decoder: Option<Decoder> = None;
    let mut opened = false;
    let mut failed = false;
    for op in &session.ops {
        match op {
            Op::Open(next) => {
                drop(decoder.take());
                open = Some(next);
                failed = false;
                work.byte(b'O');
                let header = match session.codec {
                    Codec::MovText => {
                        decoder = open_mov_text(pool, next, cancel);
                        opened = decoder.is_some();
                        decoder
                            .as_ref()
                            .and_then(Decoder::subtitle_header)
                            .map(|header| header.as_ref().to_vec())
                    }
                    Codec::Ass => {
                        opened = session
                            .label
                            .as_ref()
                            .is_none_or(|label| receiver_charset_with_glibc_euc_kr(label).is_ok());
                        next.extradata.as_ref().map(|data| data.as_ref().to_vec())
                    }
                };
                work.byte(u8::from(opened));
                if opened {
                    // The ASS header is the extradata, compared by its bytes only.
                    if session.codec == Codec::MovText {
                        work.byte(u8::from(header.is_some()));
                    }
                    work.framed(header.as_deref().unwrap_or_default());
                }
            }
            Op::Flush => {
                work.byte(b'F');
                if let Some(decoder) = &mut decoder {
                    failed |= decoder.reset(0, cancel).is_err();
                }
            }
            Op::Packet(packet) => {
                let Some(open) = open.filter(|_| opened && !failed) else {
                    continue;
                };
                work.byte(b'P');
                let result = match (session.codec, &mut decoder) {
                    (Codec::MovText, Some(decoder)) => {
                        mov_text_packet(decoder, open, packet, cancel, &mut work)
                    }
                    (Codec::Ass, _) => ass_packet(
                        pool,
                        open,
                        session.label.as_ref(),
                        packet,
                        cancel,
                        &mut work,
                    ),
                    (Codec::MovText, None) => Err(()),
                };
                if result.is_err() {
                    work.byte(b'X');
                    failed = true;
                }
            }
        }
    }
    if let Some(decoder) = &mut decoder {
        let _ = decoder.finish_input(cancel);
    }
    work.byte(b'Z');
    work
}

/// One measured document: every cue the extractor publishes, then the outcome.
fn document(pool: &Budget, raw: &Bytes, kind: Document, cancel: &AtomicBool) -> Work {
    let mut work = Work::default();
    let (format, encoding, vtt) = match kind {
        Document::SubRip(encoding) => (Format::SubRip, encoding, false),
        Document::WebVtt(encoding) => (Format::WebVtt, encoding, true),
    };
    let mut extractor = Extractor::new(pool, Limits::default(), 0);
    let mut refused = extractor
        .provide(input(format, encoding, raw, Timing::unknown()))
        .is_err();
    extractor.finish_input();
    let mut steps = 0;
    while !refused {
        steps += 1;
        match extractor.step(&mut WorkBudget::new(QUANTUM), cancel) {
            Ok(Status::Event(Event::Cue(cue))) => {
                refused = fold_cue(&mut work, &cue, vtt).is_err();
            }
            Ok(Status::Event(Event::Metadata(_)) | Status::Yield) if steps < MAX_STEPS => {}
            Ok(Status::NeedInput | Status::Finished) => break,
            _ => refused = true,
        }
    }
    work.byte(if refused { b'F' } else { b'Z' });
    work
}

fn fold_cue(work: &mut Work, cue: &Cue, vtt: bool) -> std::result::Result<(), ()> {
    let start = cue.start.and_then(|t| exact(t, MILLISECONDS)).ok_or(())?;
    let duration = cue
        .duration
        .and_then(|t| exact(t, MILLISECONDS))
        .ok_or(())?;
    let text = cue.decoded_utf8.as_ref().ok_or(())?;
    work.byte(b'C');
    work.i64(start);
    work.i64(duration);
    work.text(text.as_ref());
    if vtt {
        for field in [&cue.identifier, &cue.settings] {
            let bytes = match field {
                Some(field) => Some(field.view(&cue.input).map_err(drop)?),
                None => None,
            };
            work.framed(bytes.as_ref().map_or(&[][..], Bytes::as_ref));
        }
    }
    Ok(())
}

fn encoding(name: Option<&String>) -> Result<Encoding> {
    Ok(match name.map(String::as_str) {
        None | Some("utf8") => Encoding::Utf8,
        Some("utf16-bom") => Encoding::Utf16Bom,
        Some("utf16be") => Encoding::Utf16Be,
        Some("utf16le") => Encoding::Utf16Le,
        Some(_) => return Err("expected utf8, utf16-bom, utf16be or utf16le".into()),
    })
}

/// `subtitle-text-fold FILE KIND [ENCODING]`, `subtitle-text-bench FILE N KIND
/// [ENCODING]` or `subtitle-text-memory FILE KIND [ENCODING]`; KIND is `packets`,
/// `srt` or `vtt`.
pub fn run(args: &[String]) -> Result<()> {
    let invocation = Invocation::parse(args, PREFIX, " KIND [ENCODING]", 1..=2, false)?;
    let rest = invocation.arguments;
    let raw = invocation.input(MAX_INPUT)?;
    let pool = Budget::new(256 * 1024 * 1024)?;
    let operation = match rest[0].as_str() {
        "packets" if rest.len() == 1 => Prepared::Session(parse(&pool, &raw)?),
        "srt" => Prepared::Document(
            Bytes::copy_from_slice(&pool, &raw)?,
            Document::SubRip(encoding(rest.get(1))?),
        ),
        "vtt" => Prepared::Document(
            Bytes::copy_from_slice(&pool, &raw)?,
            Document::WebVtt(encoding(rest.get(1))?),
        ),
        _ => return Err("expected KIND packets, srt or vtt".into()),
    };
    drop(raw);
    let cancel = AtomicBool::new(false);
    measure(&invocation, OPERATION, &pool, || {
        let work = match std::hint::black_box(&operation) {
            Prepared::Session(capsule) => session(&pool, capsule, &cancel),
            Prepared::Document(raw, kind) => document(&pool, raw, *kind, &cancel),
        };
        Ok(Totals {
            packets: work.packets,
            bytes: work.bytes,
            digest: work.digest,
        })
    })
}
