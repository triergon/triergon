//! JVT conformance vectors from the FFmpeg FATE suite (downloaded under the gitignored
//! `.research/m1d4/fate-suite/h264-conformance/`). Each in-set vector must reproduce the
//! FFmpeg 9.0.2 per-frame MD5 digests bit-exactly (development oracle, see
//! `ffmpeg_framemd5_digests.txt`); out-of-set vectors must be refused with
//! `DecodeError::Unsupported`. Skips cleanly when the vectors are absent.

use std::path::PathBuf;
use triergon_codec_h264::{AnnexBNalIter, DecodeError, Decoder, Picture};

mod md5 {
    //! MD5 (RFC 1321), only used to compare decoded frames with recorded digests.
    pub fn digest(data: &[u8]) -> [u8; 16] {
        let s: [u32; 64] = [
            7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20,
            5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
            6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
        ];
        let k: [u32; 64] =
            std::array::from_fn(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32);
        let mut h: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
        let mut msg = data.to_vec();
        let bitlen = (data.len() as u64).wrapping_mul(8);
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bitlen.to_le_bytes());
        for chunk in msg.chunks(64) {
            let m: [u32; 16] = std::array::from_fn(|i| {
                u32::from_le_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap())
            });
            let (mut a, mut b, mut c, mut d) = (h[0], h[1], h[2], h[3]);
            for i in 0..64 {
                let (f, g) = match i / 16 {
                    0 => ((b & c) | (!b & d), i),
                    1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                    2 => (b ^ c ^ d, (3 * i + 5) % 16),
                    _ => (c ^ (b | !d), (7 * i) % 16),
                };
                let t = d;
                d = c;
                c = b;
                b = b.wrapping_add(
                    a.wrapping_add(f)
                        .wrapping_add(k[i])
                        .wrapping_add(m[g])
                        .rotate_left(s[i]),
                );
                a = t;
            }
            h[0] = h[0].wrapping_add(a);
            h[1] = h[1].wrapping_add(b);
            h[2] = h[2].wrapping_add(c);
            h[3] = h[3].wrapping_add(d);
        }
        let mut out = [0u8; 16];
        for i in 0..4 {
            out[i * 4..i * 4 + 4].copy_from_slice(&h[i].to_le_bytes());
        }
        out
    }
    pub fn hex(d: &[u8; 16]) -> String {
        d.iter().map(|b| format!("{b:02x}")).collect()
    }
}

fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.research/m1d4/fate-suite/h264-conformance")
}

/// Vectors outside the selected feature matrix and the feature that refuses them.
const OUT_OF_SET: &[(&str, &str)] = &[
    ("FM1_BT_B.h264", "flexible macroblock ordering"),
    ("FM1_FT_E.264", "flexible macroblock ordering"),
    ("FM2_SVA_B.264", "flexible macroblock ordering"),
    ("FM2_SVA_C.264", "flexible macroblock ordering"),
    ("sp1_bt_a.h264", "SP/SI slices"),
    ("sp2_bt_b.h264", "SP/SI slices"),
];

fn out_of_set(name: &str) -> bool {
    OUT_OF_SET.iter().any(|(n, _)| *n == name)
        || name.starts_with("FRext/Hi422")
        || name.starts_with("FRext/PPH")
}

fn frame_bytes(p: &Picture) -> Vec<u8> {
    let mut v = Vec::with_capacity(p.width * p.height * 3 / 2);
    for pl in &p.planes {
        for r in 0..pl.height {
            v.extend_from_slice(pl.row(r));
        }
    }
    v
}

/// Decode a whole Annex-B file; returns per-frame MD5s and whether an Unsupported error occurred.
fn decode_file(data: &[u8]) -> (Vec<String>, Option<DecodeError>) {
    let mut dec = Decoder::new();
    let mut md = Vec::new();
    let mut unsupported = None;
    for nal in AnnexBNalIter::new(data) {
        if let Err(e) = dec.send_nal(nal, None) {
            if matches!(e, DecodeError::Unsupported(_)) && unsupported.is_none() {
                unsupported = Some(e);
            }
        }
        while let Some(p) = dec.receive() {
            md.push(md5::hex(&md5::digest(&frame_bytes(&p))));
        }
    }
    dec.flush();
    while let Some(p) = dec.receive() {
        md.push(md5::hex(&md5::digest(&frame_bytes(&p))));
    }
    (md, unsupported)
}

#[test]
fn md5_known_answer() {
    assert_eq!(
        md5::hex(&md5::digest(b"")),
        "d41d8cd98f00b204e9800998ecf8427e"
    );
    assert_eq!(
        md5::hex(&md5::digest(b"The quick brown fox jumps over the lazy dog")),
        "9e107d9d372bb6826bd81d3542a419d6"
    );
}

#[test]
fn conformance_vectors_bit_exact() {
    let dir = vectors_dir();
    if !dir.exists() {
        eprintln!(
            "note: conformance vectors not present at {}; skipping",
            dir.display()
        );
        return;
    }
    let table = include_str!("ffmpeg_framemd5_digests.txt");
    let filter = std::env::var("H264_VECTOR").ok();
    let mut failures = Vec::new();
    let mut ran = 0;
    for line in table
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        let f: Vec<&str> = line.split_whitespace().collect();
        let (name, frames, digest) = (f[0], f[1].parse::<usize>().unwrap(), f[2]);
        if let Some(flt) = &filter {
            if !name.contains(flt.as_str()) {
                continue;
            }
        }
        let path = dir.join(name);
        let Ok(data) = std::fs::read(&path) else {
            eprintln!("note: {name} missing; skipped");
            continue;
        };
        ran += 1;
        let (md, unsupported) = decode_file(&data);
        if out_of_set(name) {
            if unsupported.is_none() {
                failures.push(format!(
                    "{name}: out-of-set vector was not refused as Unsupported"
                ));
            }
            continue;
        }
        let got = md5::hex(&md5::digest(md.join("\n").as_bytes()));
        if md.len() != frames || got != digest {
            failures.push(format!(
                "{name}: frames {}/{frames} digest {got} != {digest}",
                md.len()
            ));
        }
    }
    eprintln!("ran {ran} vectors, {} failures", failures.len());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
