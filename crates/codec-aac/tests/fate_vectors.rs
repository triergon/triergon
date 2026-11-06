//! FATE-suite vector checks. Vectors live outside git under `.research/m1d4/fate-suite/aac/`;
//! each test prints a note and returns when its input is absent.
//!
//! Without references only structure is checked (format, sample counts, error counts as
//! observed from FFmpeg 9.0.2). With `AAC_REF_DIR` pointing at FFmpeg float references
//! (`<name>.f32`, made by `ffmpeg -f aac -i X -f f32le -c:a pcm_f32le`) the frozen numeric
//! rules are applied to the vectors listed in `NUMERIC` (core, SBR and PS).
use std::path::PathBuf;
use triergon_codec_aac::{AudioFrame, Decoder, adts};

fn suite() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.research/m1d4/fate-suite/aac")
}

struct Decoded {
    rate: u32,
    channels: usize,
    samples: usize,
    errors: usize,
    interleaved: Vec<f32>,
}

fn decode_file(data: &[u8]) -> Decoded {
    let mut pos = 0;
    if data.len() > 10 && &data[..3] == b"ID3" {
        let sz = ((data[6] as usize & 0x7f) << 21)
            | ((data[7] as usize & 0x7f) << 14)
            | ((data[8] as usize & 0x7f) << 7)
            | (data[9] as usize & 0x7f);
        pos = 10 + sz;
    }
    let mut d = Decoder::new();
    let mut f = AudioFrame::default();
    let mut out = Decoded {
        rate: 0,
        channels: 0,
        samples: 0,
        errors: 0,
        interleaved: Vec::new(),
    };
    while let Some((off, h)) = adts::find_frame(data, pos) {
        let end = (off + h.frame_length).min(data.len());
        pos = end;
        match d.decode(&data[off..end], &mut f) {
            Ok(()) => {
                out.rate = f.sample_rate;
                out.channels = f.planes.len();
                out.samples += f.samples_per_channel;
                for i in 0..f.samples_per_channel {
                    for p in &f.planes {
                        out.interleaved.push(p[i]);
                    }
                }
            }
            Err(_) => out.errors += 1,
        }
    }
    out
}

/// (file, rate, channels, samples per channel, decode errors) as produced by FFmpeg 9.0.2.
const STRUCTURE: &[(&str, u32, usize, usize, usize)] = &[
    ("ct_faac-adts.aac", 44100, 2, 880_640, 0),
    ("id3v1.aac", 48000, 2, 5_120, 0),
    ("id3v2.aac", 48000, 1, 239_616, 0),
    ("id3v2_two_tags.aac", 48000, 1, 480_256, 0),
    ("foo.aac", 44100, 2, 87_040, 1),
    ("CT_DecoderCheck/sbr_i-ps_i.aac", 44100, 2, 346_112, 0),
    ("CT_DecoderCheck/File1.aac", 44100, 2, 346_112, 0),
];

/// Vectors whose reference comparison is expected to pass the numeric rules.
const NUMERIC: &[&str] = &[
    "ct_faac-adts",
    "id3v1",
    "id3v2",
    "id3v2_two_tags",
    "foo",
    "al04_44",
    "al05_44",
    "al06_44",
    "al07_96",
    "al15_44",
    "al17_44",
    "al18_44",
    "al22_chCfg0PCE_44",
    "am00_88",
    "am05_44",
    "ap05_48",
    "al_sbr_cm_48_2",
    "al_sbr_cm_48_5.1",
    "al_sbr_sr_48_2_fsaac48",
    "al_sbr_ps_04_new",
    "al_sbr_ps_06_new",
    "ct_File1_fixed_raw",
    "ct_File1_raw",
    "ct_File2",
    "ct_File3",
    "ct_File4",
    "ct_File5",
    "ct_File6",
    "ct_File7",
    "ct_sbr_bc-ps_bc",
    "ct_sbr_bc-ps_i",
    "ct_sbr_bic-ps_i",
    "ct_sbr_i-ps_bic",
    "ct_sbr_i-ps_i",
    "ct_sbr_i-ps_i_raw",
];

#[test]
fn fate_structure() {
    for &(name, rate, ch, samples, errors) in STRUCTURE {
        let p = suite().join(name);
        let Ok(data) = std::fs::read(&p) else {
            eprintln!("note: {} absent, skipped", p.display());
            continue;
        };
        let d = decode_file(&data);
        assert_eq!(
            (d.rate, d.channels, d.samples, d.errors),
            (rate, ch, samples, errors),
            "{name}"
        );
    }
}

#[test]
fn fate_numeric_with_references() {
    let Some(refdir) = std::env::var_os("AAC_REF_DIR") else {
        eprintln!("note: AAC_REF_DIR not set, numeric comparison skipped");
        return;
    };
    let refdir = PathBuf::from(refdir);
    for name in NUMERIC {
        let adts = refdir.join(format!("{name}.adts"));
        let reff = refdir.join(format!("{name}.f32"));
        let (Ok(data), Ok(rb)) = (std::fs::read(&adts), std::fs::read(&reff)) else {
            eprintln!("note: {name} input or reference absent, skipped");
            continue;
        };
        let d = decode_file(&data);
        let r: Vec<f32> = rb
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(d.interleaved.len(), r.len(), "{name}: sample count");
        for c in 0..d.channels {
            let (mut se, mut mx) = (0f64, 0f64);
            let n = d.interleaved.len() / d.channels;
            for i in 0..n {
                let e = (d.interleaved[i * d.channels + c] - r[i * d.channels + c]) as f64;
                se += e * e;
                mx = mx.max(e.abs());
            }
            let rms = (se / n as f64).sqrt();
            assert!(
                rms <= 8.8e-6 && mx <= 6.1035e-5,
                "{name} ch{c}: rms {rms:e} max {mx:e}"
            );
        }
    }
}
