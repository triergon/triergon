//! Harness worker for the capture pipeline. Same command line as
//! the capture build of harness/reference.c:
//!
//! ```text
//!   capture-worker convert  <w> <h> <flip> <frames> <in.rgba> <out.yuv>
//!   capture-worker recon    <w> <h> <flip> <frames> <bitrate> <in.rgba> <out.yuv>
//!   capture-worker pipeline <w> <h> <flip> <frames> <bitrate> <in.rgba> <out.ts>
//!   capture-worker memory   <w> <h> <flip> <frames> <bitrate> <in.rgba>
//!   capture-worker bench    <in.rgba> <iterations> <w> <h> <flip> <frames> <bitrate>
//! ```
//!
//! `bench` takes the harness's timing command line (input and iterations
//! first) and reports totals over its sessions, as every timed worker does.
//! `bench` and `memory` feed every frame's bytes to a hashing sink that keeps
//! nothing, exactly like the reference worker.
#![forbid(unsafe_code)]

use std::io::Write;
use std::process::ExitCode;
use std::time::Instant;

use triergon_capture::pixel::{Orientation, PictureView, Plane};
use triergon_capture::{Session, SessionConfig, Sink};

struct HashSink {
    hash: u64,
    bytes: u64,
}

impl HashSink {
    fn new() -> HashSink {
        HashSink {
            hash: 0xcbf2_9ce4_8422_2325,
            bytes: 0,
        }
    }
}

impl Sink for HashSink {
    fn send(&mut self, bytes: &[u8]) -> bool {
        for &b in bytes {
            self.hash ^= b as u64;
            self.hash = self.hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
        self.bytes += bytes.len() as u64;
        true
    }
}

fn fail(what: &str) -> ExitCode {
    eprintln!("capture-worker: {what}");
    ExitCode::from(2)
}

fn session(
    cfg: &SessionConfig,
    orientation: Orientation,
    frames: &[u8],
    count: usize,
    sink: &mut impl Sink,
) -> Result<f64, String> {
    let frame_bytes = cfg.width as usize * cfg.height as usize * 4;
    let mut s = Session::open(cfg).map_err(|e| e.to_string())?;
    for i in 0..count {
        s.encode(
            &frames[i * frame_bytes..(i + 1) * frame_bytes],
            orientation,
            sink,
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(s.vbv_underflow_bits())
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().collect();
    if args.len() == 9 && args[1] == "bench" {
        // The timing command line, input and iterations first, in the order of
        // the other modes: <w> <h> <flip> <frames> <bitrate> <in.rgba> <iterations>.
        args[2..].rotate_left(2);
    }
    if args.len() < 7 {
        return fail("usage");
    }
    let parse = |i: usize| args[i].parse::<u32>().ok();
    let (Some(w), Some(h), Some(flip), Some(count)) = (parse(2), parse(3), parse(4), parse(5))
    else {
        return fail("geometry");
    };
    let orientation = if flip != 0 {
        Orientation::BottomUp
    } else {
        Orientation::TopDown
    };
    let count = count as usize;
    let frame_bytes = w as usize * h as usize * 4;
    let load = |path: &str| -> Option<Vec<u8>> {
        let data = std::fs::read(path).ok()?;
        (data.len() >= frame_bytes * count).then_some(data)
    };
    match args[1].as_str() {
        "convert" if args.len() == 8 => {
            let Some(rgba) = load(&args[6]) else {
                return fail("input");
            };
            let cfg = SessionConfig::with_rate_rule(w, h, 2_500_000);
            let Ok(mut s) = Session::open(&cfg) else {
                return fail("open");
            };
            let mut out = Vec::new();
            let mut sink = HashSink::new();
            for i in 0..count {
                if s.encode(
                    &rgba[i * frame_bytes..(i + 1) * frame_bytes],
                    orientation,
                    &mut sink,
                )
                .is_err()
                {
                    return fail("encode");
                }
                out.extend_from_slice(&s.converted().visible_yuv420());
            }
            if std::fs::write(&args[7], out).is_err() {
                return fail("output");
            }
        }
        "recon" if args.len() == 9 => {
            // Encoder-side reconstruction of every frame, for drift checks
            // against an independent decode of the same stream.
            let (Some(bitrate), Some(rgba)) = (parse(6), load(&args[7])) else {
                return fail("input");
            };
            let Ok(mut s) = Session::open(&SessionConfig::with_rate_rule(w, h, bitrate)) else {
                return fail("open");
            };
            let mut out = Vec::new();
            let mut sink = HashSink::new();
            for i in 0..count {
                if s.encode(
                    &rgba[i * frame_bytes..(i + 1) * frame_bytes],
                    orientation,
                    &mut sink,
                )
                .is_err()
                {
                    return fail("encode");
                }
                visible_yuv420(&s.reconstruction(), &mut out);
            }
            if std::fs::write(&args[8], out).is_err() {
                return fail("output");
            }
        }
        "pipeline" if args.len() == 9 => {
            let (Some(bitrate), Some(rgba)) = (parse(6), load(&args[7])) else {
                return fail("input");
            };
            let cfg = SessionConfig::with_rate_rule(w, h, bitrate);
            let mut ts: Vec<u8> = Vec::new();
            let underflow = match session(&cfg, orientation, &rgba, count, &mut ts) {
                Ok(u) => u,
                Err(e) => return fail(&e),
            };
            let mut hash = HashSink::new();
            hash.send(&ts);
            if std::fs::write(&args[8], &ts).is_err() {
                return fail("output");
            }
            println!(
                "{{\"bytes\":{},\"fnv1a64\":\"{:016x}\",\"vbv_underflow_bits\":{}}}",
                ts.len(),
                hash.hash,
                underflow
            );
        }
        "bench" if args.len() == 9 => {
            // `iterations` sessions, each from a fresh session and sink, timed
            // together. Every session must produce the same stream; the counts
            // are totals over the sessions, the digest that of the stream.
            let (Some(bitrate), Some(rgba), Some(iterations)) =
                (parse(6), load(&args[7]), parse(8).filter(|&n| n > 0))
            else {
                return fail("input");
            };
            let cfg = SessionConfig::with_rate_rule(w, h, bitrate);
            let mut stream = None;
            let start = Instant::now();
            for _ in 0..iterations {
                let mut sink = HashSink::new();
                if let Err(e) = session(&cfg, orientation, &rgba, count, &mut sink) {
                    return fail(&e);
                }
                if *stream.get_or_insert((sink.bytes, sink.hash)) != (sink.bytes, sink.hash) {
                    return fail("output differs between iterations");
                }
            }
            let elapsed = start.elapsed().as_nanos();
            let (bytes, digest) = stream.unwrap_or_default();
            println!(
                "{{\"iterations\":{iterations},\"elapsed_ns\":{elapsed},\"packets\":{},\"bytes\":{},\"digest\":{digest}}}",
                count as u64 * u64::from(iterations),
                bytes * u64::from(iterations)
            );
        }
        "memory" if args.len() == 8 => {
            let (Some(bitrate), Some(rgba)) = (parse(6), load(&args[7])) else {
                return fail("input");
            };
            let cfg = SessionConfig::with_rate_rule(w, h, bitrate);
            let mut sink = HashSink::new();
            let mut stdout = std::io::stdout();
            if stdout
                .write_all(b"TRIERGON_BEGIN_V1\n")
                .and_then(|()| stdout.flush())
                .is_err()
            {
                return fail("marker");
            }
            if let Err(e) = session(&cfg, orientation, &rgba, count, &mut sink) {
                return fail(&e);
            }
            if stdout
                .write_all(b"TRIERGON_END_V1\n")
                .and_then(|()| stdout.flush())
                .is_err()
            {
                return fail("marker");
            }
            let line = format!(
                "{{\"bytes\":{},\"fnv1a64\":\"{:016x}\"}}\n",
                sink.bytes, sink.hash
            );
            if stdout.write_all(line.as_bytes()).is_err() {
                return fail("result");
            }
        }
        _ => return fail("usage"),
    }
    ExitCode::SUCCESS
}

/// The visible planes of an 8-bit 4:2:0 view, tightly packed (Y, then Cb, then Cr), as
/// `Picture::visible_yuv420` lays them out.
fn visible_yuv420(view: &PictureView<'_>, out: &mut Vec<u8>) {
    for (i, plane) in view.planes.iter().enumerate() {
        let (width, height) = match i {
            0 => (view.width, view.height),
            _ => (view.width / 2, view.height / 2),
        };
        if let Plane::U8 {
            data,
            stride,
            offset,
        } = *plane
        {
            for row in data[offset..].chunks(stride).take(height) {
                out.extend_from_slice(&row[..width]);
            }
        }
    }
}
