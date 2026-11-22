//! The picture layer: choosing I or P coding, writing the headers, coding the one
//! slice under the rate plan, and handing the reconstruction on as the next reference.

use crate::analysis::Repetition;
use crate::bits::BitWriter;
use crate::macroblock::{Inter, SliceState};
use crate::motion::Mv;
use crate::syntax::{self, PictureType};
use crate::{EncodedPicture, Encoder, Result};
use triergon_pixel::Picture;

impl Encoder {
    /// Code `input`, of the configured geometry, as the next picture into `out`.
    pub(crate) fn encode_picture(
        &mut self,
        input: &Picture,
        out: &mut Vec<u8>,
    ) -> Result<EncodedPicture> {
        debug_assert_eq!(input.geometry(), self.cfg.geometry);
        // Scheduled I pictures every `gop`; a P picture whose prediction is
        // worse than intra coding across the picture becomes an I picture and
        // restarts the GOP (the reference's default scene-change rule).
        let scheduled = self.index == 0 || self.index - self.gop_start >= u64::from(self.cfg.gop);
        let repetition = if scheduled {
            Repetition::None
        } else {
            self.repetition(input)
        };
        let mut f_code = 0;
        let picture_type = if scheduled {
            PictureType::Intra
        } else {
            let (f, scene_change) = self.analyse(input, repetition != Repetition::None)?;
            f_code = f;
            if scene_change {
                PictureType::Intra
            } else {
                PictureType::Predicted
            }
        };
        if picture_type == PictureType::Intra {
            self.gop_start = self.index;
            self.skipped_at.fill(0);
            self.intra_analysis(input);
            if self.rc.needs_intra_probe()
                && let Some(scratch) = self.probe.take()
            {
                self.probe_intra(input, scratch)?;
            }
            syntax::write_sequence_header(&mut self.writer, &self.cfg);
            syntax::write_gop_header(&mut self.writer, self.index);
        }
        let temporal_reference = ((self.index - self.gop_start) % 1024) as u32;
        syntax::write_picture_header(&mut self.writer, temporal_reference, picture_type, f_code);
        let activity: f64 = self.row_activity.iter().sum();
        let header_bits = self.writer.bit_len();
        let hard_limit = self
            .rc
            .begin(picture_type, activity, header_bits)
            .hard_limit;
        let g = self.cfg.geometry;
        // The least one macroblock row can cost: DC-only intra or skipped macroblocks.
        let min_row_bits = match picture_type {
            PictureType::Intra => 48.0 * g.macroblock_width() as f64 + 64.0,
            PictureType::Predicted => 96.0,
        };
        let coding = Inter {
            mv: Mv::ZERO,
            f_code,
            edge: false,
            tight: false,
            repeated: repetition == Repetition::Picture,
        };
        let mut q_sum = 0u64;
        let mut slice = None;
        let mut economy = false;
        let mut censored = false;
        let mut activity_done = 0.0;
        let rows = g.macroblock_height() as f64;
        for mb_y in 0..g.macroblock_height() {
            let floor = (g.macroblock_height() - mb_y) as f64 * min_row_bits;
            let (q, past_guard) = self.rc.row(
                self.writer.bit_len(),
                header_bits,
                floor,
                mb_y as f64 / rows,
                activity_done,
            );
            economy |= past_guard;
            q_sum += u64::from(q) * g.macroblock_width() as u64;
            let s = slice.get_or_insert_with(|| {
                syntax::write_slice_header(&mut self.writer, q);
                SliceState::new(q)
            });
            s.target_q = q;
            censored |= self.encode_row(input, mb_y, s, picture_type, coding, economy, hard_limit);
            activity_done += self.row_activity[mb_y];
        }
        self.writer.take_into(out)?;
        // The emitted picture includes the writer's existing byte alignment.
        // Charge those real bits to both the reservoir and the average bank.
        let bits = out.len() * 8;
        self.remember_source(input, repetition);
        let mean_q = q_sum as f64 / (g.macroblock_width() * g.macroblock_height()) as f64;
        let texture_bits = (!censored).then(|| (bits - header_bits) as f64);
        self.rc.end(bits as f64, texture_bits, mean_q);
        std::mem::swap(&mut self.reference, &mut self.recon);
        self.remember_vectors(picture_type);
        let picture = EncodedPicture {
            key: picture_type == PictureType::Intra,
            index: self.index,
            bits,
            qscale_mean: mean_q.round() as u32,
        };
        self.index += 1;
        Ok(picture)
    }

    /// First intra picture of a session: trial-encode every second row at a
    /// fixed quantiser into scratch bits to seed the intra rate model, its side
    /// bits scaled to every row. The reconstruction it leaves behind is
    /// overwritten by the real pass.
    fn probe_intra(&mut self, input: &Picture, mut scratch: BitWriter) -> Result<()> {
        const PROBE_Q: u32 = 8;
        let g = self.cfg.geometry;
        std::mem::swap(&mut self.writer, &mut scratch);
        let mut activity = 0.0;
        let mut coefficient_bits = 0;
        // Only the bit count is used: dropping each row's bytes once counted keeps
        // the scratch to one row.
        let mut discarded = 0;
        for mb_y in (0..g.macroblock_height()).step_by(2) {
            let mut s = SliceState::new(PROBE_Q);
            for mb_x in 0..g.macroblock_width() {
                coefficient_bits += self.intra_mb::<true>(
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
        let coefficient_bits = coefficient_bits as f64;
        let side_bits = (discarded * 8 + self.writer.bit_len()) as f64 - coefficient_bits;
        std::mem::swap(&mut self.writer, &mut scratch);
        scratch.check_bound()?;
        let rows = g.macroblock_height();
        let side_bits = side_bits * rows as f64 / rows.div_ceil(2) as f64;
        self.rc
            .seed_intra(side_bits, coefficient_bits, PROBE_Q, activity);
        Ok(())
    }
}
