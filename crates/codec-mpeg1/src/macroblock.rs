//! The slice and macroblock layers (ISO/IEC 11172-2 §2.4.2.5–2.4.2.7): coding one
//! macroblock row of a slice, intra and inter macroblocks with their blocks, skipped
//! macroblocks, and the reconstruction the next picture predicts from.

use crate::Encoder;
use crate::motion::{self, Mv};
use crate::rate::VBV_GUARD_BITS;
use crate::syntax::{PictureType, write_coefficients, write_dc, write_mv_component};
use crate::tables::*;
use crate::{block, dct};
use triergon_pixel::{Geometry, Picture};

/// How `inter_mb` codes one macroblock of a P picture.
#[derive(Clone, Copy)]
pub(crate) struct Inter {
    pub(crate) mv: Mv,
    pub(crate) f_code: u32,
    /// First or last macroblock of the slice, which is never skipped.
    pub(crate) edge: bool,
    /// Under the VBV guard: no coefficients.
    pub(crate) tight: bool,
    /// The source picture equals the previous one in every plane.
    pub(crate) repeated: bool,
}

/// Per-slice state reset at every slice start. Each picture is one slice
/// (ISO/IEC 11172-2 lets a slice span macroblock rows), so DC and vector
/// predictors run across row ends; per-row quantiser changes travel in
/// macroblock_quant fields instead of slice headers.
pub(crate) struct SliceState {
    /// quantizer_scale currently in force at the decoder.
    pub(crate) q: u32,
    /// quantizer_scale wanted for the current row; sent with the next
    /// macroblock that carries coefficients.
    pub(crate) target_q: u32,
    pub(crate) dc_pred: [i32; 3],
    pub(crate) pmv: Mv,
    pub(crate) last_intra: Option<usize>,
    pub(crate) skipped: usize,
}

impl SliceState {
    /// The state at a slice start with quantizer_scale `q`.
    pub(crate) fn new(q: u32) -> SliceState {
        SliceState {
            q,
            target_q: q,
            dc_pred: [128; 3],
            pmv: Mv::ZERO,
            last_intra: None,
            skipped: 0,
        }
    }
}

impl Encoder {
    /// Code one macroblock row. `picture` carries the picture's `f_code` and
    /// whether it repeats the previous source; each macroblock sets the rest.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn encode_row(
        &mut self,
        input: &Picture,
        mb_y: usize,
        s: &mut SliceState,
        kind: PictureType,
        picture: Inter,
        economy: bool,
        hard_limit: f64,
    ) -> bool {
        let g = self.cfg.geometry;
        let last = g.macroblock_width() * g.macroblock_height() - 1;
        let mut censored = false;
        for mb_x in 0..g.macroblock_width() {
            let address = mb_y * g.macroblock_width() + mb_x;
            // The first and last macroblock of a slice are never skipped.
            let edge = address == 0 || address == last;
            // Hard VBV guard: past this point only the cheapest legal codes.
            let tight = economy || self.writer.bit_len() as f64 > hard_limit - VBV_GUARD_BITS;
            censored |= tight;
            match kind {
                PictureType::Intra => {
                    self.intra_mb(input, mb_x, mb_y, address, s, PictureType::Intra, tight);
                }
                PictureType::Predicted => {
                    let a = self.analysis[address];
                    if a.intra && !tight {
                        self.skipped_at[address] = 0;
                        self.intra_mb(input, mb_x, mb_y, address, s, PictureType::Predicted, false);
                    } else {
                        let mv = if a.intra { Mv::ZERO } else { a.mv };
                        let inter = Inter {
                            mv,
                            edge,
                            tight,
                            ..picture
                        };
                        self.inter_mb(input, mb_x, mb_y, address, s, inter);
                    }
                }
            }
        }
        censored
    }

    fn write_address(&mut self, s: &mut SliceState) {
        let mut increment = s.skipped + 1;
        while increment > 33 {
            self.writer.put(ADDRESS_ESCAPE.0, ADDRESS_ESCAPE.1);
            increment -= 33;
        }
        let (code, len) = ADDRESS_INCREMENT[increment];
        self.writer.put(code, len);
        s.skipped = 0;
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn intra_mb(
        &mut self,
        input: &Picture,
        mb_x: usize,
        mb_y: usize,
        address: usize,
        s: &mut SliceState,
        kind: PictureType,
        dc_only: bool,
    ) {
        let g = self.cfg.geometry;
        if s.last_intra.is_none_or(|a| address - a > 1) {
            s.dc_pred = [128; 3];
        }
        self.write_address(s);
        let requant = s.target_q != s.q;
        let (code, len) = match (kind, requant) {
            (PictureType::Intra, false) => MB_I_INTRA,
            (PictureType::Intra, true) => MB_I_INTRA_QUANT,
            (PictureType::Predicted, false) => MB_P_INTRA,
            (PictureType::Predicted, true) => MB_P_INTRA_QUANT,
        };
        self.writer.put(code, len);
        if requant {
            self.writer.put(s.target_q, 5);
            s.q = s.target_q;
        }
        let mut coeffs = [0i32; 64];
        let mut levels = [0i32; 64];
        for b in 0..6 {
            let (plane, stride, x, y, comp) = block_origin(g, b, mb_x, mb_y);
            let src = match plane {
                0 => &input.y(),
                1 => &input.cb(),
                _ => &input.cr(),
            };
            for r in 0..8 {
                let row = &src[(y + r) * stride + x..(y + r) * stride + x + 8];
                for c in 0..8 {
                    coeffs[dct::fdct_input_index(r, c)] = row[c] as i32;
                }
            }
            dct::fdct(&mut coeffs);
            let dc = ((coeffs[0] + 4) >> 3).clamp(0, 255);
            write_dc(&mut self.writer, dc - s.dc_pred[comp], comp == 0);
            s.dc_pred[comp] = dc;
            let mask = if dc_only {
                0
            } else {
                self.quant.intra(&coeffs, s.q, &mut levels)
            };
            write_coefficients(&mut self.writer, &levels, mask, true);
            block::dequant_intra(&levels, mask, dc, s.q, &mut coeffs);
            dct::idct(&mut coeffs, mask | 1);
            let dst = match plane {
                0 => self.recon.y_mut(),
                1 => self.recon.cb_mut(),
                _ => self.recon.cr_mut(),
            };
            for r in 0..8 {
                let row = &mut dst[(y + r) * stride + x..(y + r) * stride + x + 8];
                for c in 0..8 {
                    row[c] = coeffs[r * 8 + c].clamp(0, 255) as u8;
                }
            }
        }
        s.last_intra = Some(address);
        s.pmv = Mv::ZERO;
    }

    pub(crate) fn inter_mb(
        &mut self,
        input: &Picture,
        mb_x: usize,
        mb_y: usize,
        address: usize,
        s: &mut SliceState,
        inter: Inter,
    ) {
        let Inter {
            mv,
            f_code,
            edge,
            tight,
            repeated,
        } = inter;
        let zero = mv == Mv::ZERO;
        let skipped_at = u32::from(self.skipped_at[address]);
        if repeated && zero && !edge && skipped_at != 0 && s.target_q >= skipped_at {
            // The previous picture skipped this macroblock and the picture is
            // repeated, so its residual is the one that quantised to zero
            // there, at a quantiser no coarser than this one: skip again.
            #[cfg(test)]
            {
                self.reused_skips += 1;
            }
            s.skipped += 1;
            s.pmv = Mv::ZERO;
            self.copy_reference(mb_x, mb_y);
            return;
        }
        let g = self.cfg.geometry;
        let mut pred = [[0u8; 64]; 6];
        let ls = g.luma_stride();
        motion::predict_luma_blocks(
            self.reference.y(),
            ls,
            mb_x * 16,
            mb_y * 16,
            mv.x,
            mv.y,
            &mut pred,
        );
        let cs = g.chroma_stride();
        let (cx, cy) = (motion::chroma(mv.x), motion::chroma(mv.y));
        motion::predict::<8>(
            self.reference.cb(),
            cs,
            mb_x * 8,
            mb_y * 8,
            cx,
            cy,
            &mut pred[4],
        );
        motion::predict::<8>(
            self.reference.cr(),
            cs,
            mb_x * 8,
            mb_y * 8,
            cx,
            cy,
            &mut pred[5],
        );

        let mut masks = [0u64; 6];
        let mut cbp = 0u32;
        if !tight {
            let mut coeffs = [0i32; 64];
            for b in 0..6 {
                let (plane, stride, x, y, _) = block_origin(g, b, mb_x, mb_y);
                let src = match plane {
                    0 => &input.y(),
                    1 => &input.cb(),
                    _ => &input.cr(),
                };
                // Residuals and their per-column magnitudes in 16-bit lanes
                // (a column sums at most 8 × 255).
                let mut columns = [0u16; 8];
                for r in 0..8 {
                    let row: &[u8; 8] = src[(y + r) * stride + x..(y + r) * stride + x + 8]
                        .try_into()
                        .unwrap();
                    let p: &[u8; 8] = pred[b][r * 8..r * 8 + 8].try_into().unwrap();
                    for c in 0..8 {
                        let d = i16::from(row[c]) - i16::from(p[c]);
                        coeffs[dct::fdct_input_index(r, c)] = i32::from(d);
                        columns[c] += d.unsigned_abs();
                    }
                }
                let magnitude: u32 = columns.iter().map(|&m| u32::from(m)).sum();
                if self.quant.inter_is_zero(magnitude, s.target_q) {
                    continue;
                }
                dct::fdct(&mut coeffs);
                masks[b] = self.quant.inter(&coeffs, s.target_q, &mut self.levels[b]);
                if masks[b] != 0 {
                    cbp |= 32 >> b;
                }
            }
        }

        if cbp == 0 && zero && !edge {
            // Skipped: zero vector, prediction copied, predictors reset.
            s.skipped += 1;
            s.pmv = Mv::ZERO;
            // A skip forced by the VBV guard proves nothing about the residual.
            self.skipped_at[address] = if tight { 0 } else { s.target_q as u8 };
            self.store_prediction(mb_x, mb_y, &pred);
            return;
        }
        self.skipped_at[address] = 0;
        self.write_address(s);
        let requant = cbp != 0 && s.target_q != s.q;
        match (cbp != 0, zero) {
            (true, true) => {
                let (code, len) = if requant {
                    MB_P_NOMC_CODED_QUANT
                } else {
                    MB_P_NOMC_CODED
                };
                self.writer.put(code, len);
                if requant {
                    self.writer.put(s.target_q, 5);
                    s.q = s.target_q;
                }
                s.pmv = Mv::ZERO;
            }
            (true, false) => {
                let (code, len) = if requant {
                    MB_P_MC_CODED_QUANT
                } else {
                    MB_P_MC_CODED
                };
                self.writer.put(code, len);
                if requant {
                    self.writer.put(s.target_q, 5);
                    s.q = s.target_q;
                }
                self.write_mv(s, mv, f_code);
            }
            (false, _) => {
                self.writer.put(MB_P_MC_NOT_CODED.0, MB_P_MC_NOT_CODED.1);
                self.write_mv(s, mv, f_code);
            }
        }
        if cbp != 0 {
            let (code, len) = CODED_BLOCK_PATTERN[cbp as usize];
            self.writer.put(code, len);
        }
        let mut coeffs = [0i32; 64];
        for b in 0..6 {
            if cbp & (32 >> b) == 0 {
                continue;
            }
            write_coefficients(&mut self.writer, &self.levels[b], masks[b], false);
            block::dequant_inter(&self.levels[b], masks[b], s.q, &mut coeffs);
            dct::idct(&mut coeffs, masks[b]);
            for (p, &c) in pred[b].iter_mut().zip(&coeffs) {
                *p = (*p as i32 + c).clamp(0, 255) as u8;
            }
        }
        self.store_prediction(mb_x, mb_y, &pred);
        let _ = address;
    }

    fn write_mv(&mut self, s: &mut SliceState, mv: Mv, f_code: u32) {
        for (v, p) in [(mv.x, s.pmv.x), (mv.y, s.pmv.y)] {
            write_mv_component(&mut self.writer, v - p, f_code);
        }
        s.pmv = mv;
    }

    /// Reconstruct a macroblock as the reference's co-sited samples.
    fn copy_reference(&mut self, mb_x: usize, mb_y: usize) {
        let g = self.cfg.geometry;
        for b in [0, 4, 5] {
            let (plane, stride, x, y, _) = block_origin(g, b, mb_x, mb_y);
            let size = if plane == 0 { 16 } else { 8 };
            let (src, dst) = match plane {
                0 => (self.reference.y(), self.recon.y_mut()),
                1 => (self.reference.cb(), self.recon.cb_mut()),
                _ => (self.reference.cr(), self.recon.cr_mut()),
            };
            for r in 0..size {
                let at = (y + r) * stride + x;
                dst[at..at + size].copy_from_slice(&src[at..at + size]);
            }
        }
    }

    fn store_prediction(&mut self, mb_x: usize, mb_y: usize, blocks: &[[u8; 64]; 6]) {
        let g = self.cfg.geometry;
        for (b, block) in blocks.iter().enumerate() {
            let (plane, stride, x, y, _) = block_origin(g, b, mb_x, mb_y);
            let dst = match plane {
                0 => self.recon.y_mut(),
                1 => self.recon.cb_mut(),
                _ => self.recon.cr_mut(),
            };
            for r in 0..8 {
                dst[(y + r) * stride + x..(y + r) * stride + x + 8]
                    .copy_from_slice(&block[r * 8..r * 8 + 8]);
            }
        }
    }
}

/// Plane (0 Y, 1 Cb, 2 Cr), stride, sample origin and DC-predictor component of block `b`.
#[inline]
fn block_origin(
    g: Geometry,
    b: usize,
    mb_x: usize,
    mb_y: usize,
) -> (usize, usize, usize, usize, usize) {
    match b {
        0..=3 => (
            0,
            g.luma_stride(),
            mb_x * 16 + (b & 1) * 8,
            mb_y * 16 + (b >> 1) * 8,
            0,
        ),
        4 => (1, g.chroma_stride(), mb_x * 8, mb_y * 8, 1),
        _ => (2, g.chroma_stride(), mb_x * 8, mb_y * 8, 2),
    }
}

#[cfg(test)]
mod tests;
