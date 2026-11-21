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
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]

use std::io::Write;
use std::process::ExitCode;

use triergon_capture::pixel::{Orientation, PictureView, Plane};
use triergon_capture::{Session, SessionConfig, Sink};
use triergon_harness_runner::{BenchLine, Boundary, Timer, failed};

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
    failed(Some("capture-worker"), 2, what)
}

fn open_session(cfg: &SessionConfig) -> triergon_capture::Result<Session> {
    if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
        Session::open_filtered(cfg)
    } else {
        Session::open(cfg)
    }
}

/// Runs one session over the first `count` frames into `sink`; returns its VBV
/// underflow, or the error's text. The timed `bench` loop runs this.
fn session(
    cfg: &SessionConfig,
    orientation: Orientation,
    frames: &[u8],
    count: usize,
    sink: &mut impl Sink,
) -> Result<f64, String> {
    let frame_bytes = cfg.width as usize * cfg.height as usize * 4;
    let mut s = open_session(cfg).map_err(|e| e.to_string())?;
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

/// Runs one session over the first `count` frames into a hashing sink, appends
/// `frame_output` of the session after each frame, and writes what it appended to
/// `path`. A failure names the step: `open`, `encode` or `output`.
fn write_per_frame(
    cfg: &SessionConfig,
    orientation: Orientation,
    frames: &[u8],
    count: usize,
    path: &str,
    mut frame_output: impl FnMut(&Session, &mut Vec<u8>),
) -> Result<(), &'static str> {
    let frame_bytes = cfg.width as usize * cfg.height as usize * 4;
    let mut s = open_session(cfg).map_err(|_| "open")?;
    let mut out = Vec::new();
    let mut sink = HashSink::new();
    for i in 0..count {
        s.encode(
            &frames[i * frame_bytes..(i + 1) * frame_bytes],
            orientation,
            &mut sink,
        )
        .map_err(|_| "encode")?;
        frame_output(&s, &mut out);
    }
    std::fs::write(path, out).map_err(|_| "output")
}

#[derive(PartialEq, Eq)]
enum Operation {
    Convert,
    Recon,
    Pipeline,
    Bench,
    Memory,
}

impl Operation {
    fn parse(name: &str, count: usize) -> Option<Self> {
        Some(match (name, count) {
            ("convert", 8) => Self::Convert,
            ("recon", 9) => Self::Recon,
            ("pipeline", 9) => Self::Pipeline,
            ("bench", 9) => Self::Bench,
            ("memory", 8) => Self::Memory,
            _ => return None,
        })
    }
}

#[cold]
#[inline(never)]
fn configure(cfg: &mut SessionConfig, options: &[String]) -> Result<(), &'static str> {
    if !options.len().is_multiple_of(2) {
        return Err("options");
    }
    for pair in options.chunks_exact(2) {
        let value = &pair[1];
        let number = || value.parse::<u32>().map_err(|_| "option value");
        let short = || u16::try_from(number()?).map_err(|_| "option value");
        match pair[0].as_str() {
            "--gop" => cfg.gop = number()?,
            "--max-rate" => cfg.max_rate = number()?,
            "--vbv-bits" => cfg.vbv_bits = number()?,
            "--transport-stream-id" => cfg.mux.transport_stream_id = short()?,
            "--original-network-id" => cfg.mux.original_network_id = short()?,
            "--program-number" => cfg.mux.program_number = short()?,
            "--pmt-pid" => cfg.mux.pmt_pid = short()?,
            "--video-pid" => cfg.mux.video_pid = short()?,
            "--stream-type" => {
                cfg.mux.stream_type = u8::try_from(number()?).map_err(|_| "option value")?;
            }
            "--provider-name" => cfg.mux.provider_name.clone_from(value),
            "--service-name" => cfg.mux.service_name.clone_from(value),
            _ => return Err("option name"),
        }
    }
    Ok(())
}

#[cfg_attr(test, allow(dead_code))]
fn worker_main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().collect();
    let positional = match args.get(1).map(String::as_str) {
        Some("bench" | "pipeline" | "recon") => 9,
        Some("memory") => 8,
        _ => args.len(),
    };
    let operation = args
        .get(1)
        .and_then(|name| Operation::parse(name, args.len().min(positional)));
    if operation == Some(Operation::Bench) {
        // The timing command line, input and iterations first, in the order of
        // the other modes: <w> <h> <flip> <frames> <bitrate> <in.rgba> <iterations>.
        args[2..positional].rotate_left(2);
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
    let Some(operation) = operation else {
        return fail("usage");
    };
    let options = &args[positional..];
    let config = |bitrate| {
        let mut cfg = SessionConfig::with_rate_rule(w, h, bitrate);
        if !options.is_empty() {
            configure(&mut cfg, options)?;
        }
        Ok(cfg)
    };
    match operation {
        Operation::Convert => {
            let Some(rgba) = load(&args[6]) else {
                return fail("input");
            };
            let cfg = SessionConfig::with_rate_rule(w, h, 2_500_000);
            let converted = |s: &Session, out: &mut Vec<u8>| {
                out.extend_from_slice(&s.converted().visible_yuv420());
            };
            if let Err(step) = write_per_frame(&cfg, orientation, &rgba, count, &args[7], converted)
            {
                return fail(step);
            }
        }
        Operation::Recon => {
            // Encoder-side reconstruction of every frame, for drift checks
            // against an independent decode of the same stream.
            let (Some(bitrate), Some(rgba)) = (parse(6), load(&args[7])) else {
                return fail("input");
            };
            let cfg = match config(bitrate) {
                Ok(cfg) => cfg,
                Err(error) => return fail(error),
            };
            let reconstructed =
                |s: &Session, out: &mut Vec<u8>| visible_yuv420(&s.reconstruction(), out);
            if let Err(step) =
                write_per_frame(&cfg, orientation, &rgba, count, &args[8], reconstructed)
            {
                return fail(step);
            }
        }
        Operation::Pipeline => {
            let (Some(bitrate), Some(rgba)) = (parse(6), load(&args[7])) else {
                return fail("input");
            };
            let cfg = match config(bitrate) {
                Ok(cfg) => cfg,
                Err(error) => return fail(error),
            };
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
        Operation::Bench => {
            // `iterations` sessions, each from a fresh session and sink, timed
            // together. Every session must produce the same stream; the counts
            // are totals over the sessions, the digest that of the stream.
            let (Some(bitrate), Some(rgba), Some(iterations)) =
                (parse(6), load(&args[7]), parse(8).filter(|&n| n > 0))
            else {
                return fail("input");
            };
            let cfg = match config(bitrate) {
                Ok(cfg) => cfg,
                Err(error) => return fail(error),
            };
            let mut stream = None;
            let start = Timer::start(true);
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
            let iterations = u64::from(iterations);
            println!(
                "{}",
                BenchLine {
                    schema: false,
                    iterations,
                    elapsed_ns: elapsed,
                    packets: count as u64 * iterations,
                    bytes: bytes * iterations,
                    digest,
                }
            );
        }
        Operation::Memory => {
            let (Some(bitrate), Some(rgba)) = (parse(6), load(&args[7])) else {
                return fail("input");
            };
            let cfg = match config(bitrate) {
                Ok(cfg) => cfg,
                Err(error) => return fail(error),
            };
            let mut sink = HashSink::new();
            let mut stdout = std::io::stdout();
            if Boundary::Begin.mark(&mut stdout).is_err() {
                return fail("marker");
            }
            if let Err(e) = session(&cfg, orientation, &rgba, count, &mut sink) {
                return fail(&e);
            }
            if Boundary::End.mark(&mut stdout).is_err() {
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
    }
    ExitCode::SUCCESS
}

/// C's entry point, the one exception to the worker's `deny(unsafe_code)`
/// (`triergon-worker-entry`).
#[cfg(not(test))]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
extern "C" fn main(
    _argc: std::ffi::c_int,
    _argv: *const *const std::ffi::c_char,
) -> std::ffi::c_int {
    triergon_worker_entry::enter(worker_main)
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
