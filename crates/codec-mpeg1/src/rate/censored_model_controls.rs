//! Baseline and candidate use the real rate controller; only the explicitly
//! private driver routes the same censored observation to the new method.
use super::*;
use crate::{Encoder, EncoderConfig};
use triergon_pixel::{Geometry, Picture};

fn finish(rc: &mut RateControl, bits: f64, texture: f64, q: f64, censored: bool) {
    if censored {
        rc.end_censored(bits, q);
    } else {
        rc.end(bits, texture, q);
    }
}

fn seeded() -> RateControl {
    let mut rc = RateControl::new(30_000, 42_000, 30_000, 30, 30.0);
    rc.seed_intra(800.0, 4, 1.0);
    rc
}

#[test]
fn censored_intra_does_not_relabel_missing_ac_as_lower_texture_complexity() {
    let mut rc = seeded();
    rc.begin(Kind::Intra, 1.0, 200);
    let alpha = rc.alpha_i;
    let complexity = rc.x_i;
    finish(&mut rc, 300.0, 100.0, 4.0, true);
    assert_eq!(rc.alpha_i, alpha);
    assert_eq!(rc.x_i, complexity);
    assert_eq!(rc.bank, 700.0);
    assert_eq!(rc.fullness, 30_000.0);
}

#[test]
fn repeated_censored_noise_observations_cannot_collapse_the_probe_model() {
    let mut rc = seeded();
    let alpha = rc.alpha_i;
    for _ in 0..32 {
        rc.begin(Kind::Intra, 1.0, 200);
        finish(&mut rc, 300.0, 100.0, 4.0, true);
        assert_eq!(rc.alpha_i, alpha);
    }
    assert_eq!(rc.bank, 32.0 * 700.0);
}

#[test]
fn censored_predicted_picture_still_advances_gop_without_learning_a_false_sample() {
    let mut rc = seeded();
    rc.begin(Kind::Intra, 1.0, 200);
    finish(&mut rc, 1000.0, 800.0, 4.0, false);
    rc.begin(Kind::Predicted, 1.0, 40);
    let before = rc.left_p;
    let model = rc.alpha_p;
    let complexity = rc.x_p;
    finish(&mut rc, 60.0, 20.0, 4.0, true);
    assert_eq!(rc.alpha_p, model);
    assert_eq!(rc.x_p, complexity);
    assert_eq!(rc.left_p, before - 1);
    assert!(!rc.after_intra);
}

#[test]
fn uncensored_observations_keep_the_existing_exact_smoothing_rule() {
    let mut rc = seeded();
    rc.begin(Kind::Intra, 1.0, 200);
    finish(&mut rc, 600.0, 400.0, 4.0, false);
    assert_eq!(rc.alpha_i, Some(2400.0));
    assert_eq!(rc.x_i, 2400.0);
    assert_eq!(rc.bank, 400.0);
    assert_eq!(rc.intra_q, 4.0);
    assert!(rc.after_intra);
}

#[test]
fn censored_and_uncensored_paths_debit_identical_real_bytes_and_underflow() {
    let mut a = seeded();
    let mut b = seeded();
    for bits in [60.0, 1000.0, 40_000.0, 1200.0] {
        a.begin(Kind::Intra, 1.0, 200);
        b.begin(Kind::Intra, 1.0, 200);
        finish(&mut a, bits, (bits - 200.0).max(0.0), 4.0, true);
        finish(&mut b, bits, (bits - 200.0).max(0.0), 4.0, false);
        assert_eq!(a.bank, b.bank);
        assert_eq!(a.fullness, b.fullness);
        assert_eq!(a.underflow_bits, b.underflow_bits);
        assert_eq!(a.last_q, b.last_q);
    }
    assert!(a.underflow_bits > 0.0);
}

fn config(buffer: u32) -> EncoderConfig {
    EncoderConfig {
        geometry: Geometry::new(16, 16).unwrap(),
        bit_rate: 30_000,
        max_rate: 42_000,
        vbv_bits: buffer,
        gop: 30,
    }
}

#[test]
fn real_emergency_intra_picture_reports_underflow_but_keeps_probe_identity() {
    let mut e = Encoder::new(config(1)).unwrap();
    e.rc.seed_intra(800.0, 4, 1.0);
    let model = e.rc.alpha_i;
    let mut input = Picture::new(e.cfg.geometry);
    for (i, p) in input.y_mut().iter_mut().enumerate() {
        *p = ((i * 137) % 256) as u8;
    }
    let mut output = Vec::new();
    let picture = e.encode(&input, &mut output).unwrap();
    assert!(picture.key);
    assert_eq!(picture.bits, output.len() * 8);
    assert!(e.vbv_underflow_bits() > 0.0);
    assert_eq!(e.rc.alpha_i, model);
    assert_eq!(e.rc.bank, 1000.0 - picture.bits as f64);
}

#[test]
fn real_full_intra_picture_still_learns_and_keeps_the_original_gop() {
    let mut e = Encoder::new(config(30_000)).unwrap();
    e.rc.seed_intra(800.0, 4, 1.0);
    let model = e.rc.alpha_i;
    let input = Picture::new(e.cfg.geometry);
    let mut output = Vec::new();
    let first = e.encode(&input, &mut output).unwrap();
    assert!(first.key);
    assert_ne!(e.rc.alpha_i, model);
    assert_eq!(e.vbv_underflow_bits(), 0.0);
    let next = e.encode(&input, &mut output).unwrap();
    assert!(!next.key);
    assert_eq!(next.index, 1);
}

#[test]
fn unseeded_censored_model_stays_unknown_instead_of_inventing_an_alpha() {
    let mut rc = RateControl::new(30_000, 42_000, 30_000, 30, 30.0);
    rc.begin(Kind::Intra, 1.0, 200);
    finish(&mut rc, 300.0, 100.0, 4.0, true);
    assert_eq!(rc.alpha_i, None);
    assert!(rc.needs_intra_probe());
    assert_eq!(rc.bank, 700.0);
}
