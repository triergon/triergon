//! The packet fold shared by the per-input playback profile runners (`mkv-audio-`,
//! `mp4-forward-`, `ts-program-` and `raw-video-`).
//!
//! Each runner opens one preloaded input per operation, produces every selected
//! packet through the playback engine and folds it with [`Work::packet`]. The FFmpeg
//! reference folds the same record per `AVPacket` (`harness/reference_*.h`).

use crate::runner::{Invocation, MAX_INPUT, Mode, Result, Totals, measure};
use triergon_core::{Budget, Bytes};
use triergon_harness_protocol::digest::{fold, fold_bytes, signed};

/// Packet count, payload bytes and the ordered digest of one or more operations.
#[derive(Default)]
pub struct Work {
    pub packets: u64,
    pub bytes: u64,
    pub digest: u64,
    /// Print each folded record (the `trace` mode); never set in a measured mode.
    trace: bool,
}

/// One delivered packet. Times are in the unit the runner's contract names; an
/// unknown time folds as `i64::MIN` and an unknown duration as zero.
pub struct Packet<I> {
    pub track: u64,
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub duration: u64,
    pub flags: u64,
    pub size: usize,
    pub payload: I,
}

impl Work {
    pub(crate) fn skip_samples(&mut self, bytes: Option<[u8; 10]>) {
        fold(&mut self.digest, u64::from(bytes.is_some()));
        let Some(bytes) = bytes else {
            if self.trace {
                println!("skip -");
            }
            return;
        };
        fold_bytes(&mut self.digest, &bytes);
        if self.trace {
            let leading = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            let trailing = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
            println!("skip {leading} {trailing} {} {}", bytes[8], bytes[9]);
        }
    }

    /// Fold track, PTS, DTS, duration, flags, size and every payload byte.
    pub fn packet<'a, I: IntoIterator<Item = &'a [u8]>>(
        &mut self,
        packet: Packet<I>,
    ) -> Result<()> {
        for word in [
            packet.track,
            signed(packet.pts.unwrap_or(i64::MIN)),
            signed(packet.dts.unwrap_or(i64::MIN)),
            packet.duration,
            packet.flags,
            packet.size as u64,
        ] {
            fold(&mut self.digest, word);
        }
        let mut seen = 0;
        for segment in packet.payload {
            fold_bytes(&mut self.digest, segment);
            seen += segment.len();
        }
        if seen != packet.size {
            return Err("packet payload does not match its size".into());
        }
        if self.trace {
            let time = |value: Option<i64>| value.map_or("-".to_owned(), |v| v.to_string());
            println!(
                "{} {} {} {} {} {}",
                packet.track,
                time(packet.pts),
                time(packet.dts),
                packet.duration,
                packet.flags,
                packet.size
            );
        }
        self.packets += 1;
        self.bytes += seen as u64;
        Ok(())
    }

    fn totals(&self) -> Totals {
        Totals {
            packets: self.packets,
            bytes: self.bytes,
            digest: self.digest,
            refusal: None,
        }
    }
}

/// Run `{prefix}fold FILE ARG`, `{prefix}bench FILE ITERATIONS ARG`,
/// `{prefix}memory FILE ARG` or `{prefix}trace FILE ARG` (`fold` with one line per
/// record: track, PTS, DTS, duration, flags and size) on a preloaded input.
pub fn drive(
    args: &[String],
    prefix: &str,
    operation_name: &str,
    operation: impl Fn(&Budget, &Bytes, &str, Work) -> Result<Work>,
) -> Result<()> {
    let invocation = Invocation::parse(args, prefix, " ARG", 1..=1, true)?;
    let raw = invocation.input(MAX_INPUT)?;
    if raw.is_empty() {
        return Err("input must be a bounded non-empty file".into());
    }
    let pool = Budget::new(256 * 1024 * 1024)?;
    let raw = Bytes::try_from_vec(&pool, raw).map_err(|error| error.error())?;
    let trace = invocation.mode == Mode::Trace;
    measure(&invocation, operation_name, Some(&pool), || {
        let start = Work {
            trace,
            ..Work::default()
        };
        let raw = std::hint::black_box(&raw);
        Ok(operation(&pool, raw, &invocation.arguments[0], start)?.totals())
    })
}

#[cfg(test)]
mod tests {
    use super::{Packet, Work};
    use triergon_core::{Budget, Bytes};
    use triergon_harness_protocol::digest::fold;

    type Operate = fn(&Budget, &Bytes, &str, Work) -> crate::runner::Result<Work>;
    /// Runner, input, argument and the recorded packets, bytes and digest.
    type Case = (Operate, &'static str, &'static str, (u64, u64, u64));

    #[test]
    fn payload_segments_fold_as_one_contiguous_payload() {
        let bytes: Vec<u8> = (0..=255).cycle().take(777).collect();
        let (head, tail) = bytes.split_at(300);
        let packet = |size, payload| Packet {
            track: 1,
            pts: Some(-1),
            dts: None,
            duration: 9,
            flags: 0,
            size,
            payload,
        };
        let mut split = Work::default();
        split.packet(packet(bytes.len(), vec![head, tail])).unwrap();
        let mut whole = Work::default();
        whole.packet(packet(bytes.len(), vec![&bytes[..]])).unwrap();
        assert_eq!(
            (split.packets, split.bytes, split.digest),
            (1, 777, whole.digest)
        );
        let mut short = Work::default();
        assert!(short.packet(packet(778, vec![&bytes[..]])).is_err());
    }

    /// One recorded case per runner, from the reference folds in each contract's corpus.
    #[test]
    fn each_runner_reproduces_a_recorded_reference_fold() {
        let cases: [Case; 7] = [
            (
                crate::matroska_audio::operate,
                "crates/playback/tests/fixtures/mkv-feed/audio-mixed-discovery.mkv",
                "all",
                (21, 7281, 18_368_739_302_135_961_132),
            ),
            (
                crate::matroska_audio::operate,
                "crates/playback/tests/fixtures/dolby-clock/laced-fractional-cadence.mkv",
                "all",
                (8, 3342, 9_736_466_787_568_309_955),
            ),
            (
                crate::raw_video::operate,
                "crates/framing/fixtures/raw-annex-b/hevc-aud0.hevc",
                "hevc",
                (12, 15780, 6_234_071_059_473_453_237),
            ),
            (
                crate::ts_program::operate,
                "crates/playback/tests/fixtures/ts/program-private-avc-dvb.ts",
                "ts188:256=avc,257=dvb-ac3,258=dvbsub",
                (3, 180, 8_135_355_048_293_450_801),
            ),
            (
                crate::ts_program::operate,
                "harness/fixtures/ts-program/replay-avc-aac-00.m2ts",
                "m2ts192:256=avc",
                (24, 1673, 11_987_636_495_647_279_255),
            ),
            (
                crate::mp4_forward::operate,
                "crates/playback/tests/fixtures/mp4/r-multitrack.mov",
                "regular",
                (12, 105, 8_017_515_978_755_924_262),
            ),
            (
                crate::mp4_forward::operate,
                "crates/playback/tests/fixtures/progressive/pending-pair.mp4",
                "progressive",
                (9, 108, 11_753_693_190_476_751_358),
            ),
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for (operate, file, argument, expected) in cases {
            let pool = Budget::new(64 << 20).unwrap();
            let raw = Bytes::try_from_vec(&pool, std::fs::read(root.join(file)).unwrap()).unwrap();
            let retained = pool.used();
            let work = operate(&pool, &raw, argument, Work::default()).unwrap();
            // The report folds each operation's digest into an empty total.
            let mut total = 0;
            fold(&mut total, work.digest);
            assert_eq!((work.packets, work.bytes, total), expected, "{file}");
            assert_eq!(pool.used(), retained, "{file} retained charged storage");
        }
    }
}
