//! AC-3 bsid 9/10 (reduced sample rate) and the E-AC-3 reduced-rate / non-compatible
//! dependent corner cases. The two frames are self-authored probe frames from
//! research/d4/ac3/gen_reduced_rate.py (all-zero mantissas, so the output exposes the bit
//! allocation); the expected samples were captured black-box from FFmpeg 9.0.2's decoder
//! (`ffmpeg -i k.ac3 -f f32le`), see research/d4/ac3/STATUS.md.

use triergon_codec_ac3::{
    AudioFrame, DecodeError, Decoder, DecoderOptions, Feature, Speaker, Syntax, access_unit_len,
    crc16, parse_header,
};

/// `gen_reduced_rate.py k9.ac3 9 0 1 2 1`: bsid 9, fscod 0 (24 kHz), 1/0 + LFE, frmsizecod 37.
const K9_PREFIX: &str = "0b77cd6b25483d8009f843602dd987bb2877971e48c652be4fd5ec83beaec49bde682ee870a339b4eb3d2de99299a15f\
be8870cc38c6ec360d8de730279b0c716a1ddb78590b6ca1d21b7b49437fc6338d8b3e1e49c25d0916e130";
/// `gen_reduced_rate.py k10.ac3 10 2 1 3 2`: bsid 10, fscod 2 (8 kHz), 2/0 + LFE with coupling
/// (coupling leaks, rematrixing), frmsizecod 37.
const K10_PREFIX: &str = "0b777335a550476000f1d333fe23a60128c86fcdce4b314f6a2bab1a636069af409784855088133f28e28265b2ddf2bc\
5e3352b2d09b5ca36041c8fe539580fc22748e823da116b184e94b6c91622ce3d8f013a8eb308b018977eca39e6e0a20\
c5cdcfc9cc8ce731d5a1b874da420c2a2ff0";

const IDX: [usize; 6] = [300, 555, 777, 1000, 1234, 1500];
/// FFmpeg 9.0.2 output at IDX, per plane (FC, LFE).
const K9_FFMPEG: [[f64; 6]; 2] = [
    [
        1.2713351, 1.2489988, 1.3385237, 1.5632809, 0.75903571, 1.0271463,
    ],
    [
        0.019960981,
        0.016672278,
        -0.024678653,
        0.033127766,
        0.023714921,
        0.033930581,
    ],
];
/// FFmpeg 9.0.2 output at IDX, per plane (FL, FR, LFE).
const K10_FFMPEG: [[f64; 6]; 3] = [
    [
        0.39570454,
        0.54727447,
        0.17698261,
        -0.54799724,
        -0.45110214,
        0.61131287,
    ],
    [
        0.16428848,
        0.042807981,
        -0.25074628,
        -0.51412392,
        0.64369708,
        0.10838424,
    ],
    [
        0.0072707226,
        0.0074409652,
        0.0081160888,
        0.003823359,
        0.0020314862,
        0.0026458008,
    ],
];

/// Rebuild the probe frame: header/block-0 prefix, zero mantissas, crc2.
fn frame(prefix: &str, len: usize) -> Vec<u8> {
    let mut f = vec![0u8; len];
    for (i, b) in f.iter_mut().enumerate().take(prefix.len() / 2) {
        *b = u8::from_str_radix(&prefix[2 * i..2 * i + 2], 16).unwrap();
    }
    let c2 = crc16(0, &f[2..len - 2]);
    f[len - 2..].copy_from_slice(&c2.to_be_bytes());
    f
}

fn k9() -> Vec<u8> {
    frame(K9_PREFIX, 2560)
}

fn k10() -> Vec<u8> {
    frame(K10_PREFIX, 3840)
}

fn with_bsid(f: &[u8], bsid: u8) -> Vec<u8> {
    let mut g = f.to_vec();
    g[5] = (bsid << 3) | (g[5] & 7);
    g
}

fn decode(f: &[u8]) -> (triergon_codec_ac3::FrameInfo, AudioFrame) {
    let mut out = AudioFrame::default();
    let info = Decoder::new().decode(f, &mut out).unwrap();
    (info, out)
}

/// E-AC-3 syncframe of `words` 16-bit words: header (2/0, bsid 16) then zeros.
fn eac3_frame(strmtyp: u32, fscod: u32, code2: u32, words: usize) -> Vec<u8> {
    let mut f = vec![0u8; words * 2];
    let frmsiz = words as u32 - 1;
    f[0] = 0x0b;
    f[1] = 0x77;
    f[2] = ((strmtyp << 6) | (frmsiz >> 8)) as u8;
    f[3] = frmsiz as u8;
    f[4] = ((fscod << 6) | (code2 << 4) | (2 << 1)) as u8;
    f[5] = 16 << 3;
    f
}

#[test]
fn reduced_rate_headers_divide_the_sample_rate_and_keep_the_frame_size() {
    for (fscod, base) in [(0u8, 48000u32), (1, 44100), (2, 32000)] {
        let mut f = with_bsid(&k9(), 8);
        f[4] = (fscod << 6) | (f[4] & 0x3f);
        let h8 = parse_header(&f).unwrap();
        for bsid in [9u8, 10] {
            let h = parse_header(&with_bsid(&f, bsid)).unwrap();
            assert_eq!(h.syntax, Syntax::Ac3);
            assert_eq!(h.bsid, bsid);
            assert_eq!(h.sample_rate, base >> (bsid - 8));
            assert_eq!((h.frame_bytes, h.num_blocks), (h8.frame_bytes, 6));
        }
    }
    // Framing (used by crates/sim): one bsid 9/10 syncframe is one access unit.
    for f in [k9(), k10()] {
        let two = [f.clone(), f.clone()].concat();
        assert_eq!(access_unit_len(&two).unwrap(), f.len());
    }
    // bsid > 16 is still refused (FFmpeg: "invalid bitstream id").
    assert_eq!(
        parse_header(&with_bsid(&k9(), 17)),
        Err(DecodeError::Unsupported(Feature::BitstreamId(17)))
    );
}

#[test]
fn bsid9_mono_lfe_matches_ffmpeg() {
    let (info, out) = decode(&k9());
    assert_eq!(
        (info.bsid, info.sample_rate, info.samples),
        (9, 24000, 1536)
    );
    assert!(info.crc_ok && info.concealed.is_none());
    assert_eq!(out.sample_rate, 24000);
    assert_eq!(
        out.layout.speakers(),
        &[Speaker::FrontCenter, Speaker::LowFrequency]
    );
    for (p, want) in K9_FFMPEG.iter().enumerate() {
        for (&i, &w) in IDX.iter().zip(want) {
            let got = out.planes[p][i] as f64;
            assert!(
                (got - w).abs() <= 1e-5,
                "plane {p} sample {i}: {got} vs {w}"
            );
        }
    }
    // The same payload read as bsid 8 allocates differently: the rule is observable.
    let (_, out8) = decode(&with_bsid(&k9(), 8));
    for p in 0..2 {
        let d = out.planes[p]
            .iter()
            .zip(&out8.planes[p])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(d > 1e-3, "plane {p} insensitive to bsid ({d})");
    }
}

#[test]
fn bsid10_coupled_stereo_matches_ffmpeg() {
    let (info, out) = decode(&k10());
    assert_eq!(
        (info.bsid, info.sample_rate, info.samples),
        (10, 8000, 1536)
    );
    assert!(info.crc_ok && info.concealed.is_none());
    assert_eq!(out.layout.ffmpeg_name(), "2.1");
    for (p, want) in K10_FFMPEG.iter().enumerate() {
        for (&i, &w) in IDX.iter().zip(want) {
            let got = out.planes[p][i] as f64;
            assert!(
                (got - w).abs() <= 1e-5,
                "plane {p} sample {i}: {got} vs {w}"
            );
        }
    }
    let (_, out8) = decode(&with_bsid(&k10(), 8));
    let d = out.planes[0]
        .iter()
        .zip(&out8.planes[0])
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(d > 1e-3);
}

#[test]
fn non_compatible_dependent_is_dropped() {
    // AC-3 bsid 9 core (24 kHz) + E-AC-3 dependent at 48 kHz: FFmpeg outputs the core alone
    // and labels it with the dependent's rate.
    let core = k9();
    let (_, alone) = decode(&core);
    for (fscod, rate) in [(0u32, 48000u32), (1, 44100)] {
        let au = [core.clone(), eac3_frame(1, fscod, 3, 256)].concat();
        assert_eq!(access_unit_len(&au).unwrap(), au.len());
        let (info, out) = decode(&au);
        assert_eq!((info.sample_rate, out.sample_rate), (rate, rate));
        assert_eq!(out.layout, alone.layout);
        assert_eq!(out.planes, alone.planes);
    }
    // Behind a bsid 8 core the core's rate is kept.
    let core8 = with_bsid(&core, 8);
    let au = [core8.clone(), eac3_frame(1, 1, 3, 256)].concat();
    let (info, out) = decode(&au);
    assert_eq!(info.sample_rate, 48000);
    assert_eq!(out.planes, decode(&core8).1.planes);
    // Strict mode refuses the access unit instead.
    let mut strict = Decoder::with_options(DecoderOptions {
        ffmpeg_compat: false,
        ..Default::default()
    });
    assert!(matches!(
        strict.decode(&au, &mut AudioFrame::default()),
        Err(DecodeError::InvalidData(_))
    ));
}

#[test]
fn eac3_reduced_sample_rate_is_refused_like_ffmpeg() {
    let f = eac3_frame(0, 3, 0, 256);
    assert_eq!(parse_header(&f).unwrap().sample_rate, 24000);
    let mut out = AudioFrame::default();
    assert_eq!(
        Decoder::new().decode(&f, &mut out).err(),
        Some(DecodeError::Unsupported(Feature::Eac3ReducedSampleRate))
    );
    assert_eq!(out.samples_per_channel, 0);
    // A compatible-rate fscod2 dependent behind a 24 kHz bsid 9 core refuses the whole AU.
    let au = [k9(), eac3_frame(1, 3, 0, 256)].concat();
    assert_eq!(
        Decoder::new().decode(&au, &mut out).err(),
        Some(DecodeError::Unsupported(Feature::Eac3ReducedSampleRate))
    );
    // A/52 Annex E mode still decodes it.
    let mut strict = Decoder::with_options(DecoderOptions {
        ffmpeg_compat: false,
        ..Default::default()
    });
    assert_ne!(
        strict.decode(&f, &mut out).err(),
        Some(DecodeError::Unsupported(Feature::Eac3ReducedSampleRate))
    );
}
