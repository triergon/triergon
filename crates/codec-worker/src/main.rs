//! Decoder measurement worker for the harness: the same operations as
//! the decode build of `harness/reference.c`, through the session decoders.
//!
//! usage: codec-worker KIND-CODEC-bench FILE ITERATIONS | KIND-CODEC-memory FILE |
//! KIND-CODEC-raw FILE OUT, KIND-CODEC one of video-h264, video-hevc, audio-ac3,
//! audio-eac3, audio-aac, audio-dts. Bench prints {iterations, elapsed_ns, packets, bytes, digest} where
//! packets counts output pictures/audio blocks and the digest
//! ([`triergon_harness_protocol::interleaved`]) covers the exact converted output (RGBA or
//! interleaved stereo s16le). File loading and process start are outside the timed
//! interval.
//!
//! The elementary-stream splitting below mirrors the reference worker's own parser
//! (the decode build of `harness/reference.c`) so both decode the same
//! units. It is measurement framing, not product framing:
//! playback frames access units through `triergon-framing`.
#![forbid(unsafe_code)]
use std::io::Write;
use std::time::Instant;
use triergon_codec::{AudioCodec, AudioFrame, VideoCodec};
use triergon_harness_protocol::interleaved::InterleavedDigest;
use triergon_pixel::letterbox::{CANVAS_BYTES, Scaler};
use triergon_session::mix::Downmix;
use triergon_session::{audio_decoder, video_decoder};

#[derive(Default)]
struct Output {
    frames: u64,
    bytes: u64,
    digest: InterleavedDigest,
    raw: Option<std::io::BufWriter<std::fs::File>>,
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
    let mut decoder = video_decoder(codec);
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
    let mut decoder = audio_decoder(codec);
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

fn run(op: &str, data: &[u8], out: &mut Output) {
    match op {
        "video-h264" => video(VideoCodec::H264, data, out),
        "video-hevc" => video(VideoCodec::Hevc, data, out),
        "audio-ac3" => audio(AudioCodec::Ac3, data, out),
        "audio-eac3" => audio(AudioCodec::Eac3, data, out),
        "audio-aac" => audio(AudioCodec::Aac, data, out),
        "audio-dts" => audio(AudioCodec::Dts, data, out),
        _ => {
            eprintln!("unknown operation {op}");
            std::process::exit(2);
        }
    }
}

fn main() {
    // <lane>-<codec>-bench FILE ITERATIONS | <lane>-<codec>-memory FILE | <lane>-<codec>-raw FILE OUT
    let args: Vec<String> = std::env::args().collect();
    let Some((op, kind)) = args.get(1).and_then(|a| a.rsplit_once('-')) else {
        eprintln!(
            "usage: codec-worker KIND-CODEC-bench FILE ITERATIONS | KIND-CODEC-memory FILE | KIND-CODEC-raw FILE OUT"
        );
        std::process::exit(2);
    };
    let data = std::fs::read(args.get(2).expect("input")).expect("input");
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
            stdout.write_all(b"SAFE_MEDIA_BEGIN_V1\n").expect("stdout");
            stdout.flush().expect("stdout");
            run(op, &data, &mut out);
            stdout.write_all(b"SAFE_MEDIA_END_V1\n").expect("stdout");
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
        "raw" => {
            out.raw = Some(std::io::BufWriter::new(
                std::fs::File::create(args.get(3).expect("output")).expect("raw output"),
            ));
            run(op, &data, &mut out);
            out.raw.take().expect("raw").flush().expect("raw flush");
            println!(
                "{{\"packets\":{},\"bytes\":{},\"digest\":{}}}",
                out.frames,
                out.bytes,
                out.digest.finish()
            );
        }
        _ => {
            eprintln!("unknown mode {kind}");
            std::process::exit(2);
        }
    }
}
