//! Inter macroblocks against the complete transform and quantisation, and skips
//! reused for a repeated picture against the computed path.

use super::*;
use crate::EncoderConfig;

/// The capture rate rule at 1 Mbit/s for pictures of `geometry`.
fn config(geometry: Geometry) -> EncoderConfig {
    EncoderConfig {
        geometry,
        bit_rate: 1_000_000,
        max_rate: 1_400_000,
        vbv_bits: 142_857,
        gop: 30,
    }
}

fn encoder() -> Encoder {
    Encoder::new(config(Geometry::new(16, 16).unwrap())).unwrap()
}

fn code(
    e: &mut Encoder,
    q: u32,
    residual: &[i32; 64],
    base: u8,
    tight: bool,
) -> ([i32; 64], [i32; 64], usize) {
    let mut input = Picture::new(e.cfg.geometry);
    e.reference.y_mut().fill(base);
    e.reference.cb_mut().fill(base);
    e.reference.cr_mut().fill(base);
    input.y_mut().fill(base);
    input.cb_mut().fill(base);
    input.cr_mut().fill(base);
    for y in 0..8 {
        for x in 0..8 {
            let sample = i32::from(base) + residual[y * 8 + x];
            assert!((0..=255).contains(&sample));
            input.y_mut()[y * 16 + x] = u8::try_from(sample).unwrap();
        }
    }
    let mut expected = [0; 64];
    let mut coefficients = *residual;
    dct::fdct_raster(&mut coefficients);
    e.quant.inter(&coefficients, q, &mut expected);
    e.levels.fill([0; 64]);
    let mut bytes = Vec::new();
    e.writer.take_into(&mut bytes).unwrap();
    let mut state = SliceState {
        q,
        target_q: q,
        dc_pred: [128; 3],
        pmv: Mv::ZERO,
        last_intra: None,
        skipped: 0,
    };
    let inter = Inter {
        mv: Mv::ZERO,
        f_code: 1,
        edge: true,
        tight,
        repeated: false,
    };
    e.inter_mb(&input, 0, 0, 0, &mut state, inter);
    assert_eq!(state.q, q);
    assert_eq!(state.pmv, Mv::ZERO);
    (e.levels[0], expected, e.writer.bit_len())
}

#[test]
fn a_small_dc_residual_at_the_finest_quantiser_keeps_its_level() {
    let mut e = encoder();
    let mut r = [0; 64];
    r[0] = 20;
    let (actual, expected, _) = code(&mut e, 2, &r, 0, false);
    assert!(expected.iter().any(|v| *v != 0));
    assert_eq!(actual, expected);
}

#[test]
fn sparse_residuals_of_both_signs_match_the_complete_transform() {
    let mut e = encoder();
    for (q, amount, pair) in [
        (2, 20, false),
        (3, 30, false),
        (4, 40, false),
        (8, 80, false),
        (16, 160, false),
        (31, 150, true),
    ] {
        for sign in [-1, 1] {
            let mut r: [i32; 64] = [0; 64];
            r[0] = sign * amount;
            if pair {
                r[63] = sign * amount;
            }
            assert!(r.iter().map(|v| v.unsigned_abs()).sum::<u32>() < 12 * q);
            let (actual, expected, _) = code(&mut e, q, &r, if sign > 0 { 0 } else { 255 }, false);
            assert!(expected.iter().any(|v| *v != 0));
            assert_eq!(actual, expected, "q={q} sign={sign}");
        }
    }
}

#[test]
fn every_impulse_position_at_sample_extremes_matches_the_complete_transform() {
    let mut e = encoder();
    for q in [2, 7, 16, 31] {
        for magnitude in [1, 20, 50, 160, 255] {
            for position in 0..64 {
                for sign in [-1, 1] {
                    let mut r = [0; 64];
                    r[position] = sign * magnitude;
                    let (actual, expected, _) =
                        code(&mut e, q, &r, if sign > 0 { 0 } else { 255 }, false);
                    assert_eq!(
                        actual, expected,
                        "q={q} magnitude={magnitude} position={position} sign={sign}"
                    );
                }
            }
        }
    }
}

#[test]
fn residuals_proven_to_quantise_to_zero_code_as_an_empty_block() {
    let mut e = encoder();
    for q in 2..=31 {
        let (empty, complete, empty_bits) = code(&mut e, q, &[0; 64], 128, false);
        assert_eq!(empty, complete);
        for amount in 1..=7 {
            for sign in [-1, 1] {
                let mut r = [0; 64];
                r[9] = sign * amount;
                let (actual, expected, coded_bits) = code(&mut e, q, &r, 128, false);
                assert!(expected.iter().all(|v| *v == 0));
                assert_eq!(actual, expected);
                assert_eq!(coded_bits, empty_bits);
            }
        }
    }
}

#[test]
fn checkerboards_and_dense_residuals_match_the_complete_transform() {
    let mut e = encoder();
    for q in 2..=31 {
        for seed in 0..8 {
            let r = std::array::from_fn(|i| {
                if seed == 0 {
                    if (i / 8 + i % 8) % 2 == 0 { 127 } else { -127 }
                } else {
                    ((i * 37 + seed * 83) % 255) as i32 - 127
                }
            });
            let (actual, expected, _) = code(&mut e, q, &r, 128, false);
            assert_eq!(actual, expected, "q={q} seed={seed}");
        }
    }
}

#[test]
fn a_macroblock_under_the_vbv_guard_codes_no_coefficients() {
    let mut e = encoder();
    let mut residual = [0; 64];
    residual[0] = 20;
    let (tight, expected, _) = code(&mut e, 2, &residual, 128, true);
    assert!(expected.iter().any(|v| *v != 0));
    assert!(tight.iter().all(|v| *v == 0));
    let (ordinary, _, _) = code(&mut e, 2, &residual, 128, false);
    assert_eq!(ordinary, expected);
}

#[test]
fn skips_reused_for_a_repeated_picture_match_the_computed_path() {
    let geometry = Geometry::new(64, 48).unwrap();
    let cfg = config(geometry);
    // A gradient with a little texture: the first P pictures refine it, then
    // its residual quantises to zero and the macroblocks are skipped.
    let mut input = Picture::new(geometry);
    let mut seed = 1u32;
    let mut sample = |base: usize| {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        u8::try_from(base % 160 + 40).unwrap() + (seed >> 29) as u8
    };
    for (i, v) in input.y_mut().iter_mut().enumerate() {
        *v = sample(i % 64 + i / 64);
    }
    for (i, v) in input.cb_mut().iter_mut().enumerate() {
        *v = sample(2 * i);
    }
    for (i, v) in input.cr_mut().iter_mut().enumerate() {
        *v = sample(3 * i);
    }
    let mut reused = Encoder::new(cfg).unwrap();
    let mut computed = Encoder::new(cfg).unwrap();
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for _ in 0..12 {
        // `computed` never sees a repeated picture, so it codes every
        // macroblock from its residual.
        computed.previous_chroma.fill(0);
        reused.encode(&input, &mut a).unwrap();
        computed.encode(&input, &mut b).unwrap();
        assert_eq!(a, b);
        assert_eq!(reused.reference.y(), computed.reference.y());
        assert_eq!(reused.reference.cb(), computed.reference.cb());
        assert_eq!(reused.reference.cr(), computed.reference.cr());
    }
    assert!(
        reused.reused_skips > 0,
        "no macroblock was skipped for reuse"
    );
    assert_eq!(computed.reused_skips, 0);
}

#[test]
fn the_intra_probe_counts_the_same_bits_without_reconstructing() {
    let mut probe = encoder();
    let mut final_pass = encoder();
    let mut input = Picture::new(probe.cfg.geometry);
    for (i, sample) in input.y_mut().iter_mut().enumerate() {
        *sample = (i as u8).wrapping_mul(13);
    }
    input.cb_mut().fill(72);
    input.cr_mut().fill(160);
    for e in [&mut probe, &mut final_pass] {
        e.recon.y_mut().fill(1);
        e.recon.cb_mut().fill(2);
        e.recon.cr_mut().fill(3);
    }
    let mut trial = SliceState::new(8);
    let mut coded = SliceState::new(8);
    let ac_bits = probe.intra_mb::<true>(&input, 0, 0, 0, &mut trial, PictureType::Intra, false);
    final_pass.intra_mb::<false>(&input, 0, 0, 0, &mut coded, PictureType::Intra, false);
    assert!(ac_bits > 0);
    assert_eq!(probe.writer.bit_len(), final_pass.writer.bit_len());
    assert!(probe.recon.y().iter().all(|&sample| sample == 1));
    assert!(probe.recon.cb().iter().all(|&sample| sample == 2));
    assert!(probe.recon.cr().iter().all(|&sample| sample == 3));
    assert_ne!(probe.recon.y(), final_pass.recon.y());
}
