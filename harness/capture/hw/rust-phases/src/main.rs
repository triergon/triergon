//! Phase timing of the pure-Rust capture pipeline with the same JSON shape as
//! harness/capture/hw/hw_capture.c `phases` (tooling only).
//!
//!   capture-rust-phases phases <w> <h> <flip> <frames> <bitrate> <in.rgba> <iterations>
//!   capture-rust-phases memory <w> <h> <flip> <frames> <bitrate> <in.rgba>
//!   capture-rust-phases load   <w> <h> <flip> <frames> <bitrate> <in.rgba>
//!
//! The pipeline is composed exactly as `Session::encode` does (convert, encode,
//! mux, hand to sink) so conversion and encode+mux can be timed separately.
//! There is no hardware device and no encoder delay: device startup/shutdown
//! and drain are genuinely zero-work phases. CPU time is measured by the
//! orchestrator (wait4) for this worker.
#![forbid(unsafe_code)]

use std::io::Write;
use std::time::Instant;
use triergon_capture::convert::Converter;
use triergon_capture::mpeg1::{Encoder, EncoderConfig};
use triergon_capture::ts::{MuxConfig, Muxer};
use triergon_capture::{Geometry, Orientation, Picture};

struct Pipeline {
    converter: Converter,
    input: Picture,
    encoder: Encoder,
    muxer: Muxer,
    es: Vec<u8>,
    ts: Vec<u8>,
}

impl Pipeline {
    fn open(w: u32, h: u32, bitrate: u32) -> Pipeline {
        let geometry = Geometry::new(w, h).unwrap();
        let encoder = Encoder::new(EncoderConfig {
            geometry,
            bit_rate: bitrate,
            max_rate: (bitrate as u64 * 7 / 5) as u32,
            vbv_bits: bitrate / 7,
            gop: 30,
        })
        .unwrap();
        Pipeline {
            converter: Converter::new(geometry),
            input: Picture::new(geometry),
            encoder,
            muxer: Muxer::new(&MuxConfig::default()).unwrap(),
            es: Vec::new(),
            ts: Vec::new(),
        }
    }
}

fn fnv(hash: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *hash ^= b as u64;
        *hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
}

fn stats(v: &mut [u64]) -> String {
    v.sort_unstable();
    let n = v.len();
    format!(
        "{{\"sum\":{},\"median\":{},\"p95\":{},\"max\":{}}}",
        v.iter().sum::<u64>(),
        v[n / 2],
        v[((n * 95) / 100).min(n - 1)],
        v[n - 1]
    )
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let num = |i: usize| a[i].parse::<u32>().unwrap();
    let (mode, w, h, flip, frames, bitrate) = (
        a[1].as_str(),
        num(2),
        num(3),
        num(4),
        num(5) as usize,
        num(6),
    );
    let orientation = if flip != 0 {
        Orientation::BottomUp
    } else {
        Orientation::TopDown
    };
    let fb = w as usize * h as usize * 4;
    let rgba = std::fs::read(&a[7]).unwrap();
    assert!(rgba.len() >= fb * frames);
    let mut out = std::io::stdout().lock();
    let run = |hash: &mut u64,
               bytes: &mut u64,
               conv: &mut Vec<u64>,
               enc: &mut Vec<u64>,
               total: &mut Vec<u64>| {
        let t0 = Instant::now();
        let mut p = Pipeline::open(w, h, bitrate);
        let t1 = Instant::now();
        for k in 0..frames {
            let a0 = Instant::now();
            p.converter
                .convert(&rgba[k * fb..(k + 1) * fb], orientation, &mut p.input)
                .unwrap();
            let a1 = Instant::now();
            let picture = p.encoder.encode(&p.input, &mut p.es).unwrap();
            let i = picture.index as i64;
            p.ts.clear();
            p.muxer
                .write_picture(&p.es, 3000 * (i + 1), 3000 * i, picture.key, &mut p.ts);
            fnv(hash, &p.ts);
            *bytes += p.ts.len() as u64;
            let a2 = Instant::now();
            conv.push((a1 - a0).as_nanos() as u64);
            enc.push((a2 - a1).as_nanos() as u64);
            total.push((a2 - a0).as_nanos() as u64);
        }
        let t2 = Instant::now();
        // Drain: the encoder holds no frames, nothing to flush.
        let t3 = Instant::now();
        drop(p);
        let p = Pipeline::open(w, h, bitrate);
        let t4 = Instant::now();
        drop(p);
        let t5 = Instant::now();
        [t1 - t0, t2 - t1, t3 - t2, t4 - t3, t5 - t4, t5 - t0].map(|d| d.as_nanos() as u64)
    };
    match mode {
        "phases" => {
            for _ in 0..num(8) {
                let (mut hash, mut bytes) = (0xcbf2_9ce4_8422_2325u64, 0u64);
                let (mut conv, mut enc, mut total) = (Vec::new(), Vec::new(), Vec::new());
                let [open, frames_ns, drain, reset, close, session] =
                    run(&mut hash, &mut bytes, &mut conv, &mut enc, &mut total);
                writeln!(
                    out,
                    "{{\"conv\":\"rust\",\"startup_device_ns\":0,\"open_ns\":{open},\"frames_ns\":{frames_ns},\"drain_ns\":{drain},\"reset_ns\":{reset},\"close_ns\":{close},\"shutdown_device_ns\":0,\"session_ns\":{session},\"bytes\":{bytes},\"fnv1a64\":\"{hash:016x}\",\"convert\":{},\"encode_mux\":{},\"frame\":{}}}",
                    stats(&mut conv),
                    stats(&mut enc),
                    stats(&mut total)
                )
                .unwrap();
            }
        }
        "memory" => {
            out.write_all(b"SAFE_MEDIA_BEGIN_V1\n").unwrap();
            out.flush().unwrap();
            let (mut hash, mut bytes) = (0xcbf2_9ce4_8422_2325u64, 0u64);
            let (mut c, mut e, mut t) = (
                Vec::with_capacity(frames),
                Vec::with_capacity(frames),
                Vec::with_capacity(frames),
            );
            run(&mut hash, &mut bytes, &mut c, &mut e, &mut t);
            out.write_all(b"SAFE_MEDIA_END_V1\n").unwrap();
            out.flush().unwrap();
            writeln!(out, "{{\"bytes\":{bytes},\"fnv1a64\":\"{hash:016x}\"}}").unwrap();
        }
        "load" => writeln!(out, "{{\"loaded\":{}}}", rgba.len()).unwrap(),
        _ => panic!("usage"),
    }
}
