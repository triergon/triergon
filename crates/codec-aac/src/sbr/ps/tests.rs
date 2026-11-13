//! Rules of the parameter side, with expected values from the recovery probes.

use super::*;

#[test]
fn iid_alone_sets_the_level_ratio_and_keeps_the_power() {
    // probes e14 and m04: 20 log10(L/R) is the grid value, L^2 + R^2 = 2
    let m = matrices();
    for (fine, grid) in [(0, &t::IID_COARSE[..]), (1, &t::IID_FINE[..])] {
        for (i, &db) in grid.iter().enumerate() {
            let h = m[0][fine][15 + i][0];
            let ratio = 20.0 * (f64::from(h[0]) / f64::from(h[2])).log10();
            assert!((ratio - db).abs() < 1e-4, "{fine} {i}: {ratio}");
            let power = h[0] * h[0] + h[2] * h[2];
            assert!((power - 2.0).abs() < 1e-6);
            assert!(h[1].abs() < 1e-7 && h[3].abs() < 1e-7);
        }
    }
}

#[test]
fn icc_alone_scales_the_sum_by_cos_alpha() {
    // probe e15: (L + R) / 2 = cos(alpha) M, alpha = arccos(ICC) / 2, per index
    let fitted = [
        1.0,
        0.984_124_0,
        0.959_473_8,
        0.894_684_3,
        0.826_934_1,
        std::f64::consts::FRAC_1_SQRT_2, // fitted 0.7071068
        0.453_321_1,
        0.0,
    ];
    for (c, &want) in fitted.iter().enumerate() {
        let h = matrices()[0][0][15][c];
        assert!((f64::from(h[0] + h[2]) / 2.0 - want).abs() < 2e-7, "{c}");
        assert!((h[1] + h[3]).abs() < 1e-7);
    }
}

#[test]
fn second_procedure_floors_icc() {
    // probe m04: ICC indices 5, 6 and 7 mix alike in ICC modes 3..5
    let m = matrices();
    for i in 0..31 {
        assert_eq!(m[1][1][i][5], m[1][1][i][7]);
        assert_eq!(m[1][1][i][6], m[1][1][i][7]);
    }
}

#[test]
fn coded_values_map_onto_the_configuration() {
    // probe h06: 20 -> 34 pairs take the index mean truncated toward zero; 10 -> 20 copies
    let mut src = [0i8; MAX_PAR];
    src[..4].copy_from_slice(&[-7, 6, -1, -2]);
    let mut out = [0i8; MAX_PAR];
    to_config(&src, 20, true, &mut out);
    assert_eq!((out[1], out[4]), (0, -1));
    let mut ten = [0i8; MAX_PAR];
    ten[..10].copy_from_slice(&[0, 1, 2, 3, 4, 5, 6, 7, -1, -2]);
    to_config(&ten, 10, false, &mut out);
    assert_eq!(
        &out[..20],
        &[
            0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, -1, -1, -2, -2
        ]
    );
}

#[test]
fn ipd_of_34_bands_maps_onto_20_by_truncated_means() {
    // probe p07: IPD indices (m s + c) % 8 of 17 bands, applied in 20 bands
    let cases = [
        (1, 0, [0u8, 1, 3, 4, 6, 0, 2, 3, 4, 6, 0]),
        (3, 1, [2, 6, 3, 1, 4, 2, 7, 2, 2, 4, 1]),
    ];
    for (m, c, want) in cases {
        let mut src = [0u8; MAX_PAR];
        for (s, v) in src.iter_mut().enumerate().take(17) {
            *v = ((m * s + c) % 8) as u8;
        }
        let mut out = [0u8; MAX_PAR];
        to_config(&src, 34, false, &mut out);
        assert_eq!(&out[..11], &want);
        assert!(out[11..].iter().all(|&v| v == 0));
    }
}

#[test]
fn class_one_borders_get_an_envelope_to_the_end() {
    // probes t03, t21 and t23
    let raw = |class1, coded_env, borders| Raw {
        header: None,
        class1,
        coded_env,
        borders,
        iid: [(false, [0; MAX_PAR]); 4],
        icc: [(false, [0; MAX_PAR]); 4],
        ipd: None,
    };
    let (b, n) = Ps::borders(&raw(true, 2, [4, 20, 0, 0])).unwrap();
    assert_eq!(&b[..n], &[4, 20, 31]);
    let (b, n) = Ps::borders(&raw(false, 4, [0; 4])).unwrap();
    assert_eq!(&b[..n], &[7, 15, 23, 31]);
    let (b, n) = Ps::borders(&raw(false, 0, [0; 4])).unwrap();
    assert_eq!(&b[..n], &[31]);
    assert!(Ps::borders(&raw(true, 3, [9, 4, 31, 0])).is_none());
}
