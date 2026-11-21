//! The picture layer: choosing I or P coding, writing the headers, coding the one
//! slice under the rate plan, and handing the reconstruction on as the next reference.

use crate::bits::BitWriter;
use crate::macroblock::{Inter, SliceState};
use crate::motion::Mv;
use crate::rate::PictureType;
use crate::{EncodedPicture, Encoder, Result};
use triergon_pixel::Picture;

impl Encoder {
    pub(crate) fn encode_picture(
        &mut self,
        input: &Picture,
        out: &mut Vec<u8>,
    ) -> Result<EncodedPicture> {
        debug_assert_eq!(input.geometry(), self.cfg.geometry);
        // Scheduled I pictures every `gop`; a P picture whose prediction is
        // worse than intra coding across the picture becomes an I picture and
        // restarts the GOP (the reference's default scene-change rule).
        let mut key = self.index == 0 || self.index - self.gop_start >= self.cfg.gop as u64;
        let same_source_luma = !key && input.y() == &self.previous_luma[..];
        let (previous_cb, previous_cr) = self.previous_chroma.split_at(input.cb().len());
        let repeated = same_source_luma && input.cb() == previous_cb && input.cr() == previous_cr;
        let mut f_code = 0;
        if !key {
            let (f, scene_change) = self.analyse(input, same_source_luma)?;
            f_code = f;
            key = scene_change;
        }
        let kind = if key {
            PictureType::Intra
        } else {
            PictureType::Predicted
        };
        if key {
            self.gop_start = self.index;
            self.skipped_at.fill(0);
            self.intra_analysis(input);
            if self.rc.needs_intra_probe()
                && let Some(scratch) = self.probe.take()
            {
                self.probe_intra(input, scratch)?;
            }
            self.write_sequence_header();
            self.write_gop_header();
        }
        self.write_picture_header(kind, f_code);
        let activity: f64 = self.row_activity.iter().sum();
        let header_bits = self.writer.bit_len();
        let plan = self.rc.begin(kind, activity, header_bits);
        let g = self.cfg.geometry;
        let total = (g.macroblock_width() * g.macroblock_height()) as f64;
        let mut q_sum = 0u64;
        let mut slice: Option<SliceState> = None;
        let mut economy = false;
        let mut censored = false;
        let mut activity_done = 0.0;
        for mb_y in 0..g.macroblock_height() {
            let spent = (self.writer.bit_len() - header_bits) as f64;
            let mut q = self.rc.row_q(spent, activity_done);
            let rows_left = (g.macroblock_height() - mb_y) as f64;
            // Conservative floor of what the remaining rows cost at minimum.
            let floor = rows_left
                * if key {
                    48.0 * g.macroblock_width() as f64 + 64.0
                } else {
                    96.0
                };
            let remaining_target = (plan.target - header_bits as f64).max(0.0);
            let planned_rest = remaining_target * (1.0 - activity_done / activity.max(1.0));
            // Escalate only when the remainder, at the planned pace or at its
            // minimum cost (whichever is larger), would no longer fit the VBV.
            if self.writer.bit_len() as f64 + floor.max(planned_rest) > plan.hard_limit * 0.97 {
                q = q.max(plan.q + 4).min(31);
            }
            economy |= self.writer.bit_len() as f64 + floor > plan.hard_limit - 8192.0;
            q_sum += q as u64 * g.macroblock_width() as u64;
            let mut s = slice.take().unwrap_or_else(|| {
                self.writer.start_code(1);
                self.writer.put(q, 5);
                self.writer.put(0, 1); // extra_bit_slice
                SliceState {
                    q,
                    target_q: q,
                    dc_pred: [128; 3],
                    pmv: Mv::ZERO,
                    last_intra: None,
                    skipped: 0,
                }
            });
            s.target_q = q;
            let picture = Inter {
                mv: Mv::ZERO,
                f_code,
                edge: false,
                tight: false,
                repeated,
            };
            censored |=
                self.encode_row(input, mb_y, &mut s, kind, picture, economy, plan.hard_limit);
            slice = Some(s);
            activity_done += self.row_activity[mb_y];
        }
        self.writer.take_into(out)?;
        // The emitted picture includes the writer's existing byte alignment.
        // Charge those real bits to both the reservoir and the average bank.
        let bits = out.len() * 8;
        if !same_source_luma {
            self.previous_luma.copy_from_slice(input.y());
        }
        if !repeated {
            let (cb, cr) = self.previous_chroma.split_at_mut(input.cb().len());
            cb.copy_from_slice(input.cb());
            cr.copy_from_slice(input.cr());
        }
        let mean_q = q_sum as f64 / total;
        if censored {
            self.rc.end_censored(bits as f64, mean_q);
        } else {
            self.rc
                .end(bits as f64, (bits - header_bits) as f64, mean_q);
        }
        std::mem::swap(&mut self.reference, &mut self.recon);
        if !key {
            for (prev, a) in self.previous_mvs.iter_mut().zip(self.analysis.iter()) {
                *prev = if a.intra { Mv::ZERO } else { a.mv };
            }
        } else {
            self.previous_mvs.fill(Mv::ZERO);
        }
        let picture = EncodedPicture {
            key,
            index: self.index,
            bits: out.len() * 8,
            qscale_mean: mean_q.round() as u32,
        };
        self.index += 1;
        Ok(picture)
    }

    /// First intra picture of a session: trial-encode every second row at a
    /// fixed quantiser into scratch bits to seed the intra rate model. The
    /// reconstruction it leaves behind is overwritten by the real pass.
    fn probe_intra(&mut self, input: &Picture, mut scratch: BitWriter) -> Result<()> {
        const PROBE_Q: u32 = 8;
        let g = self.cfg.geometry;
        std::mem::swap(&mut self.writer, &mut scratch);
        let mut activity = 0.0;
        // Only the bit count is used: dropping each row's bytes once counted keeps
        // the scratch to one row.
        let mut discarded = 0;
        for mb_y in (0..g.macroblock_height()).step_by(2) {
            let mut s = SliceState {
                q: PROBE_Q,
                target_q: PROBE_Q,
                dc_pred: [128; 3],
                pmv: Mv::ZERO,
                last_intra: None,
                skipped: 0,
            };
            for mb_x in 0..g.macroblock_width() {
                self.intra_mb(
                    input,
                    mb_x,
                    mb_y,
                    mb_y * g.macroblock_width() + mb_x,
                    &mut s,
                    PictureType::Intra,
                    false,
                );
            }
            activity += self.row_activity[mb_y];
            discarded += self.writer.discard_flushed();
        }
        let bits = (discarded * 8 + self.writer.bit_len()) as f64;
        std::mem::swap(&mut self.writer, &mut scratch);
        scratch.check_bound()?;
        self.rc.seed_intra(bits, PROBE_Q, activity);
        Ok(())
    }
}
