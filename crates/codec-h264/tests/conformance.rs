//! JVT conformance vectors from the FFmpeg FATE suite (downloaded under the gitignored
//! `.research/m1d4/fate-suite/h264-conformance/`). Each in-set vector must reproduce the
//! FFmpeg 9.0.2 per-frame MD5 digests bit-exactly (development oracle, see
//! `ffmpeg_framemd5_digests.txt`); out-of-set vectors must be refused with
//! `DecodeError::Unsupported`. Skips cleanly when the vectors are absent.

use std::path::PathBuf;
use triergon_codec_h264::{AnnexBNalIter, DecodeError, Decoder, Picture};

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
        if let Err(e) = dec.send_nal(nal, None)
            && matches!(e, DecodeError::Unsupported(_))
            && unsupported.is_none()
        {
            unsupported = Some(e);
        }
        while let Some(p) = dec.receive() {
            md.push(triergon_harness_protocol::md5::hex_digest(&frame_bytes(&p)));
        }
    }
    dec.flush();
    while let Some(p) = dec.receive() {
        md.push(triergon_harness_protocol::md5::hex_digest(&frame_bytes(&p)));
    }
    (md, unsupported)
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
        if let Some(flt) = &filter
            && !name.contains(flt.as_str())
        {
            continue;
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
        let got = triergon_harness_protocol::md5::hex_digest(md.join("\n").as_bytes());
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
