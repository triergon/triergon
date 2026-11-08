//! Intra macroblock reconstruction (clause 8.3) and residual application (clause 8.5).

use crate::SliceError;
use crate::mbinfo::Kind;
use crate::recon::*;
use crate::slicedec::SliceDecoder;
use crate::tables::*;

/// Scale a 4x4 block of levels (8.5.12.1); coefficient 0 is replaced by the caller for DC blocks.
/// The arithmetic wraps, as in the transform kernels of `recon`.
#[inline(always)]
fn dequant4(coef: &[i32; 16], ls: &[i32; 16], qp: i32) -> [i32; 16] {
    let mut d = [0i32; 16];
    let sh = qp / 6;
    if sh >= 4 {
        for i in 0..16 {
            d[i] = coef[i].wrapping_mul(ls[i]) << (sh - 4);
        }
    } else {
        let add = 1 << (3 - sh);
        let s = 4 - sh;
        for i in 0..16 {
            d[i] = coef[i].wrapping_mul(ls[i]).wrapping_add(add) >> s;
        }
    }
    d
}

/// Scale an 8x8 block of levels (8.5.13.1), wrapping like `dequant4`.
#[inline(always)]
fn dequant8(coef: &[i32; 64], ls: &[i32; 64], qp: i32) -> [i32; 64] {
    let mut d = [0i32; 64];
    let sh = qp / 6;
    if sh >= 6 {
        for i in 0..64 {
            d[i] = coef[i].wrapping_mul(ls[i]) << (sh - 6);
        }
    } else {
        let add = 1 << (5 - sh);
        let s = 6 - sh;
        for i in 0..64 {
            d[i] = coef[i].wrapping_mul(ls[i]).wrapping_add(add) >> s;
        }
    }
    d
}

impl SliceDecoder<'_> {
    #[inline]
    fn intra_avail(&self, n: usize) -> bool {
        n == self.cur || !(self.s.constrained_intra && !self.pic.mbs[n].kind.is_intra())
    }

    /// Plane offset of luma location (xN, yN) relative to the current MB, if available for intra.
    #[inline]
    pub(crate) fn nb_luma(&self, xn: i32, yn: i32) -> Option<usize> {
        if !self.pic.mbaff {
            // Frames and fields: neighbours are adjacent in the (field) plane.
            let n = self.nb_mb_simple(xn, yn, 16, 16)?;
            if !self.intra_avail(n) {
                return None;
            }
            let g = &self.geom;
            return Some(
                (g.off_y as isize + yn as isize * g.stride_y as isize + xn as isize) as usize,
            );
        }
        let (n, xw, yw) = self.loc(xn, yn, 16, 16)?;
        if !self.intra_avail(n) {
            return None;
        }
        let g = if n == self.cur {
            self.geom
        } else {
            self.geom_of(n)
        };
        Some(g.off_y + yw as usize * g.stride_y + xw as usize)
    }

    #[inline]
    fn nb_chroma(&self, xn: i32, yn: i32) -> Option<usize> {
        if !self.pic.mbaff {
            let n = self.nb_mb_simple(xn, yn, 8, 8)?;
            if !self.intra_avail(n) {
                return None;
            }
            let g = &self.geom;
            return Some(
                (g.off_c as isize + yn as isize * g.stride_c as isize + xn as isize) as usize,
            );
        }
        let (n, xw, yw) = self.loc(xn, yn, 8, 8)?;
        if !self.intra_avail(n) {
            return None;
        }
        let g = if n == self.cur {
            self.geom
        } else {
            self.geom_of(n)
        };
        Some(g.off_c + yw as usize * g.stride_c + xw as usize)
    }

    /// Intra4x4PredMode / Intra8x8PredMode of the neighbouring block for mode prediction.
    /// Returns None when dcPredModePredictedFlag must be set (unavailable / constrained inter).
    fn nb_pred_mode(
        &self,
        xn: i32,
        yn: i32,
        is8: bool,
        n_sel: usize,
        cur_b8: usize,
    ) -> Option<i32> {
        let (n, xw, yw) = self.loc(xn, yn, 16, 16)?;
        if n == self.cur {
            let r = ((yw / 4) * 4 + xw / 4) as usize;
            return Some(self.mb.modes[r] as i32);
        }
        let m = &self.pic.mbs[n];
        if self.s.constrained_intra && m.kind.is_inter() {
            return None;
        }
        match m.kind {
            Kind::I4x4 => {
                if is8 {
                    // 8-72: Intra4x4PredMode[luma8x8BlkIdxN * 4 + n]
                    let b8n = ((yw / 8) * 2 + xw / 8) as usize;
                    let mut nn = n_sel;
                    if n_sel == 1
                        && self.pic.mbaff
                        && !self.pic.mbs[self.cur].field
                        && m.field
                        && cur_b8 == 2
                    {
                        nn = 3;
                    }
                    let r = BLK_TO_RASTER[b8n * 4 + nn] as usize;
                    Some(m.modes[r] as i32)
                } else {
                    let r = ((yw / 4) * 4 + xw / 4) as usize;
                    Some(m.modes[r] as i32)
                }
            }
            Kind::I8x8 => {
                let r = ((yw / 4) * 4 + xw / 4) as usize;
                Some(m.modes[r] as i32)
            }
            _ => Some(2),
        }
    }

    fn pred_mode(&self, x: i32, y: i32, is8: bool, b8: usize, rem: Option<u8>) -> i8 {
        let a = self.nb_pred_mode(x - 1, y, is8, 1, b8);
        let b = self.nb_pred_mode(x, y - 1, is8, 2, b8);
        let pred = match (a, b) {
            (Some(a), Some(b)) => a.min(b),
            _ => 2,
        };
        match rem {
            None => pred as i8,
            Some(r) => {
                if (r as i32) < pred {
                    r as i8
                } else {
                    r as i8 + 1
                }
            }
        }
    }

    pub(crate) fn intra_mb(&mut self) -> Result<(), SliceError> {
        let qp = self.qp;
        match self.mb.kind {
            Kind::I4x4 => {
                for blk in 0..16 {
                    let r = BLK_TO_RASTER[blk] as usize;
                    let x = (r % 4) as i32 * 4;
                    let y = (r / 4) as i32 * 4;
                    let mode = self.pred_mode(x, y, false, blk / 4, self.mb.rem[blk]);
                    self.mb.modes[r] = mode;
                    let e = self.edge4(x, y, blk);
                    let g = self.geom;
                    let off = g.off_y + y as usize * g.stride_y + x as usize;
                    pred4x4(mode as u8, &e, &mut self.pic.buf.y, off, g.stride_y);
                    if self.mb.blk & (1 << r) != 0 {
                        self.add_luma4x4(r, qp, false, None);
                    }
                }
            }
            Kind::I8x8 => {
                for b8 in 0..4 {
                    let x = (b8 as i32 & 1) * 8;
                    let y = (b8 as i32 >> 1) * 8;
                    let mode = self.pred_mode(x, y, true, b8, self.mb.rem[b8]);
                    let r0 = ((y / 4) * 4 + x / 4) as usize;
                    for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        self.mb.modes[r0 + dy * 4 + dx] = mode;
                    }
                    let e = self.edge8(x, y, b8);
                    let g = self.geom;
                    let off = g.off_y + y as usize * g.stride_y + x as usize;
                    pred8x8(mode as u8, &e, &mut self.pic.buf.y, off, g.stride_y);
                    if self.mb.b8 & (1 << b8) != 0 {
                        self.add_luma8x8(b8, qp, false);
                    }
                }
            }
            Kind::I16x16 => {
                let mut top = [0u8; 16];
                let mut left = [0u8; 16];
                let mut tl = 0u8;
                let ot = self.nb_luma(0, -1);
                let has_top = ot.is_some();
                if let Some(o) = ot {
                    top.copy_from_slice(&self.pic.buf.y[o..o + 16]);
                }
                let mut has_left = true;
                for yy in 0..16 {
                    match self.nb_luma(-1, yy) {
                        Some(o) => left[yy as usize] = self.pic.buf.y[o],
                        None => has_left = false,
                    }
                }
                let otl = self.nb_luma(-1, -1);
                if let Some(o) = otl {
                    tl = self.pic.buf.y[o];
                }
                let mode = self.mb.i16_mode;
                let ok = match mode {
                    0 => has_top,
                    1 => has_left,
                    3 => has_top && has_left && otl.is_some(),
                    _ => true,
                };
                if !ok {
                    return Err(SliceError::InvalidData(
                        "Intra16x16 mode uses unavailable samples",
                    ));
                }
                let g = self.geom;
                pred16x16(
                    mode,
                    &top,
                    &left,
                    tl,
                    has_top,
                    has_left,
                    &mut self.pic.buf.y,
                    g.off_y,
                    g.stride_y,
                );
                self.residual_i16(qp);
            }
            _ => {}
        }
        // Chroma.
        if self.s.chroma {
            let mut top = [[0u8; 8]; 2];
            let mut left = [[0u8; 8]; 2];
            let mut tl = [0u8; 2];
            let ot = self.nb_chroma(0, -1);
            let has_top = ot.is_some();
            if let Some(o) = ot {
                top[0].copy_from_slice(&self.pic.buf.cb[o..o + 8]);
                top[1].copy_from_slice(&self.pic.buf.cr[o..o + 8]);
            }
            let mut has_left_half = [true; 2];
            for yy in 0..8 {
                match self.nb_chroma(-1, yy) {
                    Some(o) => {
                        left[0][yy as usize] = self.pic.buf.cb[o];
                        left[1][yy as usize] = self.pic.buf.cr[o];
                    }
                    None => has_left_half[yy as usize / 4] = false,
                }
            }
            let has_left = has_left_half[0] && has_left_half[1];
            let otl = self.nb_chroma(-1, -1);
            if let Some(o) = otl {
                tl = [self.pic.buf.cb[o], self.pic.buf.cr[o]];
            }
            let mode = self.mb.chroma_mode;
            let ok = match mode {
                1 => has_left,
                2 => has_top,
                3 => has_top && has_left && otl.is_some(),
                _ => true,
            };
            if !ok {
                return Err(SliceError::InvalidData(
                    "chroma intra mode uses unavailable samples",
                ));
            }
            let g = self.geom;
            pred_chroma8(
                mode,
                &top[0],
                &left[0],
                tl[0],
                has_top,
                has_left_half,
                &mut self.pic.buf.cb,
                g.off_c,
                g.stride_c,
            );
            pred_chroma8(
                mode,
                &top[1],
                &left[1],
                tl[1],
                has_top,
                has_left_half,
                &mut self.pic.buf.cr,
                g.off_c,
                g.stride_c,
            );
            self.residual_chroma(true);
        }
        self.pic.mbs[self.cur].modes = self.mb.modes;
        Ok(())
    }

    /// Neighbour samples for Intra_4x4 block with luma4x4BlkIdx `blk` at (x, y).
    fn edge4(&self, x: i32, y: i32, blk: usize) -> Edge<4, 8> {
        let buf = &self.pic.buf.y;
        let mut e = Edge {
            top: [0; 8],
            left: [0; 4],
            tl: 0,
            has_top: false,
            has_left: true,
            has_tl: false,
        };
        if let Some(o) = self.nb_luma(x, y - 1) {
            e.top[..4].copy_from_slice(&buf[o..o + 4]);
            e.has_top = true;
            // Top-right (x + 4, y - 1).
            let tr = if x + 4 < 16 && y > 0 {
                // Inside the MB: available iff that block is already decoded.
                let rb = RASTER_TO_BLK[(((y - 1) / 4) * 4 + (x + 4) / 4) as usize] as usize;
                if rb < blk {
                    self.nb_luma(x + 4, y - 1)
                } else {
                    None
                }
            } else {
                self.nb_luma(x + 4, y - 1)
            };
            match tr {
                Some(o2) => e.top[4..].copy_from_slice(&buf[o2..o2 + 4]),
                None => {
                    let v = e.top[3];
                    e.top[4..].fill(v);
                }
            }
        }
        for yy in 0..4 {
            match self.nb_luma(x - 1, y + yy) {
                Some(o) => e.left[yy as usize] = buf[o],
                None => e.has_left = false,
            }
        }
        if let Some(o) = self.nb_luma(x - 1, y - 1) {
            e.tl = buf[o];
            e.has_tl = true;
        }
        e
    }

    fn edge8(&self, x: i32, y: i32, b8: usize) -> Edge<8, 16> {
        let buf = &self.pic.buf.y;
        let mut e = Edge {
            top: [0; 16],
            left: [0; 8],
            tl: 0,
            has_top: false,
            has_left: true,
            has_tl: false,
        };
        if let Some(o) = self.nb_luma(x, y - 1) {
            e.top[..8].copy_from_slice(&buf[o..o + 8]);
            e.has_top = true;
            let tr = if x + 8 < 16 && y > 0 {
                if b8 == 2 {
                    self.nb_luma(x + 8, y - 1)
                } else {
                    None
                }
            } else {
                self.nb_luma(x + 8, y - 1)
            };
            match tr {
                Some(o2) => e.top[8..].copy_from_slice(&buf[o2..o2 + 8]),
                None => {
                    let v = e.top[7];
                    e.top[8..].fill(v);
                }
            }
        }
        for yy in 0..8 {
            match self.nb_luma(x - 1, y + yy) {
                Some(o) => e.left[yy as usize] = buf[o],
                None => e.has_left = false,
            }
        }
        if let Some(o) = self.nb_luma(x - 1, y - 1) {
            e.tl = buf[o];
            e.has_tl = true;
        }
        e
    }

    /// Scale and inverse-transform luma 4x4 block (raster r), adding to the picture. When `dc`
    /// is given it replaces coefficient (0,0) (Intra16x16).
    pub(crate) fn add_luma4x4(&mut self, r: usize, qp: i32, inter: bool, dc: Option<i32>) {
        let base = r * 16;
        let ls = &self.s.ls4[if inter { 3 } else { 0 }][(qp % 6) as usize];
        let coef: &mut [i32; 16] = (&mut self.mb.coef[base..base + 16]).try_into().unwrap();
        let d = dequant4(coef, ls, qp);
        let any_ac = coef[1..].iter().fold(0, |a, &c| a | c) != 0;
        coef.fill(0);
        let mut d = d;
        if let Some(v) = dc {
            d[0] = v;
        }
        let g = self.geom;
        let off = g.off_y + (r / 4) * 4 * g.stride_y + (r % 4) * 4;
        if any_ac {
            idct4_add(&d, &mut self.pic.buf.y, off, g.stride_y);
        } else {
            dc_add(d[0], 4, &mut self.pic.buf.y, off, g.stride_y);
        }
    }

    pub(crate) fn add_luma8x8(&mut self, b8: usize, qp: i32, inter: bool) {
        let base = b8 * 64;
        let ls = &self.s.ls8[inter as usize][(qp % 6) as usize];
        let coef: &mut [i32; 64] = (&mut self.mb.coef[base..base + 64]).try_into().unwrap();
        let d = dequant8(coef, ls, qp);
        let any_ac = coef[1..].iter().fold(0, |a, &c| a | c) != 0;
        coef.fill(0);
        let g = self.geom;
        let off = g.off_y + (b8 >> 1) * 8 * g.stride_y + (b8 & 1) * 8;
        if any_ac {
            idct8_add(&d, &mut self.pic.buf.y, off, g.stride_y);
        } else {
            dc_add(d[0], 8, &mut self.pic.buf.y, off, g.stride_y);
        }
    }

    #[allow(clippy::needless_range_loop)] // indexed form over parallel fixed-size arrays (mirrors the spec loops, keeps the hot loop vectorisable)
    fn residual_i16(&mut self, qp: i32) {
        let has_dc = self.mb.blk & (1 << 31) != 0;
        let mut dc = [0i32; 16];
        if has_dc {
            dc.copy_from_slice(&self.mb.dc[..16]);
            self.mb.dc[..16].fill(0);
            let ls00 = self.s.ls4[0][(qp % 6) as usize][0];
            luma_dc_dequant(&mut dc, qp, ls00);
        }
        for r in 0..16 {
            if self.mb.blk & (1 << r) != 0 || dc[r] != 0 {
                self.add_luma4x4(r, qp, false, Some(dc[r]));
            }
        }
    }

    /// Chroma residual (DC + AC) for both components.
    #[allow(clippy::needless_range_loop)] // indexed form over parallel fixed-size arrays (mirrors the spec loops, keeps the hot loop vectorisable)
    pub(crate) fn residual_chroma(&mut self, intra: bool) {
        let li = if intra { 0 } else { 3 };
        for comp in 0..2 {
            let qpc = self.pic.mbs[self.cur].qpc[comp] as i32;
            let mut dc = [0i32; 4];
            if self.mb.blk & (1 << (29 + comp)) != 0 {
                dc.copy_from_slice(&self.mb.dc[16 + comp * 4..20 + comp * 4]);
                self.mb.dc[16 + comp * 4..20 + comp * 4].fill(0);
                let ls00 = self.s.ls4[li + 1 + comp][(qpc % 6) as usize][0];
                chroma_dc_dequant(&mut dc, qpc, ls00);
            }
            let ls = self.s.ls4[li + 1 + comp][(qpc % 6) as usize];
            for k in 0..4 {
                let has_ac = self.mb.blk & (1 << (16 + comp * 4 + k)) != 0;
                if !has_ac && dc[k] == 0 {
                    continue;
                }
                let base = 256 + comp * 64 + k * 16;
                let mut d = [0i32; 16];
                let mut any_ac = false;
                if has_ac {
                    let coef: &mut [i32; 16] =
                        (&mut self.mb.coef[base..base + 16]).try_into().unwrap();
                    d = dequant4(coef, &ls, qpc);
                    any_ac = coef[1..].iter().fold(0, |a, &c| a | c) != 0;
                    coef.fill(0);
                }
                d[0] = dc[k];
                let g = self.geom;
                let off = g.off_c + (k >> 1) * 4 * g.stride_c + (k & 1) * 4;
                let plane = if comp == 0 {
                    &mut self.pic.buf.cb
                } else {
                    &mut self.pic.buf.cr
                };
                if any_ac {
                    idct4_add(&d, plane, off, g.stride_c);
                } else {
                    dc_add(d[0], 4, plane, off, g.stride_c);
                }
            }
        }
    }

    /// Residual of an inter macroblock added on top of the motion-compensated prediction.
    pub(crate) fn residual_inter(&mut self, qp: i32, _qpc: [i32; 2]) {
        if self.mb.t8x8 {
            for b8 in 0..4 {
                if self.mb.b8 & (1 << b8) != 0 {
                    self.add_luma8x8(b8, qp, true);
                }
            }
        } else {
            for r in 0..16 {
                if self.mb.blk & (1 << r) != 0 {
                    self.add_luma4x4(r, qp, true, None);
                }
            }
        }
        if self.s.chroma && self.mb.cbp >> 4 != 0 {
            self.residual_chroma(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scaling as C's `int` arithmetic computes it: exact products, truncated to 32 bits
    /// before the right shift.
    fn scaled(c: i32, l: i32, qp: i32, base: i32) -> i32 {
        let (p, sh) = (c as i64 * l as i64, qp / 6);
        if sh >= base {
            (p << (sh - base)) as i32
        } else {
            ((p + (1 << (base - 1 - sh))) as i32) >> (base - sh)
        }
    }

    #[test]
    fn scaling_wraps_extreme_levels() {
        // The largest level scales: weight 255 times normAdjust 25 (4x4) and 32 (8x8).
        for (seed, l) in [(1, 255 * 25), (2, 255 * 32), (3, 16)] {
            let c4: [i32; 16] = extreme_levels(seed);
            let c8: [i32; 64] = extreme_levels(seed);
            for qp in 0..=51 {
                let d4 = dequant4(&c4, &[l; 16], qp);
                let d8 = dequant8(&c8, &[l; 64], qp);
                assert!(d4.iter().zip(c4).all(|(&d, c)| d == scaled(c, l, qp, 4)));
                assert!(d8.iter().zip(c8).all(|(&d, c)| d == scaled(c, l, qp, 6)));
            }
        }
    }
}
