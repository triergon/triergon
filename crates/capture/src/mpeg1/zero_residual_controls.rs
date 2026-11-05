//! Real existing encoder path; baseline compiles these same guards before fix.
use super::*;

fn encoder() -> Encoder {
    Encoder::new(EncoderConfig {
        geometry: Geometry::new(16, 16).unwrap(),
        bit_rate: 1_000_000,
        max_rate: 1_400_000,
        vbv_bits: 142_857,
        gop: 30,
    })
    .unwrap()
}

fn code(
    e: &mut Encoder,
    q: u32,
    residual: &[i32; 64],
    base: u8,
    tight: bool,
) -> ([i32; 64], [i32; 64], usize) {
    let mut input = Picture::new(e.cfg.geometry);
    e.reference.y.fill(base);
    e.reference.cb.fill(base);
    e.reference.cr.fill(base);
    input.y.fill(base);
    input.cb.fill(base);
    input.cr.fill(base);
    for y in 0..8 {
        for x in 0..8 {
            let sample = i32::from(base) + residual[y * 8 + x];
            assert!((0..=255).contains(&sample));
            input.y[y * 16 + x] = u8::try_from(sample).unwrap();
        }
    }
    let mut expected = [0; 64];
    let mut coefficients = *residual;
    dct::fdct(&mut coefficients);
    e.quant.inter(&coefficients, q, &mut expected);
    e.levels.fill([0; 64]);
    let mut bytes = Vec::new();
    e.writer.take_into(&mut bytes);
    let mut state = SliceState {
        q,
        target_q: q,
        dc_pred: [128; 3],
        pmv: Mv::ZERO,
        last_intra: None,
        skipped: 0,
    };
    e.inter_mb(&input, 0, 0, 0, &mut state, Mv::ZERO, 1, true, tight);
    assert_eq!(state.q, q);
    assert_eq!(state.pmv, Mv::ZERO);
    (e.levels[0], expected, e.writer.bit_len())
}

#[test]
fn q2_low_sad_impulse_must_not_erase_nonzero_quantized_coefficients() {
    let mut e = encoder();
    let mut r = [0; 64];
    r[0] = 20;
    let (actual, expected, _) = code(&mut e, 2, &r, 0, false);
    assert!(expected.iter().any(|v| *v != 0));
    assert_eq!(
        actual, expected,
        "SAD20 < old24, yet complete fixed transform is not empty"
    );
}

#[test]
fn sparse_residuals_of_both_signs_and_all_quantizers_match_complete_transform() {
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
fn every_impulse_position_at_pixel_extremes_matches_complete_quantization() {
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
fn provably_small_residuals_remain_empty_with_the_same_actual_syntax() {
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
fn checkerboards_and_dense_signed_residuals_match_all64_quantized_levels() {
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
fn fixed_transform_magnitude_is_bounded_by_ceil_sad_over_four() {
    for seed in 0..256 {
        let input = std::array::from_fn(|i| {
            if seed < 64 {
                if i == seed { 255 } else { 0 }
            } else if seed < 128 {
                if i == seed - 64 { -255 } else { 0 }
            } else {
                ((i * 137 + seed * 293) % 511) as i32 - 255
            }
        });
        let bound = input
            .iter()
            .map(|v| v.unsigned_abs())
            .sum::<u32>()
            .div_ceil(4);
        let mut coefficients = input;
        dct::fdct(&mut coefficients);
        assert!(coefficients.iter().all(|v| v.unsigned_abs() <= bound));
    }
}

#[test]
fn explicit_existing_vbv_tight_policy_remains_separate_from_zero_proof() {
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
fn geometry_rejection_keeps_owned_output_and_subsequent_gop_state_unchanged() {
    let mut e = encoder();
    let wrong = Picture::new(Geometry::new(32, 16).unwrap());
    let mut output = vec![9, 8, 7];
    assert!(matches!(
        e.encode(&wrong, &mut output),
        Err(CaptureError::PictureGeometry { .. })
    ));
    assert_eq!(output, [9, 8, 7]);
    assert_eq!(e.index, 0);
    assert_eq!(e.gop_start, 0);
    let input = Picture::new(e.cfg.geometry);
    let first = e.encode(&input, &mut output).unwrap();
    assert!(first.key);
    assert_eq!(first.index, 0);
    assert_eq!(first.bits, output.len() * 8);
    let next = e.encode(&input, &mut output).unwrap();
    assert!(!next.key);
    assert_eq!(next.index, 1);
    assert_eq!(next.bits, output.len() * 8);
}
