//! Decoder measurement workers for the harness: the same operations as
//! the decode build of `harness/reference.c`, through the decoders the session uses.
//! `codec-worker` serves every operation; `codec-aac-worker` serves the AAC decode
//! operations alone, so that a build of the AAC decoder with other features or another
//! compiler measures nothing else.
//!
//! usage: `codec-worker KIND-CODEC-bench FILE ITERATIONS | KIND-CODEC-memory FILE |
//! KIND-CODEC-raw FILE OUT | KIND-CODEC-fold FILE | packets FORMAT FILE [SEEK] |
//! seek ogg FILE TRACK SECONDS POLICY`, KIND-CODEC
//! one of video-h264, video-hevc, audio-ac3, audio-eac3, audio-aac, audio-dts, audio-flac,
//! audio-mp3, audio-vorbis, audio-opus, audio-aac-access-units. Bench prints {iterations,
//! elapsed_ns, packets, bytes, digest} where
//! packets counts output pictures/audio blocks and the digest
//! ([`triergon_harness_protocol::interleaved`]) covers the exact converted output (RGBA or
//! interleaved stereo s16le). Configured AAC instead folds every native f32 plane.
//! File loading and process start are outside the timed
//! interval.
//!
//! The elementary-stream splitting below mirrors the reference worker's own parser
//! (the decode build of `harness/reference.c`) so both decode the same
//! units. It is measurement framing, not product framing:
//! playback frames access units through `triergon-framing`. FLAC, MPEG audio, Vorbis
//! and Opus lanes read their file format as a player does, through `AnyReader` and the
//! session's track decoder, as the reference reads them through its demuxer.
//! `packets` prints what a container reader emits per packet.
#![forbid(unsafe_code)]
mod aac_access_units;
mod isobmff_packets;
mod packets;

use std::io::Write;
use std::time::Instant;
use triergon::{Budget, ContainerReader, MediaKind, PacketInfo, open_sequential};
use triergon_codec::{
    AudioCodec, AudioDecoder, AudioFrame, DownmixLevels, VideoCodec, VideoDecoder,
};
use triergon_harness_protocol::interleaved::InterleavedDigest;
use triergon_pixel::letterbox::{CANVAS_BYTES, Scaler};
use triergon_session::TrackDecoder;
use triergon_session::mix::Downmix;

/// A refused access unit as a `decode error` line prints it: the `Debug` form of the
/// decoder's own error. It has no `Debug` of its own, which lets every such error
/// convert to it.
struct Refusal(String);

impl<E: std::fmt::Debug> From<E> for Refusal {
    fn from(error: E) -> Self {
        Refusal(format!("{error:?}"))
    }
}

#[derive(Default)]
struct Output {
    frames: u64,
    bytes: u64,
    digest: InterleavedDigest,
    raw: Option<std::io::BufWriter<std::fs::File>>,
    aac_speakers: Option<([u8; 8], usize)>,
}
impl Output {
    fn emit(&mut self, bytes: &[u8]) {
        self.digest.update(bytes);
        self.frames += 1;
        self.bytes += bytes.len() as u64;
        if let Some(raw) = self.raw.as_mut() {
            raw.write_all(bytes).expect("raw output");
        }
    }
}

/// Annex-B access units, streamed to `each` without an index: a new unit starts
/// at an AUD/SPS/PPS/SEI (H.264) or VPS/SPS/PPS/AUD/prefix SEI (HEVC) after a coded
/// slice, or at a slice that begins a new picture (first_mb_in_slice == 0 /
/// first_slice_segment flag).
fn access_units<'a>(data: &'a [u8], hevc: bool, mut each: impl FnMut(&'a [u8])) {
    let mut unit_start = None;
    let mut has_slice = false;
    let mut i = 0;
    while i + 3 <= data.len() {
        if !(data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1) {
            i += 1;
            continue;
        }
        let begin = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
        let payload = i + 3;
        i = payload;
        let Some(&header) = data.get(payload) else {
            break;
        };
        let (starts_unit, is_slice) = if hevc {
            let kind = header >> 1 & 0x3f;
            let first_slice = data.get(payload + 2).is_some_and(|b| b & 0x80 != 0);
            match kind {
                0..=31 => (first_slice, true),
                32..=35 | 39 => (true, false),
                _ => (false, false),
            }
        } else {
            let kind = header & 0x1f;
            let first_mb = data.get(payload + 1).is_some_and(|b| b & 0x80 != 0);
            match kind {
                1 | 5 => (first_mb, true),
                6..=9 => (true, false),
                _ => (false, false),
            }
        };
        if starts_unit && has_slice {
            if let Some(start) = unit_start {
                each(&data[start..begin]);
            }
            unit_start = Some(begin);
            has_slice = false;
        }
        if unit_start.is_none() {
            unit_start = Some(begin);
        }
        has_slice |= is_slice;
    }
    if let Some(start) = unit_start {
        each(&data[start..]);
    }
}

fn video(codec: VideoCodec, data: &[u8], out: &mut Output) {
    let mut decoder: Box<dyn VideoDecoder<Error = Refusal>> = match codec {
        VideoCodec::H264 => triergon_codec_h264::Decoder::new().boxed(),
        VideoCodec::Hevc => triergon_codec_hevc::Decoder::new().boxed(),
    };
    let mut scaler = Scaler::default();
    let mut rgba = vec![0_u8; CANVAS_BYTES];
    let mut n = 0;
    access_units(data, codec == VideoCodec::Hevc, |unit| {
        if let Err(error) = decoder.send(unit, Some(n)) {
            eprintln!("decode error: {}", error.0);
        }
        n += 1;
        while let Some(view) = decoder.receive() {
            scaler.letterbox(&view, &mut rgba);
            out.emit(&rgba);
        }
    });
    decoder.flush();
    while let Some(view) = decoder.receive() {
        scaler.letterbox(&view, &mut rgba);
        out.emit(&rgba);
    }
}

/// DTS core frame length from its header, as the reference parser reads it.
fn dts_frame_len(data: &[u8]) -> Option<usize> {
    // 16-bit big-endian core sync (the selected feed); FSIZE+1 bytes.
    if data.len() < 10 || data[..4] != [0x7f, 0xfe, 0x80, 0x01] {
        return None;
    }
    let v = u64::from_be_bytes(data[..8].try_into().ok()?);
    Some(((v >> 4) & 0x3fff) as usize + 1)
}

/// Elementary-stream frames, streamed to `each` without an index.
fn audio_units<'a>(codec: AudioCodec, data: &'a [u8], mut each: impl FnMut(&'a [u8])) {
    let mut at = 0;
    while at < data.len() {
        let rest = &data[at..];
        let len = match codec {
            AudioCodec::Ac3 | AudioCodec::Eac3 => triergon_codec_ac3::access_unit_len(rest).ok(),
            AudioCodec::Aac => triergon_codec_aac::AdtsHeader::parse(rest)
                .ok()
                .map(|h| h.frame_length),
            AudioCodec::Dts => dts_frame_len(rest),
            AudioCodec::Flac | AudioCodec::Opus | AudioCodec::Vorbis | AudioCodec::MpegAudio => {
                unreachable!("file-format lanes read through their container")
            }
        };
        match len {
            Some(len) if len > 0 && len <= rest.len() => {
                each(&rest[..len]);
                at += len;
            }
            // A truncated final frame still reaches the decoder, which decides
            // (as the reference parser hands it over too).
            Some(_) => {
                each(rest);
                break;
            }
            None => at += 1,
        }
    }
}

fn audio(codec: AudioCodec, data: &[u8], out: &mut Output) {
    let mut decoder: Box<dyn AudioDecoder<Error = Refusal>> = match codec {
        AudioCodec::Ac3 | AudioCodec::Eac3 => triergon_codec_ac3::Decoder::new().boxed(),
        AudioCodec::Aac => triergon_codec_aac::Decoder::new().boxed(),
        AudioCodec::Dts => triergon_codec_dca::Decoder::new().boxed(),
        AudioCodec::Flac | AudioCodec::Opus | AudioCodec::Vorbis | AudioCodec::MpegAudio => {
            unreachable!("file-format lanes read through their container")
        }
    };
    let mut frame = AudioFrame::default();
    let mut downmix = Downmix::default();
    let mut pcm = Vec::new();
    let mut bytes = Vec::new();
    audio_units(codec, data, |unit| match decoder.decode(unit, &mut frame) {
        Ok(_) if frame.samples_per_channel == 0 => {}
        Ok(levels) => {
            pcm.clear();
            downmix.mix(&frame, levels.unwrap_or_default(), &mut pcm);
            bytes.clear();
            bytes.extend(pcm.iter().flat_map(|s| s.to_le_bytes()));
            out.emit(&bytes);
        }
        Err(error) => eprintln!("decode error: {}", error.0),
    });
}

/// A file-format lane: the first audio track of the file, decoded with its trims and
/// mixed to stereo s16 like the elementary lanes. The file is read sequentially, as the
/// reference reads its piped input.
fn file_audio(data: &[u8], out: &mut Output) {
    let pool = Budget::new(256 << 20).expect("pool");
    let mut source = open_sequential(data, &pool).expect("open");
    let mut decoder: Option<(triergon::TrackId, TrackDecoder)> = None;
    let mut frame = AudioFrame::default();
    let mut downmix = Downmix::default();
    let (mut pcm, mut bytes) = (Vec::new(), Vec::new());
    while let Some(packet) = source.next_packet().expect("read") {
        let (track, decoder) = decoder.get_or_insert_with(|| {
            let reader = source.reader();
            let track = (0..reader.track_count())
                .filter_map(|index| reader.track(index))
                .find(|track| track.kind() == MediaKind::Audio)
                .expect("an audio track");
            (track.id(), TrackDecoder::new(track).expect("decoder"))
        });
        if packet.track() != *track {
            continue;
        }
        let decoded = decoder.decode(&packet, &mut frame).map_err(Refusal::from);
        emit_mixed(decoded, &frame, &mut downmix, &mut pcm, &mut bytes, out);
    }
    if let Some((_, decoder)) = decoder.as_mut() {
        let flushed = decoder.flush(&mut frame).map(|()| None);
        emit_mixed(
            flushed.map_err(Refusal::from),
            &frame,
            &mut downmix,
            &mut pcm,
            &mut bytes,
            out,
        );
    }
}

/// Mix one decode result to stereo s16 and emit it.
fn emit_mixed(
    decoded: Result<Option<DownmixLevels>, Refusal>,
    frame: &AudioFrame,
    downmix: &mut Downmix,
    pcm: &mut Vec<i16>,
    bytes: &mut Vec<u8>,
    out: &mut Output,
) {
    match decoded {
        Ok(_) if frame.samples_per_channel == 0 => {}
        Ok(levels) => {
            pcm.clear();
            downmix.mix(frame, levels.unwrap_or_default(), pcm);
            bytes.clear();
            bytes.extend(pcm.iter().flat_map(|s| s.to_le_bytes()));
            out.emit(bytes);
        }
        Err(error) => eprintln!("decode error: {}", error.0),
    }
}

/// The operations a worker binary serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operations {
    /// Every decode operation, and the `packets` and `seek` reader commands.
    All,
    /// The AAC decode operations: `audio-aac` and the access-unit operations.
    Aac,
}

impl Operations {
    fn serve(self, op: &str) -> bool {
        self == Operations::All
            || matches!(
                op,
                "audio-aac" | aac_access_units::OPERATION | aac_access_units::COMPAT_OPERATION
            )
    }
}

fn run(op: &str, data: &[u8], out: &mut Output) {
    match op {
        "video-h264" => video(VideoCodec::H264, data, out),
        "video-hevc" => video(VideoCodec::Hevc, data, out),
        "audio-ac3" => audio(AudioCodec::Ac3, data, out),
        "audio-eac3" => audio(AudioCodec::Eac3, data, out),
        "audio-aac" => audio(AudioCodec::Aac, data, out),
        aac_access_units::OPERATION | aac_access_units::COMPAT_OPERATION => {
            use triergon_session::compat::aac::Policy;
            let layout = if op == aac_access_units::OPERATION {
                Policy::Specification
            } else {
                Policy::ConventionalSurround
            };
            if let Err(error) = aac_access_units::decode(data, layout, out) {
                eprintln!("{error}");
                std::process::exit(2);
            }
        }
        "audio-dts" => audio(AudioCodec::Dts, data, out),
        "audio-flac" | "audio-mp3" | "audio-vorbis" | "audio-opus" => file_audio(data, out),
        _ => {
            eprintln!("unknown operation {op}");
            std::process::exit(2);
        }
    }
}

/// Run the worker binary `name`, which serves `operations`, on the process arguments.
pub fn main(name: &str, operations: Operations) {
    // <lane>-<codec>-bench FILE ITERATIONS | <lane>-<codec>-memory FILE | <lane>-<codec>-raw FILE OUT
    let args: Vec<String> = std::env::args().collect();
    if let [_, command, format, path, seek @ ..] = args.as_slice()
        && command == "packets"
        && seek.len() <= 1
        && operations == Operations::All
    {
        if let Err(error) = packets::run(format, path, seek.first().map(String::as_str)) {
            eprintln!("{error}");
            std::process::exit(2);
        }
        return;
    }
    if let [_, command, format, path, track, seconds, policy] = args.as_slice()
        && command == "seek"
        && operations == Operations::All
    {
        if let Err(error) = packets::Seek::parse(track, seconds, policy)
            .and_then(|seek| packets::seek(format, path, &seek))
        {
            eprintln!("{error}");
            std::process::exit(2);
        }
        return;
    }
    let Some((op, kind)) = args.get(1).and_then(|a| a.rsplit_once('-')) else {
        eprintln!(
            "usage: {name} KIND-CODEC-bench FILE ITERATIONS | KIND-CODEC-memory FILE | KIND-CODEC-raw FILE OUT | KIND-CODEC-fold FILE{}",
            if operations == Operations::All {
                " | packets FORMAT FILE [TRACK_ID@MICROSECONDS] | seek ogg FILE TRACK SECONDS exact|preroll"
            } else {
                ""
            }
        );
        std::process::exit(2);
    };
    if !operations.serve(op) {
        eprintln!("{name}: unknown operation {op}");
        std::process::exit(2);
    }
    let input = args.get(2).expect("input");
    let data = if matches!(
        op,
        aac_access_units::OPERATION | aac_access_units::COMPAT_OPERATION
    ) {
        aac_access_units::load(input).unwrap_or_else(|error| {
            eprintln!("{input}: {error}");
            std::process::exit(1);
        })
    } else {
        std::fs::read(input).expect("input")
    };
    let mut out = Output::default();
    match kind {
        "bench" => {
            let iterations: u32 = args
                .get(3)
                .expect("iterations")
                .parse()
                .expect("iterations");
            let start = Instant::now();
            for _ in 0..iterations {
                run(op, &data, &mut out);
            }
            let elapsed = start.elapsed().as_nanos();
            println!(
                "{{\"schema\":1,\"iterations\":{iterations},\"elapsed_ns\":{elapsed},\"packets\":{},\"bytes\":{},\"digest\":{}}}",
                out.frames,
                out.bytes,
                out.digest.finish()
            );
        }
        "memory" => {
            // Same observer protocol as the container workers: one whole operation between markers.
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(b"TRIERGON_BEGIN_V1\n").expect("stdout");
            stdout.flush().expect("stdout");
            run(op, &data, &mut out);
            stdout.write_all(b"TRIERGON_END_V1\n").expect("stdout");
            stdout.flush().expect("stdout");
            // Equal memory work is equal units and bytes; the digest goes to stderr
            // because audio may differ within the graded 1 LSB.
            writeln!(
                stdout,
                "{{\"packets\":{},\"bytes\":{}}}",
                out.frames, out.bytes
            )
            .expect("stdout");
            eprintln!("{{\"digest\":{}}}", out.digest.finish());
        }
        "fold" => {
            // One decode and its output identity, without timing: what a fuzz campaign
            // compares between builds.
            run(op, &data, &mut out);
            println!(
                "{{\"packets\":{},\"bytes\":{},\"digest\":{}}}",
                out.frames,
                out.bytes,
                out.digest.finish()
            );
        }
        "raw" => {
            out.raw = Some(std::io::BufWriter::new(
                std::fs::File::create(args.get(3).expect("output")).expect("raw output"),
            ));
            run(op, &data, &mut out);
            out.raw.take().expect("raw").flush().expect("raw flush");
            print!(
                "{{\"packets\":{},\"bytes\":{},\"digest\":{}",
                out.frames,
                out.bytes,
                out.digest.finish()
            );
            if let Some((speakers, channels)) = out.aac_speakers {
                print!(",\"speakers\":{:?}", &speakers[..channels]);
            }
            println!("}}");
        }
        _ => {
            eprintln!("unknown mode {kind}");
            std::process::exit(2);
        }
    }
}
