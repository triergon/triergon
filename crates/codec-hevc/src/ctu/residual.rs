//! Residual coding (7.3.8.11): coefficient parsing, scaling and inverse transform (8.6).

use super::{Cu, Scratch, SliceDecoder};
use crate::cabac::{Bins, ctx};
use crate::picture::Pixel;
use crate::tables::{
    CTX_IDX_MAP_SCAN, LEVEL_SCALE, SCAN_INV_4X4, SCAN_POS_4X4, SIG_CTX_SCAN, qpc_from_qpi,
    sub_block_scan, sub_block_scan_inv,
};
use crate::transform;

impl<P: Pixel> SliceDecoder<'_, P> {
    pub(super) fn residual(
        &mut self,
        cu: &Cu,
        x0: u32,
        y0: u32,
        log2: u32,
        c_idx: usize,
        pred_mode_intra: u32,
    ) {
        let n = 1usize << log2;
        let mut cab = self.cabac;
        let c = &mut self.c;
        let mut transform_skip = false;
        if self.pps.transform_skip && !cu.bypass && log2 <= 2 {
            transform_skip = cab.decision(&mut c[ctx::TRANSFORM_SKIP + (c_idx > 0) as usize]) != 0;
        }
        let scan_idx = if cu.intra && (log2 == 2 || (log2 == 3 && c_idx == 0)) {
            if (6..=14).contains(&pred_mode_intra) {
                2
            } else if (22..=30).contains(&pred_mode_intra) {
                1
            } else {
                0
            }
        } else {
            0
        };
        // Quantization parameters (8.6.1, 8.6.2).
        let bd = if c_idx == 0 { self.bd_y } else { self.bd_c };
        let qp = if c_idx == 0 {
            self.qp_y + self.qp_bd_off
        } else {
            let off = if c_idx == 1 {
                self.pps.cb_qp_offset + self.sh.cb_qp_offset
            } else {
                self.pps.cr_qp_offset + self.sh.cr_qp_offset
            };
            let qpi = (self.qp_y + off).clamp(-self.qp_bd_off, 57);
            qpc_from_qpi(qpi) + self.qp_bd_off
        };
        let bd_shift = bd + log2 + 10 - 15; // (8-301) with log2TransformRange = 15
        let scale_list: Option<&[u8]> =
            if self.sps.scaling_list_enabled && !(transform_skip && n > 4) {
                let sl = self
                    .pps
                    .scaling_list
                    .as_ref()
                    .or(self.sps.scaling_list.as_ref())
                    .unwrap();
                let matrix = c_idx + if cu.intra { 0 } else { 3 };
                Some(match log2 {
                    2 => &sl.sl4[matrix][..],
                    3 => &sl.sl8[matrix][..],
                    4 => &sl.sl16[matrix][..],
                    _ => &sl.sl32[if matrix >= 3 { 3 } else { 0 }][..],
                })
            } else {
                None
            };
        let ls = (LEVEL_SCALE[(qp % 6) as usize] as i64) << (qp / 6);
        let flat_scale = 16 * ls;
        let rnd = 1i64 << (bd_shift - 1);
        let sign_hiding_allowed = self.pps.sign_data_hiding && !cu.bypass;

        // last_sig_coeff_x/y_prefix (9.3.4.2.3)
        let (ctx_off, ctx_shift) = if c_idx == 0 {
            (3 * (log2 - 2) + ((log2 - 1) >> 2), (log2 + 1) >> 2)
        } else {
            (15, log2 - 2)
        };
        let max_prefix = (log2 << 1) - 1;
        let mut px = 0u32;
        while px < max_prefix
            && cab.decision(&mut c[ctx::LAST_X_PREFIX + (ctx_off + (px >> ctx_shift)) as usize])
                != 0
        {
            px += 1;
        }
        let mut py = 0u32;
        while py < max_prefix
            && cab.decision(&mut c[ctx::LAST_Y_PREFIX + (ctx_off + (py >> ctx_shift)) as usize])
                != 0
        {
            py += 1;
        }
        let mut last_x = px;
        if px > 3 {
            let nb = (px >> 1) - 1;
            last_x = (1 << nb) * (2 + (px & 1)) + cab.bypass_bits(nb);
        }
        let mut last_y = py;
        if py > 3 {
            let nb = (py >> 1) - 1;
            last_y = (1 << nb) * (2 + (py & 1)) + cab.bypass_bits(nb);
        }
        // The prefix bound keeps both below n: the largest is 3 << nb plus nb bypass bins.
        if scan_idx == 2 {
            core::mem::swap(&mut last_x, &mut last_y);
        }

        let log2_sb = log2 - 2;
        let sb_w = 1usize << log2_sb;
        let sb_scan = sub_block_scan(log2_sb, scan_idx);
        let pos_scan = &SCAN_POS_4X4[scan_idx as usize];
        let last_sb_x = (last_x >> 2) as u8;
        let last_sb_y = (last_y >> 2) as u8;
        let last_sub_block = sub_block_scan_inv(log2_sb, scan_idx)
            [last_sb_y as usize * sb_w + last_sb_x as usize] as usize;
        let in_pos = ((last_y & 3) << 2) | (last_x & 3);
        let last_scan_pos = SCAN_INV_4X4[scan_idx as usize][in_pos as usize] as usize;
        let mut csbf = 0u64; // coded_sub_block_flag, bit yS * sb_w + xS
        let coeffs = &mut self.sc.coeffs;
        let mut max_x = 0usize;
        let mut max_y = 0usize;
        let mut greater1_ctx_carry = 1u32;
        let mut first_sb_processed = true;
        let chroma = c_idx > 0;
        let gt1_base = ctx::GT1 + if chroma { 16 } else { 0 };
        let gt2_base = ctx::GT2 + if chroma { 4 } else { 0 };
        let sig_base = ctx::SIG_COEFF + if chroma { 27 } else { 0 };
        let csbf_base = ctx::CODED_SUB_BLOCK + if chroma { 2 } else { 0 };
        // Size-dependent part of sigCtx (9-49 .. 9-53).
        let size_off = if chroma {
            if log2 == 3 { 9 } else { 12 }
        } else if log2 == 3 {
            if scan_idx == 0 { 9 } else { 15 }
        } else {
            21
        };
        for i in (0..=last_sub_block).rev() {
            let (xs, ys) = (sb_scan[i].0 as usize, sb_scan[i].1 as usize);
            let mut infer_sb_dc = false;
            let bit = ys * sb_w + xs;
            let right = if xs + 1 < sb_w {
                (csbf >> (bit + 1)) as u32 & 1
            } else {
                0
            };
            let below = if ys + 1 < sb_w {
                (csbf >> (bit + sb_w)) as u32 & 1
            } else {
                0
            };
            let coded = if i < last_sub_block && i > 0 {
                let inc = (right | below) as usize;
                infer_sb_dc = true;
                cab.decision(&mut c[csbf_base + inc])
            } else {
                1
            };
            if coded == 0 {
                continue;
            }
            csbf |= 1u64 << bit;
            // Per-position sig_coeff_flag contexts for this sub-block (9.3.4.2.5), in
            // scan order: sigCtx = tab[n] + off, except the block DC which uses ctx0.
            let (tab, off) = if log2 == 2 {
                (&CTX_IDX_MAP_SCAN[scan_idx as usize], 0usize)
            } else {
                let off = size_off + if !chroma && (xs + ys) > 0 { 3 } else { 0 };
                (
                    &SIG_CTX_SCAN[scan_idx as usize][(right | (below << 1)) as usize],
                    off as usize,
                )
            };
            let sig_ctx = &mut c[sig_base + off..];
            let mut sig_pos = [0u8; 16];
            let mut nsig = 0usize;
            let start = if i == last_sub_block {
                sig_pos[0] = last_scan_pos as u8;
                nsig = 1;
                last_scan_pos as isize - 1
            } else {
                15
            };
            let mut np = start;
            // Branch-free bookkeeping: significance bins are poorly predictable.
            let mut any_sig = 0u32;
            while np > 0 {
                let nn = np as usize & 15;
                let b = cab.decision(&mut sig_ctx[tab[nn] as usize]);
                sig_pos[nsig] = nn as u8;
                nsig += b as usize;
                any_sig |= b;
                np -= 1;
            }
            infer_sb_dc &= any_sig == 0;
            if np == 0 {
                // Position 0 of the sub-block.
                if !infer_sb_dc {
                    let s0 = if log2 > 2 && xs == 0 && ys == 0 {
                        &mut c[sig_base]
                    } else {
                        &mut c[sig_base + off + tab[0] as usize]
                    };
                    if cab.decision(s0) != 0 {
                        sig_pos[nsig] = 0;
                        nsig += 1;
                    }
                } else {
                    sig_pos[nsig] = 0;
                    nsig += 1;
                }
            }
            if nsig == 0 {
                continue;
            }
            // greater1 / greater2 flags (9.3.4.2.6, 9.3.4.2.7)
            let mut ctx_set = if i == 0 || chroma { 0 } else { 2 };
            if !first_sb_processed && greater1_ctx_carry == 0 {
                ctx_set += 1;
            }
            first_sb_processed = false;
            let mut greater1_ctx = 1u32;
            let mut abs = [1i32; 16];
            let mut gt1_first: usize = 16;
            let ngt1 = nsig.min(8);
            let gt1_ctx = &mut c[gt1_base + ctx_set * 4..gt1_base + ctx_set * 4 + 4];
            for (k, a) in abs.iter_mut().enumerate().take(ngt1) {
                let b = cab.decision(&mut gt1_ctx[greater1_ctx.min(3) as usize]);
                *a = 1 + b as i32;
                // greater1Ctx: 0 after a 1-bin, else incremented while > 0 (9.3.4.2.6).
                greater1_ctx = if b != 0 {
                    0
                } else {
                    greater1_ctx + (greater1_ctx > 0) as u32
                };
                if b != 0 && gt1_first == 16 {
                    gt1_first = k;
                }
            }
            greater1_ctx_carry = greater1_ctx;
            if gt1_first < 16 && cab.decision(&mut c[gt2_base + ctx_set]) != 0 {
                abs[gt1_first] = 3;
            }
            let first_sig_scan = sig_pos[nsig - 1] as isize;
            let last_sig_scan = sig_pos[0] as isize;
            let sign_hidden = sign_hiding_allowed && last_sig_scan - first_sig_scan > 3;
            let nsigns = if sign_hidden { nsig - 1 } else { nsig };
            let sign_bits = cab.bypass_bits(nsigns as u32) << (32 - nsigns as u32);
            // Remaining levels (9.3.3.11).
            let mut rice = 0u32;
            let mut sum_abs = 0i32;
            for k in 0..nsig {
                let base = abs[k];
                let threshold = if k < 8 {
                    if k == gt1_first { 3 } else { 2 }
                } else {
                    1
                };
                let mut level = base;
                if base == threshold {
                    let rem = cab.level_remaining(rice);
                    level = base.wrapping_add(rem as i32);
                    if level > 3 * (1 << rice) {
                        rice = (rice + 1).min(4);
                    }
                }
                sum_abs = sum_abs.wrapping_add(level);
                let p = pos_scan[sig_pos[k] as usize];
                let xc = (xs << 2) + (p & 3) as usize;
                let yc = (ys << 2) + (p >> 2) as usize;
                let neg = if k < nsigns {
                    (sign_bits << k) & 0x8000_0000 != 0
                } else {
                    sum_abs & 1 == 1
                };
                let v = if neg { -level } else { level };
                let d = if cu.bypass {
                    v
                } else {
                    let scale = match scale_list {
                        Some(sl) => sl[yc * n + xc] as i64 * ls,
                        None => flat_scale,
                    };
                    ((v as i64 * scale + rnd) >> bd_shift).clamp(-32768, 32767) as i32
                };
                coeffs[yc * n + xc] = d;
                max_x = max_x.max(xc);
                max_y = max_y.max(yc);
            }
        }
        self.cabac = cab;
        // Reconstruction.
        let max = (1i32 << bd) - 1;
        let (x0u, y0u) = (x0 as usize, y0 as usize);
        let Scratch {
            coeffs, res, tr, ..
        } = &mut *self.sc;
        let plane = &mut self.frame.planes[c_idx];
        if cu.bypass {
            res[..n * n].copy_from_slice(&coeffs[..n * n]);
            transform::add_residual(plane, x0u, y0u, n, res, max);
        } else if transform_skip {
            transform::transform_skip(coeffs, log2, bd, res);
            transform::add_residual(plane, x0u, y0u, n, res, max);
        } else {
            let dst = cu.intra && log2 == 2 && c_idx == 0;
            if !dst && max_x == 0 && max_y == 0 {
                let v = transform::dc_only_value(coeffs[0], bd);
                transform::add_constant(plane, x0u, y0u, n, v, max);
            } else {
                transform::inverse_transform(coeffs, log2, max_x, max_y, dst, bd, res, tr);
                transform::add_residual(plane, x0u, y0u, n, res, max);
            }
        }
        // Clear the coefficient area for the next block (4-wide chunks: inline stores).
        let cw = (max_x + 4) & !3;
        for row in coeffs[..(max_y + 1) * n].chunks_exact_mut(n) {
            for ch in row[..cw].chunks_exact_mut(4) {
                ch.fill(0);
            }
        }
    }
}
