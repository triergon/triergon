//! Inter prediction: motion vector prediction (8.4.1.3), P_Skip (8.4.1.1), direct modes
//! (8.4.1.2), and the motion compensation / weighted prediction dispatch (8.4.2).

use crate::DecodeError;
use crate::mbinfo::Kind;
use crate::mc::*;
use crate::picture::*;
use crate::slicedec::{MbGeom, SliceCtx, SliceDecoder, Wp};
use crate::tables::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum VertScale {
    One,
    FrmToFld,
    FldToFrm,
}

/// Neighbouring partition motion: None = partition not available.
type NbMv = Option<([i32; 2], i32)>;

/// A motion vector as stored in the 16-bit motion arrays.
#[inline(always)]
fn clip_mv(mv: [i32; 2]) -> [i32; 2] {
    [mv[0] as i16 as i32, mv[1] as i16 as i32]
}

#[inline]
fn median(a: i32, b: i32, c: i32) -> i32 {
    a.max(b).min(a.min(b).max(c))
}

#[inline]
fn min_positive(x: i32, y: i32) -> i32 {
    if x >= 0 && y >= 0 { x.min(y) } else { x.max(y) }
}

impl<'a> SliceDecoder<'a> {
    /// Motion data of the neighbouring partition covering luma location (xN, yN) (8.4.1.3.2).
    #[inline]
    fn nb_mv(&self, list: usize, xn: i32, yn: i32) -> NbMv {
        let (n, xw, yw) = if self.pic.mbaff {
            self.loc(xn, yn, 16, 16)?
        } else {
            (
                self.nb_mb_simple(xn, yn, 16, 16)?,
                (xn + 16) & 15,
                (yn + 16) & 15,
            )
        };
        let b4 = ((yw / 4) * 4 + xw / 4) as usize;
        let b8 = ((yw / 8) * 2 + xw / 8) as usize;
        let m = &self.pic.motion;
        let r = m.ref_idx[list][n * 4 + b8] as i32;
        if n != self.cur && self.pic.mbs[n].kind.is_intra() {
            return Some(([0, 0], -1));
        }
        if r < 0 {
            return Some(([0, 0], -1));
        }
        let mv = m.mv[list][n * 16 + b4];
        let mut mv = [mv[0] as i32, mv[1] as i32];
        let mut r = r;
        if self.pic.mbaff {
            let cf = self.pic.mbs[self.cur].field;
            let nf = self.pic.mbs[n].field;
            if cf && !nf {
                mv[1] /= 2;
                r *= 2;
            } else if !cf && nf {
                mv[1] *= 2;
                r /= 2;
            }
        }
        Some((mv, r))
    }

    /// Is the location inside the current MB already decoded relative to the partition at (x, y)?
    fn c_inside_ok(x: i32, y: i32, xn: i32, yn: i32) -> bool {
        if xn >= 16 || yn < 0 || xn < 0 {
            return true; // outside: handled by loc()
        }
        let b8c = (yn / 8) * 2 + xn / 8;
        let b8 = (y / 8) * 2 + x / 8;
        b8c <= b8
    }

    /// Neighbours A, B, C (C replaced by D when unavailable) for a partition.
    fn nb_abc(&self, list: usize, x: i32, y: i32, w: i32) -> (NbMv, NbMv, NbMv) {
        let a = self.nb_mv(list, x - 1, y);
        let b = self.nb_mv(list, x, y - 1);
        let mut c = if Self::c_inside_ok(x, y, x + w, y - 1) {
            self.nb_mv(list, x + w, y - 1)
        } else {
            None
        };
        if c.is_none() {
            c = self.nb_mv(list, x - 1, y - 1);
        }
        (a, b, c)
    }

    /// Luma motion vector prediction (8.4.1.3). `shape`: 0 generic, 1 16x8, 2 8x16 with
    /// partition index `p`.
    #[allow(clippy::too_many_arguments)] // kernel takes explicit sample geometry instead of a wrapper struct
    fn mvp(
        &self,
        list: usize,
        ref_idx: i32,
        x: i32,
        y: i32,
        w: i32,
        shape: u8,
        p: usize,
    ) -> [i32; 2] {
        let abc = self.nb_abc(list, x, y, w);
        Self::mvp_from(abc, ref_idx, shape, p)
    }

    /// 8.4.1.3 from already derived neighbours A, B, C.
    #[inline]
    fn mvp_from(abc: (NbMv, NbMv, NbMv), ref_idx: i32, shape: u8, p: usize) -> [i32; 2] {
        let (a, b, c) = abc;
        let ra = a.map(|v| v.1).unwrap_or(-1);
        let rb = b.map(|v| v.1).unwrap_or(-1);
        let rc = c.map(|v| v.1).unwrap_or(-1);
        let ma = a.map(|v| v.0).unwrap_or([0, 0]);
        let mb = b.map(|v| v.0).unwrap_or([0, 0]);
        let mc = c.map(|v| v.0).unwrap_or([0, 0]);
        if shape == 1 {
            if p == 0 && rb == ref_idx {
                return mb;
            }
            if p == 1 && ra == ref_idx {
                return ma;
            }
        } else if shape == 2 {
            if p == 0 && ra == ref_idx {
                return ma;
            }
            if p == 1 && rc == ref_idx {
                return mc;
            }
        }
        // 8.4.1.3.1 median.
        let (mut ra, mut rb, mut rc, mut mb2, mut mc2) = (ra, rb, rc, mb, mc);
        if b.is_none() && c.is_none() && a.is_some() {
            mb2 = ma;
            mc2 = ma;
            rb = ra;
            rc = ra;
        }
        let _ = &mut ra;
        let cnt = (ra == ref_idx) as u32 + (rb == ref_idx) as u32 + (rc == ref_idx) as u32;
        if cnt == 1 {
            if ra == ref_idx {
                return ma;
            }
            if rb == ref_idx {
                return mb2;
            }
            return mc2;
        }
        [median(ma[0], mb2[0], mc2[0]), median(ma[1], mb2[1], mc2[1])]
    }

    fn store_mv(&mut self, list: usize, x: i32, y: i32, w: i32, h: i32, mv: [i32; 2]) {
        let cur = self.cur;
        let m = &mut self.pic.motion;
        let v = [mv[0] as i16, mv[1] as i16];
        for by in y / 4..(y + h) / 4 {
            for bx in x / 4..(x + w) / 4 {
                m.mv[list][cur * 16 + (by * 4 + bx) as usize] = v;
            }
        }
    }

    fn store_ref(&mut self, list: usize, b8: usize, r: i32) {
        let cur = self.cur;
        let id = if r >= 0 {
            self.ref_pic(list, r).map(|p| p.id).unwrap_or(-1)
        } else {
            -1
        };
        let m = &mut self.pic.motion;
        m.ref_idx[list][cur * 4 + b8] = r as i8;
        m.ref_id[list][cur * 4 + b8] = id;
    }

    /// Reference picture for refIdx (field MBs in MBAFF index the derived field list).
    pub(crate) fn ref_pic(&self, list: usize, r: i32) -> Option<RefPic> {
        let l = &self.s.ref_list[list];
        if self.pic.mbaff && self.pic.mbs[self.cur].field {
            let f = *l.get((r >> 1) as usize)?;
            let cur_par = 1 + (self.cur & 1) as u8;
            let par = if r & 1 == 0 { cur_par } else { 3 - cur_par };
            let st = &self.refs.dpb[f.idx];
            let poc = st.poc[(par - 1) as usize];
            let long = st.mark[(par - 1) as usize] == RefMark::Long;
            Some(RefPic {
                idx: f.idx,
                parity: par,
                poc,
                long,
                id: ref_id(st.uid, par),
            })
        } else {
            l.get(r as usize).copied()
        }
    }

    /// POC of the current picture or (MBAFF field MB) the current MB's field.
    fn cur_poc(&self) -> i32 {
        if self.pic.mbaff && self.pic.mbs[self.cur].field {
            self.pic.poc[self.cur & 1]
        } else {
            self.pic.pic_poc()
        }
    }

    // ------------------------------------------------------------------ main entry

    pub(crate) fn inter_mb(&mut self) -> Result<(), DecodeError> {
        let cur = self.cur;
        let kind = self.mb.kind;
        let field = self.pic.mbs[cur].field;
        self.pic.motion.mb_flags[cur] = if field { MBF_FIELD } else { 0 };
        // Reset motion of the MB.
        for l in 0..2 {
            for b8 in 0..4 {
                self.pic.motion.ref_idx[l][cur * 4 + b8] = -1;
                self.pic.motion.ref_id[l][cur * 4 + b8] = -1;
            }
            self.pic.motion.mv[l][cur * 16..cur * 16 + 16].fill([0, 0]);
        }
        let mut direct8 = 0u8;
        self.spatial_cache = None;
        self.mb_motion = None;
        match kind {
            Kind::PSkip => {
                let mv = self.pskip_mv();
                self.mb_motion = Some(([0, -1], [clip_mv(mv), [0, 0]]));
                self.store_ref(0, 0, 0);
                self.store_ref(0, 1, 0);
                self.store_ref(0, 2, 0);
                self.store_ref(0, 3, 0);
                self.store_mv(0, 0, 0, 16, 16, mv);
            }
            Kind::P16x16 | Kind::P16x8 | Kind::P8x16 | Kind::B16x16 | Kind::B16x8 | Kind::B8x16 => {
                let nparts = if matches!(kind, Kind::P16x16 | Kind::B16x16) {
                    1
                } else {
                    2
                };
                let shape = match kind {
                    Kind::P16x8 | Kind::B16x8 => 1,
                    Kind::P8x16 | Kind::B8x16 => 2,
                    _ => 0,
                };
                let mut whole = ([-1i32; 2], [[0i32; 2]; 2]);
                for p in 0..nparts {
                    let (x, y, w, h) = self.part_rect(p);
                    for list in 0..2 {
                        if self.mb.part_pred[p] & (1 << list) == 0 {
                            continue;
                        }
                        let b8 = ((y / 8) * 2 + x / 8) as usize;
                        let r = self.mb.ref_idx[list][b8] as i32;
                        for by in 0..h / 8 {
                            for bx in 0..w / 8 {
                                self.store_ref(list, b8 + (by * 2 + bx) as usize, r);
                            }
                        }
                        let mvp = self.mvp(list, r, x, y, w, shape, p);
                        let d = self.mb.mvd[list][((y / 4) * 4 + x / 4) as usize];
                        let mv = [mvp[0] + d[0], mvp[1] + d[1]];
                        self.store_mv(list, x, y, w, h, mv);
                        whole.0[list] = r;
                        whole.1[list] = clip_mv(mv);
                    }
                }
                if nparts == 1 {
                    self.mb_motion = Some(whole);
                }
            }
            Kind::P8x8 | Kind::B8x8 => {
                let is_b = kind == Kind::B8x8;
                for i in 0..4 {
                    let x0 = (i as i32 & 1) * 8;
                    let y0 = (i as i32 >> 1) * 8;
                    if is_b && self.mb.sub[i] == 0 {
                        direct8 |= 1 << i;
                        self.direct_8x8(i)?;
                        continue;
                    }
                    let shape = if is_b {
                        B_SUB_TYPES[self.mb.sub[i] as usize].0
                    } else {
                        self.mb.sub[i]
                    };
                    let (sw, sh_) = match shape {
                        0 => (8, 8),
                        1 => (8, 4),
                        2 => (4, 8),
                        _ => (4, 4),
                    };
                    for list in 0..2 {
                        if self.mb.part_pred[i] & (1 << list) == 0 {
                            continue;
                        }
                        let r = self.mb.ref_idx[list][i] as i32;
                        self.store_ref(list, i, r);
                        for s in 0..SUB_PARTS[shape as usize] {
                            let (sx, sy) = match shape {
                                0 => (0, 0),
                                1 => (0, s as i32 * 4),
                                2 => (s as i32 * 4, 0),
                                _ => ((s as i32 & 1) * 4, (s as i32 >> 1) * 4),
                            };
                            let x = x0 + sx;
                            let y = y0 + sy;
                            let mvp = self.mvp(list, r, x, y, sw, 0, 0);
                            let d = self.mb.mvd[list][((y / 4) * 4 + x / 4) as usize];
                            self.store_mv(list, x, y, sw, sh_, [mvp[0] + d[0], mvp[1] + d[1]]);
                        }
                    }
                }
            }
            Kind::BSkip | Kind::BDirect => {
                direct8 = 15;
                if self.s.sh.direct_spatial_mv_pred {
                    self.direct_spatial_mb()?;
                } else {
                    for i in 0..4 {
                        self.direct_temporal(i)?;
                    }
                }
            }
            _ => {}
        }
        self.pic.mbs[cur].direct8 = direct8;
        self.motion_compensate(direct8)
    }

    /// P_Skip motion vector (8.4.1.1).
    fn pskip_mv(&self) -> [i32; 2] {
        let abc = self.nb_abc(0, 0, 0, 16);
        match (abc.0, abc.1) {
            (None, _) | (_, None) => [0, 0],
            (Some((ma, ra)), Some((mb, rb))) => {
                if (ra == 0 && ma == [0, 0]) || (rb == 0 && mb == [0, 0]) {
                    [0, 0]
                } else {
                    Self::mvp_from(abc, 0, 0, 0)
                }
            }
        }
    }

    // ------------------------------------------------------------------ direct

    fn direct_8x8(&mut self, i: usize) -> Result<(), DecodeError> {
        if self.s.sh.direct_spatial_mv_pred {
            self.direct_spatial(i)
        } else {
            self.direct_temporal(i)
        }
    }

    /// Spatial direct reference indices and predictors, cached per MB.
    fn spatial_refs(&mut self) -> ([i32; 2], [[i32; 2]; 2], bool) {
        if let Some(v) = self.spatial_cache {
            return v;
        }
        let mut refs = [-1i32; 2];
        let mut mvps = [[0i32; 2]; 2];
        let abcs = if self.pic.mbaff {
            [self.nb_abc(0, 0, 0, 16), self.nb_abc(1, 0, 0, 16)]
        } else {
            // 16x16 partition neighbours without MBAFF (6.4.11.7): A = block 3 of mbAddrA, B =
            // block 12 of mbAddrB, C = block 12 of mbAddrC, else D = block 15 of mbAddrD.
            let nb = self.nb;
            let (cn, cb) = if nb.c.is_some() {
                (nb.c, 12)
            } else {
                (nb.d, 15)
            };
            let parts = [(nb.a, 3usize), (nb.b, 12), (cn, cb)];
            let m = &self.pic.motion;
            let mbs = &self.pic.mbs;
            let get = |list: usize, (n, b4): (Option<usize>, usize)| -> NbMv {
                let n = n?;
                let r = m.ref_idx[list][n * 4 + (b4 / 8) * 2 + (b4 % 4) / 2] as i32;
                if r < 0 || mbs[n].kind.is_intra() {
                    return Some(([0, 0], -1));
                }
                let v = m.mv[list][n * 16 + b4];
                Some(([v[0] as i32, v[1] as i32], r))
            };
            [0, 1].map(|l| (get(l, parts[0]), get(l, parts[1]), get(l, parts[2])))
        };
        for list in 0..2 {
            let (a, b, c) = abcs[list];
            let ra = a.map(|v| v.1).unwrap_or(-1);
            let rb = b.map(|v| v.1).unwrap_or(-1);
            let rc = c.map(|v| v.1).unwrap_or(-1);
            refs[list] = min_positive(ra, min_positive(rb, rc));
        }
        let mut zero = false;
        if refs[0] < 0 && refs[1] < 0 {
            refs = [0, 0];
            zero = true;
        }
        for list in 0..2 {
            if !zero && refs[list] >= 0 {
                mvps[list] = Self::mvp_from(abcs[list], refs[list], 0, 0);
            }
        }
        let v = (refs, mvps, zero);
        self.spatial_cache = Some(v);
        v
    }

    /// Spatial direct prediction of a whole B_Skip / B_Direct_16x16 macroblock (8.4.1.2.2): the
    /// same derivation as `direct_spatial` on its four 8x8 blocks, with the shared reference
    /// indices stored once and colZeroFlag gathered as a 4x4-block mask. The MB's motion must
    /// have been reset (refIdx -1, zero vectors).
    fn direct_spatial_mb(&mut self) -> Result<(), DecodeError> {
        let (refs, mvps, zero) = self.spatial_refs();
        let cur = self.cur;
        for list in 0..2 {
            let id = if refs[list] >= 0 {
                self.ref_pic(list, refs[list]).map(|p| p.id).unwrap_or(-1)
            } else {
                -1
            };
            let m = &mut self.pic.motion;
            m.ref_idx[list][cur * 4..cur * 4 + 4].fill(refs[list] as i8);
            m.ref_id[list][cur * 4..cur * 4 + 4].fill(id);
        }
        if zero || (refs[0] < 0 && refs[1] < 0) {
            self.mb_motion = Some((refs, [[0, 0]; 2]));
            return Ok(()); // all motion vectors are zero
        }
        let col = self.s.col.ok_or(DecodeError::InvalidData(
            "direct prediction without RefPicList1",
        ))?;
        // colZeroFlag only matters for a list whose refIdx is 0 (8-190 / 8-191).
        let mut colz: u16 = 0;
        let frm_frm = self.pic.structure == 0
            && !self.pic.mbaff
            && self.refs.dpb[col.idx].coding == Coding::Frame;
        if col.short_term
            && (refs[0] == 0 || refs[1] == 0)
            && frm_frm
            && self.s.direct_8x8_inference
        {
            // FRM / FRM with 8x8 inference (Table 8-6): the co-located MB is CurrMbAddr and each
            // 8x8 uses its corner 4x4 block, as in `colocated`.
            let motion = &self.refs.dpb[col.idx].motion[0];
            if cur >= motion.mbs {
                return Err(DecodeError::InvalidData(
                    "co-located macroblock out of range",
                ));
            }
            if motion.mb_flags[cur] & MBF_INTRA == 0 {
                for (i, b4) in [0usize, 3, 12, 15].into_iter().enumerate() {
                    let b8 = cur * 4 + i;
                    let (l, r) = if motion.ref_idx[0][b8] >= 0 {
                        (0, motion.ref_idx[0][b8])
                    } else {
                        (1, motion.ref_idx[1][b8])
                    };
                    let mv = motion.mv_at(l, cur, b4);
                    if r == 0 && (-1..=1).contains(&mv[0]) && (-1..=1).contains(&mv[1]) {
                        colz |= 0x33 << ((i >> 1) * 8 + (i & 1) * 2);
                    }
                }
            }
        } else if col.short_term && (refs[0] == 0 || refs[1] == 0) {
            let is_zero = |c: ([i32; 2], i32, i32, VertScale)| {
                c.1 == 0 && (-1..=1).contains(&c.0[0]) && (-1..=1).contains(&c.0[1])
            };
            if self.s.direct_8x8_inference {
                // Corner 4x4 block of each 8x8 decides the whole 8x8.
                for i in 0..4i32 {
                    let (x0, y0) = ((i & 1) * 8, (i >> 1) * 8);
                    if is_zero(self.colocated(x0 * 3 / 2, y0 * 3 / 2)?) {
                        colz |= 0x33 << ((y0 / 4) * 4 + x0 / 4);
                    }
                }
            } else {
                for b in 0..16i32 {
                    let (x, y) = ((b % 4) * 4, (b / 4) * 4);
                    if is_zero(self.colocated(x, y)?) {
                        colz |= 1 << b;
                    }
                }
            }
        }
        let uniform = (0..2).all(|l| refs[l] != 0 || colz == 0 || colz == 0xFFFF);
        if uniform {
            let mv = |l: usize| {
                if refs[l] < 0 || (refs[l] == 0 && colz == 0xFFFF) {
                    [0, 0]
                } else {
                    clip_mv(mvps[l])
                }
            };
            self.mb_motion = Some((refs, [mv(0), mv(1)]));
        }
        for list in 0..2 {
            let r = refs[list];
            if r < 0 {
                continue;
            }
            let zmask = if r == 0 { colz } else { 0 };
            let mv = [mvps[list][0] as i16, mvps[list][1] as i16];
            let mvs = &mut self.pic.motion.mv[list][cur * 16..cur * 16 + 16];
            for (b, v) in mvs.iter_mut().enumerate() {
                *v = if (zmask >> b) & 1 != 0 { [0, 0] } else { mv };
            }
        }
        Ok(())
    }

    #[inline(never)]
    #[allow(clippy::needless_range_loop)] // indexed form over parallel fixed-size arrays (mirrors the spec loops, keeps the hot loop vectorisable)
    fn direct_spatial(&mut self, i: usize) -> Result<(), DecodeError> {
        let (refs, mvps, zero) = self.spatial_refs();
        let x0 = (i as i32 & 1) * 8;
        let y0 = (i as i32 >> 1) * 8;
        for list in 0..2 {
            self.store_ref(list, i, refs[list]);
        }
        let l1_short = self.s.col.map(|c| c.short_term).unwrap_or(false);
        let blocks: &[(i32, i32)] = if self.s.direct_8x8_inference {
            &[(0, 0)]
        } else {
            &[(0, 0), (4, 0), (0, 4), (4, 4)]
        };
        let bsize = if self.s.direct_8x8_inference { 8 } else { 4 };
        for &(dx, dy) in blocks {
            let x = x0 + dx;
            let y = y0 + dy;
            // Colocated 4x4: corner block with 8x8 inference.
            let (cx, cy) = if self.s.direct_8x8_inference {
                (x0 * 3 / 2, y0 * 3 / 2)
            } else {
                (x, y)
            };
            let (mv_col, ref_col, _, _) = self.colocated(cx, cy)?;
            let col_zero = l1_short
                && ref_col == 0
                && (-1..=1).contains(&mv_col[0])
                && (-1..=1).contains(&mv_col[1]);
            for list in 0..2 {
                let r = refs[list];
                let mv = if zero || r < 0 || (r == 0 && col_zero) {
                    [0, 0]
                } else {
                    mvps[list]
                };
                if r >= 0 {
                    self.store_mv(list, x, y, bsize, bsize, mv);
                }
            }
        }
        Ok(())
    }

    /// Co-located 4x4 partition (8.4.1.2.1) for luma location (xCol, yCol) of the current MB.
    /// Returns (mvCol, refIdxCol, refPicCol id, vertMvScale).
    #[inline(never)]
    fn colocated(
        &self,
        xcol: i32,
        ycol: i32,
    ) -> Result<([i32; 2], i32, i32, VertScale), DecodeError> {
        let col = self.s.col.ok_or(DecodeError::InvalidData(
            "direct prediction without RefPicList1",
        ))?;
        let st = &self.refs.dpb[col.idx];
        if self.pic.structure == 0 && !self.pic.mbaff && st.coding == Coding::Frame {
            // FRM / FRM: co-located MB is CurrMbAddr, yM = yCol, One_To_One (Table 8-8).
            let motion = &st.motion[0];
            let mb_col = self.cur;
            if mb_col >= motion.mbs {
                return Err(DecodeError::InvalidData(
                    "co-located macroblock out of range",
                ));
            }
            if motion.mb_flags[mb_col] & MBF_INTRA != 0 {
                return Ok(([0, 0], -1, -1, VertScale::One));
            }
            let b4 = ((ycol / 4) * 4 + xcol / 4) as usize;
            let b8 = mb_col * 4 + ((ycol / 8) * 2 + xcol / 8) as usize;
            let (l, r) = if motion.ref_idx[0][b8] >= 0 {
                (0, motion.ref_idx[0][b8])
            } else {
                (1, motion.ref_idx[1][b8])
            };
            if r < 0 {
                return Ok(([0, 0], -1, -1, VertScale::One));
            }
            let mv = motion.mv_at(l, mb_col, b4);
            return Ok((
                [mv[0] as i32, mv[1] as i32],
                r as i32,
                motion.ref_id[l][b8],
                VertScale::One,
            ));
        }
        let cur = self.cur;
        let w = self.pic.w;
        let cur_poc = self.pic.pic_poc();
        // Select colPic motion, mbAddrCol, yM, vertMvScale (Tables 8-6, 8-8).
        let (motion, mb_col, ym, vs): (&Motion, usize, i32, VertScale);
        let cur_motion_is = |idx: usize| idx == self.pic.store;
        let _ = cur_motion_is;
        let top_closer = {
            let t = (st.poc[0] - cur_poc).abs();
            let b = (st.poc[1] - cur_poc).abs();
            t < b
        };
        if self.pic.structure != 0 {
            // Current is a field.
            match st.coding {
                Coding::Fields => {
                    motion = &st.motion[(col.parity.max(1) - 1) as usize];
                    mb_col = cur;
                    ym = ycol;
                    vs = VertScale::One;
                }
                Coding::Frame => {
                    motion = &st.motion[0];
                    mb_col = 2 * w * (cur / w) + (cur % w) + w * (ycol as usize / 8);
                    ym = (2 * ycol) % 16;
                    vs = VertScale::FrmToFld;
                }
                Coding::Mbaff => {
                    motion = &st.motion[0];
                    let x = 2 * cur;
                    if motion.mb_flags.get(x).copied().unwrap_or(0) & MBF_FIELD == 0 {
                        mb_col = 2 * cur + (ycol as usize / 8);
                        ym = (2 * ycol) % 16;
                        vs = VertScale::FrmToFld;
                    } else {
                        mb_col = 2 * cur + (self.pic.structure as usize - 1);
                        ym = ycol;
                        vs = VertScale::One;
                    }
                }
            }
        } else if !self.pic.mbaff {
            match st.coding {
                Coding::Fields => {
                    motion = &st.motion[if top_closer { 0 } else { 1 }];
                    mb_col = w * (cur / (2 * w)) + (cur % w);
                    ym = 8 * ((cur / w) % 2) as i32 + 4 * (ycol / 8);
                    vs = VertScale::FldToFrm;
                }
                _ => {
                    motion = &st.motion[0];
                    mb_col = cur;
                    ym = ycol;
                    vs = VertScale::One;
                }
            }
        } else {
            let cur_field = self.pic.mbs[cur].field;
            match st.coding {
                Coding::Fields => {
                    mb_col = cur / 2;
                    if !cur_field {
                        motion = &st.motion[if top_closer { 0 } else { 1 }];
                        ym = 8 * (cur % 2) as i32 + 4 * (ycol / 8);
                        vs = VertScale::FldToFrm;
                    } else {
                        motion = &st.motion[cur & 1];
                        ym = ycol;
                        vs = VertScale::One;
                    }
                }
                _ => {
                    motion = &st.motion[0];
                    let x_field = motion.mb_flags.get(cur).copied().unwrap_or(0) & MBF_FIELD != 0;
                    if !cur_field {
                        if !x_field {
                            mb_col = cur;
                            ym = ycol;
                            vs = VertScale::One;
                        } else {
                            mb_col = 2 * (cur / 2) + if top_closer { 0 } else { 1 };
                            ym = 8 * (cur % 2) as i32 + 4 * (ycol / 8);
                            vs = VertScale::FldToFrm;
                        }
                    } else if !x_field {
                        mb_col = 2 * (cur / 2) + (ycol as usize / 8);
                        ym = (2 * ycol) % 16;
                        vs = VertScale::FrmToFld;
                    } else {
                        mb_col = cur;
                        ym = ycol;
                        vs = VertScale::One;
                    }
                }
            }
        }
        if mb_col >= motion.mbs {
            return Err(DecodeError::InvalidData(
                "co-located macroblock out of range",
            ));
        }
        if motion.mb_flags[mb_col] & MBF_INTRA != 0 {
            return Ok(([0, 0], -1, -1, vs));
        }
        let b4 = ((ym / 4) * 4 + xcol / 4) as usize;
        let b8 = mb_col * 4 + ((ym / 8) * 2 + xcol / 8) as usize;
        let (l, r) = if motion.ref_idx[0][b8] >= 0 {
            (0, motion.ref_idx[0][b8])
        } else {
            (1, motion.ref_idx[1][b8])
        };
        if r < 0 {
            return Ok(([0, 0], -1, -1, vs));
        }
        let mv = motion.mv_at(l, mb_col, b4);
        Ok((
            [mv[0] as i32, mv[1] as i32],
            r as i32,
            motion.ref_id[l][b8],
            vs,
        ))
    }

    #[inline(never)]
    fn direct_temporal(&mut self, i: usize) -> Result<(), DecodeError> {
        let x0 = (i as i32 & 1) * 8;
        let y0 = (i as i32 >> 1) * 8;
        let blocks: &[(i32, i32)] = if self.s.direct_8x8_inference {
            &[(0, 0)]
        } else {
            &[(0, 0), (4, 0), (0, 4), (4, 4)]
        };
        let bsize = if self.s.direct_8x8_inference { 8 } else { 4 };
        let mut ref0_set = None;
        for &(dx, dy) in blocks {
            let x = x0 + dx;
            let y = y0 + dy;
            let (cx, cy) = if self.s.direct_8x8_inference {
                (x0 * 3 / 2, y0 * 3 / 2)
            } else {
                (x, y)
            };
            let (mut mv_col, ref_col, ref_col_id, vs) = self.colocated(cx, cy)?;
            let ref0 = if ref_col < 0 {
                0
            } else {
                self.map_col_to_list0(ref_col_id, vs)?
            };
            match vs {
                VertScale::FrmToFld => mv_col[1] /= 2,
                VertScale::FldToFrm => mv_col[1] *= 2,
                VertScale::One => {}
            }
            let p0 = self
                .ref_pic(0, ref0)
                .ok_or(DecodeError::InvalidData("direct ref_idx_l0"))?;
            let p1 = self
                .ref_pic(1, 0)
                .ok_or(DecodeError::InvalidData("direct ref_idx_l1"))?;
            let (mv0, mv1) = if p0.long || p1.poc - p0.poc == 0 {
                (mv_col, [0, 0])
            } else {
                let tb = (self.cur_poc() - p0.poc).clamp(-128, 127);
                let td = (p1.poc - p0.poc).clamp(-128, 127);
                let tx = (16384 + (td / 2).abs()) / td;
                let dsf = ((tb * tx + 32) >> 6).clamp(-1024, 1023);
                let m0 = [(dsf * mv_col[0] + 128) >> 8, (dsf * mv_col[1] + 128) >> 8];
                (m0, [m0[0] - mv_col[0], m0[1] - mv_col[1]])
            };
            if ref0_set.is_none() {
                ref0_set = Some(ref0);
                self.store_ref(0, i, ref0);
                self.store_ref(1, i, 0);
            }
            self.store_mv(0, x, y, bsize, bsize, mv0);
            self.store_mv(1, x, y, bsize, bsize, mv1);
        }
        Ok(())
    }

    /// MapColToList0 (8.4.1.2.3) from the identity of refPicCol.
    fn map_col_to_list0(&self, id: i32, vs: VertScale) -> Result<i32, DecodeError> {
        let l0 = &self.s.ref_list[0];
        let uid = id >> 2;
        let st_code = id & 3;
        let field_pic = self.pic.structure != 0;
        let cur_field_mb = self.pic.mbaff && self.pic.mbs[self.cur].field;
        let find = |pred: &dyn Fn(&RefPic) -> bool| -> Option<i32> {
            l0.iter().position(pred).map(|p| p as i32)
        };
        let uid_of = |p: &RefPic| self.refs.dpb[p.idx].uid as i32;
        let res = match vs {
            VertScale::One => {
                if !field_pic && cur_field_mb {
                    // refPicCol is a field; find the frame containing it.
                    find(&|p: &RefPic| uid_of(p) == uid).map(|f| {
                        let cur_par = 1 + (self.cur & 1) as i32;
                        if st_code == cur_par {
                            f << 1
                        } else {
                            (f << 1) + 1
                        }
                    })
                } else {
                    find(&|p: &RefPic| p.id == id)
                }
            }
            VertScale::FrmToFld => {
                if !field_pic {
                    find(&|p: &RefPic| uid_of(p) == uid).map(|f| f << 1)
                } else {
                    let want = ref_id(uid as u32, self.pic.structure);
                    find(&|p: &RefPic| p.id == want)
                }
            }
            VertScale::FldToFrm => find(&|p: &RefPic| uid_of(p) == uid),
        };
        res.ok_or(DecodeError::InvalidData(
            "temporal direct: co-located reference not in RefPicList0",
        ))
    }

    // ------------------------------------------------------------------ MC

    #[inline(never)]
    fn motion_compensate(&mut self, direct8: u8) -> Result<(), DecodeError> {
        if let (Some((r, mv)), false) = (self.mb_motion.take(), self.self_ref_possible) {
            // One 16x16 prediction block with motion known from its derivation.
            let p = self.make_part(0, 0, 16, 16, r, mv)?;
            let dpb = self.refs.dpb;
            for rp in p.refs.iter().flatten() {
                if dpb[rp.idx].buf.y.is_empty() {
                    return Err(DecodeError::InvalidData("reference picture missing"));
                }
            }
            let ctx = McCtx {
                s: self.s,
                g: self.geom,
                field_dbl: self.pic.mbaff && self.pic.mbs[self.cur].field,
            };
            mc_part(
                &ctx,
                |i| &*dpb[i].buf,
                &p,
                &mut self.pic.buf,
                &mut self.mc_tmp,
            );
            return Ok(());
        }
        // Partition the MB into prediction blocks with uniform motion.
        let kind = self.mb.kind;
        let mut parts: [(i32, i32, i32, i32); 16] = [(0, 0, 0, 0); 16];
        let mut n = 0;
        // Direct / 8x8 macroblocks whose motion is uniform are predicted as one 16x16 block
        // (identical samples, fewer kernel invocations).
        let uniform = matches!(kind, Kind::BSkip | Kind::BDirect | Kind::B8x8 | Kind::P8x8) && {
            let cur = self.cur;
            let m = &self.pic.motion;
            (0..2).all(|l| {
                let r = &m.ref_idx[l][cur * 4..cur * 4 + 4];
                let mv = &m.mv[l][cur * 16..cur * 16 + 16];
                r.iter().all(|&x| x == r[0]) && (r[0] < 0 || mv.iter().all(|&v| v == mv[0]))
            })
        };
        let kind = if uniform { Kind::P16x16 } else { kind };
        match kind {
            Kind::PSkip | Kind::P16x16 | Kind::B16x16 => {
                parts[0] = (0, 0, 16, 16);
                n = 1;
            }
            Kind::P16x8 | Kind::B16x8 => {
                parts[0] = (0, 0, 16, 8);
                parts[1] = (0, 8, 16, 8);
                n = 2;
            }
            Kind::P8x16 | Kind::B8x16 => {
                parts[0] = (0, 0, 8, 16);
                parts[1] = (8, 0, 8, 16);
                n = 2;
            }
            _ => {
                for i in 0..4 {
                    let x0 = (i as i32 & 1) * 8;
                    let y0 = (i as i32 >> 1) * 8;
                    let shape = if direct8 & (1 << i) != 0 {
                        if self.s.direct_8x8_inference { 0 } else { 3 }
                    } else if kind == Kind::B8x8 {
                        B_SUB_TYPES[self.mb.sub[i] as usize].0
                    } else {
                        self.mb.sub[i]
                    };
                    for s in 0..SUB_PARTS[shape as usize] {
                        parts[n] = match shape {
                            0 => (x0, y0, 8, 8),
                            1 => (x0, y0 + s as i32 * 4, 8, 4),
                            2 => (x0 + s as i32 * 4, y0, 4, 8),
                            _ => (x0 + (s as i32 & 1) * 4, y0 + (s as i32 >> 1) * 4, 4, 4),
                        };
                        n += 1;
                    }
                }
            }
        }
        let field_dbl = self.pic.mbaff && self.pic.mbs[self.cur].field;
        if !self.self_ref_possible {
            // Predict straight into the picture.
            let ctx = McCtx {
                s: self.s,
                g: self.geom,
                field_dbl,
            };
            let dpb = self.refs.dpb;
            for &(x, y, w, h) in &parts[..n] {
                let p = self.part_motion(x, y, w, h)?;
                for r in p.refs.iter().flatten() {
                    if dpb[r.idx].buf.y.is_empty() {
                        return Err(DecodeError::InvalidData("reference picture missing"));
                    }
                }
                mc_part(
                    &ctx,
                    |i| &*dpb[i].buf,
                    &p,
                    &mut self.pic.buf,
                    &mut self.mc_tmp,
                );
            }
        } else {
            // A reference may be the first field of the current frame: predict into a scratch MB.
            let mut pm = [PartMv::default(); 16];
            for k in 0..n {
                let (x, y, w, h) = parts[k];
                pm[k] = self.part_motion(x, y, w, h)?;
                for r in pm[k].refs.iter().flatten() {
                    if r.idx != self.pic.store && self.refs.dpb[r.idx].buf.y.is_empty() {
                        return Err(DecodeError::InvalidData("reference picture missing"));
                    }
                }
            }
            let mut tmp = McTmp::new();
            let g = self.geom;
            let mut out = FrameBuf {
                y: vec![0; 256],
                cb: vec![0; 64],
                cr: vec![0; 64],
                width: 16,
                height: 16,
                cwidth: 8,
                cheight: 8,
            };
            let lctx = McCtx {
                s: self.s,
                g: MbGeom {
                    off_y: 0,
                    stride_y: 16,
                    off_c: 0,
                    stride_c: 8,
                    ..g
                },
                field_dbl,
            };
            let dpb = self.refs.dpb;
            let cur_store = self.pic.store;
            let cur_buf = &self.pic.buf;
            for p in &pm[..n] {
                mc_part(
                    &lctx,
                    |i| {
                        if i == cur_store {
                            cur_buf
                        } else {
                            &*dpb[i].buf
                        }
                    },
                    p,
                    &mut out,
                    &mut tmp,
                );
            }
            let buf = &mut self.pic.buf;
            for yy in 0..16 {
                let o = g.off_y + yy * g.stride_y;
                buf.y[o..o + 16].copy_from_slice(&out.y[yy * 16..yy * 16 + 16]);
            }
            if self.s.chroma {
                for yy in 0..8 {
                    let o = g.off_c + yy * g.stride_c;
                    buf.cb[o..o + 8].copy_from_slice(&out.cb[yy * 8..yy * 8 + 8]);
                    buf.cr[o..o + 8].copy_from_slice(&out.cr[yy * 8..yy * 8 + 8]);
                }
            }
        }
        Ok(())
    }

    /// Motion of one prediction block (both lists) resolved for motion compensation.
    fn part_motion(&self, x: i32, y: i32, w: i32, h: i32) -> Result<PartMv, DecodeError> {
        let cur = self.cur;
        let b8 = ((y / 8) * 2 + x / 8) as usize;
        let b4 = ((y / 4) * 4 + x / 4) as usize;
        let m = &self.pic.motion;
        let r = [0, 1].map(|l| m.ref_idx[l][cur * 4 + b8] as i32);
        let mv = [0, 1].map(|l| {
            let v = m.mv[l][cur * 16 + b4];
            [v[0] as i32, v[1] as i32]
        });
        self.make_part(x, y, w, h, r, mv)
    }

    /// Prediction block with refIdx `r` (-1: list unused) and vectors `mv` per list.
    #[allow(clippy::too_many_arguments)] // kernel takes explicit sample geometry instead of a wrapper struct
    fn make_part(
        &self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        r: [i32; 2],
        mv: [[i32; 2]; 2],
    ) -> Result<PartMv, DecodeError> {
        let mut pm = PartMv {
            x,
            y,
            w,
            h,
            refs: [None; 2],
            mv: [[0; 2]; 2],
            ref_idx: [-1; 2],
            wbi: (32, 32),
        };
        for list in 0..2 {
            let r = r[list];
            if r < 0 {
                continue;
            }
            pm.ref_idx[list] = r;
            pm.refs[list] = Some(
                self.ref_pic(list, r)
                    .ok_or(DecodeError::InvalidData("ref_idx beyond reference list"))?,
            );
            pm.mv[list] = mv[list];
        }
        if self.s.wp == Wp::Implicit {
            if let (Some(p0), Some(p1)) = (pm.refs[0], pm.refs[1]) {
                pm.wbi = self.implicit_weights(&p0, &p1);
            }
        }
        Ok(pm)
    }

    /// Implicit bi-predictive weights (8.4.2.3.1, equations 8-275 .. 8-280).
    fn implicit_weights(&self, p0: &RefPic, p1: &RefPic) -> (i32, i32) {
        let cur_poc = self.cur_poc();
        let tb = (cur_poc - p0.poc).clamp(-128, 127);
        let td = (p1.poc - p0.poc).clamp(-128, 127);
        if p1.poc - p0.poc == 0 || p0.long || p1.long {
            return (32, 32);
        }
        let tx = (16384 + (td / 2).abs()) / td;
        let dsf = ((tb * tx + 32) >> 6).clamp(-1024, 1023);
        if (dsf >> 2) < -64 || (dsf >> 2) > 128 {
            return (32, 32);
        }
        (64 - (dsf >> 2), dsf >> 2)
    }
}

/// Motion of one prediction block.
#[derive(Clone, Copy, Default)]
struct PartMv {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    refs: [Option<RefPic>; 2],
    mv: [[i32; 2]; 2],
    ref_idx: [i32; 2],
    /// Implicit bi-prediction weights (w0, w1).
    wbi: (i32, i32),
}

struct McCtx<'s> {
    s: &'s SliceCtx,
    /// Destination geometry (picture position via mbx / y_pic, write offsets via off_* / stride_*).
    g: MbGeom,
    field_dbl: bool,
}

/// Prediction scratch of both lists: luma 16x16 and the two 8x8 chroma blocks.
pub(crate) struct McTmp {
    y: [[u8; 256]; 2],
    c: [[[u8; 64]; 2]; 2],
}

impl McTmp {
    pub(crate) fn new() -> Self {
        McTmp {
            y: [[0; 256]; 2],
            c: [[[0; 64]; 2]; 2],
        }
    }
}

/// Reference-plane geometry of one list of a block.
struct ListPos<'r> {
    fb: &'r FrameBuf,
    parity: u8,
    /// Luma integer position and quarter-sample fraction.
    xi: i32,
    yi: i32,
    fx: u32,
    fy: u32,
    /// Chroma integer position and eighth-sample fraction.
    xc: i32,
    yc: i32,
    cfx: u32,
    cfy: u32,
}

fn list_pos<'r>(g: &MbGeom, p: &PartMv, list: usize, rp: &RefPic, fb: &'r FrameBuf) -> ListPos<'r> {
    let [mvx, mvy] = p.mv[list];
    let xa = (g.mbx * 16) as i32 + p.x;
    let ya = g.y_pic as i32 + p.y;
    // Chroma vertical offset between fields of opposite parity (Table 8-9 / 8-10).
    let mut mvcy = mvy;
    if g.field_parity != 0 && rp.parity != 0 {
        if rp.parity == 1 && g.field_parity == 2 {
            mvcy += 2;
        } else if rp.parity == 2 && g.field_parity == 1 {
            mvcy -= 2;
        }
    }
    ListPos {
        fb,
        parity: rp.parity,
        xi: xa + (mvx >> 2),
        yi: ya + (mvy >> 2),
        fx: (mvx & 3) as u32,
        fy: (mvy & 3) as u32,
        xc: xa / 2 + (mvx >> 3),
        yc: ya / 2 + (mvcy >> 3),
        cfx: (mvx & 7) as u32,
        cfy: (mvcy & 7) as u32,
    }
}

/// Luma prediction samples of one list as (samples, stride): a window of the reference plane
/// at a full-sample position inside it, else interpolated into `tmp` (stride 16).
#[inline]
fn luma_src<'a>(lp: &ListPos<'a>, w: usize, h: usize, tmp: &'a mut [u8; 256]) -> (&'a [u8], usize) {
    let fb = lp.fb;
    let r = PlaneRef::new(&fb.y, fb.width, fb.height, lp.parity);
    if lp.fx == 0
        && lp.fy == 0
        && lp.xi >= 0
        && lp.yi >= 0
        && lp.xi + w as i32 <= r.w
        && lp.yi + h as i32 <= r.h
    {
        let o = r.base + lp.yi as usize * r.stride + lp.xi as usize;
        return (&r.data[o..], r.stride);
    }
    luma_mc(&r, lp.xi, lp.yi, lp.fx, lp.fy, w, h, &mut tmp[..], 16);
    (&tmp[..], 16)
}

/// Chroma counterpart of `luma_src` for plane `k` (stride 8 in `tmp`).
#[inline]
fn chroma_src<'a>(
    lp: &ListPos<'a>,
    k: usize,
    w: usize,
    h: usize,
    tmp: &'a mut [u8; 64],
) -> (&'a [u8], usize) {
    let fb = lp.fb;
    let plane: &'a [u8] = if k == 0 { &fb.cb } else { &fb.cr };
    let r = PlaneRef::new(plane, fb.cwidth, fb.cheight, lp.parity);
    if lp.cfx == 0
        && lp.cfy == 0
        && lp.xc >= 0
        && lp.yc >= 0
        && lp.xc + w as i32 <= r.w
        && lp.yc + h as i32 <= r.h
    {
        let o = r.base + lp.yc as usize * r.stride + lp.xc as usize;
        return (&r.data[o..], r.stride);
    }
    chroma_mc(&r, lp.xc, lp.yc, lp.cfx, lp.cfy, w, h, &mut tmp[..], 8);
    (&tmp[..], 8)
}

/// Motion-compensated prediction of one block (8.4.2.2) with weighted sample prediction
/// (8.4.2.3), written into `out` at the geometry's offsets.
#[inline]
fn mc_part<'r, F: Fn(usize) -> &'r FrameBuf>(
    c: &McCtx,
    refbuf: F,
    p: &PartMv,
    out: &mut FrameBuf,
    tmp: &mut McTmp,
) {
    let g = c.g;
    let (wu, hu) = (p.w as usize, p.h as usize);
    let (wc, hc) = (wu / 2, hu / 2);
    let (x, y) = (p.x as usize, p.y as usize);
    let oy = g.off_y + y * g.stride_y + x;
    let oc = g.off_c + (y / 2) * g.stride_c + x / 2;
    let (sy, sc) = (g.stride_y, g.stride_c);
    let chroma = c.s.chroma;
    let s = c.s;
    if let (Some(r0), Some(r1)) = (&p.refs[0], &p.refs[1]) {
        // Bi-prediction: both predictions as sample windows, combined into the picture.
        let l0 = list_pos(&g, p, 0, r0, refbuf(r0.idx));
        let l1 = list_pos(&g, p, 1, r1, refbuf(r1.idx));
        // (logWD, w0, w1, o) per colour component (8-272).
        let wts: [(u32, i32, i32, i32); 3] = match s.wp {
            Wp::Default => [(5, 32, 32, 0); 3],
            Wp::Implicit => {
                let (w0, w1) = p.wbi;
                [(5, w0, w1, 0); 3]
            }
            Wp::Explicit => {
                let ri = |l: usize| {
                    (if c.field_dbl {
                        p.ref_idx[l] >> 1
                    } else {
                        p.ref_idx[l]
                    }) as usize
                };
                let w0 = s.weights[0][ri(0)];
                let w1 = s.weights[1][ri(1)];
                std::array::from_fn(|k| {
                    (
                        s.log_wd[(k > 0) as usize],
                        w0[k].0,
                        w1[k].0,
                        (w0[k].1 + w1[k].1 + 1) >> 1,
                    )
                })
            }
        };
        let [ty0, ty1] = &mut tmp.y;
        let (a, sa) = luma_src(&l0, wu, hu, ty0);
        let (b, sb) = luma_src(&l1, wu, hu, ty1);
        let (lw, w0, w1, o) = wts[0];
        weight_bi(&mut out.y[oy..], sy, a, sa, b, sb, wu, hu, lw, w0, w1, o);
        if chroma {
            let [tc0, tc1] = &mut tmp.c;
            for (k, (t0, t1)) in tc0.iter_mut().zip(tc1.iter_mut()).enumerate() {
                let (a, sa) = chroma_src(&l0, k, wc, hc, t0);
                let (b, sb) = chroma_src(&l1, k, wc, hc, t1);
                let (lw, w0, w1, o) = wts[1 + k];
                let pl = if k == 0 { &mut out.cb } else { &mut out.cr };
                weight_bi(&mut pl[oc..], sc, a, sa, b, sb, wc, hc, lw, w0, w1, o);
            }
        }
        return;
    }
    let list = if p.refs[0].is_some() { 0 } else { 1 };
    let Some(rp) = &p.refs[list] else { return };
    let lp = list_pos(&g, p, list, rp, refbuf(rp.idx));
    let fb = lp.fb;
    let ry = PlaneRef::new(&fb.y, fb.width, fb.height, lp.parity);
    luma_mc(
        &ry,
        lp.xi,
        lp.yi,
        lp.fx,
        lp.fy,
        wu,
        hu,
        &mut out.y[oy..],
        sy,
    );
    if chroma {
        let rcb = PlaneRef::new(&fb.cb, fb.cwidth, fb.cheight, lp.parity);
        let rcr = PlaneRef::new(&fb.cr, fb.cwidth, fb.cheight, lp.parity);
        chroma_mc(
            &rcb,
            lp.xc,
            lp.yc,
            lp.cfx,
            lp.cfy,
            wc,
            hc,
            &mut out.cb[oc..],
            sc,
        );
        chroma_mc(
            &rcr,
            lp.xc,
            lp.yc,
            lp.cfx,
            lp.cfy,
            wc,
            hc,
            &mut out.cr[oc..],
            sc,
        );
    }
    if s.wp == Wp::Explicit {
        let r = p.ref_idx[list];
        let r = if c.field_dbl { r >> 1 } else { r } as usize;
        let wt = s.weights[list][r];
        weight_uni(&mut out.y[oy..], sy, wu, hu, s.log_wd[0], wt[0].0, wt[0].1);
        if chroma {
            weight_uni(&mut out.cb[oc..], sc, wc, hc, s.log_wd[1], wt[1].0, wt[1].1);
            weight_uni(&mut out.cr[oc..], sc, wc, hc, s.log_wd[1], wt[2].0, wt[2].1);
        }
    }
}
