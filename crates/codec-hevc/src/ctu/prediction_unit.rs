//! Prediction unit syntax (7.3.8.6, 7.3.8.9) and motion vector derivation: merge
//! candidates, temporal motion vectors and AMVP (8.5.3.2).

use super::{
    Cu, PART_2NX2N, PART_2NXN, PART_2NXND, PART_2NXNU, PART_NLX2N, PART_NRX2N, PART_NX2N,
    SliceDecoder,
};
use crate::cabac::ctx;
use crate::error::{DecodeError, Result};
use crate::picture::{ColMotion, Mv, Pixel, PuMotion};
use crate::slice::{SLICE_B, SLICE_P};
use crate::state::*;

impl<P: Pixel> SliceDecoder<'_, P> {
    /// Parses a prediction unit, derives its motion, stores it and performs motion
    /// compensation. Returns merge_flag.
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    pub(super) fn prediction_unit(
        &mut self,
        cu: &Cu,
        xp: u32,
        yp: u32,
        w: u32,
        h: u32,
        part_idx: u32,
        skip: bool,
    ) -> Result<bool> {
        let merge = if skip {
            true
        } else {
            self.cabac.decode(&mut self.c[ctx::MERGE_FLAG]) != 0
        };
        let mut m;
        if merge {
            let mut idx = 0;
            let max = self.sh.max_num_merge_cand;
            if max > 1 && self.cabac.decode(&mut self.c[ctx::MERGE_IDX]) != 0 {
                idx = 1;
                while idx < max - 1 && self.cabac.bypass() == 1 {
                    idx += 1;
                }
            }
            m = self.merge_candidate(cu, xp, yp, w, h, part_idx, idx as usize);
        } else {
            let mut pred_idc = 0u32; // 0 = L0, 1 = L1, 2 = BI
            if self.sh.slice_type == SLICE_B {
                if w + h != 12 {
                    let d = self.blk(cu.x0, cu.y0).depth as usize;
                    if self.cabac.decode(&mut self.c[ctx::INTER_PRED_IDC + d]) != 0 {
                        pred_idc = 2;
                    } else {
                        pred_idc = self.cabac.decode(&mut self.c[ctx::INTER_PRED_IDC + 4]);
                    }
                } else {
                    pred_idc = self.cabac.decode(&mut self.c[ctx::INTER_PRED_IDC + 4]);
                }
            }
            let mut mvd = [[0i32; 2]; 2];
            let mut ref_idx = [-1i32; 2];
            let mut mvp_flag = [0u32; 2];
            if pred_idc != 1 {
                ref_idx[0] = self.parse_ref_idx(self.sh.num_ref_idx[0]) as i32;
                mvd[0] = self.mvd_coding();
                mvp_flag[0] = self.cabac.decode(&mut self.c[ctx::MVP_FLAG]);
            }
            if pred_idc != 0 {
                ref_idx[1] = self.parse_ref_idx(self.sh.num_ref_idx[1]) as i32;
                if self.sh.mvd_l1_zero && pred_idc == 2 {
                    mvd[1] = [0, 0];
                } else {
                    mvd[1] = self.mvd_coding();
                }
                mvp_flag[1] = self.cabac.decode(&mut self.c[ctx::MVP_FLAG]);
            }
            m = PuMotion::default();
            for l in 0..2 {
                if ref_idx[l] < 0 {
                    continue;
                }
                if ref_idx[l] as usize >= self.refs.num[l] {
                    return Err(DecodeError::InvalidData("ref_idx out of range"));
                }
                let mvp = self.amvp(
                    cu,
                    xp,
                    yp,
                    w,
                    h,
                    part_idx,
                    ref_idx[l] as usize,
                    l,
                    mvp_flag[l],
                );
                let ux = (mvp.x as i32 + mvd[l][0] + 65536) & 0xffff;
                let uy = (mvp.y as i32 + mvd[l][1] + 65536) & 0xffff;
                let mx = if ux >= 32768 { ux - 65536 } else { ux };
                let my = if uy >= 32768 { uy - 65536 } else { uy };
                m.mv[l] = Mv::new(mx, my);
                m.ref_idx[l] = ref_idx[l] as i8;
            }
        }
        // Store motion.
        self.fill_motion(xp, yp, w, h, m);
        self.motion_compensate(xp, yp, w, h, &m)?;
        Ok(merge)
    }

    fn parse_ref_idx(&mut self, num: u32) -> u32 {
        if num <= 1 {
            return 0;
        }
        let cmax = num - 1;
        let mut v = 0;
        while v < cmax {
            let bin = if v < 2 {
                self.cabac.decode(&mut self.c[ctx::REF_IDX + v as usize])
            } else {
                self.cabac.bypass()
            };
            if bin == 0 {
                break;
            }
            v += 1;
        }
        v
    }

    fn mvd_coding(&mut self) -> [i32; 2] {
        let g0x = self.cabac.decode(&mut self.c[ctx::ABS_MVD_GT0]) != 0;
        let g0y = self.cabac.decode(&mut self.c[ctx::ABS_MVD_GT0]) != 0;
        let g1x = if g0x {
            self.cabac.decode(&mut self.c[ctx::ABS_MVD_GT1]) != 0
        } else {
            false
        };
        let g1y = if g0y {
            self.cabac.decode(&mut self.c[ctx::ABS_MVD_GT1]) != 0
        } else {
            false
        };
        let mut out = [0i32; 2];
        for (i, (g0, g1)) in [(g0x, g1x), (g0y, g1y)].into_iter().enumerate() {
            if g0 {
                let mut a = 1i32;
                if g1 {
                    a = 2 + self.eg_k(1) as i32;
                }
                if self.cabac.bypass() == 1 {
                    a = -a;
                }
                out[i] = a;
            }
        }
        out
    }

    /// k-th order Exp-Golomb bypass decoding (9.3.3.3).
    pub(super) fn eg_k(&mut self, mut k: u32) -> u32 {
        let mut v = 0u32;
        let mut guard = 0;
        while self.cabac.bypass() == 1 {
            v = v.wrapping_add(1 << k);
            k += 1;
            guard += 1;
            if guard > 31 {
                break;
            }
        }
        if k > 0 {
            v = v.wrapping_add(self.cabac.bypass_bits(k.min(31)));
        }
        v
    }

    // ------------------------------------------------------------------ motion derivation

    /// Clause 6.4.2 prediction block availability.
    #[allow(clippy::too_many_arguments)]
    fn avail_pb(
        &self,
        cu: &Cu,
        xp: u32,
        yp: u32,
        w: u32,
        h: u32,
        part_idx: u32,
        xn: i32,
        yn: i32,
    ) -> bool {
        let n = 1i32 << cu.log2;
        let (xc, yc) = (cu.x0 as i32, cu.y0 as i32);
        let same_cb = xc <= xn && yc <= yn && xc + n > xn && yc + n > yn;
        let avail = if !same_cb {
            self.avail(xp, yp, xn, yn)
        } else {
            !((w << 1) as i32 == n
                && (h << 1) as i32 == n
                && part_idx == 1
                && yc + h as i32 <= yn
                && xc + w as i32 > xn)
        };
        avail && self.blk(xn as u32, yn as u32).flags & F_INTRA == 0
    }

    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    fn merge_candidate(
        &self,
        cu: &Cu,
        xp0: u32,
        yp0: u32,
        w0: u32,
        h0: u32,
        part_idx0: u32,
        merge_idx: usize,
    ) -> PuMotion {
        let (mut xp, mut yp, mut w, mut h, mut part_idx) = (xp0, yp0, w0, h0, part_idx0);
        let pml = self.pps.log2_parallel_merge_level;
        if pml > 2 && cu.log2 == 3 {
            xp = cu.x0;
            yp = cu.y0;
            w = 8;
            h = 8;
            part_idx = 0;
        }
        let part = if pml > 2 && cu.log2 == 3 {
            PART_2NX2N
        } else {
            cu.part
        };
        let mut cand = [PuMotion::default(); 5];
        let mut n = 0usize;
        let same_mer = |xn: i32, yn: i32| {
            (xp as i32 >> pml) == (xn >> pml) && (yp as i32 >> pml) == (yn >> pml)
        };
        // Candidates are appended in order and each depends only on earlier ones, so the
        // derivation stops as soon as candidate merge_idx is known.
        let finish = |mut m: PuMotion| {
            if m.ref_idx[0] >= 0 && m.ref_idx[1] >= 0 && w0 + h0 == 12 {
                m.ref_idx[1] = -1;
                m.mv[1] = Mv::ZERO;
            }
            m
        };
        // A1
        let a1 = (xp as i32 - 1, yp as i32 + h as i32 - 1);
        let av_a1 = self.avail_pb(cu, xp, yp, w, h, part_idx, a1.0, a1.1)
            && !same_mer(a1.0, a1.1)
            && !(part_idx == 1 && matches!(part, PART_NX2N | PART_NLX2N | PART_NRX2N));
        let m_a1 = if av_a1 {
            self.motion_at(a1.0 as u32, a1.1 as u32)
        } else {
            PuMotion::default()
        };
        if av_a1 {
            cand[n] = m_a1;
            n += 1;
        }
        if merge_idx < n {
            return finish(cand[merge_idx]);
        }
        // B1
        let b1 = (xp as i32 + w as i32 - 1, yp as i32 - 1);
        let av_b1 = self.avail_pb(cu, xp, yp, w, h, part_idx, b1.0, b1.1)
            && !same_mer(b1.0, b1.1)
            && !(part_idx == 1 && matches!(part, PART_2NXN | PART_2NXNU | PART_2NXND));
        let m_b1 = if av_b1 {
            self.motion_at(b1.0 as u32, b1.1 as u32)
        } else {
            PuMotion::default()
        };
        let f_b1 = av_b1 && !(av_a1 && m_a1 == m_b1);
        if f_b1 {
            cand[n] = m_b1;
            n += 1;
        }
        if merge_idx < n {
            return finish(cand[merge_idx]);
        }
        // B0
        let b0 = (xp as i32 + w as i32, yp as i32 - 1);
        let av_b0 = self.avail_pb(cu, xp, yp, w, h, part_idx, b0.0, b0.1) && !same_mer(b0.0, b0.1);
        let f_b0 = av_b0 && {
            let m = self.motion_at(b0.0 as u32, b0.1 as u32);
            !(av_b1 && m_b1 == m)
        };
        if f_b0 {
            cand[n] = self.motion_at(b0.0 as u32, b0.1 as u32);
            n += 1;
        }
        if merge_idx < n {
            return finish(cand[merge_idx]);
        }
        // A0
        let a0 = (xp as i32 - 1, yp as i32 + h as i32);
        let av_a0 = self.avail_pb(cu, xp, yp, w, h, part_idx, a0.0, a0.1) && !same_mer(a0.0, a0.1);
        let f_a0 = av_a0 && {
            let m = self.motion_at(a0.0 as u32, a0.1 as u32);
            !(av_a1 && m_a1 == m)
        };
        if f_a0 {
            cand[n] = self.motion_at(a0.0 as u32, a0.1 as u32);
            n += 1;
        }
        if merge_idx < n {
            return finish(cand[merge_idx]);
        }
        // B2
        let b2 = (xp as i32 - 1, yp as i32 - 1);
        let av_b2 = self.avail_pb(cu, xp, yp, w, h, part_idx, b2.0, b2.1) && !same_mer(b2.0, b2.1);
        let f_b2 = av_b2 && n != 4 && {
            let m = self.motion_at(b2.0 as u32, b2.1 as u32);
            !(av_a1 && m_a1 == m) && !(av_b1 && m_b1 == m)
        };
        if f_b2 {
            cand[n] = self.motion_at(b2.0 as u32, b2.1 as u32);
            n += 1;
        }
        let max = self.sh.max_num_merge_cand as usize;
        if merge_idx < n {
            return finish(cand[merge_idx]);
        }
        // Temporal candidate.
        if self.sh.temporal_mvp {
            let mut col = PuMotion::default();
            let mut avail = false;
            if let Some(mv) = self.temporal_mv(xp, yp, w, h, 0, 0) {
                col.mv[0] = mv;
                col.ref_idx[0] = 0;
                avail = true;
            }
            if self.sh.slice_type == SLICE_B
                && let Some(mv) = self.temporal_mv(xp, yp, w, h, 0, 1)
            {
                col.mv[1] = mv;
                col.ref_idx[1] = 0;
                avail = true;
            }
            if avail && n < 5 {
                cand[n] = col;
                n += 1;
            }
        }
        if merge_idx < n {
            return finish(cand[merge_idx]);
        }
        let num_orig = n;
        // Combined bi-predictive candidates (8.5.3.2.4).
        if self.sh.slice_type == SLICE_B && num_orig > 1 && num_orig < max {
            const L0: [usize; 12] = [0, 1, 0, 2, 1, 2, 0, 3, 1, 3, 2, 3];
            const L1: [usize; 12] = [1, 0, 2, 0, 2, 1, 3, 0, 3, 1, 3, 2];
            let mut comb_idx = 0;
            loop {
                let l0 = cand[L0[comb_idx]];
                let l1 = cand[L1[comb_idx]];
                if l0.ref_idx[0] >= 0 && l1.ref_idx[1] >= 0 {
                    let p0 = self.refs.list[0][l0.ref_idx[0] as usize].poc;
                    let p1 = self.refs.list[1][l1.ref_idx[1] as usize].poc;
                    if p0 != p1 || l0.mv[0] != l1.mv[1] {
                        cand[n] = PuMotion {
                            mv: [l0.mv[0], l1.mv[1]],
                            ref_idx: [l0.ref_idx[0], l1.ref_idx[1]],
                        };
                        n += 1;
                    }
                }
                comb_idx += 1;
                if comb_idx == num_orig * (num_orig - 1) || n == max {
                    break;
                }
            }
        }
        if merge_idx < n {
            return finish(cand[merge_idx]);
        }
        // Zero candidates (8.5.3.2.5).
        let num_ref = if self.sh.slice_type == SLICE_P {
            self.sh.num_ref_idx[0]
        } else {
            self.sh.num_ref_idx[0].min(self.sh.num_ref_idx[1])
        } as usize;
        let mut zero_idx = 0usize;
        while n < max {
            let r = if zero_idx < num_ref {
                zero_idx as i8
            } else {
                0
            };
            cand[n] = PuMotion {
                mv: [Mv::ZERO; 2],
                ref_idx: [r, if self.sh.slice_type == SLICE_B { r } else { -1 }],
            };
            n += 1;
            zero_idx += 1;
        }
        finish(cand[merge_idx.min(n - 1)])
    }

    #[inline]
    fn scale_mv(mv: Mv, td: i32, tb: i32) -> Mv {
        let td = td.clamp(-128, 127);
        let tb = tb.clamp(-128, 127);
        if td == 0 {
            return mv;
        }
        let tx = (16384 + (td.abs() >> 1)) / td;
        let dsf = ((tb * tx + 32) >> 6).clamp(-4096, 4095);
        let s = |v: i16| {
            let p = dsf * v as i32;
            let r = p.signum() * ((p.abs() + 127) >> 8);
            r.clamp(-32768, 32767)
        };
        Mv::new(s(mv.x), s(mv.y))
    }

    /// Temporal luma motion vector prediction (8.5.3.2.8 / 8.5.3.2.9).
    fn temporal_mv(
        &self,
        xp: u32,
        yp: u32,
        w: u32,
        h: u32,
        ref_idx: usize,
        lx: usize,
    ) -> Option<Mv> {
        if !self.sh.temporal_mvp {
            return None;
        }
        let col = self.refs.col?;
        let xbr = xp + w;
        let ybr = yp + h;
        if (yp >> self.log2_ctb) == (ybr >> self.log2_ctb)
            && ybr < self.height
            && xbr < self.width
            && let Some(mv) = self.col_mv(col, xbr, ybr, ref_idx, lx)
        {
            return Some(mv);
        }
        let xc = xp + (w >> 1);
        let yc = yp + (h >> 1);
        self.col_mv(col, xc, yc, ref_idx, lx)
    }

    fn col_mv(&self, col: &ColMotion, x: u32, y: u32, ref_idx: usize, lx: usize) -> Option<Mv> {
        let cm = col.data[(y >> 4) as usize * col.w16 + (x >> 4) as usize];
        let p0 = cm.flags & 1 != 0;
        let p1 = cm.flags & 2 != 0;
        if !p0 && !p1 {
            return None;
        }
        let l = if !p0 {
            1
        } else if !p1 {
            0
        } else if self.refs.no_backward_pred {
            lx
        } else if self.sh.collocated_from_l0 {
            1
        } else {
            0
        };
        let col_lt = cm.flags & (4 << l) != 0;
        let cur = &self.refs.list[lx][ref_idx];
        if cur.lt != col_lt {
            return None;
        }
        let mv = cm.mv[l];
        let col_diff = self.refs.col_poc - cm.ref_poc[l];
        let cur_diff = self.poc - cur.poc;
        if cur.lt || col_diff == cur_diff {
            Some(mv)
        } else {
            Some(Self::scale_mv(mv, col_diff, cur_diff))
        }
    }

    /// Luma motion vector prediction (8.5.3.2.6 / 8.5.3.2.7).
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    fn amvp(
        &self,
        cu: &Cu,
        xp: u32,
        yp: u32,
        w: u32,
        h: u32,
        part_idx: u32,
        ref_idx: usize,
        lx: usize,
        flag: u32,
    ) -> Mv {
        let ly = 1 - lx;
        let cur = self.refs.list[lx][ref_idx];
        let poc_of = |l: usize, i: i8| self.refs.list[l][i as usize].poc;
        let lt_of = |l: usize, i: i8| self.refs.list[l][i as usize].lt;
        let a_pos = [
            (xp as i32 - 1, yp as i32 + h as i32),
            (xp as i32 - 1, yp as i32 + h as i32 - 1),
        ];
        let av_a = [
            self.avail_pb(cu, xp, yp, w, h, part_idx, a_pos[0].0, a_pos[0].1),
            self.avail_pb(cu, xp, yp, w, h, part_idx, a_pos[1].0, a_pos[1].1),
        ];
        let is_scaled = av_a[0] || av_a[1];
        let mut avail_a = false;
        let mut mv_a = Mv::ZERO;
        for k in 0..2 {
            if av_a[k] && !avail_a {
                let m = self.motion_at(a_pos[k].0 as u32, a_pos[k].1 as u32);
                if m.ref_idx[lx] >= 0 && poc_of(lx, m.ref_idx[lx]) == cur.poc {
                    avail_a = true;
                    mv_a = m.mv[lx];
                } else if m.ref_idx[ly] >= 0 && poc_of(ly, m.ref_idx[ly]) == cur.poc {
                    avail_a = true;
                    mv_a = m.mv[ly];
                }
            }
        }
        if !avail_a {
            for k in 0..2 {
                if av_a[k] && !avail_a {
                    let m = self.motion_at(a_pos[k].0 as u32, a_pos[k].1 as u32);
                    let mut found = None;
                    if m.ref_idx[lx] >= 0 && lt_of(lx, m.ref_idx[lx]) == cur.lt {
                        found = Some((
                            m.mv[lx],
                            poc_of(lx, m.ref_idx[lx]),
                            lt_of(lx, m.ref_idx[lx]),
                        ));
                    } else if m.ref_idx[ly] >= 0 && lt_of(ly, m.ref_idx[ly]) == cur.lt {
                        found = Some((
                            m.mv[ly],
                            poc_of(ly, m.ref_idx[ly]),
                            lt_of(ly, m.ref_idx[ly]),
                        ));
                    }
                    if let Some((mv, rpoc, rlt)) = found {
                        avail_a = true;
                        mv_a = mv;
                        if rpoc != cur.poc && !rlt && !cur.lt {
                            mv_a = Self::scale_mv(mv, self.poc - rpoc, self.poc - cur.poc);
                        }
                    }
                }
            }
        }
        // mvpListLX[0] is mvLXA whenever A is available (B only replaces an unavailable A).
        if flag == 0 && avail_a {
            return mv_a;
        }
        let b_pos = [
            (xp as i32 + w as i32, yp as i32 - 1),
            (xp as i32 + w as i32 - 1, yp as i32 - 1),
            (xp as i32 - 1, yp as i32 - 1),
        ];
        let av_b = [
            self.avail_pb(cu, xp, yp, w, h, part_idx, b_pos[0].0, b_pos[0].1),
            self.avail_pb(cu, xp, yp, w, h, part_idx, b_pos[1].0, b_pos[1].1),
            self.avail_pb(cu, xp, yp, w, h, part_idx, b_pos[2].0, b_pos[2].1),
        ];
        let mut avail_b = false;
        let mut mv_b = Mv::ZERO;
        for k in 0..3 {
            if av_b[k] && !avail_b {
                let m = self.motion_at(b_pos[k].0 as u32, b_pos[k].1 as u32);
                if m.ref_idx[lx] >= 0 && poc_of(lx, m.ref_idx[lx]) == cur.poc {
                    avail_b = true;
                    mv_b = m.mv[lx];
                } else if m.ref_idx[ly] >= 0 && poc_of(ly, m.ref_idx[ly]) == cur.poc {
                    avail_b = true;
                    mv_b = m.mv[ly];
                }
            }
        }
        if !is_scaled && avail_b {
            avail_a = true;
            mv_a = mv_b;
        }
        if !is_scaled {
            avail_b = false;
            for k in 0..3 {
                if av_b[k] && !avail_b {
                    let m = self.motion_at(b_pos[k].0 as u32, b_pos[k].1 as u32);
                    let mut found = None;
                    if m.ref_idx[lx] >= 0 && lt_of(lx, m.ref_idx[lx]) == cur.lt {
                        found = Some((
                            m.mv[lx],
                            poc_of(lx, m.ref_idx[lx]),
                            lt_of(lx, m.ref_idx[lx]),
                        ));
                    } else if m.ref_idx[ly] >= 0 && lt_of(ly, m.ref_idx[ly]) == cur.lt {
                        found = Some((
                            m.mv[ly],
                            poc_of(ly, m.ref_idx[ly]),
                            lt_of(ly, m.ref_idx[ly]),
                        ));
                    }
                    if let Some((mv, rpoc, rlt)) = found {
                        avail_b = true;
                        mv_b = mv;
                        if rpoc != cur.poc && !rlt && !cur.lt {
                            mv_b = Self::scale_mv(mv, self.poc - rpoc, self.poc - cur.poc);
                        }
                    }
                }
            }
        }
        let mut list = [Mv::ZERO; 2];
        let mut i = 0;
        if avail_a {
            list[i] = mv_a;
            i += 1;
            if avail_b && mv_a != mv_b {
                list[i] = mv_b;
                i += 1;
            }
        } else if avail_b {
            list[i] = mv_b;
            i += 1;
        }
        if (flag as usize) < i {
            return list[flag as usize];
        }
        if i < 2
            && !(avail_a && avail_b && mv_a != mv_b)
            && let Some(mv) = self.temporal_mv(xp, yp, w, h, ref_idx, lx)
        {
            list[i] = mv;
            i += 1;
        }
        let _ = i;
        list[flag as usize]
    }
}
