//! PGS display sets through the FFmpeg-compatible receiver
//! (`triergon_subtitles::compat::pgs::ReceiverDecoder`), the same operation as the
//! reference worker's `pgssub` modes.
//!
//! The input is a case capsule (`SMBMO001`): the packet time base, the forced-only
//! option and up to 64 records, each a complete PGS packet with its raw PTS and DTS
//! or a hard reset that reopens the decoder. Every packet is decoded and each
//! published subtitle is folded: its packet, consumed bytes, PTS in microseconds,
//! display window and canvas, and per rectangle the geometry, forced flag, stride,
//! index plane, the 256-entry ARGB palette and the ARGB value of every pixel. A
//! decoder refusal ends the case with a refusal marker. The digest is the ordered
//! fold of the byte transcript that the recorded cases under
//! `crates/subtitles/tests/native-compatibility-gold/` store as `expected.bin`.

use crate::runner::{Invocation, Result, Totals, measure};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, TimeBase, WorkBudget};
use triergon_harness_protocol::digest::fold_bytes;
use triergon_subtitles::compat::pgs::ReceiverDecoder;
use triergon_subtitles::pgs::{DisplayConfig, DisplayLimits, PacketClock, RecoveryStatus};

pub const PREFIX: &str = "pgs-";
const OPERATION: &str = "pgs-receiver-display";
const MAX_INPUT: usize = 16 * 1024 * 1024;
const MAX_RECORDS: usize = 64;
const MAX_PIXELS: u64 = 8 * 1024 * 1024;
const MAX_POLLS: usize = 1_000_000;
/// `31^4`: folding one little-endian ARGB word is `h * 31^4 + word fold`.
const STEP_4: u64 = 923_521;

struct Record {
    reset: bool,
    pts: Option<i64>,
    dts: Option<i64>,
    /// The record's transcript header: `P`, index, reset, PTS and DTS facts, size.
    wire: [u8; 28],
    payload: Bytes,
}

struct Case {
    header: Vec<u8>,
    config: DisplayConfig,
    timebase: Option<TimeBase>,
    records: Vec<Record>,
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
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }
    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into()?))
    }
}

/// Parse a capsule, load its payloads and build its transcript header, all
/// outside the measured operation.
fn parse(pool: &Budget, raw: &[u8]) -> Result<Case> {
    let mut c = Cursor { bytes: raw, at: 0 };
    if c.take(8)? != b"SMBMO001" || c.byte()? != 1 {
        return Err("not a PGS case capsule".into());
    }
    let flags = c.u32()?;
    let numerator = c.u32()?;
    let denominator = c.u32()?;
    if flags & !1 != 0
        || numerator == 0
        || denominator == 0
        || numerator > i32::MAX as u32
        || denominator > i32::MAX as u32
    {
        return Err("capsule configuration".into());
    }
    // The receiver takes no private configuration; neither does the reference.
    if c.u32()? != 0 {
        return Err("capsule private configuration".into());
    }
    let count = c.u32()?;
    if count == 0 || count as usize > MAX_RECORDS {
        return Err("capsule record limit".into());
    }
    let mut header = Vec::with_capacity(29);
    header.extend_from_slice(b"SMBOUT02");
    header.push(1);
    for word in [flags, numerator, denominator, 0, count] {
        header.extend_from_slice(&word.to_le_bytes());
    }
    let mut records = Vec::with_capacity(count as usize);
    let mut total = 0usize;
    for index in 0..count {
        let reset = c.byte()?;
        let pts_known = c.byte()?;
        let pts = c.i64()?;
        let dts_known = c.byte()?;
        let dts = c.i64()?;
        let size = c.u32()?;
        total = total
            .checked_add(size as usize)
            .ok_or("capsule payload overflow")?;
        if (reset != 0 && reset != 2)
            || pts_known > 1
            || dts_known > 1
            || total > MAX_INPUT
            || (reset != 0) != (size == 0)
        {
            return Err("capsule record".into());
        }
        let mut wire = [0; 28];
        wire[0] = b'P';
        wire[1..5].copy_from_slice(&index.to_le_bytes());
        wire[5] = reset;
        wire[6] = pts_known;
        wire[7..15].copy_from_slice(&pts.to_le_bytes());
        wire[15] = dts_known;
        wire[16..24].copy_from_slice(&dts.to_le_bytes());
        wire[24..].copy_from_slice(&size.to_le_bytes());
        records.push(Record {
            reset: reset != 0,
            pts: (pts_known != 0).then_some(pts),
            dts: (dts_known != 0).then_some(dts),
            wire,
            payload: Bytes::copy_from_slice(pool, c.take(size as usize)?)?,
        });
    }
    if c.at != raw.len() {
        return Err("trailing capsule bytes".into());
    }
    Ok(Case {
        header,
        config: DisplayConfig {
            receiver_forced_only: flags & 1 != 0,
        },
        timebase: TimeBase::new(u64::from(numerator), u64::from(denominator)).ok(),
        records,
    })
}

#[derive(Default)]
struct Work {
    digest: u64,
    events: u64,
    pixels: u64,
}

impl Work {
    fn data(&mut self, bytes: &[u8]) {
        fold_bytes(&mut self.digest, bytes);
    }
    fn u32(&mut self, value: u32) {
        self.data(&value.to_le_bytes());
    }
    fn i32(&mut self, value: i32) {
        self.data(&value.to_le_bytes());
    }

    /// One rectangle: validated completely before any of it is folded.
    fn rectangle(
        &mut self,
        order: usize,
        geometry: [i32; 6],
        palette: &[u32; 256],
        pixels: &[u8],
    ) -> std::result::Result<(), ()> {
        let [_, _, width, height, _, stride] = geometry;
        let count = u64::from(width.unsigned_abs()) * u64::from(height.unsigned_abs());
        if width < 0
            || height < 0
            || stride < width
            || pixels.len() as u64 != count
            || count > MAX_PIXELS
            || self.pixels + count > MAX_PIXELS
        {
            return Err(());
        }
        self.u32(order as u32);
        for value in geometry {
            self.i32(value);
        }
        self.u32(256);
        self.data(&count.to_le_bytes());
        self.data(pixels);
        let mut words = [0u64; 256];
        for (word, colour) in words.iter_mut().zip(palette) {
            self.u32(*colour);
            let [a, b, c, d] = colour.to_le_bytes();
            *word = u64::from(a) * 29_791 + u64::from(b) * 961 + u64::from(c) * 31 + u64::from(d);
        }
        for &index in pixels {
            self.digest = self
                .digest
                .wrapping_mul(STEP_4)
                .wrapping_add(words[usize::from(index)]);
        }
        self.pixels += count;
        Ok(())
    }
}

fn open(pool: &Budget, case: &Case) -> std::result::Result<ReceiverDecoder, ()> {
    ReceiverDecoder::new(pool, DisplayLimits::default(), case.config).map_err(drop)
}

/// Decode every record, folding each published subtitle. `Err` is a refusal.
fn decode(
    pool: &Budget,
    case: &Case,
    cancel: &AtomicBool,
    work: &mut Work,
) -> std::result::Result<(), ()> {
    let timebase = case.timebase.ok_or(())?;
    let mut decoder = open(pool, case)?;
    for (index, record) in case.records.iter().enumerate() {
        if record.reset {
            drop(decoder);
            decoder = open(pool, case)?;
            continue;
        }
        let clock = PacketClock {
            pts: record.pts,
            dts: record.dts,
            timebase,
        };
        let mut owned = Some(record.payload.clone());
        decoder.push(&mut owned, clock, cancel).map_err(drop)?;
        let mut output = None;
        for _ in 0..MAX_POLLS {
            let status = decoder
                .poll(&mut WorkBudget::new(1 << 20), cancel)
                .map_err(drop)?;
            if status == RecoveryStatus::OutputReady {
                output = Some(decoder.take_output(cancel).map_err(drop)?);
                break;
            }
        }
        let output = output.ok_or(())?;
        let Some(event) = output.receiver_output() else {
            continue;
        };
        let size = i32::try_from(record.payload.len()).map_err(drop)?;
        work.data(b"E");
        work.u32(index as u32);
        work.u32(0);
        work.i32(size);
        work.data(&[0, 0, 0]);
        work.data(&[u8::from(event.pts_microseconds.is_some())]);
        work.data(&event.pts_microseconds.unwrap_or(0).to_le_bytes());
        work.u32(0);
        work.u32(u32::MAX);
        work.i32(i32::from(output.context_canvas[0]));
        work.i32(i32::from(output.context_canvas[1]));
        work.u32(event.rectangles().len() as u32);
        work.events += 1;
        for (order, r) in event.rectangles().iter().enumerate() {
            work.rectangle(
                order,
                [
                    i32::from(r.x),
                    i32::from(r.y),
                    i32::from(r.width),
                    i32::from(r.height),
                    i32::from(r.forced),
                    i32::from(r.stride),
                ],
                r.palette(),
                r.pixels().as_ref(),
            )?;
        }
    }
    decoder.finish_input(cancel).map_err(drop)?;
    for _ in 0..MAX_POLLS {
        if decoder
            .poll(&mut WorkBudget::new(1 << 20), cancel)
            .map_err(drop)?
            == RecoveryStatus::Ended
        {
            return Ok(());
        }
    }
    Err(())
}

/// One measured operation: the transcript header, every record, then the decode.
fn operation(pool: &Budget, case: &Case, cancel: &AtomicBool) -> Work {
    let mut work = Work::default();
    work.data(&case.header);
    for record in &case.records {
        work.data(&record.wire);
        work.data(record.payload.as_ref());
    }
    let refused = decode(pool, case, cancel, &mut work).is_err();
    work.data(if refused { b"F" } else { b"Z" });
    work
}

/// `pgs-fold FILE`, `pgs-bench FILE ITERATIONS` or `pgs-memory FILE`.
pub fn run(args: &[String]) -> Result<()> {
    let invocation = Invocation::parse(args, PREFIX, "", 0..=0, false)?;
    let raw = invocation.input((MAX_INPUT + 65536 + 4096) as u64)?;
    let pool = Budget::new(256 * 1024 * 1024)?;
    let case = parse(&pool, &raw)?;
    drop(raw);
    let cancel = AtomicBool::new(false);
    measure(&invocation, OPERATION, &pool, || {
        let work = operation(&pool, std::hint::black_box(&case), &cancel);
        Ok(Totals {
            packets: work.events,
            bytes: work.pixels,
            digest: work.digest,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::{Budget, operation, parse};
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use triergon_harness_protocol::digest::fold_bytes;

    /// Every recorded case folds to the digest of its recorded transcript.
    #[test]
    fn fold_equals_the_digest_of_every_recorded_transcript() {
        let gold = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../subtitles/tests/native-compatibility-gold");
        let mut cases = 0;
        for entry in std::fs::read_dir(&gold).unwrap() {
            let dir = entry.unwrap().path();
            if !dir.is_dir() {
                continue;
            }
            let pool = Budget::new(256 * 1024 * 1024).unwrap();
            let case = parse(&pool, &std::fs::read(dir.join("input.bin")).unwrap()).unwrap();
            let mut expected = 0;
            fold_bytes(
                &mut expected,
                &std::fs::read(dir.join("expected.bin")).unwrap(),
            );
            let work = operation(&pool, &case, &AtomicBool::new(false));
            assert_eq!(work.digest, expected, "{}", dir.display());
            cases += 1;
        }
        assert_eq!(cases, 229);
    }
}
