//! One independently addressable TS188 HLS segment through the playback engine:
//! open it from a preloaded forward-only source, select one video and one audio
//! stream, retain every unit through the end of the segment, resolve the
//! application audio clock, materialize each contiguous video feed, and fold
//! video units first and audio units second, followed by their presentation tails.

use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;
use triergon_core::{Budget, Bytes, InputCursor, SourceCapabilities, SourceId, WorkBudget};
use triergon_playback::{
    ByteSource,
    hls_clock::HlsParserOnlyAudioClock,
    hls_segment::{
        HlsAudioCodec, HlsFeedUnit, HlsMediaProfile, HlsMediaSegment, HlsMediaSegmentReader,
        HlsSegment, HlsSegmentLimits, HlsSegmentProgress, HlsSegmentReader, HlsSegmentTracks,
        HlsVideoCodec,
    },
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const MAX_INPUT: u64 = 64 * 1024 * 1024;
const OPERATION: &str = "hls-segment-feed";

// One reader lives on the stack per operation; boxing the larger variant would add a
// measured heap allocation to every operation.
#[allow(clippy::large_enum_variant)]
enum Reader {
    AvcAac(HlsSegmentReader<ByteSource>),
    Family(HlsMediaSegmentReader<ByteSource>),
}

enum Segment {
    AvcAac(HlsSegment<HlsParserOnlyAudioClock>),
    Family(HlsMediaSegment),
}

#[derive(Default)]
struct Work {
    digest: u64,
    packets: usize,
    bytes: usize,
}

impl Work {
    fn mix(&mut self, word: u64) {
        self.digest = self.digest.wrapping_mul(31).wrapping_add(word);
    }

    fn fields(&mut self, fields: &[u64]) {
        for &field in fields {
            self.mix(field);
        }
    }

    fn payload(&mut self, data: &[u8]) -> Result<()> {
        for &byte in data {
            self.mix(u64::from(byte));
        }
        self.bytes += data.len();
        Ok(())
    }

    fn video(
        &mut self,
        pts: i64,
        dts: i64,
        rap: bool,
        raster: (u32, u32),
        bytes: &[u8],
    ) -> Result<()> {
        let (width, height) = raster;
        let rap = u8::from(rap);
        self.fields(&[
            1,
            pts as u64,
            dts as u64,
            rap.into(),
            width.into(),
            height.into(),
            bytes.len() as u64,
        ]);
        self.payload(bytes)?;
        self.end_packet()
    }

    fn audio(&mut self, pts: i64, duration: i64, bytes: &[u8]) -> Result<()> {
        self.fields(&[2, pts as u64, duration as u64, bytes.len() as u64]);
        self.payload(bytes)?;
        self.end_packet()
    }

    fn end_packet(&mut self) -> Result<()> {
        self.packets += 1;
        Ok(())
    }
}

fn open_segment(pool: &Budget, raw: &Bytes, profile: HlsMediaProfile) -> Result<Segment> {
    let id = SourceId(1);
    let input = InputCursor::new(
        pool,
        id,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: false,
        },
        32768,
    )?;
    let source = ByteSource::new(id, raw.clone(), 32768)?;
    let limits = HlsSegmentLimits::default();
    let mut reader =
        if profile.video == HlsVideoCodec::Avc && profile.audio == Some(HlsAudioCodec::Aac) {
            Reader::AvcAac(
                HlsSegmentReader::new(input, source, HlsSegmentTracks::default(), limits)
                    .map_err(|error| format!("open AVC/AAC segment: {error:?}"))?,
            )
        } else {
            Reader::Family(
                HlsMediaSegmentReader::new(input, source, profile, limits)
                    .map_err(|error| format!("open family segment: {error:?}"))?,
            )
        };
    let cancel = AtomicBool::new(false);
    for _ in 0..2_000_000 {
        let mut budget = WorkBudget::new(1 << 20);
        let progress = match &mut reader {
            Reader::AvcAac(reader) => reader.advance(&mut budget, &cancel),
            Reader::Family(reader) => reader.advance(&mut budget, &cancel),
        }
        .map_err(|error| format!("read segment: {error:?}"))?;
        match progress {
            HlsSegmentProgress::Ready => {
                let segment = match &mut reader {
                    Reader::AvcAac(reader) => reader
                        .take_parser_only(&cancel)
                        .map(|s| s.map(Segment::AvcAac)),
                    Reader::Family(reader) => reader.take(&cancel).map(|s| s.map(Segment::Family)),
                }
                .map_err(|error| format!("take sealed segment: {error:?}"))?;
                return segment.ok_or_else(|| "missing sealed segment".into());
            }
            HlsSegmentProgress::Yield | HlsSegmentProgress::NeedInput(_) => {}
            other @ HlsSegmentProgress::Backpressure { .. } => {
                return Err(format!("unexpected preloaded source state: {other:?}").into());
            }
        }
    }
    Err("segment never reached logical EOF".into())
}

fn measure(pool: &Budget, segment: Segment) -> Result<Work> {
    let mut work = Work::default();
    let (video_tail, audio_tail, feed_bytes, count) = match segment {
        Segment::AvcAac(segment) => {
            for video_pass in [true, false] {
                for unit in segment.access_units() {
                    match unit {
                        HlsFeedUnit::Video(unit) if video_pass => {
                            let clock = unit.clock.ok_or("missing AVC PES clock")?;
                            let bytes = unit.contiguous_feed(pool)?;
                            work.video(
                                clock.pts90.ok_or("missing AVC PTS")?,
                                clock.dts90.ok_or("missing AVC DTS")?,
                                unit.idr,
                                (unit.raster.width, unit.raster.height),
                                bytes.as_ref(),
                            )?;
                        }
                        HlsFeedUnit::Audio(unit, clock) if !video_pass => {
                            work.audio(clock.pts_ns, clock.duration_ns, unit.bytes.as_ref())?;
                        }
                        _ => {}
                    }
                }
            }
            (
                segment.video_tail90(),
                segment.audio_tail_ns(),
                segment.feed_bytes(),
                segment.access_units().len(),
            )
        }
        Segment::Family(segment) => {
            let raster = segment.initial_raster();
            for video_pass in [true, false] {
                for unit in segment.access_units() {
                    match unit {
                        HlsFeedUnit::Video(unit) if video_pass => {
                            let clock = unit.clock().ok_or("missing video PES clock")?;
                            let bytes = unit.contiguous_feed(pool)?;
                            work.video(
                                clock.pts90.ok_or("missing video PTS")?,
                                clock.dts90.ok_or("missing video DTS")?,
                                unit.random_access(),
                                (raster.width, raster.height),
                                bytes.as_ref(),
                            )?;
                        }
                        HlsFeedUnit::Audio(unit, clock) if !video_pass => {
                            work.audio(clock.pts_ns, clock.duration_ns, unit.bytes.as_ref())?;
                        }
                        _ => {}
                    }
                }
            }
            (
                segment.video_tail90(),
                segment.audio_tail_ns(),
                segment.feed_bytes(),
                segment.access_units().len(),
            )
        }
    };
    if work.bytes != feed_bytes || work.packets != count {
        return Err("fold did not account for all sealed feed bytes and units".into());
    }
    work.fields(&[
        video_tail as u64,
        audio_tail.ok_or("no selected audio")? as u64,
    ]);
    Ok(work)
}

/// `hls-bench FILE ITERATIONS VIDEO AUDIO`, `hls-fold FILE VIDEO AUDIO` or
/// `hls-memory FILE VIDEO AUDIO`; VIDEO is `avc` or `hevc`, AUDIO `aac`, `ac3` or `eac3`.
pub fn run(args: &[String]) -> Result<()> {
    let usage = "usage: playback-worker hls-bench FILE N VIDEO AUDIO | hls-fold FILE VIDEO AUDIO | hls-memory FILE VIDEO AUDIO";
    let (mode, path, video, audio, iterations) = match args {
        [mode, path, video, audio] if mode == "hls-fold" || mode == "hls-memory" => {
            (mode, path, video, audio, 1)
        }
        [mode, path, count, video, audio] if mode == "hls-bench" => {
            (mode, path, video, audio, count.parse::<u64>()?)
        }
        _ => return Err(usage.into()),
    };
    if !(1..=1_000_000).contains(&iterations) {
        return Err(usage.into());
    }
    let memory = mode == "hls-memory";
    let profile = HlsMediaProfile {
        video: match video.as_str() {
            "avc" => HlsVideoCodec::Avc,
            "hevc" => HlsVideoCodec::Hevc,
            _ => return Err("expected avc or hevc".into()),
        },
        audio: Some(match audio.as_str() {
            "aac" => HlsAudioCodec::Aac,
            "ac3" => HlsAudioCodec::Ac3,
            "eac3" => HlsAudioCodec::Eac3,
            _ => return Err("expected aac, ac3 or eac3".into()),
        }),
    };
    let path = Path::new(path);
    let size = path.metadata()?.len();
    if size == 0 || size > MAX_INPUT || size % 188 != 0 {
        return Err("input must be bounded TS188".into());
    }
    let pool = Budget::new(256 * 1024 * 1024)?;
    let raw = Bytes::try_from_vec(&pool, std::fs::read(path)?).map_err(|error| error.error())?;
    let retained = pool.used();
    let mut out = io::stdout().lock();
    if memory {
        out.write_all(b"TRIERGON_BEGIN_V1\n")?;
        out.flush()?;
    }
    let start = (mode == "hls-bench").then(Instant::now);
    let mut total = Work::default();
    for _ in 0..iterations {
        let segment = open_segment(&pool, std::hint::black_box(&raw), profile)?;
        let work = measure(&pool, segment)?;
        total.mix(work.digest);
        total.packets += work.packets;
        total.bytes += work.bytes;
        if pool.used() != retained {
            return Err("segment operation retained charged storage".into());
        }
    }
    let elapsed = start.map_or(0, |begin| begin.elapsed().as_nanos());
    if memory {
        out.write_all(b"TRIERGON_END_V1\n")?;
        out.flush()?;
    }
    writeln!(
        out,
        "{{\"schema\":1,\"operation\":\"{OPERATION}\",\"iterations\":{iterations},\"packets\":{},\"bytes\":{},\"digest\":{},\"elapsed_ns\":{elapsed},\"eof\":true}}",
        total.packets, total.bytes, total.digest,
    )?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{HlsAudioCodec, HlsMediaProfile, HlsVideoCodec, measure, open_segment};
    use triergon_core::{Budget, Bytes, Error};

    #[test]
    fn measurement_materializes_video_with_the_source_pool() {
        for (fixture, video, audio) in [
            (
                "replay-avc-aac-00.ts",
                HlsVideoCodec::Avc,
                HlsAudioCodec::Aac,
            ),
            (
                "replay-avc-ac3-00.ts",
                HlsVideoCodec::Avc,
                HlsAudioCodec::Ac3,
            ),
            (
                "replay-hevc-ac3-00.ts",
                HlsVideoCodec::Hevc,
                HlsAudioCodec::Ac3,
            ),
        ] {
            let pool = Budget::new(256 * 1024 * 1024).unwrap();
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../harness/fixtures/hls")
                .join(fixture);
            let raw = Bytes::try_from_vec(&pool, std::fs::read(path).unwrap()).unwrap();
            let profile = HlsMediaProfile {
                video,
                audio: Some(audio),
            };
            let segment = open_segment(&pool, &raw, profile).unwrap();
            let foreign_pool = Budget::new(256 * 1024 * 1024).unwrap();
            let Err(error) = measure(&foreign_pool, segment) else {
                panic!("video measurement skipped contiguous_feed for {fixture}");
            };
            assert_eq!(error.downcast_ref::<Error>(), Some(&Error::ForeignPool));
        }
    }
}
