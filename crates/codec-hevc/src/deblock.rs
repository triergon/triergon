//! Deblocking filter (clause 8.7.2), applied to the whole picture after all slices are
//! decoded: all vertical edges first, then all horizontal edges. Boundary strengths were
//! derived during CU decoding and stored per 4-sample edge segment on the 8x8 grid.
//!
//! Each 4-line luma segment is loaded once into `s[i][k]` (i = p3..q3 across the edge,
//! k = line) so the decisions (8.7.2.5.3) and filters (8.7.2.5.7) run on fixed arrays.

use crate::picture::{Frame, Pixel};
use crate::ps::{Pps, Sps};
use crate::state::{BlkInfo, F_NOFILTER, PicState};
use crate::tables::{BETA_TABLE, TC_TABLE, qpc_from_qpi};

/// Per-lane parameters for 16 lines = four 4-line luma edge segments, for the SIMD kernels.
/// A segment that is not filtered has tc = 0; mp / mq are -1 in lanes whose p / q samples
/// may change (not excluded by pcm_loop_filter_disabled / cu_transquant_bypass).
pub struct LumaLanes {
    pub beta: [i16; 16],
    pub tc: [i16; 16],
    pub mp: [i16; 16],
    pub mq: [i16; 16],
}

impl LumaLanes {
    fn new() -> Self {
        LumaLanes {
            beta: [0; 16],
            tc: [0; 16],
            mp: [0; 16],
            mq: [0; 16],
        }
    }
    /// Set segment k (lanes 4k..4k+4).
    #[inline(always)]
    fn set(&mut self, k: usize, beta: i32, tc: i32, np: bool, nq: bool) {
        let r = 4 * k..4 * k + 4;
        self.beta[r.clone()].fill(beta as i16);
        self.tc[r.clone()].fill(tc as i16);
        self.mp[r.clone()].fill(-(!np as i16));
        self.mq[r].fill(-(!nq as i16));
    }
}

/// beta and tc of a luma edge (8.7.2.5.3); tc = 0 means the segment is not filtered.
#[inline(always)]
fn luma_params(bs: i32, bp: BlkInfo, bq: BlkInfo, off: (i32, i32), bd: u32) -> (i32, i32) {
    let qpl = (bq.qp as i32 + bp.qp as i32 + 1) >> 1;
    let qb = (qpl + off.0).clamp(0, 51);
    let beta = (BETA_TABLE[qb as usize] as i32) << (bd - 8);
    let qt = (qpl + 2 * (bs - 1) + off.1).clamp(0, 53);
    let tc = (TC_TABLE[qt as usize] as i32) << (bd - 8);
    if beta == 0 { (0, 0) } else { (beta, tc) }
}

/// tc of a chroma edge (8.7.2.5.5); 0 means the segment is not filtered.
#[inline(always)]
fn chroma_tc(bp: BlkInfo, bq: BlkInfo, tc_off: i32, c_qp_off: i32, bd: u32) -> i32 {
    let qpi = ((bq.qp as i32 + bp.qp as i32 + 1) >> 1) + c_qp_off;
    let qt = (qpc_from_qpi(qpi) + 2 + tc_off).clamp(0, 53);
    (TC_TABLE[qt as usize] as i32) << (bd - 8)
}

/// (beta_offset, tc_offset) of the slice containing the CTB of q0 of an edge.
pub struct Offsets<'a> {
    st: &'a PicState,
    log2: u32,
    w: usize,
}

impl<'a> Offsets<'a> {
    pub fn new(st: &'a PicState, sps: &Sps) -> Self {
        Offsets {
            st,
            log2: sps.log2_ctb,
            w: sps.pic_w_ctb as usize,
        }
    }
    #[inline(always)]
    fn at(&self, x: usize, y: usize) -> (i32, i32) {
        let i = self.st.ctb_slice_idx[(y >> self.log2) * self.w + (x >> self.log2)] as usize;
        let s = &self.st.slices[i.min(self.st.slices.len().saturating_sub(1))];
        (s.beta_offset_div2 * 2, s.tc_offset_div2 * 2)
    }
}

/// Load s[i][k] (i = p3 p2 p1 p0 q0 q1 q2 q3, k = line) for a segment whose q0 of line 0 is
/// at `pos`. VERT: lines are rows and samples across the edge are contiguous.
#[inline(always)]
fn load<P: Pixel, const VERT: bool>(d: &[P], pos: usize, stride: usize) -> [[i32; 4]; 8] {
    let mut s = [[0i32; 4]; 8];
    if VERT {
        for k in 0..4 {
            let r = &d[pos + k * stride - 4..pos + k * stride + 4];
            for i in 0..8 {
                s[i][k] = r[i].to_i32();
            }
        }
    } else {
        for (i, si) in s.iter_mut().enumerate() {
            let b = pos + i * stride - 4 * stride;
            let r = &d[b..b + 4];
            for k in 0..4 {
                si[k] = r[k].to_i32();
            }
        }
    }
    s
}

/// Store rows i in [i0, i1) of s back.
#[inline(always)]
fn store<P: Pixel, const VERT: bool>(
    d: &mut [P],
    pos: usize,
    stride: usize,
    s: &[[i32; 4]; 8],
    i0: usize,
    i1: usize,
) {
    if VERT {
        for k in 0..4 {
            let r = &mut d[pos + k * stride - 4..pos + k * stride + 4];
            for i in i0..i1 {
                r[i] = P::from_i32(s[i][k]);
            }
        }
    } else {
        for (i, si) in s.iter().enumerate().take(i1).skip(i0) {
            let b = pos + i * stride - 4 * stride;
            for (o, &v) in d[b..b + 4].iter_mut().zip(si.iter()) {
                *o = P::from_i32(v);
            }
        }
    }
}

/// Vertical-edge luma segment processed row by row on the 8 contiguous samples
/// p3..q3 of each line (same decisions and filters as `luma_segment`).
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn luma_segment_v<P: Pixel>(
    d: &mut [P],
    pos: usize,
    stride: usize,
    beta: i32,
    tc: i32,
    no_p: bool,
    no_q: bool,
    max: i32,
) {
    let ld = |d: &[P], k: usize| -> [i32; 8] {
        let r = &d[pos + k * stride - 4..pos + k * stride + 4];
        [
            r[0].to_i32(),
            r[1].to_i32(),
            r[2].to_i32(),
            r[3].to_i32(),
            r[4].to_i32(),
            r[5].to_i32(),
            r[6].to_i32(),
            r[7].to_i32(),
        ]
    };
    let l0 = ld(d, 0);
    let l3 = ld(d, 3);
    let dp0 = (l0[1] - 2 * l0[2] + l0[3]).abs();
    let dp3 = (l3[1] - 2 * l3[2] + l3[3]).abs();
    let dq0 = (l0[6] - 2 * l0[5] + l0[4]).abs();
    let dq3 = (l3[6] - 2 * l3[5] + l3[4]).abs();
    let dpq0 = dp0 + dq0;
    let dpq3 = dp3 + dq3;
    if dpq0 + dpq3 >= beta {
        return;
    }
    let dsam = |l: &[i32; 8], dpq: i32| -> bool {
        dpq < (beta >> 2)
            && (l[0] - l[3]).abs() + (l[4] - l[7]).abs() < (beta >> 3)
            && (l[3] - l[4]).abs() < ((5 * tc + 1) >> 1)
    };
    let strong = dsam(&l0, 2 * dpq0) && dsam(&l3, 2 * dpq3);
    if strong {
        let tc2 = 2 * tc;
        for k in 0..4 {
            let r = &mut d[pos + k * stride - 4..pos + k * stride + 4];
            let (p3, p2, p1, p0) = (r[0].to_i32(), r[1].to_i32(), r[2].to_i32(), r[3].to_i32());
            let (q0, q1, q2, q3) = (r[4].to_i32(), r[5].to_i32(), r[6].to_i32(), r[7].to_i32());
            if !no_p {
                r[3] = P::from_i32(
                    ((p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3)
                        .max(p0 - tc2)
                        .min(p0 + tc2),
                );
                r[2] = P::from_i32(((p2 + p1 + p0 + q0 + 2) >> 2).max(p1 - tc2).min(p1 + tc2));
                r[1] = P::from_i32(
                    ((2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3)
                        .max(p2 - tc2)
                        .min(p2 + tc2),
                );
            }
            if !no_q {
                r[4] = P::from_i32(
                    ((p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3)
                        .max(q0 - tc2)
                        .min(q0 + tc2),
                );
                r[5] = P::from_i32(((p0 + q0 + q1 + q2 + 2) >> 2).max(q1 - tc2).min(q1 + tc2));
                r[6] = P::from_i32(
                    ((p0 + q0 + q1 + 3 * q2 + 2 * q3 + 4) >> 3)
                        .max(q2 - tc2)
                        .min(q2 + tc2),
                );
            }
        }
    } else {
        let side = (beta + (beta >> 1)) >> 3;
        let dep = dp0 + dp3 < side;
        let deq = dq0 + dq3 < side;
        let tch = tc >> 1;
        for k in 0..4 {
            let r = &mut d[pos + k * stride - 4..pos + k * stride + 4];
            let (p2, p1, p0) = (r[1].to_i32(), r[2].to_i32(), r[3].to_i32());
            let (q0, q1, q2) = (r[4].to_i32(), r[5].to_i32(), r[6].to_i32());
            let delta = (9 * (q0 - p0) - 3 * (q1 - p1) + 8) >> 4;
            if delta.abs() >= tc * 10 {
                continue;
            }
            let delta = delta.max(-tc).min(tc);
            if !no_p {
                r[3] = P::from_i32((p0 + delta).max(0).min(max));
                if dep {
                    let dpv = ((((p2 + p0 + 1) >> 1) - p1 + delta) >> 1)
                        .max(-tch)
                        .min(tch);
                    r[2] = P::from_i32((p1 + dpv).max(0).min(max));
                }
            }
            if !no_q {
                r[4] = P::from_i32((q0 - delta).max(0).min(max));
                if deq {
                    let dqv = ((((q2 + q0 + 1) >> 1) - q1 - delta) >> 1)
                        .max(-tch)
                        .min(tch);
                    r[5] = P::from_i32((q1 + dqv).max(0).min(max));
                }
            }
        }
    }
}

/// Luma segment of 4 lines (8.7.2.5.3 decisions, 8.7.2.5.7 filtering).
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn luma_segment<P: Pixel, const VERT: bool>(
    d: &mut [P],
    pos: usize,
    stride: usize,
    beta: i32,
    tc: i32,
    no_p: bool,
    no_q: bool,
    max: i32,
) {
    let s = load::<P, VERT>(d, pos, stride);
    let (p3, p2, p1, p0, q0, q1, q2, q3) = (s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]);
    let dp0 = (p2[0] - 2 * p1[0] + p0[0]).abs();
    let dp3 = (p2[3] - 2 * p1[3] + p0[3]).abs();
    let dq0 = (q2[0] - 2 * q1[0] + q0[0]).abs();
    let dq3 = (q2[3] - 2 * q1[3] + q0[3]).abs();
    let dpq0 = dp0 + dq0;
    let dpq3 = dp3 + dq3;
    if dpq0 + dpq3 >= beta {
        return;
    }
    let dsam = |k: usize, dpq: i32| -> bool {
        dpq < (beta >> 2)
            && (p3[k] - p0[k]).abs() + (q0[k] - q3[k]).abs() < (beta >> 3)
            && (p0[k] - q0[k]).abs() < ((5 * tc + 1) >> 1)
    };
    let strong = dsam(0, 2 * dpq0) && dsam(3, 2 * dpq3);
    let mut o = s;
    if strong {
        let tc2 = 2 * tc;
        for k in 0..4 {
            o[3][k] = ((p2[k] + 2 * p1[k] + 2 * p0[k] + 2 * q0[k] + q1[k] + 4) >> 3)
                .max(p0[k] - tc2)
                .min(p0[k] + tc2);
            o[2][k] = ((p2[k] + p1[k] + p0[k] + q0[k] + 2) >> 2)
                .max(p1[k] - tc2)
                .min(p1[k] + tc2);
            o[1][k] = ((2 * p3[k] + 3 * p2[k] + p1[k] + p0[k] + q0[k] + 4) >> 3)
                .max(p2[k] - tc2)
                .min(p2[k] + tc2);
            o[4][k] = ((p1[k] + 2 * p0[k] + 2 * q0[k] + 2 * q1[k] + q2[k] + 4) >> 3)
                .max(q0[k] - tc2)
                .min(q0[k] + tc2);
            o[5][k] = ((p0[k] + q0[k] + q1[k] + q2[k] + 2) >> 2)
                .max(q1[k] - tc2)
                .min(q1[k] + tc2);
            o[6][k] = ((p0[k] + q0[k] + q1[k] + 3 * q2[k] + 2 * q3[k] + 4) >> 3)
                .max(q2[k] - tc2)
                .min(q2[k] + tc2);
        }
        if !no_p {
            store::<P, VERT>(d, pos, stride, &o, 1, 4);
        }
        if !no_q {
            store::<P, VERT>(d, pos, stride, &o, 4, 7);
        }
    } else {
        let side = (beta + (beta >> 1)) >> 3;
        let dep = dp0 + dp3 < side;
        let deq = dq0 + dq3 < side;
        let tch = tc >> 1;
        let tc10 = tc * 10;
        for k in 0..4 {
            let delta = (9 * (q0[k] - p0[k]) - 3 * (q1[k] - p1[k]) + 8) >> 4;
            let on = delta.abs() < tc10;
            let delta = delta.max(-tc).min(tc);
            let np0 = (p0[k] + delta).max(0).min(max);
            let nq0 = (q0[k] - delta).max(0).min(max);
            let dpv = ((((p2[k] + p0[k] + 1) >> 1) - p1[k] + delta) >> 1)
                .max(-tch)
                .min(tch);
            let dqv = ((((q2[k] + q0[k] + 1) >> 1) - q1[k] - delta) >> 1)
                .max(-tch)
                .min(tch);
            let np1 = (p1[k] + dpv).max(0).min(max);
            let nq1 = (q1[k] + dqv).max(0).min(max);
            o[3][k] = if on { np0 } else { p0[k] };
            o[4][k] = if on { nq0 } else { q0[k] };
            o[2][k] = if on && dep { np1 } else { p1[k] };
            o[5][k] = if on && deq { nq1 } else { q1[k] };
        }
        if !no_p {
            store::<P, VERT>(d, pos, stride, &o, 2, 4);
        }
        if !no_q {
            store::<P, VERT>(d, pos, stride, &o, 4, 6);
        }
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn chroma_segment<P: Pixel>(
    d: &mut [P],
    pos: usize,
    xs: usize,
    ls: usize,
    tc: i32,
    no_p: bool,
    no_q: bool,
    max: i32,
) {
    for k in 0..4 {
        let b = pos + k * ls;
        let p0 = d[b - xs].to_i32();
        let p1 = d[b - 2 * xs].to_i32();
        let q0 = d[b].to_i32();
        let q1 = d[b + xs].to_i32();
        let delta = ((((q0 - p0) << 2) + p1 - q1 + 4) >> 3).max(-tc).min(tc);
        if !no_p {
            d[b - xs] = P::from_i32((p0 + delta).max(0).min(max));
        }
        if !no_q {
            d[b] = P::from_i32((q0 - delta).max(0).min(max));
        }
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn filter_luma<P: Pixel, const VERT: bool>(
    d: &mut [P],
    pos: usize,
    stride: usize,
    bs: i32,
    bp: BlkInfo,
    bq: BlkInfo,
    off: (i32, i32),
    bd: u32,
    max: i32,
) {
    let qpl = (bq.qp as i32 + bp.qp as i32 + 1) >> 1;
    let qb = (qpl + off.0).clamp(0, 51);
    let beta = (BETA_TABLE[qb as usize] as i32) << (bd - 8);
    let qt = (qpl + 2 * (bs - 1) + off.1).clamp(0, 53);
    let tc = (TC_TABLE[qt as usize] as i32) << (bd - 8);
    if tc == 0 || beta == 0 {
        return;
    }
    let (np, nq) = (bp.flags & F_NOFILTER != 0, bq.flags & F_NOFILTER != 0);
    if VERT {
        luma_segment_v(d, pos, stride, beta, tc, np, nq, max);
    } else {
        luma_segment::<P, false>(d, pos, stride, beta, tc, np, nq, max);
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn filter_chroma<P: Pixel>(
    d: &mut [P],
    pos: usize,
    xs: usize,
    ls: usize,
    bp: BlkInfo,
    bq: BlkInfo,
    tc_off: i32,
    c_qp_off: i32,
    bd: u32,
    max: i32,
) {
    let qpi = ((bq.qp as i32 + bp.qp as i32 + 1) >> 1) + c_qp_off;
    let qpc = qpc_from_qpi(qpi);
    let qt = (qpc + 2 + tc_off).clamp(0, 53);
    let tc = (TC_TABLE[qt as usize] as i32) << (bd - 8);
    if tc == 0 {
        return;
    }
    chroma_segment(
        d,
        pos,
        xs,
        ls,
        tc,
        bp.flags & F_NOFILTER != 0,
        bq.flags & F_NOFILTER != 0,
        max,
    );
}

#[inline(never)]
/// Deblock the luma rows [ys, ye) (multiples of 16): vertical edges in the band, then the
/// horizontal edges with y in [ys, ye) (the edge at ys modifies the 3 rows above).
#[allow(clippy::too_many_arguments)]
pub fn deblock_band<P: Pixel>(
    frame: &mut Frame<P>,
    st: &PicState,
    sps: &Sps,
    pps: &Pps,
    offs: &Offsets<'_>,
    ys: usize,
    ye: usize,
) {
    let w = sps.width as usize;
    let h = sps.height as usize;
    let ye = ye.min(h);
    let bd_y = sps.bit_depth_luma;
    let bd_c = sps.bit_depth_chroma;
    let max_y = (1 << bd_y) - 1;
    let max_c = (1 << bd_c) - 1;
    let w4 = st.w4;
    let blk = |x: usize, y: usize| st.blk[(y >> 2) * w4 + (x >> 2)];
    let nx8 = w.div_ceil(8);
    // ---- vertical edges (EDGE_VER), then horizontal (EDGE_HOR)
    {
        let pl = &mut frame.planes[0];
        let stride = pl.stride;
        let d = pl.data.as_mut_slice();
        // Groups of four segments (16 rows) per edge column with the SIMD kernel.
        let mut y4s = ys / 4;
        if P::SIMD_FILTERS {
            let mut lanes = LumaLanes::new();
            let bsv = |y4: usize, x8: usize| st.bs_v[y4 * st.bs_v_stride + x8];
            while y4s + 4 <= ye / 4 {
                for x8 in 1..nx8 {
                    let g = [
                        bsv(y4s, x8),
                        bsv(y4s + 1, x8),
                        bsv(y4s + 2, x8),
                        bsv(y4s + 3, x8),
                    ];
                    if g == [0; 4] {
                        continue;
                    }
                    let x = x8 * 8;
                    let mut any = false;
                    for (k, &bs) in g.iter().enumerate() {
                        let y = (y4s + k) * 4;
                        let (beta, tc, np, nq) = if bs == 0 {
                            (0, 0, true, true)
                        } else {
                            let (bp, bq) = (blk(x - 1, y), blk(x, y));
                            let (beta, tc) = luma_params(bs as i32, bp, bq, offs.at(x, y), bd_y);
                            (
                                beta,
                                tc,
                                bp.flags & F_NOFILTER != 0,
                                bq.flags & F_NOFILTER != 0,
                            )
                        };
                        any |= tc != 0;
                        lanes.set(k, beta, tc, np, nq);
                    }
                    if any {
                        P::simd_luma_v16(d, y4s * 4 * stride + x, stride, &lanes, max_y);
                    }
                }
                y4s += 4;
            }
        }
        for y4 in y4s..ye / 4 {
            let row = &st.bs_v[y4 * st.bs_v_stride..y4 * st.bs_v_stride + nx8];
            for (x8, &bs) in row.iter().enumerate().skip(1) {
                if bs == 0 {
                    continue;
                }
                let (x, y) = (x8 * 8, y4 * 4);
                filter_luma::<P, true>(
                    d,
                    y * stride + x,
                    stride,
                    bs as i32,
                    blk(x - 1, y),
                    blk(x, y),
                    offs.at(x, y),
                    bd_y,
                    max_y,
                );
            }
        }
    }
    for (c, pl) in frame.planes.iter_mut().enumerate().skip(1) {
        let off = if c == 1 {
            pps.cb_qp_offset
        } else {
            pps.cr_qp_offset
        };
        let stride = pl.stride;
        let d = pl.data.as_mut_slice();
        let cw = w / 2;
        let mut yms = ys / 8;
        if P::SIMD_FILTERS {
            let mut lanes = LumaLanes::new();
            while yms + 4 <= ye / 8 {
                let mut xc = 8;
                while xc < cw {
                    let lx = xc * 2;
                    let bsv =
                        |k: usize| st.bs_v[(((yms + k) * 8) >> 2) * st.bs_v_stride + (lx >> 3)];
                    if (0..4).all(|k| bsv(k) != 2) {
                        xc += 8;
                        continue;
                    }
                    let mut any = false;
                    for k in 0..4 {
                        let ly = (yms + k) * 8;
                        let (tc, np, nq) = if bsv(k) == 2 {
                            let (bp, bq) = (blk(lx - 1, ly), blk(lx, ly));
                            (
                                chroma_tc(bp, bq, offs.at(lx, ly).1, off, bd_c),
                                bp.flags & F_NOFILTER != 0,
                                bq.flags & F_NOFILTER != 0,
                            )
                        } else {
                            (0, true, true)
                        };
                        any |= tc != 0;
                        lanes.set(k, 0, tc, np, nq);
                    }
                    if any {
                        P::simd_chroma16(true, d, yms * 4 * stride + xc, stride, &lanes, max_c);
                    }
                    xc += 8;
                }
                yms += 4;
            }
        }
        for ym in yms..ye / 8 {
            let ly = ym * 8;
            let row = &st.bs_v[(ly >> 2) * st.bs_v_stride..];
            let mut xc = 8;
            while xc < cw {
                let lx = xc * 2;
                if row[lx >> 3] == 2 {
                    let tc_off = offs.at(lx, ly).1;
                    filter_chroma(
                        d,
                        ym * 4 * stride + xc,
                        1,
                        stride,
                        blk(lx - 1, ly),
                        blk(lx, ly),
                        tc_off,
                        off,
                        bd_c,
                        max_c,
                    );
                }
                xc += 8;
            }
        }
    }
    {
        let pl = &mut frame.planes[0];
        let stride = pl.stride;
        let d = pl.data.as_mut_slice();
        let mut lanes = LumaLanes::new();
        for y8 in (ys / 8).max(1)..ye.div_ceil(8) {
            let row = &st.bs_h[y8 * st.bs_h_stride..y8 * st.bs_h_stride + w / 4];
            let y = y8 * 8;
            // Four segments (16 columns) at a time with the SIMD kernel.
            let simd_end = if P::SIMD_FILTERS { row.len() & !3 } else { 0 };
            for g in (0..simd_end).step_by(4) {
                if row[g..g + 4] == [0; 4] {
                    continue;
                }
                let mut any = false;
                for k in 0..4 {
                    let bs = row[g + k];
                    let x = (g + k) * 4;
                    let (beta, tc, np, nq) = if bs == 0 {
                        (0, 0, true, true)
                    } else {
                        let (bp, bq) = (blk(x, y - 1), blk(x, y));
                        let (beta, tc) = luma_params(bs as i32, bp, bq, offs.at(x, y), bd_y);
                        (
                            beta,
                            tc,
                            bp.flags & F_NOFILTER != 0,
                            bq.flags & F_NOFILTER != 0,
                        )
                    };
                    any |= tc != 0;
                    lanes.set(k, beta, tc, np, nq);
                }
                if any {
                    P::simd_luma_h16(d, y * stride + g * 4, stride, &lanes, max_y);
                }
            }
            for (x4, &bs) in row.iter().enumerate().skip(simd_end) {
                if bs == 0 {
                    continue;
                }
                let x = x4 * 4;
                filter_luma::<P, false>(
                    d,
                    y * stride + x,
                    stride,
                    bs as i32,
                    blk(x, y - 1),
                    blk(x, y),
                    offs.at(x, y),
                    bd_y,
                    max_y,
                );
            }
        }
    }
    for (c, pl) in frame.planes.iter_mut().enumerate().skip(1) {
        let off = if c == 1 {
            pps.cb_qp_offset
        } else {
            pps.cr_qp_offset
        };
        let stride = pl.stride;
        let d = pl.data.as_mut_slice();
        let (cw, chh) = (w / 2, h / 2);
        let mut chroma_lanes = LumaLanes::new();
        let mut yc = (ys / 2).max(8);
        while yc < chh.min(ye.div_ceil(2)) {
            let ly = yc * 2;
            let row = &st.bs_h[(ly >> 3) * st.bs_h_stride..];
            let simd_end = if P::SIMD_FILTERS { (cw / 4) & !3 } else { 0 };
            for g in (0..simd_end).step_by(4) {
                if (0..4).all(|k| row[(g + k) * 2] != 2) {
                    continue;
                }
                let mut any = false;
                for k in 0..4 {
                    let lx = (g + k) * 8;
                    let (tc, np, nq) = if row[lx >> 2] == 2 {
                        let (bp, bq) = (blk(lx, ly - 1), blk(lx, ly));
                        (
                            chroma_tc(bp, bq, offs.at(lx, ly).1, off, bd_c),
                            bp.flags & F_NOFILTER != 0,
                            bq.flags & F_NOFILTER != 0,
                        )
                    } else {
                        (0, true, true)
                    };
                    any |= tc != 0;
                    chroma_lanes.set(k, 0, tc, np, nq);
                }
                if any {
                    P::simd_chroma16(false, d, yc * stride + g * 4, stride, &chroma_lanes, max_c);
                }
            }
            for xm in simd_end..cw / 4 {
                let lx = xm * 8;
                if row[lx >> 2] == 2 {
                    let tc_off = offs.at(lx, ly).1;
                    filter_chroma(
                        d,
                        yc * stride + xm * 4,
                        stride,
                        1,
                        blk(lx, ly - 1),
                        blk(lx, ly),
                        tc_off,
                        off,
                        bd_c,
                        max_c,
                    );
                }
            }
            yc += 8;
        }
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use crate::picture::Plane;

    /// Development micro-benchmark (not evidence).
    #[test]
    #[ignore]
    fn bench_deblock() {
        let mut pl = Plane::<u8>::new(256, 256, 0);
        let mut seed = 3u32;
        for (i, v) in pl.data.iter_mut().enumerate() {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            *v = (100 + (i % 7) as u32 * 3 + ((seed >> 28) & 3)) as u8;
        }
        let stride = pl.stride;
        for (name, beta, tc) in [("weak", 20, 4), ("strong", 64, 8)] {
            let iters = 1_000_000;
            let mut best = [f64::MAX; 2];
            for _ in 0..5 {
                let t = std::time::Instant::now();
                for i in 0..iters {
                    let pos = (16 + (i & 63)) * stride + 64;
                    luma_segment_v::<u8>(&mut pl.data, pos, stride, beta, tc, false, false, 255);
                }
                best[0] = best[0].min(t.elapsed().as_secs_f64() / iters as f64);
                let t = std::time::Instant::now();
                for i in 0..iters {
                    let pos = 64 * stride + 16 + (i & 63) * 2;
                    luma_segment::<u8, false>(
                        &mut pl.data,
                        pos,
                        stride,
                        beta,
                        tc,
                        false,
                        false,
                        255,
                    );
                }
                best[1] = best[1].min(t.elapsed().as_secs_f64() / iters as f64);
            }
            eprintln!(
                "luma segment {name}: vert {:.1} ns, hor {:.1} ns",
                best[0] * 1e9,
                best[1] * 1e9
            );
        }
    }
}
