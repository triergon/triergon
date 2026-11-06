//! Inter prediction: motion compensation and weighted sample prediction (8.5.3.3).

use super::{Scratch, SliceDecoder};
use crate::error::{DecodeError, Result};
use crate::inter;
use crate::picture::{Frame, Pixel, PuMotion};
use crate::slice::SLICE_P;

impl<'a, P: Pixel> SliceDecoder<'a, P> {
    #[inline(never)]
    pub(super) fn motion_compensate(
        &mut self,
        xp: u32,
        yp: u32,
        w: u32,
        h: u32,
        m: &PuMotion,
    ) -> Result<()> {
        let bd_y = self.bd_y;
        let bd_c = self.bd_c;
        let weighted = match self.sh.slice_type {
            SLICE_P => self.pps.weighted_pred,
            _ => self.pps.weighted_bipred,
        } && self.sh.pwt.is_some();
        let bi = m.ref_idx[0] >= 0 && m.ref_idx[1] >= 0;
        let (w, h) = (w as usize, h as usize);
        // Fetch reference frames first.
        let mut frames: [Option<&Frame<P>>; 2] = [None, None];
        for (l, f) in frames.iter_mut().enumerate() {
            if m.ref_idx[l] >= 0 {
                let e = self.refs.list[l][m.ref_idx[l] as usize];
                *f = Some(
                    e.frame
                        .ok_or(DecodeError::InvalidData("missing reference picture"))?,
                );
            }
        }
        for c in 0..3usize {
            let (bw, bh, x0, y0) = if c == 0 {
                (w, h, xp as usize, yp as usize)
            } else {
                (w / 2, h / 2, xp as usize / 2, yp as usize / 2)
            };
            let bd = if c == 0 { bd_y } else { bd_c };
            let Scratch { pred, mc, .. } = &mut *self.sc;
            let [pred0, pred1] = pred;
            let plane = &mut self.frame.planes[c];
            let dst_stride = plane.stride;
            let dst_off = y0 * dst_stride + x0;
            // Explicit weighting with weight 2^denom and offset 0 on every used list equals
            // the default weighted prediction exactly (8-275 / 8-277 reduce to 8-262 / 8-264).
            let weighted_c = weighted && {
                let t = self.sh.pwt.as_ref().unwrap();
                let denom = if c == 0 {
                    t.luma_log2_denom
                } else {
                    t.chroma_log2_denom
                };
                (0..2).any(|l| {
                    m.ref_idx[l] >= 0 && {
                        let e = &t.l[l][m.ref_idx[l] as usize];
                        let (wt, o) = if c == 0 {
                            (e.luma_weight, e.luma_offset)
                        } else {
                            (e.chroma_weight[c - 1], e.chroma_offset[c - 1])
                        };
                        wt != 1 << denom || o != 0
                    }
                })
            };
            if weighted_c && !bi {
                let l = if m.ref_idx[0] >= 0 { 0 } else { 1 };
                let mv = m.mv[l];
                let int_pos = if c == 0 {
                    mv.x & 3 == 0 && mv.y & 3 == 0
                } else {
                    mv.x & 7 == 0 && mv.y & 7 == 0
                };
                if int_pos {
                    let t = self.sh.pwt.as_ref().unwrap();
                    let e = &t.l[l][m.ref_idx[l] as usize];
                    let (log2d, wt, o) = if c == 0 {
                        (t.luma_log2_denom, e.luma_weight, e.luma_offset)
                    } else {
                        (
                            t.chroma_log2_denom,
                            e.chroma_weight[c - 1],
                            e.chroma_offset[c - 1],
                        )
                    };
                    let (xi, yi) = if c == 0 {
                        (
                            xp as i32 + (mv.x as i32 >> 2),
                            yp as i32 + (mv.y as i32 >> 2),
                        )
                    } else {
                        (
                            xp as i32 / 2 + (mv.x as i32 >> 3),
                            yp as i32 / 2 + (mv.y as i32 >> 3),
                        )
                    };
                    if inter::weighted_copy(
                        plane,
                        x0,
                        y0,
                        &frames[l].unwrap().planes[c],
                        mc,
                        xi,
                        yi,
                        bw,
                        bh,
                        bd,
                        log2d,
                        wt,
                        o,
                    ) {
                        continue;
                    }
                }
            }
            let mut run =
                |l: usize, mode: u8, pred_buf: &mut [i16], other: &[i16], dst: &mut [P]| {
                    let f = frames[l].unwrap();
                    let mv = m.mv[l];
                    let mut o = inter::Out {
                        pred: pred_buf,
                        dst,
                        dst_off,
                        dst_stride,
                        other,
                    };
                    if c == 0 {
                        let xi = xp as i32 + (mv.x as i32 >> 2);
                        let yi = yp as i32 + (mv.y as i32 >> 2);
                        inter::mc_luma(
                            &f.planes[0],
                            mc,
                            xi,
                            yi,
                            (mv.x & 3) as usize,
                            (mv.y & 3) as usize,
                            bw,
                            bh,
                            bd,
                            mode,
                            &mut o,
                        );
                    } else {
                        // mvCLX = mvLX * 2 / SubWidthC = mvLX (eighth chroma samples) for 4:2:0
                        let (mvcx, mvcy) = (mv.x as i32, mv.y as i32);
                        let xi = (xp as i32 / 2) + (mvcx >> 3);
                        let yi = (yp as i32 / 2) + (mvcy >> 3);
                        inter::mc_chroma(
                            &f.planes[c],
                            mc,
                            xi,
                            yi,
                            (mvcx & 7) as usize,
                            (mvcy & 7) as usize,
                            bw,
                            bh,
                            bd,
                            mode,
                            &mut o,
                        );
                    }
                };
            if weighted_c {
                for (l, f) in frames.iter().enumerate() {
                    if f.is_some() {
                        let buf: &mut [i16] = if l == 0 { pred0 } else { pred1 };
                        run(l, 0, buf, &[], &mut []);
                    }
                }
                let t = self.sh.pwt.as_ref().unwrap();
                let log2d = if c == 0 {
                    t.luma_log2_denom
                } else {
                    t.chroma_log2_denom
                };
                let wo = |e: &crate::slice::WeightEntry| {
                    if c == 0 {
                        (e.luma_weight, e.luma_offset)
                    } else {
                        (e.chroma_weight[c - 1], e.chroma_offset[c - 1])
                    }
                };
                if bi {
                    let (w0, o0) = wo(&t.l[0][m.ref_idx[0] as usize]);
                    let (w1, o1) = wo(&t.l[1][m.ref_idx[1] as usize]);
                    inter::put_weighted_bi(
                        plane, x0, y0, bw, bh, pred0, pred1, bd, log2d, w0, w1, o0, o1,
                    );
                } else {
                    let l = if m.ref_idx[0] >= 0 { 0 } else { 1 };
                    let (wt, o) = wo(&t.l[l][m.ref_idx[l] as usize]);
                    let buf: &[i16] = if l == 0 { pred0 } else { pred1 };
                    inter::put_weighted_uni(plane, x0, y0, bw, bh, buf, bd, log2d, wt, o);
                }
            } else if bi {
                run(0, 0, pred0, &[], &mut []);
                run(1, 2, &mut [], pred0, &mut plane.data);
            } else {
                let l = if m.ref_idx[0] >= 0 { 0 } else { 1 };
                let mv = m.mv[l];
                let int_pos = if c == 0 {
                    mv.x & 3 == 0 && mv.y & 3 == 0
                } else {
                    mv.x & 7 == 0 && mv.y & 7 == 0
                };
                if int_pos {
                    // Uni-prediction at an integer position is an exact copy.
                    let f = frames[l].unwrap();
                    let (xi, yi) = if c == 0 {
                        (
                            xp as i32 + (mv.x as i32 >> 2),
                            yp as i32 + (mv.y as i32 >> 2),
                        )
                    } else {
                        (
                            xp as i32 / 2 + (mv.x as i32 >> 3),
                            yp as i32 / 2 + (mv.y as i32 >> 3),
                        )
                    };
                    inter::copy_block(plane, x0, y0, &f.planes[c], mc, xi, yi, bw, bh);
                } else {
                    run(l, 1, &mut [], &[], &mut plane.data);
                }
            }
        }
        Ok(())
    }
}
