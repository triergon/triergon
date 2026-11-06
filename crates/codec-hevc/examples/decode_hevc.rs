//! Decode an H.265 Annex-B elementary stream to raw planar YUV (u8, or little-endian u16
//! for >8-bit) in output order. One JSON line per output picture is written to stderr.
//!
//! Usage: decode <input.hevc> <output.yuv | - | null> [--quiet] [--repeat N]
//! `null` decodes without writing samples (the equivalent of `ffmpeg -f null`).
//! `--repeat N` decodes the stream N times (output written once) and prints the fastest
//! in-process decode time to stderr (development timing only).

use std::io::{Read, Write};

use triergon_codec_hevc::{AccessUnitSplitter, Decoder, Picture, Row};

fn emit(
    p: &Picture,
    out: &mut dyn Write,
    buf: &mut Vec<u8>,
    (quiet, discard): (bool, bool),
    idx: usize,
) -> std::io::Result<()> {
    // Stream row by row (no full-frame staging buffer).
    for pl in p.planes.iter().filter(|_| !discard) {
        for y in 0..pl.height {
            match pl.row(y) {
                Row::U8(r) => out.write_all(r)?,
                Row::U16(r) => {
                    // Little-endian bytes; the fixed-size chunk loop vectorizes.
                    buf.resize(2 * r.len(), 0);
                    for (o, v) in buf.as_chunks_mut::<2>().0.iter_mut().zip(r) {
                        o.copy_from_slice(&v.to_le_bytes());
                    }
                    out.write_all(buf)?;
                }
            }
        }
    }
    if !quiet {
        let pts = match p.pts {
            Some(v) => v.to_string(),
            None => "null".to_string(),
        };
        eprintln!(
            "{{\"frame\":{},\"width\":{},\"height\":{},\"bitdepth\":{},\"pts\":{},\"poc\":{},\"sar\":[{},{}],\"colour\":[{},{},{},{}]}}",
            idx,
            p.width,
            p.height,
            p.bit_depth,
            pts,
            p.poc,
            p.sar.0,
            p.sar.1,
            p.colour.primaries,
            p.colour.transfer,
            p.colour.matrix,
            p.colour.full_range as u8
        );
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: decode <input.hevc> <output.yuv|-|null> [--quiet] [--repeat N]");
        std::process::exit(2);
    }
    let quiet = args.iter().any(|a| a == "--quiet");
    let repeat: usize = args
        .iter()
        .position(|a| a == "--repeat")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let discard = args[2] == "null";
    let mut out: Box<dyn Write> = if discard {
        Box::new(std::io::sink())
    } else if args[2] == "-" {
        Box::new(std::io::BufWriter::with_capacity(
            1 << 16,
            std::io::stdout(),
        ))
    } else {
        Box::new(std::io::BufWriter::with_capacity(
            1 << 16,
            std::fs::File::create(&args[2]).expect("create output"),
        ))
    };
    let mut dec = Decoder::new();
    let mut buf = Vec::new();
    let mut n = 0usize;
    let mut errors = 0usize;
    // Stream the input: split access units from a growing buffer, keeping the last
    // (possibly incomplete) access unit until more data or EOF arrives.
    let mut file = std::fs::File::open(&args[1]).expect("open input");
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 1 << 20];
    let mut au_index = 0usize;
    let mut eof = false;
    while !eof {
        let got = file.read(&mut chunk).expect("read input");
        if got == 0 {
            eof = true;
        } else {
            pending.extend_from_slice(&chunk[..got]);
        }
        let aus: Vec<&[u8]> = AccessUnitSplitter::new(&pending).collect();
        let complete = if eof {
            aus.len()
        } else {
            aus.len().saturating_sub(1)
        };
        let mut consumed = 0usize;
        for au in &aus[..complete] {
            consumed += au.len();
            if let Err(e) = dec.send(au, Some(au_index as i64)) {
                errors += 1;
                eprintln!("{{\"error\":\"{}\",\"au\":{}}}", e, au_index);
            }
            au_index += 1;
            while let Some(p) = dec.receive() {
                emit(&p, &mut *out, &mut buf, (quiet, discard), n).expect("write");
                n += 1;
            }
        }
        pending.drain(..consumed);
    }
    dec.flush();
    while let Some(p) = dec.receive() {
        emit(&p, &mut *out, &mut buf, (quiet, discard), n).expect("write");
        n += 1;
    }
    out.flush().expect("flush");
    if repeat > 1 {
        let data = std::fs::read(&args[1]).expect("read input");
        let aus: Vec<&[u8]> = AccessUnitSplitter::new(&data).collect();
        let mut best = f64::MAX;
        for _ in 1..repeat {
            let t = std::time::Instant::now();
            let mut d = Decoder::new();
            for (i, au) in aus.iter().enumerate() {
                let _ = d.send(au, Some(i as i64));
                while d.receive().is_some() {}
            }
            d.flush();
            while d.receive().is_some() {}
            best = best.min(t.elapsed().as_secs_f64());
        }
        eprintln!("{{\"best_decode_seconds\":{best:.4}}}");
    }
    if errors > 0 {
        std::process::exit(1);
    }
}
