//! FATE vector checks (skip cleanly when the gitignored media is absent).
//!
//! Frame/sample counts and layouts are FFmpeg 9.0.2's (ffprobe). Sample values are compared
//! against the FATE `*_v2.pcm` s16 references only coarsely: both decoders inject
//! non-normative pseudo-random dither/SPX noise, so exact comparison is impossible here
//! (the exact, dither-stripped comparison lives in harness/codecs/ac3/).

use std::path::PathBuf;

use triergon_codec_ac3::{AudioFrame, Decoder, access_unit_len};

fn suite() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.research/fate-samples")
}

struct Expect {
    path: &'static str,
    frames: usize,
    samples: usize,
    layout: &'static str,
    pcm: Option<&'static str>,
}

const VECTORS: &[Expect] = &[
    Expect {
        path: "ac3/millers_crossing_4.0.ac3",
        frames: 62,
        samples: 95232,
        layout: "4.0",
        pcm: Some("ac3/millers_crossing_4.0_v2.pcm"),
    },
    Expect {
        path: "ac3/monsters_inc_2.0_192_small.ac3",
        frames: 131,
        samples: 201216,
        layout: "stereo",
        pcm: Some("ac3/monsters_inc_2.0_192_small_v2.pcm"),
    },
    Expect {
        path: "ac3/monsters_inc_5.1_448_small.ac3",
        frames: 56,
        samples: 86016,
        layout: "5.1(side)",
        pcm: Some("ac3/monsters_inc_5.1_448_small_v2.pcm"),
    },
    Expect {
        path: "eac3/csi_miami_5.1_256_spx_small.eac3",
        frames: 47,
        samples: 72192,
        layout: "5.1(side)",
        pcm: Some("eac3/csi_miami_5.1_256_spx_small_v2.pcm"),
    },
    Expect {
        path: "eac3/csi_miami_stereo_128_spx.eac3",
        frames: 651,
        samples: 999936,
        layout: "stereo",
        pcm: None,
    },
    Expect {
        path: "eac3/csi_miami_stereo_128_spx_small.eac3",
        frames: 488,
        samples: 749568,
        layout: "stereo",
        pcm: Some("eac3/csi_miami_stereo_128_spx_small_v2.pcm"),
    },
    Expect {
        path: "eac3/matrix2_commentary1_stereo_192_small.eac3",
        frames: 130,
        samples: 199680,
        layout: "stereo",
        pcm: Some("eac3/matrix2_commentary1_stereo_192_small_v2.pcm"),
    },
    Expect {
        path: "eac3/serenity_english_5.1_1536_small.eac3",
        frames: 97,
        samples: 24832,
        layout: "5.1(side)",
        pcm: Some("eac3/serenity_english_5.1_1536_small_v2.pcm"),
    },
    Expect {
        path: "eac3/the_great_wall_7.1.eac3",
        frames: 157,
        samples: 241152,
        layout: "7.1",
        pcm: Some("eac3/the_great_wall_7.1.pcm"),
    },
];

#[test]
fn fate_vectors_frame_counts_layouts_and_coarse_values() {
    let root = suite();
    if !root.exists() {
        eprintln!(
            "note: {} absent, skipping FATE vector checks",
            root.display()
        );
        return;
    }
    for v in VECTORS {
        let Ok(data) = std::fs::read(root.join(v.path)) else {
            eprintln!("note: {} absent, skipped", v.path);
            continue;
        };
        let mut dec = Decoder::new();
        let mut frame = AudioFrame::default();
        let mut interleaved: Vec<f32> = Vec::new();
        let (mut frames, mut pos) = (0usize, 0usize);
        let mut layout = String::new();
        while pos + 8 <= data.len() {
            let Ok(len) = access_unit_len(&data[pos..]) else {
                pos += 1;
                continue;
            };
            let end = (pos + len).min(data.len());
            frame.clear();
            if let Ok(info) = dec.decode(&data[pos..end], &mut frame) {
                frames += 1;
                layout = triergon_codec_ac3::layout_name(&info.layout);
                for i in 0..frame.samples_per_channel {
                    for p in &frame.planes {
                        interleaved.push(p[i]);
                    }
                }
            }
            pos = end;
        }
        let ch = frame.planes.len();
        assert_eq!(frames, v.frames, "{}", v.path);
        assert_eq!(interleaved.len() / ch, v.samples, "{}", v.path);
        assert_eq!(layout, v.layout, "{}", v.path);
        if let Some(pcm) = v.pcm.and_then(|p| std::fs::read(root.join(p)).ok()) {
            let refs: Vec<f64> = pcm
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| i16::from_le_bytes([c[0], c[1]]) as f64 / 32768.0)
                .collect();
            assert_eq!(refs.len(), interleaved.len(), "{}", v.path);
            let (mut se, mut mx) = (0f64, 0f64);
            for (a, b) in interleaved.iter().zip(&refs) {
                let d = (*a as f64).clamp(-1.0, 32767.0 / 32768.0) - b;
                se += d * d;
                mx = mx.max(d.abs());
            }
            let rms = (se / refs.len() as f64).sqrt();
            assert!(
                rms < 1e-3 && mx < 5e-2,
                "{}: rms {rms:e} max {mx:e}",
                v.path
            );
        }
    }
}

/// Corrupted input must never panic (run in debug builds to catch arithmetic overflow).
#[test]
fn corrupted_vectors_never_panic() {
    let root = suite();
    if !root.exists() {
        eprintln!("note: {} absent, skipping corruption test", root.display());
        return;
    }
    let mut seed = 0x1234_5678u32;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    for (v, rate) in VECTORS
        .iter()
        .flat_map(|v| [(v, 2000usize), (v, 200), (v, 20)])
    {
        let Ok(mut data) = std::fs::read(root.join(v.path)) else {
            continue;
        };
        data.truncate(100_000);
        // Flip bits, but keep sync words intact so frames still reach the audio-block parser.
        for _ in 0..data.len() / rate {
            let i = rnd() as usize % data.len();
            if i >= 2 && !(data[i - 1] == 0x0b || data[i] == 0x0b || data[i] == 0x77) {
                data[i] ^= 1 << (rnd() % 8);
            }
        }
        let mut dec = Decoder::new();
        let mut frame = AudioFrame::default();
        let mut pos = 0;
        while pos + 8 <= data.len() {
            let Ok(len) = access_unit_len(&data[pos..]) else {
                pos += 1;
                continue;
            };
            let end = (pos + len).min(data.len());
            frame.clear();
            let _ = dec.decode(&data[pos..end], &mut frame);
            for p in &frame.planes {
                assert!(p.iter().all(|x| x.is_finite()), "{}", v.path);
            }
            pos = end.max(pos + 1);
        }
    }
}
