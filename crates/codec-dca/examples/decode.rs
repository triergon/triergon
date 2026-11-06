//! Decode a DTS core elementary stream to interleaved f32le (plane order) on stdout or
//! into a file. A one-line JSON summary goes to stderr.
//!
//! usage: decode IN.dts [OUT.f32]
//!
//! Frames are split with this example's own sync/FSIZE parsing; DTS-HD extension
//! substreams (sync 0x64582025) between core frames are skipped.
use std::io::Write;
use triergon_codec_dca::{AudioFrame, Decoder, Packing};

/// Byte length of the frame starting at `data` (which begins with a core sync word).
fn frame_len(data: &[u8], packing: Packing) -> Option<usize> {
    // Read the first 8 header bytes in canonical order.
    let mut hdr = [0u8; 8];
    match packing {
        Packing::Be16 => hdr.copy_from_slice(data.get(..8)?),
        Packing::Le16 => {
            for i in 0..4 {
                hdr[2 * i] = *data.get(2 * i + 1)?;
                hdr[2 * i + 1] = *data.get(2 * i)?;
            }
        }
        Packing::Be14 | Packing::Le14 => {
            // 5 words carry 70 payload bits; keep the first 64.
            let mut acc: u128 = 0;
            for i in 0..5 {
                let (a, b) = (*data.get(2 * i)?, *data.get(2 * i + 1)?);
                let w = if packing == Packing::Le14 {
                    u16::from_le_bytes([a, b])
                } else {
                    u16::from_be_bytes([a, b])
                };
                acc = (acc << 14) | u128::from(w & 0x3FFF);
            }
            hdr = ((acc >> 6) as u64).to_be_bytes();
        }
    }
    let v = u64::from_be_bytes(hdr);
    let fsize = ((v >> (64 - 46 - 14)) & 0x3FFF) as usize + 1;
    // 14-bit: FSIZE counts bytes of the 16-bit-equivalent bitstream (as FFmpeg reads it);
    // the container holds 14 of every 16 bits. 16-bit little endian frames occupy whole words.
    Some(match packing {
        Packing::Be14 | Packing::Le14 => (fsize * 8).div_ceil(14) * 2,
        Packing::Le16 => fsize.div_ceil(2) * 2,
        Packing::Be16 => fsize,
    })
}

/// Length of the next unit at the start of `rest`: `Some((len, Some(packing)))` for a core
/// frame, `Some((len, None))` for a skipped extension substream, `None` when more data is
/// needed (or for 1 byte of garbage to skip when `rest` is long enough).
fn next_unit(rest: &[u8]) -> Option<(usize, Option<Packing>)> {
    if rest.len() < 16 {
        return None;
    }
    if rest[..4] == [0x64, 0x58, 0x20, 0x25] {
        // DTS-HD extension substream header, ETSI TS 102 114 clause 7.5.2.
        let v = u128::from_be_bytes(rest[..16].try_into().ok()?);
        let header_type = (v >> (128 - 43)) & 1;
        let (hbits, fbits) = if header_type == 1 { (12, 20) } else { (8, 16) };
        let fsize = ((v >> (128 - 43 - hbits - fbits)) & ((1 << fbits) - 1)) as usize + 1;
        return Some((fsize.max(4), None));
    }
    match Packing::detect(rest) {
        Some(p) => Some((frame_len(rest, p)?, Some(p))),
        None => Some((1, None)),
    }
}

fn main() {
    use std::io::Read;
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: decode IN.dts [OUT.f32 | --null]");
        std::process::exit(2);
    }
    let mut input = std::fs::File::open(&args[1]).expect("open input");
    let null = args.get(2).map(String::as_str) == Some("--null");
    let mut sink: Box<dyn Write> = if null {
        Box::new(std::io::sink())
    } else if args.len() > 2 {
        Box::new(std::io::BufWriter::new(
            std::fs::File::create(&args[2]).expect("create output"),
        ))
    } else {
        Box::new(std::io::BufWriter::new(std::io::stdout().lock()))
    };
    let mut dec = Decoder::new();
    let mut frame = AudioFrame::default();
    let mut frames = 0usize;
    let mut total = 0usize;
    let mut errors = 0usize;
    let mut offset = 0usize; // absolute byte offset of buf[0]
    let mut first: Option<(u32, String, Vec<&'static str>)> = None;
    let mut interleaved: Vec<u8> = Vec::new();
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 17);
    let mut start = 0usize;
    let mut eof = false;
    loop {
        // Refill when fewer than one maximal frame (16 KiB, 14-bit: ~19 KiB) is buffered.
        if !eof && buf.len() - start < 1 << 15 {
            buf.drain(..start);
            offset += start;
            start = 0;
            let old = buf.len();
            buf.resize(old + (1 << 16), 0);
            let n = input.read(&mut buf[old..]).expect("read input");
            buf.truncate(old + n);
            eof = n == 0;
            continue;
        }
        let rest = &buf[start..];
        let Some((len, packing)) = next_unit(rest) else {
            break;
        };
        if len > rest.len() && !eof {
            // incomplete unit; force a refill
            if buf.len() - start >= 1 << 15 {
                break;
            }
            continue;
        }
        let len = len.min(rest.len());
        if packing.is_none() {
            start += len;
            continue;
        }
        match dec.decode(&rest[..len], &mut frame) {
            Ok(()) => {
                if first.is_none() {
                    first = Some((
                        frame.sample_rate,
                        frame
                            .layout
                            .layout_name()
                            .map(str::to_string)
                            .unwrap_or_else(|| {
                                let names: Vec<&str> =
                                    frame.layout.speakers().iter().map(|s| s.name()).collect();
                                format!("{} channels ({})", names.len(), names.join("+"))
                            }),
                        frame
                            .layout
                            .speakers()
                            .iter()
                            .map(|s| s.name())
                            .collect::<Vec<_>>(),
                    ));
                }
                let n = frame.samples_per_channel;
                if !null {
                    interleaved.clear();
                    for i in 0..n {
                        for p in &frame.planes {
                            interleaved.extend_from_slice(&p[i].to_le_bytes());
                        }
                    }
                    sink.write_all(&interleaved).expect("write");
                }
                frames += 1;
                total += n;
            }
            Err(e) => {
                errors += 1;
                eprintln!("frame at byte {}: {e}", offset + start);
            }
        }
        start += len;
    }
    sink.flush().expect("flush");
    let (rate, layout, speakers) = first.unwrap_or((0, "none".into(), Vec::new()));
    let sp: Vec<String> = speakers.iter().map(|s| format!("\"{s}\"")).collect();
    eprintln!(
        "{{\"sample_rate\":{rate},\"channels\":{},\"layout\":\"{layout}\",\"speakers\":[{}],\"frames\":{frames},\"samples_per_channel\":{total},\"errors\":{errors}}}",
        speakers.len(),
        sp.join(",")
    );
}
