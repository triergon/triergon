//! Transform tree and transform unit parsing (7.3.8.8, 7.3.8.10) with the intra
//! prediction of each transform block (8.4.4.2).

use super::{Cu, PART_2NX2N, SliceDecoder};
use crate::cabac::ctx;
use crate::error::{DecodeError, Result};
use crate::intra::{self, MAX_REF};
use crate::picture::Pixel;
use crate::state::*;

impl<P: Pixel> SliceDecoder<'_, P> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn transform_tree(
        &mut self,
        cu: &Cu,
        x0: u32,
        y0: u32,
        xb: u32,
        yb: u32,
        log2: u32,
        depth: u32,
        blk_idx: u32,
        parent_cb: bool,
        parent_cr: bool,
    ) -> Result<()> {
        let sps = self.sps;
        let inter_split =
            sps.max_th_depth_inter == 0 && !cu.intra && cu.part != PART_2NX2N && depth == 0;
        let split = if log2 <= sps.log2_max_tb
            && log2 > sps.log2_min_tb
            && depth < cu.max_trafo_depth
            && !(cu.intra_split && depth == 0)
        {
            self.cabac
                .decision(&mut self.c[ctx::SPLIT_TRANSFORM + (5 - log2) as usize])
                != 0
        } else {
            log2 > sps.log2_max_tb || (cu.intra_split && depth == 0) || inter_split
        };
        let mut cbf_cb = false;
        let mut cbf_cr = false;
        if log2 > 2 {
            if depth == 0 || parent_cb {
                cbf_cb = self
                    .cabac
                    .decision(&mut self.c[ctx::CBF_CHROMA + depth as usize])
                    != 0;
            }
            if depth == 0 || parent_cr {
                cbf_cr = self
                    .cabac
                    .decision(&mut self.c[ctx::CBF_CHROMA + depth as usize])
                    != 0;
            }
        }
        if split {
            let h = 1 << (log2 - 1);
            self.transform_tree(cu, x0, y0, x0, y0, log2 - 1, depth + 1, 0, cbf_cb, cbf_cr)?;
            self.transform_tree(
                cu,
                x0 + h,
                y0,
                x0,
                y0,
                log2 - 1,
                depth + 1,
                1,
                cbf_cb,
                cbf_cr,
            )?;
            self.transform_tree(
                cu,
                x0,
                y0 + h,
                x0,
                y0,
                log2 - 1,
                depth + 1,
                2,
                cbf_cb,
                cbf_cr,
            )?;
            self.transform_tree(
                cu,
                x0 + h,
                y0 + h,
                x0,
                y0,
                log2 - 1,
                depth + 1,
                3,
                cbf_cb,
                cbf_cr,
            )?;
            Ok(())
        } else {
            let cbf_luma = if cu.intra || depth != 0 || cbf_cb || cbf_cr {
                self.cabac
                    .decision(&mut self.c[ctx::CBF_LUMA + if depth == 0 { 1 } else { 0 }])
                    != 0
            } else {
                true
            };
            self.transform_unit(
                cu, x0, y0, xb, yb, log2, blk_idx, cbf_luma, cbf_cb, cbf_cr, parent_cb, parent_cr,
            )
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    fn transform_unit(
        &mut self,
        cu: &Cu,
        x0: u32,
        y0: u32,
        xb: u32,
        yb: u32,
        log2: u32,
        blk_idx: u32,
        cbf_luma: bool,
        cbf_cb: bool,
        cbf_cr: bool,
        parent_cb: bool,
        parent_cr: bool,
    ) -> Result<()> {
        let cbf_chroma = if log2 == 2 {
            parent_cb || parent_cr
        } else {
            cbf_cb || cbf_cr
        };
        if (cbf_luma || cbf_chroma) && self.pps.cu_qp_delta_enabled && !self.is_cu_qp_delta_coded {
            // cu_qp_delta_abs (9.3.3.10)
            let mut v = 0i32;
            while v < 5 {
                let inc = if v == 0 { 0 } else { 1 };
                if self.cabac.decision(&mut self.c[ctx::CU_QP_DELTA_ABS + inc]) == 0 {
                    break;
                }
                v += 1;
            }
            if v == 5 {
                v += self.eg_k(0) as i32;
            }
            if v != 0 && self.cabac.bypass() == 1 {
                v = -v;
            }
            let off = self.qp_bd_off;
            if v < -(26 + off / 2) || v > 25 + off / 2 {
                return Err(DecodeError::InvalidData("CuQpDeltaVal"));
            }
            self.is_cu_qp_delta_coded = true;
            self.cu_qp_delta = v;
            self.update_qp();
        }
        let n = 1u32 << log2;
        // Deblocking transform edge marks (8.7.2.2), interior TU edges only.
        let dbk = !self.sh.deblocking_disabled;
        if dbk && x0 != cu.x0 && x0 & 7 == 0 {
            self.inner_edges = true;
            let s = self.st.bs_v_stride;
            for y in (y0 >> 2)..((y0 + n) >> 2) {
                self.st.bs_v[y as usize * s + (x0 >> 3) as usize] = 2;
            }
        }
        if dbk && y0 != cu.y0 && y0 & 7 == 0 {
            self.inner_edges = true;
            let s = self.st.bs_h_stride;
            for x in (x0 >> 2)..((x0 + n) >> 2) {
                self.st.bs_h[(y0 >> 3) as usize * s + x as usize] = 2;
            }
        }
        // Luma
        let luma_mode = if cu.intra {
            self.blk(x0, y0).ipm as u32
        } else {
            0
        };
        if cu.intra {
            self.intra_predict(0, x0, y0, log2, luma_mode);
        }
        if cbf_luma {
            self.set_blk_area(x0, y0, n, n, |b| b.flags |= F_NZ);
            self.residual(cu, x0, y0, log2, 0, luma_mode)?;
        }
        // Chroma
        if log2 > 2 {
            let xc = x0 / 2;
            let yc = y0 / 2;
            for (c, cbf) in [(1usize, cbf_cb), (2usize, cbf_cr)] {
                if cu.intra {
                    self.intra_predict(c, xc, yc, log2 - 1, cu.chroma_mode);
                }
                if cbf {
                    self.residual(cu, xc, yc, log2 - 1, c, cu.chroma_mode)?;
                }
            }
        } else if blk_idx == 3 {
            let xc = xb / 2;
            let yc = yb / 2;
            for (c, cbf) in [(1usize, parent_cb), (2usize, parent_cr)] {
                if cu.intra {
                    self.intra_predict(c, xc, yc, 2, cu.chroma_mode);
                }
                if cbf {
                    self.residual(cu, xc, yc, 2, c, cu.chroma_mode)?;
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ intra

    /// Intra prediction of one transform block of component c at component position (x, y).
    #[inline(never)]
    fn intra_predict(&mut self, c: usize, x: u32, y: u32, log2: u32, mode: u32) {
        let n = 1usize << log2;
        let (xl, yl, unit_shift) = if c == 0 { (x, y, 2) } else { (x * 2, y * 2, 1) };
        let constrained = self.pps.constrained_intra_pred;
        let plane = &self.frame.planes[c];
        let (pw, ph) = (plane.width as i32, plane.height as i32);
        let stride = plane.stride;
        let mut r = [0i32; MAX_REF];
        let mut av = [false; MAX_REF];
        let mut any_missing = false;
        let unit = 1usize << unit_shift; // samples per min block in this component
        let check = |s: &Self, cx: i32, cy: i32| -> bool {
            // Component coordinates -> luma.
            let (lx, ly) = if c == 0 { (cx, cy) } else { (cx * 2, cy * 2) };
            if !s.avail(xl, yl, lx, ly) {
                return false;
            }
            !(constrained && s.blk(lx as u32, ly as u32).flags & F_INTRA == 0)
        };
        let (x, y) = (x as i32, y as i32);
        // Left column, bottom to top: r[2n-1-i] = p[-1][i].
        let mut i = 0;
        while i < 2 * n {
            let cy = y + i as i32;
            let a = cy < ph && check(self, x - 1, cy);
            for k in 0..unit.min(2 * n - i) {
                let idx = 2 * n - 1 - (i + k);
                av[idx] = a;
                if a {
                    r[idx] = plane.data[(cy as usize + k) * stride + (x - 1) as usize].to_i32();
                } else {
                    any_missing = true;
                }
            }
            i += unit;
        }
        // Corner
        {
            let a = check(self, x - 1, y - 1);
            av[2 * n] = a;
            if a {
                r[2 * n] = plane.data[(y - 1) as usize * stride + (x - 1) as usize].to_i32();
            } else {
                any_missing = true;
            }
        }
        // Top row
        let mut i = 0;
        while i < 2 * n {
            let cx = x + i as i32;
            let a = cx < pw && check(self, cx, y - 1);
            for k in 0..unit.min(2 * n - i) {
                let idx = 2 * n + 1 + i + k;
                av[idx] = a;
                if a {
                    r[idx] = plane.data[(y - 1) as usize * stride + cx as usize + k].to_i32();
                } else {
                    any_missing = true;
                }
            }
            i += unit;
        }
        let bd = if c == 0 { self.bd_y } else { self.bd_c };
        if any_missing {
            intra::substitute(&mut r, &av, n, bd);
        }
        if c == 0 {
            intra::filter(&mut r, n, mode, self.sps.strong_intra_smoothing, bd);
        }
        intra::predict(
            &r,
            n,
            mode,
            c == 0,
            bd,
            &mut self.frame.planes[c],
            (x as usize, y as usize),
        );
    }
}
