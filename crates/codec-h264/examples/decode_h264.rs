//! Decode an H.264 Annex-B elementary stream to raw yuv420p (8-bit) in output order.
//! Usage: decode IN.264 OUT.yuv   (per-picture JSON lines on stderr)
use std::io::{Read, Write};
use triergon_codec_h264::{AnnexBNalIter, Decoder, Picture};

/// Offset of the last 0x000001 start code (including a preceding zero byte), if any.
fn last_start_code(b: &[u8]) -> Option<usize> {
    if b.len() < 3 {
        return None;
    }
    let mut i = b.len() - 3;
    loop {
        if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
            return Some(if i > 0 && b[i - 1] == 0 { i - 1 } else { i });
        }
        if i == 0 {
            return None;
        }
        i -= 1;
    }
}

fn first_mb_is_zero(nal: &[u8]) -> bool {
    // first_mb_in_slice ue(v) == 0 <=> first payload bit is 1 (no emulation issue for 1 bit).
    nal.len() > 1 && nal[1] & 0x80 != 0
}

fn write_pic(out: &mut Option<std::io::BufWriter<std::fs::File>>, p: &Picture, n: usize) {
    if let Some(out) = out {
        for pl in &p.planes {
            for r in 0..pl.height {
                out.write_all(pl.row(r)).unwrap();
            }
        }
    }
    eprintln!(
        "{{\"n\":{},\"width\":{},\"height\":{},\"pts\":{},\"poc\":{},\"sar\":[{},{}]}}",
        n,
        p.width,
        p.height,
        p.pts.map_or("null".into(), |v| v.to_string()),
        p.poc,
        p.sar.0,
        p.sar.1
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: decode IN.264 OUT.yuv");
        std::process::exit(2);
    }
    // Stream the input in 1 MiB chunks; a NAL unit is complete once the next start code is seen.
    let mut input = std::fs::File::open(&args[1]).expect("open input");
    // "/dev/null" (or "-") skips writing, matching `ffmpeg -f null` for timing.
    let mut out = if args[2] == "/dev/null" || args[2] == "-" {
        None
    } else {
        Some(std::io::BufWriter::new(
            std::fs::File::create(&args[2]).expect("create output"),
        ))
    };
    let mut dec = Decoder::new();
    let mut au: i64 = -1;
    let mut n = 0;
    let mut errors = 0;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 1 << 20];
    let mut eof = false;
    while !eof {
        let k = input.read(&mut chunk).expect("read input");
        if k == 0 {
            eof = true;
        } else {
            buf.extend_from_slice(&chunk[..k]);
        }
        // Keep the tail after the last start code unless at EOF (it may be an incomplete NAL).
        let cut = if eof {
            buf.len()
        } else {
            last_start_code(&buf).unwrap_or(0)
        };
        if cut == 0 {
            continue;
        }
        for nal in AnnexBNalIter::new(&buf[..cut]) {
            let t = nal[0] & 31;
            if (t == 1 || t == 5) && first_mb_is_zero(nal) {
                au += 1;
            }
            if let Err(e) = dec.send_nal(nal, Some(au.max(0))) {
                errors += 1;
                if errors <= 5 {
                    eprintln!("{{\"error\":\"{e}\"}}");
                }
            }
            while let Some(p) = dec.receive() {
                write_pic(&mut out, &p, n);
                n += 1;
            }
        }
        buf.drain(..cut);
    }
    dec.flush();
    while let Some(p) = dec.receive() {
        write_pic(&mut out, &p, n);
        n += 1;
    }
    if let Some(o) = out.as_mut() {
        o.flush().unwrap();
    }
    if errors > 0 {
        eprintln!("{{\"errors\":{errors}}}");
    }
}
