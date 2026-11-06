//! Test tooling: rewrite an AC-3 / E-AC-3 elementary stream so that no audio block uses
//! dither (every dithflag = 0), fixing up CRCs. Decoders then produce noise-free output
//! for all bap = 0 mantissas, which allows exact comparison against another decoder.
//! (E-AC-3 SPX noise blending is not affected.)
//!
//! usage: strip_dither <in> <out>

use triergon_codec_ac3::{AudioFrame, Decoder, access_unit_len, crc16};

fn to_bits(b: &[u8]) -> Vec<bool> {
    b.iter()
        .flat_map(|&x| (0..8).map(move |i| x & (0x80 >> i) != 0))
        .collect()
}

fn from_bits(bits: &[bool]) -> Vec<u8> {
    bits.chunks(8)
        .map(|c| {
            c.iter()
                .enumerate()
                .fold(0u8, |a, (i, &v)| a | ((v as u8) << (7 - i)))
        })
        .collect()
}

/// crc1 value making the CRC over frame[2..five8_bytes] zero (A/52 7.10.1).
fn solve_crc1(frame: &[u8], five8_bytes: usize) -> u16 {
    let n = five8_bytes - 4;
    let mut msg = vec![0u8; 2 + n];
    msg[2..].copy_from_slice(&frame[4..five8_bytes]);
    let target = crc16(0, &msg);
    let mut cols = [0u16; 16];
    for (i, c) in cols.iter_mut().enumerate() {
        let mut m = vec![0u8; 2 + n];
        let v = 1u16 << i;
        m[0] = (v >> 8) as u8;
        m[1] = v as u8;
        *c = crc16(0, &m);
    }
    // Solve sum_i x_i * cols[i] = target over GF(2).
    let mut rows: Vec<(u16, u16)> = (0..16)
        .map(|r| {
            let mut lhs = 0u16;
            for (i, c) in cols.iter().enumerate() {
                if c & (1 << r) != 0 {
                    lhs |= 1 << i;
                }
            }
            (lhs, (target >> r) & 1)
        })
        .collect();
    let mut x = 0u16;
    let mut pivots = Vec::new();
    let mut r = 0;
    for col in 0..16 {
        let Some(p) = (r..16).find(|&k| rows[k].0 & (1 << col) != 0) else {
            continue;
        };
        rows.swap(r, p);
        for k in 0..16 {
            if k != r && rows[k].0 & (1 << col) != 0 {
                rows[k].0 ^= rows[r].0;
                rows[k].1 ^= rows[r].1;
            }
        }
        pivots.push((col, r));
        r += 1;
    }
    for (col, row) in pivots {
        if rows[row].1 != 0 {
            x |= 1 << col;
        }
    }
    x
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).expect("read");
    let mut out: Vec<u8> = Vec::with_capacity(data.len() + data.len() / 16);
    let mut dec = Decoder::new();
    dec.set_trace(true);
    let mut frame = AudioFrame::default();
    let mut pos = 0;
    let (mut ac3_frames, mut eac3_flip, mut eac3_insert, mut eac3_grow, mut failed) =
        (0, 0, 0, 0, 0);
    while pos < data.len() {
        let len = match access_unit_len(&data[pos..]) {
            Ok(l) if pos + l <= data.len() => l,
            Ok(_) => {
                // Truncated final access unit: copy unchanged.
                out.extend_from_slice(&data[pos..]);
                break;
            }
            Err(_) => {
                // Not at a syncword: copy the byte and resynchronise.
                out.push(data[pos]);
                pos += 1;
                continue;
            }
        };
        frame.clear();
        if dec.decode(&data[pos..pos + len], &mut frame).is_err() {
            failed += 1;
            out.extend_from_slice(&data[pos..pos + len]);
            pos += len;
            continue;
        }
        let traces = dec.trace().frames.clone();
        let mut cursor = 0usize; // AU-relative bytes already emitted
        for t in traces {
            if t.offset > cursor {
                out.extend_from_slice(&data[pos + cursor..pos + t.offset]);
            }
            let start = pos + t.offset;
            let fb = &data[start..start + t.frame_bytes];
            cursor = t.offset + t.frame_bytes;
            let mut bits = to_bits(fb);
            if !t.eac3 {
                for &b in &t.dithflag_bits {
                    bits[b] = false;
                }
                let mut nf = from_bits(&bits);
                let words = nf.len() / 2;
                let five8 = ((words >> 1) + (words >> 3)) * 2;
                let c1 = solve_crc1(&nf, five8);
                nf[2..4].copy_from_slice(&c1.to_be_bytes());
                let l = nf.len();
                let c2 = crc16(0, &nf[2..l - 2]);
                nf[l - 2..].copy_from_slice(&c2.to_be_bytes());
                assert_eq!(crc16(0, &nf[2..five8]), 0);
                assert_eq!(crc16(0, &nf[2..]), 0);
                out.extend_from_slice(&nf);
                ac3_frames += 1;
                continue;
            }
            if !t.dithflag_bits.is_empty() {
                for &b in &t.dithflag_bits {
                    bits[b] = false;
                }
                eac3_flip += 1;
            } else {
                // dithflage = 0: set it and insert nfchans zero bits per block. Drop the
                // same number of unused bits after the audio data, or grow the frame.
                let k = t.num_blocks * t.nfchans;
                let avail = t.frame_bytes * 8 - 18 - t.audio_end_bit;
                let (drop, pad) = if avail >= k {
                    (k, 0)
                } else {
                    (0, k.div_ceil(16) * 16 - k)
                };
                let mut nb = Vec::with_capacity(bits.len() + k + pad);
                for (i, &b) in bits.iter().enumerate() {
                    if t.dithflag_insert_bits.contains(&i) {
                        nb.extend(std::iter::repeat_n(false, t.nfchans));
                    }
                    if i == t.audio_end_bit {
                        nb.extend(std::iter::repeat_n(false, pad));
                    }
                    if Some(i) == t.dithflage_bit {
                        nb.push(true);
                        continue;
                    }
                    if i >= t.audio_end_bit && i < t.audio_end_bit + drop {
                        continue;
                    }
                    nb.push(b);
                }
                assert_eq!(nb.len() % 16, 0);
                if pad > 0 || drop == 0 {
                    // Rewrite frmsiz (bits 21..32): words - 1.
                    let words = nb.len() / 16 - 1;
                    for j in 0..11 {
                        nb[21 + j] = (words >> (10 - j)) & 1 != 0;
                    }
                    eac3_grow += 1;
                } else {
                    eac3_insert += 1;
                }
                bits = nb;
            }
            let mut nf = from_bits(&bits);
            let l = nf.len();
            let c2 = crc16(0, &nf[2..l - 2]);
            nf[l - 2..].copy_from_slice(&c2.to_be_bytes());
            out.extend_from_slice(&nf);
        }
        if len > cursor {
            out.extend_from_slice(&data[pos + cursor..pos + len]);
        }
        pos += len;
    }
    std::fs::write(&args[2], &out).expect("write");
    eprintln!(
        "ac3 frames {ac3_frames}, eac3 flipped {eac3_flip}, eac3 inserted {eac3_insert}, eac3 grown {eac3_grow}, failed {failed}"
    );
}
