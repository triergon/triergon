//! Deblocking edge and boundary strength derivation for a coding unit (8.7.2).

use super::{
    Cu, PART_2NXN, PART_2NXND, PART_2NXNU, PART_NLX2N, PART_NRX2N, PART_NX2N, PART_NXN,
    SliceDecoder,
};
use crate::picture::{Mv, Pixel, PuMotion};
use crate::state::*;

impl<P: Pixel> SliceDecoder<'_, P> {
    #[inline(never)]
    pub(super) fn deblock_cu_edges(&mut self, cu: &Cu) {
        let n = 1u32 << cu.log2;
        let (x0, y0) = (cu.x0, cu.y0);
        let pps = self.pps;
        let layout = self.layout;
        let cur_ctb = self.ctb_of(x0, y0);
        let cur_tile = layout.tile_id[layout.ctb_addr_rs_to_ts[cur_ctb] as usize];
        let edge_ok = |s: &Self, xn: u32, yn: u32| -> bool {
            let cn = s.ctb_of(xn, yn);
            if cn == cur_ctb {
                return true;
            }
            if s.st.ctb_slice_addr[cn] != s.sh.slice_addr_rs && !s.sh.loop_filter_across_slices {
                return false;
            }
            if !pps.loop_filter_across_tiles
                && layout.tile_id[layout.ctb_addr_rs_to_ts[cn] as usize] != cur_tile
            {
                return false;
            }
            true
        };
        // CU left/top edges are transform edges with p in a neighbouring CU (possibly in
        // another CTB and slice); interior edges (TU splits, PU boundaries) share the slice.
        let sv = self.st.bs_v_stride;
        let sh = self.st.bs_h_stride;
        let w4 = self.st.w4;
        let intra = cu.intra;
        let sq = self.slice_idx as usize;
        let cmask = self.ctb_size - 1;
        let left = x0 > 0 && x0 & 7 == 0 && edge_ok(self, x0 - 1, y0);
        let top = y0 > 0 && y0 & 7 == 0 && edge_ok(self, x0, y0 - 1);
        let sp_left = if left && x0 & cmask == 0 {
            self.st.ctb_slice_idx[self.ctb_of(x0 - 1, y0)] as usize
        } else {
            sq
        };
        let sp_top = if top && y0 & cmask == 0 {
            self.st.ctb_slice_idx[self.ctb_of(x0, y0 - 1)] as usize
        } else {
            sq
        };
        {
            let PicState {
                blk,
                motion,
                slices,
                bs_v,
                bs_h,
                ..
            } = &mut *self.st;
            let (blk, motion, slices) = (&blk[..], &motion[..], &slices[..]);
            let (y4a, y4b) = ((y0 >> 2) as usize, ((y0 + n) >> 2) as usize);
            let (x4a, x4b) = ((x0 >> 2) as usize, ((x0 + n) >> 2) as usize);
            if left {
                let col = (x0 >> 3) as usize;
                for y4 in y4a..y4b {
                    let iq = y4 * w4 + x4a;
                    bs_v[y4 * sv + col] = if intra {
                        2
                    } else {
                        bs_at(blk, motion, slices, iq - 1, iq, true, sp_left, sq)
                    };
                }
            }
            if top {
                let row = y4a * w4;
                let base = (y0 >> 3) as usize * sh;
                let out = &mut bs_h[base + x4a..base + x4b];
                if intra {
                    out.fill(2);
                } else {
                    for (k, o) in out.iter_mut().enumerate() {
                        let iq = row + x4a + k;
                        *o = bs_at(blk, motion, slices, iq - w4, iq, true, sp_top, sq);
                    }
                }
            }
        }
        self.st.any_deblock = true;
        let pu_v: Option<u32> = match cu.part {
            PART_NX2N | PART_NXN => Some(n / 2),
            PART_NLX2N => Some(n / 4),
            PART_NRX2N => Some(3 * n / 4),
            _ => None,
        };
        let pu_h: Option<u32> = match cu.part {
            PART_2NXN | PART_NXN => Some(n / 2),
            PART_2NXNU => Some(n / 4),
            PART_2NXND => Some(3 * n / 4),
            _ => None,
        };
        if !self.inner_edges && pu_v.is_none() && pu_h.is_none() {
            return;
        }
        if let Some(off) = pu_v {
            let x = x0 + off;
            if x & 7 == 0 {
                for y in (y0 >> 2)..((y0 + n) >> 2) {
                    let e = &mut self.st.bs_v[y as usize * sv + (x >> 3) as usize];
                    if *e == 0 {
                        *e = 1;
                    }
                }
            }
        }
        if let Some(off) = pu_h {
            let y = y0 + off;
            if y & 7 == 0 {
                for x in (x0 >> 2)..((x0 + n) >> 2) {
                    let e = &mut self.st.bs_h[(y >> 3) as usize * sh + x as usize];
                    if *e == 0 {
                        *e = 1;
                    }
                }
            }
        }
        // Interior edges marked by the transform tree (2) or PU partitioning (1).
        for y4 in (y0 >> 2) as usize..((y0 + n) >> 2) as usize {
            let base = y4 * sv;
            for x8 in (x0 >> 3) as usize + 1..((x0 + n) >> 3) as usize {
                let mark = self.st.bs_v[base + x8];
                if mark != 0 {
                    let iq = y4 * w4 + x8 * 2;
                    self.st.bs_v[base + x8] = if intra {
                        2
                    } else {
                        bs_at(
                            &self.st.blk,
                            &self.st.motion,
                            &self.st.slices,
                            iq - 1,
                            iq,
                            mark == 2,
                            sq,
                            sq,
                        )
                    };
                }
            }
        }
        for y8 in (y0 >> 3) as usize + 1..((y0 + n) >> 3) as usize {
            let base = y8 * sh;
            let row = y8 * 2 * w4;
            for x4 in (x0 >> 2) as usize..((x0 + n) >> 2) as usize {
                let mark = self.st.bs_h[base + x4];
                if mark != 0 {
                    let iq = row + x4;
                    self.st.bs_h[base + x4] = if intra {
                        2
                    } else {
                        bs_at(
                            &self.st.blk,
                            &self.st.motion,
                            &self.st.slices,
                            iq - w4,
                            iq,
                            mark == 2,
                            sq,
                            sq,
                        )
                    };
                }
            }
        }
    }
}

/// Boundary filtering strength (8.7.2.4) between the 4x4 blocks with indices ip and iq
/// (into blk / motion); sp / sq index the slices containing p and q.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn bs_at(
    blk: &[BlkInfo],
    motion: &[PuMotion],
    slices: &[SliceInfo],
    ip: usize,
    iq: usize,
    tu_edge: bool,
    sp: usize,
    sq: usize,
) -> u8 {
    let fp = blk[ip].flags;
    let fq = blk[iq].flags;
    if (fp | fq) & F_INTRA != 0 {
        return 2;
    }
    if tu_edge && (fp | fq) & F_NZ != 0 {
        return 1;
    }
    let mp = &motion[ip];
    let mq = &motion[iq];
    if sp == sq && mp == mq {
        return 0;
    }
    bs_motion(mp, mq, &slices[sp], &slices[sq])
}

/// Motion part of 8.7.2.4 for two inter blocks.
#[inline(never)]
fn bs_motion(mp: &PuMotion, mq: &PuMotion, sp: &SliceInfo, sq: &SliceInfo) -> u8 {
    let ref_p = |l: usize| {
        if mp.ref_idx[l] >= 0 {
            Some(sp.ref_poc[l][mp.ref_idx[l] as usize])
        } else {
            None
        }
    };
    let ref_q = |l: usize| {
        if mq.ref_idx[l] >= 0 {
            Some(sq.ref_poc[l][mq.ref_idx[l] as usize])
        } else {
            None
        }
    };
    let np = mp.ref_idx.iter().filter(|&&r| r >= 0).count();
    let nq = mq.ref_idx.iter().filter(|&&r| r >= 0).count();
    if np != nq {
        return 1;
    }
    let diff =
        |a: Mv, b: Mv| (a.x as i32 - b.x as i32).abs() >= 4 || (a.y as i32 - b.y as i32).abs() >= 4;
    if np == 1 {
        let (lp, lq) = (
            if mp.ref_idx[0] >= 0 { 0 } else { 1 },
            if mq.ref_idx[0] >= 0 { 0 } else { 1 },
        );
        if ref_p(lp) != ref_q(lq) {
            return 1;
        }
        return diff(mp.mv[lp], mq.mv[lq]) as u8;
    }
    // Two motion vectors each.
    let (p0, p1) = (ref_p(0).unwrap(), ref_p(1).unwrap());
    let (q0, q1) = (ref_q(0).unwrap(), ref_q(1).unwrap());
    if !((p0 == q0 && p1 == q1) || (p0 == q1 && p1 == q0)) {
        return 1;
    }
    if p0 != p1 {
        if p0 == q0 {
            (diff(mp.mv[0], mq.mv[0]) || diff(mp.mv[1], mq.mv[1])) as u8
        } else {
            (diff(mp.mv[0], mq.mv[1]) || diff(mp.mv[1], mq.mv[0])) as u8
        }
    } else {
        let c1 = diff(mp.mv[0], mq.mv[0]) || diff(mp.mv[1], mq.mv[1]);
        let c2 = diff(mp.mv[0], mq.mv[1]) || diff(mp.mv[1], mq.mv[0]);
        (c1 && c2) as u8
    }
}
