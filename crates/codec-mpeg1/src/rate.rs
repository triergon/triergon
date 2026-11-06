//! Picture-level rate control with explicit whole-picture and texture budgets.
//!
//! Bit allocation across a GOP follows the published MPEG-2 Test Model 5
//! complexity rule (I/P only, I quantiser ≈ 0.8× the P quantiser). Each
//! picture then uses ONE quantiser, chosen from a per-type model
//! `bits ≈ α · activity / q` whose α is learned from the previous picture of
//! the same type; a uniform quantiser within a picture is what keeps the
//! distortion low for a given size. Rows only deviate from it by a bounded
//! amount when the running total drifts from the activity-weighted plan, or
//! when the VBV would otherwise underflow.
//!
//! The VBV model is the variable-rate case of ISO/IEC 11172-2 Annex C /
//! H.262 Annex C: the buffer fills at `max_rate` until full and each picture
//! is removed whole at its decode time; a picture larger than the occupancy
//! is an underflow.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Intra,
    Predicted,
}

#[derive(Clone, Copy, Debug)]
pub struct Plan {
    pub q: u32,
    /// Unrounded model quantiser; rows mix its two neighbours.
    pub q_exact: f64,
    /// Complete picture target, including the headers already emitted.
    pub target: f64,
    /// Remaining target after those actual headers; never negative.
    pub texture_target: f64,
    /// Physical VBV occupancy available for this picture. Any emitted
    /// overshoot is still reported by `end`, never converted into credit.
    pub hard_limit: f64,
}

#[derive(Debug)]
pub struct RateControl {
    frame_bits: f64,
    gop: u32,
    vbv_size: f64,
    refill: f64,
    fullness: f64,
    /// Σ(frame budget − picture bits) over the session: TM5's GOP carry-over
    /// expressed per picture, so a scene-change I picture can restart the GOP.
    bank: f64,
    /// P pictures left in the current GOP (including the current one).
    left_p: u32,
    x_i: f64,
    x_p: f64,
    alpha_i: Option<f64>,
    alpha_p: Option<f64>,
    kind: Kind,
    plan: Plan,
    activity: f64,
    /// Total VBV underflow in bits (should stay 0).
    pub underflow_bits: f64,
    pub last_q: u32,
    /// Mean quantiser of the latest I picture and whether the next P picture
    /// is the first after it.
    intra_q: f64,
    after_intra: bool,
}

/// TM5 K_p, expressed as FFmpeg's default I/P quantiser ratio 1/0.8.
const KP: f64 = 1.25;
/// Largest share of the current VBV occupancy one picture may plan for; the
/// rest absorbs model error before the in-picture guard escalates.
const TARGET_VBV_SHARE: f64 = 0.96;
/// Lower bound of the first P quantiser relative to the preceding I picture.
const FIRST_P_RATIO: f64 = 0.75;

impl RateControl {
    pub fn new(bit_rate: u32, max_rate: u32, vbv_bits: u32, gop: u32, fps: f64) -> RateControl {
        let b = bit_rate as f64;
        RateControl {
            frame_bits: b / fps,
            gop,
            vbv_size: vbv_bits as f64,
            refill: max_rate as f64 / fps,
            fullness: vbv_bits as f64,
            bank: 0.0,
            left_p: 0,
            x_i: 160.0 * b / 115.0,
            x_p: 60.0 * b / 115.0,
            alpha_i: None,
            alpha_p: None,
            kind: Kind::Intra,
            plan: Plan {
                q: 8,
                q_exact: 8.0,
                target: 0.0,
                texture_target: 0.0,
                hard_limit: 0.0,
            },
            activity: 1.0,
            underflow_bits: 0.0,
            last_q: 8,
            intra_q: 8.0,
            after_intra: false,
        }
    }

    /// Whether the intra model still needs a probe measurement.
    pub fn needs_intra_probe(&self) -> bool {
        self.alpha_i.is_none()
    }

    /// Seed α for intra pictures from a sampled trial encode.
    pub fn seed_intra(&mut self, bits: f64, q: u32, activity: f64) {
        self.alpha_i = Some(bits * q as f64 / activity.max(1.0));
    }

    /// Plan a complete picture, debiting the actual headers already emitted
    /// before applying the learned texture-bit model to the remaining budget.
    pub fn begin(&mut self, kind: Kind, activity: f64, header_bits: usize) -> Plan {
        if kind == Kind::Intra {
            self.left_p = self.gop - 1;
        }
        // TM5's remaining GOP budget R: carry-over plus the pictures still due.
        let pictures = self.left_p as f64 + (kind == Kind::Intra) as u32 as f64;
        let remaining = self.bank + pictures * self.frame_bits;
        let target = match kind {
            Kind::Intra => remaining / (1.0 + self.left_p as f64 * self.x_p / (self.x_i * KP)),
            Kind::Predicted => remaining / self.left_p.max(1) as f64,
        };
        let hard_limit = self.fullness;
        let target = target
            .max(self.frame_bits / 8.0)
            .min(TARGET_VBV_SHARE * hard_limit);
        let texture_target = (target - header_bits as f64).max(0.0);
        let activity = activity.max(1.0);
        let alpha = match kind {
            Kind::Intra => self.alpha_i,
            // Before the first P picture, inter residual costs are taken to
            // follow the intra model scaled by K_p.
            Kind::Predicted => self.alpha_p.or(self.alpha_i.map(|a| a * KP)),
        };
        let q_exact = if texture_target == 0.0 {
            31.0
        } else {
            match alpha {
                Some(a) => a * activity / texture_target,
                None => self.last_q as f64,
            }
        }
        .clamp(2.0, 31.0);
        // The first P picture after an I picture predicts from a reference
        // quantised at the intra step; refining it far below that step
        // re-spends bits the next I picture discards anyway.
        let q_exact = if kind == Kind::Predicted && self.after_intra {
            q_exact.max(FIRST_P_RATIO * self.intra_q).min(31.0)
        } else {
            q_exact
        };
        let q = q_exact.round() as u32;
        self.kind = kind;
        self.activity = activity;
        self.plan = Plan {
            q,
            q_exact,
            target,
            texture_target,
            hard_limit,
        };
        self.plan
    }

    /// Quantiser for the next row: the model quantiser scaled by the ratio of
    /// bits spent to bits planned for the activity coded so far, rounded, and
    /// kept within -2/+3 of the picture quantiser. Rows therefore mix the
    /// neighbours of a fractional quantiser instead of swinging widely.
    pub fn row_q(&self, bits_so_far: f64, activity_done: f64) -> u32 {
        let base = self.plan.q_exact;
        if self.plan.texture_target == 0.0 {
            return 31;
        }
        if activity_done <= 0.0 {
            return self.plan.q;
        }
        let expected = (self.plan.texture_target * activity_done / self.activity).max(1.0);
        let ratio = (bits_so_far / expected).clamp(0.5, 2.0);
        (base * ratio)
            .clamp(base - 2.0, base + 3.0)
            .clamp(2.0, 31.0)
            .round() as u32
    }

    /// Record a coded picture: `texture_bits` excludes picture/sequence headers.
    pub fn end(&mut self, bits: f64, texture_bits: f64, mean_q: f64) {
        self.record_end(bits, texture_bits, mean_q, true);
    }

    /// Emergency DC-only/empty-residual coding did not measure the ordinary
    /// quantizer's texture cost. Keep that model unknown or at its last real
    /// observation, while debiting all emitted bytes and physical underflow.
    pub fn end_censored(&mut self, bits: f64, mean_q: f64) {
        self.record_end(bits, 0.0, mean_q, false);
    }

    fn record_end(&mut self, bits: f64, texture_bits: f64, mean_q: f64, learn_model: bool) {
        if learn_model {
            let complexity = bits * mean_q;
            let measured = texture_bits.max(1.0) * mean_q / self.activity;
            let smooth =
                |old: Option<f64>| Some(old.map_or(measured, |o| 0.5 * o + 0.5 * measured));
            match self.kind {
                Kind::Intra => {
                    self.x_i = complexity;
                    self.alpha_i = smooth(self.alpha_i);
                }
                Kind::Predicted => {
                    self.x_p = complexity;
                    self.alpha_p = smooth(self.alpha_p);
                }
            }
        }
        match self.kind {
            Kind::Intra => {
                self.intra_q = mean_q;
                self.after_intra = true;
            }
            Kind::Predicted => {
                self.after_intra = false;
                self.left_p = self.left_p.saturating_sub(1);
            }
        }
        self.bank += self.frame_bits - bits;
        self.last_q = mean_q.round() as u32;
        self.fullness -= bits;
        if self.fullness < 0.0 {
            self.underflow_bits += -self.fullness;
            self.fullness = 0.0;
        }
        self.fullness = (self.fullness + self.refill).min(self.vbv_size);
    }
}

#[cfg(test)]
mod header_budget_tests {
    use super::*;

    // Baseline driver: the old encoder has already emitted these headers but
    // its real begin() API accepts no header count. Call that real API once.
    // Candidate driver passes the same actual count to the new private API.
    fn after_headers(rc: &mut RateControl, kind: Kind, activity: f64, header_bits: usize) -> Plan {
        rc.begin(kind, activity, header_bits)
    }

    fn seeded(gop: u32) -> RateControl {
        let mut rc = RateControl::new(30_000, 42_000, 30_000, gop, 30.0);
        rc.seed_intra(800.0, 4, 1.0); // alpha=3200 texture-bit units.
        rc
    }

    #[test]
    fn header_budget_model_uses_remaining_texture_bits() {
        let mut rc = seeded(1);
        let plan = after_headers(&mut rc, Kind::Intra, 1.0, 200);
        assert_eq!(plan.target, 1000.0);
        assert_eq!(plan.q_exact, 4.0); // 3200/(1000-200), not3200/1000.
    }

    #[test]
    fn header_budget_row_pacing_excludes_emitted_headers() {
        let mut rc = seeded(1);
        rc.seed_intra(10_000.0, 4, 1.0);
        let plan = after_headers(&mut rc, Kind::Intra, 1.0, 200);
        assert_eq!(plan.q, 31); // Both models hit this clamp, isolating pacing.
        assert_eq!(rc.row_q(200.0, 0.25), 31);
    }

    #[test]
    fn header_budget_no_texture_room_selects_maximum_quantizer() {
        let mut rc = seeded(1);
        let plan = after_headers(&mut rc, Kind::Intra, 1.0, 1000);
        assert_eq!(plan.target, 1000.0);
        assert_eq!(plan.q, 31);
        assert_eq!(rc.row_q(0.0, 0.5), 31);
    }

    #[test]
    fn header_budget_more_headers_cannot_get_the_same_texture_allowance() {
        let mut no_header = seeded(1);
        let mut with_header = seeded(1);
        let a = after_headers(&mut no_header, Kind::Intra, 1.0, 0);
        let b = after_headers(&mut with_header, Kind::Intra, 1.0, 200);
        assert_eq!(a.target, b.target);
        assert!(b.q_exact > a.q_exact);
    }

    #[test]
    fn header_budget_zero_headers_preserves_the_original_model() {
        let mut rc = seeded(1);
        let plan = after_headers(&mut rc, Kind::Intra, 1.0, 0);
        assert_eq!(plan.q_exact, 3.2);
        assert_eq!(plan.target, 1000.0);
        assert_eq!(plan.hard_limit, 30_000.0);
    }

    #[test]
    fn header_budget_never_creates_reservoir_or_average_rate_credit() {
        let mut rc = seeded(1);
        let before = after_headers(&mut rc, Kind::Intra, 1.0, 200);
        rc.end(1000.0, 800.0, 4.0);
        let after = after_headers(&mut rc, Kind::Intra, 1.0, 200);
        assert_eq!(rc.bank, 0.0);
        assert_eq!(rc.fullness, 30_000.0);
        assert_eq!(before.target, after.target);
        assert_eq!(before.hard_limit, after.hard_limit);
    }

    #[test]
    fn header_budget_predicted_picture_uses_its_own_actual_header() {
        let mut a = seeded(2);
        let mut b = seeded(2);
        after_headers(&mut a, Kind::Intra, 1.0, 100);
        after_headers(&mut b, Kind::Intra, 1.0, 100);
        a.end(1000.0, 900.0, 4.0);
        b.end(1000.0, 900.0, 4.0);
        let without = after_headers(&mut a, Kind::Predicted, 1.0, 0);
        let with = after_headers(&mut b, Kind::Predicted, 1.0, 200);
        assert_eq!(without.target, with.target);
        assert!(with.q_exact > without.q_exact);
    }

    #[test]
    fn header_budget_header_larger_than_target_does_not_enlarge_capacity() {
        let mut rc = RateControl::new(30_000, 42_000, 600, 1, 30.0);
        rc.seed_intra(800.0, 4, 1.0);
        let plan = after_headers(&mut rc, Kind::Intra, 1.0, 700);
        assert_eq!(plan.hard_limit, 600.0);
        assert_eq!(plan.target, 576.0);
        assert_eq!(plan.q, 31);
    }

    #[test]
    fn header_budget_average_bank_counts_real_byte_alignment() {
        use crate::{Encoder, EncoderConfig};
        use triergon_pixel::{Geometry, Picture};

        let geometry = Geometry::new(16, 16).unwrap();
        let mut picture = Picture::new(geometry);
        picture.y_mut().fill(128);
        picture.cb_mut().fill(128);
        picture.cr_mut().fill(128);
        let mut encoder = Encoder::new(EncoderConfig {
            geometry,
            bit_rate: 30_000,
            max_rate: 42_000,
            vbv_bits: 30_000,
            gop: 30,
        })
        .unwrap();
        let mut output = Vec::new();
        let report = encoder.encode(&picture, &mut output).unwrap();
        assert_eq!(report.bits, output.len() * 8);
        assert_eq!(encoder.rc.bank, 1000.0 - report.bits as f64);
    }
}

#[cfg(test)]
mod censored_model_controls;
