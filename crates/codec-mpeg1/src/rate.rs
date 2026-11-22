//! Picture-level rate control with explicit whole-picture and texture budgets.
//!
//! Bit allocation across a GOP follows the published MPEG-2 Test Model 5
//! complexity rule (I/P only, I quantiser ≈ 0.8× the P quantiser). Each
//! picture then uses one quantiser, chosen from a per-type model
//! `bits ≈ side + α · activity / q` whose α is learned from the previous picture
//! of the same type; a uniform quantiser within a picture is what keeps the
//! distortion low for a given size. For I pictures `side` is what no quantiser
//! changes: every macroblock's header and every block's DC differential and
//! end_of_block, which outweigh the AC coefficients when a large picture has few
//! bits per macroblock. P pictures model all their bits as `α · activity / q`.
//! Rows only deviate from the quantiser by a bounded amount when the running
//! total drifts from the plan (side bits evenly per row, the rest by activity),
//! or when the VBV would otherwise underflow.
//!
//! The VBV model is the variable-rate case of ISO/IEC 11172-2 Annex C /
//! H.262 Annex C: the buffer fills at `max_rate` until full and each picture
//! is removed whole at its decode time; a picture larger than the occupancy
//! is an underflow.

use crate::syntax::PictureType;

/// Bits short of a picture's VBV occupancy from which only the cheapest codes are
/// written: DC-only intra and coefficient-free inter macroblocks.
pub(crate) const VBV_GUARD_BITS: f64 = 8192.0;

/// The plan of one picture.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Plan {
    pub(crate) q: u32,
    /// Unrounded model quantiser; rows mix its two neighbours.
    pub(crate) q_exact: f64,
    /// Complete picture target, including the headers already emitted.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "only the tests read the whole target")
    )]
    pub(crate) target: f64,
    /// Remaining target after those actual headers; never negative.
    pub(crate) texture_target: f64,
    /// Of `texture_target`, the bits planned for what no quantiser changes.
    side_bits: f64,
    /// Physical VBV occupancy available for this picture. Any emitted
    /// overshoot is still reported by `end`, never converted into credit.
    pub(crate) hard_limit: f64,
}

/// The rate controller of one sequence.
#[derive(Debug)]
pub(crate) struct RateControl {
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
    /// Bits of an I picture's macroblocks that no quantiser changes (headers, DC
    /// differentials, end_of_block codes), as the trial pass measured them.
    side_i: f64,
    kind: PictureType,
    plan: Plan,
    activity: f64,
    /// Total VBV underflow in bits (should stay 0).
    pub(crate) underflow_bits: f64,
    pub(crate) last_q: u32,
    /// Mean quantiser of the I picture the next P picture follows directly; `None`
    /// after a P picture.
    intra_q: Option<f64>,
}

/// TM5 K_p, expressed as FFmpeg's default I/P quantiser ratio 1/0.8.
const KP: f64 = 1.25;
/// Largest share of the current VBV occupancy one picture may plan for; the
/// rest absorbs model error before the in-picture guard escalates.
const TARGET_VBV_SHARE: f64 = 0.96;
/// Lower bound of the first P quantiser relative to the preceding I picture.
const FIRST_P_RATIO: f64 = 0.75;
/// Share of the VBV occupancy beyond which a row's quantiser escalates.
const ESCALATION_SHARE: f64 = 0.97;

impl RateControl {
    pub(crate) fn new(
        bit_rate: u32,
        max_rate: u32,
        vbv_bits: u32,
        gop: u32,
        fps: f64,
    ) -> RateControl {
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
            side_i: 0.0,
            kind: PictureType::Intra,
            plan: Plan {
                q: 8,
                q_exact: 8.0,
                target: 0.0,
                texture_target: 0.0,
                side_bits: 0.0,
                hard_limit: 0.0,
            },
            activity: 1.0,
            underflow_bits: 0.0,
            last_q: 8,
            intra_q: None,
        }
    }

    /// Whether the intra model still needs a probe measurement.
    pub(crate) fn needs_intra_probe(&self) -> bool {
        self.alpha_i.is_none()
    }

    /// Seed the intra model from a sampled trial encode: `coefficient_bits` of AC
    /// coefficients at quantiser `q` over `activity`, and `side_bits` estimated for
    /// the whole picture.
    pub(crate) fn seed_intra(
        &mut self,
        side_bits: f64,
        coefficient_bits: f64,
        q: u32,
        activity: f64,
    ) {
        self.alpha_i = Some(coefficient_bits.max(1.0) * q as f64 / activity.max(1.0));
        self.side_i = side_bits;
    }

    /// Plan a complete picture, debiting the actual headers already emitted
    /// before applying the learned texture-bit model to the remaining budget.
    pub(crate) fn begin(&mut self, kind: PictureType, activity: f64, header_bits: usize) -> Plan {
        if kind == PictureType::Intra {
            self.left_p = self.gop - 1;
        }
        // TM5's remaining GOP budget R: carry-over plus the pictures still due.
        let pictures = self.left_p as f64 + (kind == PictureType::Intra) as u32 as f64;
        let remaining = self.bank + pictures * self.frame_bits;
        let target = match kind {
            PictureType::Intra => {
                remaining / (1.0 + self.left_p as f64 * self.x_p / (self.x_i * KP))
            }
            PictureType::Predicted => remaining / self.left_p.max(1) as f64,
        };
        let hard_limit = self.fullness;
        let target = target
            .max(self.frame_bits / 8.0)
            .min(TARGET_VBV_SHARE * hard_limit);
        let texture_target = (target - header_bits as f64).max(0.0);
        let activity = activity.max(1.0);
        let (alpha, side_bits) = match kind {
            PictureType::Intra => (self.alpha_i, self.side_i.min(texture_target)),
            // Before the first P picture, inter residual costs are taken to
            // follow the intra coefficient model scaled by K_p.
            PictureType::Predicted => (self.alpha_p.or(self.alpha_i.map(|a| a * KP)), 0.0),
        };
        let coefficient_target = texture_target - side_bits;
        let q_exact = if coefficient_target == 0.0 {
            31.0
        } else {
            match alpha {
                Some(a) => a * activity / coefficient_target,
                None => self.last_q as f64,
            }
        }
        .clamp(2.0, 31.0);
        // The first P picture after an I picture predicts from a reference
        // quantised at the intra step; refining it far below that step
        // re-spends bits the next I picture discards anyway.
        let q_exact = match (kind, self.intra_q) {
            (PictureType::Predicted, Some(intra_q)) => {
                q_exact.max(FIRST_P_RATIO * intra_q).min(31.0)
            }
            _ => q_exact,
        };
        let q = q_exact.round() as u32;
        self.kind = kind;
        self.activity = activity;
        self.plan = Plan {
            q,
            q_exact,
            target,
            texture_target,
            side_bits,
            hard_limit,
        };
        self.plan
    }

    /// The texture bits planned for the rows coded so far: `rows_done` of the
    /// picture's rows (a share) carrying `activity_done` of its activity.
    fn planned(&self, rows_done: f64, activity_done: f64) -> f64 {
        let side = self.plan.side_bits;
        side * rows_done + (self.plan.texture_target - side) * activity_done / self.activity
    }

    /// Quantiser for the next row: the model quantiser scaled by the ratio of
    /// bits spent to bits planned for the rows coded so far, rounded, and kept
    /// within -2/+3 of the picture quantiser. Rows therefore mix the neighbours
    /// of a fractional quantiser instead of swinging widely.
    pub(crate) fn row_q(&self, bits_so_far: f64, rows_done: f64, activity_done: f64) -> u32 {
        let base = self.plan.q_exact;
        if self.plan.texture_target == 0.0 {
            return 31;
        }
        if activity_done <= 0.0 {
            return self.plan.q;
        }
        let expected = self.planned(rows_done, activity_done).max(1.0);
        let ratio = (bits_so_far / expected).clamp(0.5, 2.0);
        (base * ratio)
            .clamp(base - 2.0, base + 3.0)
            .clamp(2.0, 31.0)
            .round() as u32
    }

    /// The quantiser of the next row, and whether the rest of the picture must take
    /// the cheapest codes. `written` is the picture's bits so far, headers included,
    /// `floor` the least the remaining rows can cost, and `rows_done` and
    /// `activity_done` what [`row_q`](Self::row_q) takes.
    ///
    /// The quantiser escalates by 4 above the picture's once the remainder, at the
    /// planned pace or at its least cost (whichever is larger), would no longer fit
    /// the VBV; the cheapest codes start once even the least cost comes within
    /// [`VBV_GUARD_BITS`] of it.
    pub(crate) fn row(
        &self,
        written: usize,
        header_bits: usize,
        floor: f64,
        rows_done: f64,
        activity_done: f64,
    ) -> (u32, bool) {
        let mut q = self.row_q((written - header_bits) as f64, rows_done, activity_done);
        let side = self.plan.side_bits;
        let planned_rest = side * (1.0 - rows_done)
            + (self.plan.texture_target - side) * (1.0 - activity_done / self.activity);
        let written = written as f64;
        if written + floor.max(planned_rest) > self.plan.hard_limit * ESCALATION_SHARE {
            q = q.max(self.plan.q + 4).min(31);
        }
        (q, written + floor > self.plan.hard_limit - VBV_GUARD_BITS)
    }

    /// Record a coded picture of `bits`, of which `texture_bits` code its
    /// macroblocks. `texture_bits` is `None` when the VBV guard cut the picture's
    /// coefficients: its texture cost was not measured, so the model keeps its last
    /// real observation (or stays unknown). The bits and any underflow are debited
    /// either way.
    pub(crate) fn end(&mut self, bits: f64, texture_bits: Option<f64>, mean_q: f64) {
        if let Some(texture_bits) = texture_bits {
            let complexity = bits * mean_q;
            // An I picture's coefficients cost what its side bits leave.
            let modelled = match self.kind {
                PictureType::Intra => texture_bits - self.side_i,
                PictureType::Predicted => texture_bits,
            };
            let measured = modelled.max(1.0) * mean_q / self.activity;
            let smooth =
                |old: Option<f64>| Some(old.map_or(measured, |o| 0.5 * o + 0.5 * measured));
            match self.kind {
                PictureType::Intra => {
                    self.x_i = complexity;
                    self.alpha_i = smooth(self.alpha_i);
                }
                PictureType::Predicted => {
                    self.x_p = complexity;
                    self.alpha_p = smooth(self.alpha_p);
                }
            }
        }
        match self.kind {
            PictureType::Intra => self.intra_q = Some(mean_q),
            PictureType::Predicted => {
                self.intra_q = None;
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
mod tests;
