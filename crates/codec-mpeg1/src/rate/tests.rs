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
    let plan = rc.begin(PictureType::Intra, 1.0, 200, false);
    assert_eq!(plan.target, 1000.0);
    assert_eq!(plan.q_exact, 4.0); // 3200 / (1000 - 200), not 3200 / 1000.
}

#[test]
fn row_pacing_excludes_the_headers() {
    let mut rc = seeded(1);
    rc.seed_intra(0.0, 10_000.0, 4, 1.0);
    let plan = rc.begin(PictureType::Intra, 1.0, 200, false);
    assert_eq!(plan.q, 31); // Both models hit this clamp, isolating pacing.
    assert_eq!(rc.row_q(200.0, 0.25, 0.25), 31);
}

#[test]
fn headers_filling_the_target_select_the_coarsest_quantiser() {
    let mut rc = seeded(1);
    let plan = rc.begin(PictureType::Intra, 1.0, 1000, false);
    assert_eq!(plan.target, 1000.0);
    assert_eq!(plan.q, 31);
    assert_eq!(rc.row_q(0.0, 0.5, 0.5), 31);
}

#[test]
fn more_header_bits_leave_less_texture_allowance() {
    let mut no_header = seeded(1);
    let mut with_header = seeded(1);
    let a = no_header.begin(PictureType::Intra, 1.0, 0, false);
    let b = with_header.begin(PictureType::Intra, 1.0, 200, false);
    assert_eq!(a.target, b.target);
    assert!(b.q_exact > a.q_exact);
}

#[test]
fn without_headers_the_texture_target_is_the_picture_target() {
    let mut rc = seeded(1);
    let plan = rc.begin(PictureType::Intra, 1.0, 0, false);
    assert_eq!(plan.q_exact, 3.2);
    assert_eq!(plan.target, 1000.0);
    assert_eq!(plan.hard_limit, 30_000.0);
}

#[test]
fn headers_never_create_reservoir_or_average_rate_credit() {
    let mut rc = seeded(1);
    let before = rc.begin(PictureType::Intra, 1.0, 200, false);
    rc.end(1000.0, Some(800.0), 4.0);
    let after = rc.begin(PictureType::Intra, 1.0, 200, false);
    assert_eq!(rc.bank, 0.0);
    assert_eq!(rc.fullness, 30_000.0);
    assert_eq!(before.target, after.target);
    assert_eq!(before.hard_limit, after.hard_limit);
}

#[test]
fn a_predicted_picture_plans_with_its_own_headers() {
    let mut a = seeded(2);
    let mut b = seeded(2);
    a.begin(PictureType::Intra, 1.0, 100, false);
    b.begin(PictureType::Intra, 1.0, 100, false);
    a.end(1000.0, Some(900.0), 4.0);
    b.end(1000.0, Some(900.0), 4.0);
    let without = a.begin(PictureType::Predicted, 1.0, 0, false);
    let with = b.begin(PictureType::Predicted, 1.0, 200, false);
    assert_eq!(without.target, with.target);
    assert!(with.q_exact > without.q_exact);
}

#[test]
fn headers_larger_than_the_target_do_not_enlarge_the_buffer() {
    let mut rc = RateControl::new(30_000, 42_000, 600, 1, 30.0);
    rc.seed_intra(0.0, 800.0, 4, 1.0);
    let plan = rc.begin(PictureType::Intra, 1.0, 700, false);
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
    rc.begin(PictureType::Intra, 1.0, 200, false);
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
        rc.begin(PictureType::Intra, 1.0, 200, false);
        finish(&mut rc, 300.0, 100.0, 4.0, true);
        assert_eq!(rc.alpha_i, alpha);
    }
    assert_eq!(rc.bank, 32.0 * 700.0);
}

#[test]
fn a_censored_predicted_picture_advances_the_gop_without_learning() {
    let mut rc = seeded(30);
    rc.begin(PictureType::Intra, 1.0, 200, false);
    finish(&mut rc, 1000.0, 800.0, 4.0, false);
    rc.begin(PictureType::Predicted, 1.0, 40, false);
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
    rc.begin(PictureType::Intra, 1.0, 200, false);
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
        a.begin(PictureType::Intra, 1.0, 200, false);
        b.begin(PictureType::Intra, 1.0, 200, false);
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
    rc.begin(PictureType::Intra, 1.0, 200, false);
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
    let plan = rc.begin(PictureType::Intra, 1.0, 0, false);
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
    let share = without.begin(PictureType::Intra, 1.0, 0, false).target / 30_000.0;
    let target = with.begin(PictureType::Intra, 1.0, 0, false).target;
    // The GOP's 30,000 bits less the 600 side bits are shared as before.
    assert!(
        (target - (600.0 + share * 29_400.0)).abs() < 1e-9,
        "{target}"
    );
}

#[test]
fn the_first_i_target_expects_p_pictures_predicted_from_the_picture_before() {
    let mut rc = RateControl::new(30_000, 42_000, 30_000, 30, 30.0);
    rc.seed_intra(0.0, 1600.0, 4, 1.0);
    let plan = rc.begin(PictureType::Intra, 1.0, 0, false);
    // TM5's I share with 29 P pictures at its initial 42 : 160 complexity.
    assert_eq!(plan.target, 30_000.0 / (1.0 + 29.0 * 42.0 / (160.0 * KP)));
}

#[test]
fn side_bits_filling_the_vbv_select_the_coarsest_quantiser() {
    let mut rc = RateControl::new(30_000, 42_000, 1000, 1, 30.0);
    rc.seed_intra(1200.0, 1600.0, 4, 1.0);
    let plan = rc.begin(PictureType::Intra, 1.0, 0, false);
    assert_eq!(plan.side_bits, 960.0);
    assert_eq!(plan.q, 31);
}

#[test]
fn row_pacing_spreads_side_bits_by_rows_and_coefficients_by_activity() {
    let mut rc = seeded_with_side_bits();
    let plan = rc.begin(PictureType::Intra, 1.0, 0, false);
    // Half the rows with a quarter of the activity: 300 + 400 / 4 planned.
    assert_eq!(rc.row_q(400.0, 0.5, 0.25), plan.q);
    assert_eq!(rc.row(400, 0, 0.0, 0.5, 0.25), (plan.q, false));
}

#[test]
fn a_measured_intra_picture_learns_the_coefficients_beyond_its_side_bits() {
    let mut rc = seeded_with_side_bits();
    rc.begin(PictureType::Intra, 1.0, 0, false);
    rc.end(1000.0, Some(1000.0), 8.0);
    // Halfway from 6400 to (1000 - 600) · 8; the side bits stay the trial's.
    assert_eq!(rc.alpha_i, Some(4800.0));
    assert_eq!(rc.side_i, 600.0);
}

#[test]
fn a_predicted_picture_models_all_its_texture_bits() {
    let mut rc = seeded_with_side_bits();
    rc.gop = 30;
    rc.begin(PictureType::Intra, 1.0, 0, false);
    rc.end(30_000.0, Some(30_000.0), 4.0);
    let plan = rc.begin(PictureType::Predicted, 1.0, 0, false);
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

/// The luma squared error of the encoder's reconstruction against `input`.
fn luma_error(e: &Encoder, input: &Picture) -> u64 {
    let g = e.cfg.geometry;
    (0..g.height())
        .flat_map(|y| (0..g.width()).map(move |x| y * g.luma_stride() + x))
        .map(|i| u64::from(e.reference.y()[i].abs_diff(input.y()[i])).pow(2))
        .sum()
}

/// A static scene whose I picture its share of the GOP codes coarsely, with a
/// VBV buffer that can carry a large picture: the first P picture repeats the
/// source, so it is planned as the I picture of the rest of the GOP and refines
/// the reference below the quantiser that bounds a first P picture.
#[test]
fn a_static_scene_is_refined_by_its_first_repeated_picture() {
    let geometry = Geometry::new(480, 272).unwrap();
    let mut e = Encoder::new(EncoderConfig {
        geometry,
        bit_rate: 450_000,
        max_rate: 630_000,
        vbv_bits: 1_000_000,
        gop: 30,
    })
    .unwrap();
    let mut input = Picture::new(geometry);
    let stride = geometry.luma_stride();
    let mut seed = 7u32;
    for (i, p) in input.y_mut().iter_mut().enumerate() {
        let (x, y) = (i % stride, i / stride);
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        *p = (((x / 24 + y / 12) % 2) * 120 + 60 + (x % 8) * 3 + (seed >> 29) as usize) as u8;
    }
    input.cb_mut().fill(120);
    input.cr_mut().fill(136);
    let mut output = Vec::new();
    let intra = e.encode(&input, &mut output).unwrap();
    let intra_error = luma_error(&e, &input);
    let refined = e.encode(&input, &mut output).unwrap();
    assert!(!refined.key);
    assert!(
        4 * refined.qscale_mean < 3 * intra.qscale_mean,
        "{refined:?} {intra:?}"
    );
    assert!(refined.bits > 4 * 15_000, "{refined:?}");
    assert!(3 * luma_error(&e, &input) < 2 * intra_error);
}

/// A controller after an I picture of 20,000 bits at q 8 that left 10,000 bits
/// of the GOP for its 29 P pictures.
fn after_a_large_intra_picture() -> RateControl {
    let mut rc = RateControl::new(30_000, 42_000, 30_000, 30, 30.0);
    rc.seed_intra(0.0, 1600.0, 4, 1.0);
    rc.begin(PictureType::Intra, 1.0, 0, false);
    rc.end(20_000.0, Some(20_000.0), 8.0);
    rc
}

#[test]
fn a_repeated_picture_is_planned_as_the_i_picture_of_the_rest_of_the_gop() {
    let mut moving = after_a_large_intra_picture();
    let mut repeated = after_a_large_intra_picture();
    assert_eq!(
        moving.begin(PictureType::Predicted, 1.0, 0, false).target,
        10_000.0 / 29.0
    );
    let rest = 28.0 * repeated.x_p / (repeated.x_i * KP);
    assert_eq!(
        repeated.begin(PictureType::Predicted, 1.0, 0, true).target,
        10_000.0 / (1.0 + rest)
    );
}

#[test]
fn only_a_moving_first_p_picture_keeps_near_the_intra_quantiser() {
    let mut moving = after_a_large_intra_picture();
    let mut repeated = after_a_large_intra_picture();
    moving.alpha_p = Some(100.0);
    repeated.alpha_p = Some(100.0);
    assert_eq!(moving.begin(PictureType::Predicted, 1.0, 0, false).q, 6);
    assert_eq!(repeated.begin(PictureType::Predicted, 1.0, 0, true).q, 2);
}
