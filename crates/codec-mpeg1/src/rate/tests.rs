//! Picture plans against the headers already written and an I picture's side
//! bits, and what a picture coded under the VBV guard (censored: its texture cost
//! was not measured) may teach the model.

use super::*;
use crate::{Encoder, EncoderConfig};
use triergon_pixel::{Geometry, Picture};

/// A controller at 30 kbit/s whose intra model is seeded at α = 3200.
fn seeded(gop: u32) -> RateControl {
    let mut rc = RateControl::new(30_000, 42_000, 30_000, gop, 30.0);
    rc.seed_intra(0.0, 800.0, 4, 1.0);
    rc
}

fn finish(rc: &mut RateControl, bits: f64, texture: f64, q: f64, censored: bool) {
    rc.end(bits, (!censored).then_some(texture), q);
}

/// A 16×16 encoder at 30 kbit/s with a VBV buffer of `vbv_bits`.
fn encoder(vbv_bits: u32) -> Encoder {
    Encoder::new(EncoderConfig {
        geometry: Geometry::new(16, 16).unwrap(),
        bit_rate: 30_000,
        max_rate: 42_000,
        vbv_bits,
        gop: 30,
    })
    .unwrap()
}

#[test]
fn the_quantiser_models_the_texture_bits_left_after_the_headers() {
    let mut rc = seeded(1);
    let plan = rc.begin(PictureType::Intra, 1.0, 200);
    assert_eq!(plan.target, 1000.0);
    assert_eq!(plan.q_exact, 4.0); // 3200 / (1000 - 200), not 3200 / 1000.
}

#[test]
fn row_pacing_excludes_the_headers() {
    let mut rc = seeded(1);
    rc.seed_intra(0.0, 10_000.0, 4, 1.0);
    let plan = rc.begin(PictureType::Intra, 1.0, 200);
    assert_eq!(plan.q, 31); // Both models hit this clamp, isolating pacing.
    assert_eq!(rc.row_q(200.0, 0.25, 0.25), 31);
}

#[test]
fn headers_filling_the_target_select_the_coarsest_quantiser() {
    let mut rc = seeded(1);
    let plan = rc.begin(PictureType::Intra, 1.0, 1000);
    assert_eq!(plan.target, 1000.0);
    assert_eq!(plan.q, 31);
    assert_eq!(rc.row_q(0.0, 0.5, 0.5), 31);
}

#[test]
fn more_header_bits_leave_less_texture_allowance() {
    let mut no_header = seeded(1);
    let mut with_header = seeded(1);
    let a = no_header.begin(PictureType::Intra, 1.0, 0);
    let b = with_header.begin(PictureType::Intra, 1.0, 200);
    assert_eq!(a.target, b.target);
    assert!(b.q_exact > a.q_exact);
}

#[test]
fn without_headers_the_texture_target_is_the_picture_target() {
    let mut rc = seeded(1);
    let plan = rc.begin(PictureType::Intra, 1.0, 0);
    assert_eq!(plan.q_exact, 3.2);
    assert_eq!(plan.target, 1000.0);
    assert_eq!(plan.hard_limit, 30_000.0);
}

#[test]
fn headers_never_create_reservoir_or_average_rate_credit() {
    let mut rc = seeded(1);
    let before = rc.begin(PictureType::Intra, 1.0, 200);
    rc.end(1000.0, Some(800.0), 4.0);
    let after = rc.begin(PictureType::Intra, 1.0, 200);
    assert_eq!(rc.bank, 0.0);
    assert_eq!(rc.fullness, 30_000.0);
    assert_eq!(before.target, after.target);
    assert_eq!(before.hard_limit, after.hard_limit);
}

#[test]
fn a_predicted_picture_plans_with_its_own_headers() {
    let mut a = seeded(2);
    let mut b = seeded(2);
    a.begin(PictureType::Intra, 1.0, 100);
    b.begin(PictureType::Intra, 1.0, 100);
    a.end(1000.0, Some(900.0), 4.0);
    b.end(1000.0, Some(900.0), 4.0);
    let without = a.begin(PictureType::Predicted, 1.0, 0);
    let with = b.begin(PictureType::Predicted, 1.0, 200);
    assert_eq!(without.target, with.target);
    assert!(with.q_exact > without.q_exact);
}

#[test]
fn headers_larger_than_the_target_do_not_enlarge_the_buffer() {
    let mut rc = RateControl::new(30_000, 42_000, 600, 1, 30.0);
    rc.seed_intra(0.0, 800.0, 4, 1.0);
    let plan = rc.begin(PictureType::Intra, 1.0, 700);
    assert_eq!(plan.hard_limit, 600.0);
    assert_eq!(plan.target, 576.0);
    assert_eq!(plan.q, 31);
}

#[test]
fn the_average_bank_counts_the_byte_alignment() {
    let mut encoder = encoder(30_000);
    let mut picture = Picture::new(encoder.cfg.geometry);
    picture.y_mut().fill(128);
    picture.cb_mut().fill(128);
    picture.cr_mut().fill(128);
    let mut output = Vec::new();
    let report = encoder.encode(&picture, &mut output).unwrap();
    assert_eq!(report.bits, output.len() * 8);
    assert_eq!(encoder.rc.bank, 1000.0 - report.bits as f64);
}

#[test]
fn a_censored_intra_picture_leaves_the_texture_model_alone() {
    let mut rc = seeded(30);
    rc.begin(PictureType::Intra, 1.0, 200);
    let alpha = rc.alpha_i;
    let complexity = rc.x_i;
    finish(&mut rc, 300.0, 100.0, 4.0, true);
    assert_eq!(rc.alpha_i, alpha);
    assert_eq!(rc.x_i, complexity);
    assert_eq!(rc.bank, 700.0);
    assert_eq!(rc.fullness, 30_000.0);
}

#[test]
fn repeated_censored_pictures_cannot_collapse_the_probe_model() {
    let mut rc = seeded(30);
    let alpha = rc.alpha_i;
    for _ in 0..32 {
        rc.begin(PictureType::Intra, 1.0, 200);
        finish(&mut rc, 300.0, 100.0, 4.0, true);
        assert_eq!(rc.alpha_i, alpha);
    }
    assert_eq!(rc.bank, 32.0 * 700.0);
}

#[test]
fn a_censored_predicted_picture_advances_the_gop_without_learning() {
    let mut rc = seeded(30);
    rc.begin(PictureType::Intra, 1.0, 200);
    finish(&mut rc, 1000.0, 800.0, 4.0, false);
    rc.begin(PictureType::Predicted, 1.0, 40);
    let before = rc.left_p;
    let model = rc.alpha_p;
    let complexity = rc.x_p;
    finish(&mut rc, 60.0, 20.0, 4.0, true);
    assert_eq!(rc.alpha_p, model);
    assert_eq!(rc.x_p, complexity);
    assert_eq!(rc.left_p, before - 1);
    assert_eq!(rc.intra_q, None);
}

#[test]
fn measured_pictures_smooth_the_model_by_halves() {
    let mut rc = seeded(30);
    rc.begin(PictureType::Intra, 1.0, 200);
    finish(&mut rc, 600.0, 400.0, 4.0, false);
    assert_eq!(rc.alpha_i, Some(2400.0));
    assert_eq!(rc.x_i, 2400.0);
    assert_eq!(rc.bank, 400.0);
    assert_eq!(rc.intra_q, Some(4.0));
}

#[test]
fn censored_and_measured_pictures_debit_the_same_bits_and_underflow() {
    let mut a = seeded(30);
    let mut b = seeded(30);
    for bits in [60.0, 1000.0, 40_000.0, 1200.0] {
        a.begin(PictureType::Intra, 1.0, 200);
        b.begin(PictureType::Intra, 1.0, 200);
        finish(&mut a, bits, (bits - 200.0).max(0.0), 4.0, true);
        finish(&mut b, bits, (bits - 200.0).max(0.0), 4.0, false);
        assert_eq!(a.bank, b.bank);
        assert_eq!(a.fullness, b.fullness);
        assert_eq!(a.underflow_bits, b.underflow_bits);
        assert_eq!(a.last_q, b.last_q);
    }
    assert!(a.underflow_bits > 0.0);
}

#[test]
fn an_intra_picture_under_the_vbv_guard_underflows_and_keeps_the_seeded_model() {
    let mut e = encoder(1);
    e.rc.seed_intra(0.0, 800.0, 4, 1.0);
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
fn a_measured_intra_picture_updates_the_model_and_keeps_the_gop() {
    let mut e = encoder(30_000);
    e.rc.seed_intra(0.0, 800.0, 4, 1.0);
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
fn an_unseeded_censored_model_stays_unknown() {
    let mut rc = RateControl::new(30_000, 42_000, 30_000, 30, 30.0);
    rc.begin(PictureType::Intra, 1.0, 200);
    finish(&mut rc, 300.0, 100.0, 4.0, true);
    assert_eq!(rc.alpha_i, None);
    assert!(rc.needs_intra_probe());
    assert_eq!(rc.bank, 700.0);
}

/// A controller at 30 kbit/s with GOP 1 seeded with 600 side bits and 1600
/// coefficient bits at q 4 over unit activity (α = 6400).
fn seeded_with_side_bits() -> RateControl {
    let mut rc = RateControl::new(30_000, 42_000, 30_000, 1, 30.0);
    rc.seed_intra(600.0, 1600.0, 4, 1.0);
    rc
}

#[test]
fn the_intra_quantiser_models_only_the_coefficient_bits() {
    let mut rc = seeded_with_side_bits();
    let plan = rc.begin(PictureType::Intra, 1.0, 0);
    assert_eq!(plan.target, 1000.0);
    // 6400 / (1000 - 600): the side bits are paid at any quantiser.
    assert_eq!(plan.q_exact, 16.0);
}

#[test]
fn the_i_target_pays_its_side_bits_before_the_tm5_split() {
    let mut without = RateControl::new(30_000, 42_000, 30_000, 30, 30.0);
    let mut with = RateControl::new(30_000, 42_000, 30_000, 30, 30.0);
    without.seed_intra(0.0, 1600.0, 4, 1.0);
    with.seed_intra(600.0, 1600.0, 4, 1.0);
    let share = without.begin(PictureType::Intra, 1.0, 0).target / 30_000.0;
    let target = with.begin(PictureType::Intra, 1.0, 0).target;
    // The GOP's 30,000 bits less the 600 side bits are shared as before.
    assert!(
        (target - (600.0 + share * 29_400.0)).abs() < 1e-9,
        "{target}"
    );
}

#[test]
fn side_bits_filling_the_vbv_select_the_coarsest_quantiser() {
    let mut rc = RateControl::new(30_000, 42_000, 1000, 1, 30.0);
    rc.seed_intra(1200.0, 1600.0, 4, 1.0);
    let plan = rc.begin(PictureType::Intra, 1.0, 0);
    assert_eq!(plan.side_bits, 960.0);
    assert_eq!(plan.q, 31);
}

#[test]
fn row_pacing_spreads_side_bits_by_rows_and_coefficients_by_activity() {
    let mut rc = seeded_with_side_bits();
    let plan = rc.begin(PictureType::Intra, 1.0, 0);
    // Half the rows with a quarter of the activity: 300 + 400 / 4 planned.
    assert_eq!(rc.row_q(400.0, 0.5, 0.25), plan.q);
    assert_eq!(rc.row(400, 0, 0.0, 0.5, 0.25), (plan.q, false));
}

#[test]
fn a_measured_intra_picture_learns_the_coefficients_beyond_its_side_bits() {
    let mut rc = seeded_with_side_bits();
    rc.begin(PictureType::Intra, 1.0, 0);
    rc.end(1000.0, Some(1000.0), 8.0);
    // Halfway from 6400 to (1000 - 600) · 8; the side bits stay the trial's.
    assert_eq!(rc.alpha_i, Some(4800.0));
    assert_eq!(rc.side_i, 600.0);
}

#[test]
fn a_predicted_picture_models_all_its_texture_bits() {
    let mut rc = seeded_with_side_bits();
    rc.gop = 30;
    rc.begin(PictureType::Intra, 1.0, 0);
    rc.end(30_000.0, Some(30_000.0), 4.0);
    let plan = rc.begin(PictureType::Predicted, 1.0, 0);
    assert_eq!(plan.side_bits, 0.0);
    rc.end(800.0, Some(800.0), 5.0);
    assert_eq!(rc.alpha_p, Some(4000.0));
}

/// An I picture whose 8×8 blocks are flat has no AC coefficient at any
/// quantiser: all its bits are side bits, which the trial pass over every second
/// row measures for the whole picture.
#[test]
fn the_trial_pass_measures_the_side_bits_of_the_whole_picture() {
    let geometry = Geometry::new(480, 272).unwrap();
    let mut e = Encoder::new(EncoderConfig {
        geometry,
        bit_rate: 600_000,
        max_rate: 840_000,
        vbv_bits: 1_000_000,
        gop: 30,
    })
    .unwrap();
    let mut input = Picture::new(geometry);
    let stride = geometry.luma_stride();
    for (i, p) in input.y_mut().iter_mut().enumerate() {
        let (x, y) = (i % stride / 8, i / stride / 8);
        *p = [40, 180, 110][(x + 2 * y) % 3];
    }
    input.cb_mut().fill(128);
    input.cr_mut().fill(128);
    let mut output = Vec::new();
    let picture = e.encode(&input, &mut output).unwrap();
    let side = e.rc.side_i;
    assert!(
        (picture.bits as f64 - side).abs() < 0.02 * side,
        "{} {side}",
        picture.bits
    );
}
