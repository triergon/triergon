//! Macroblock layer parsing (clause 7.3.5) for CAVLC and CABAC, and the per-macroblock
//! decoding driver (prediction + residual + bookkeeping).

use crate::DecodeError;
use crate::cabac::{CBF_CAT_OFF, off};
use crate::cavlc;
use crate::mbinfo::*;
use crate::slice::SliceType;
use crate::slicedec::SliceDecoder;
use crate::tables::*;

fn inval(s: &'static str) -> DecodeError {
    DecodeError::InvalidData(s)
}

/// Parsed macroblock syntax (scratch, reused).
pub(crate) struct MbData {
    pub kind: Kind,
    pub i16_mode: u8,
    pub cbp: u8,
    pub t8x8: bool,
    pub chroma_mode: u8,
    pub modes: [i8; 16],
    /// prev_intra_pred_mode_flag / rem per block (None = predicted).
    pub rem: [Option<u8>; 16],
    pub sub: [u8; 4],
    /// Prediction kind per partition (16x16: [0]; 16x8/8x16: [0..2]; 8x8: per 8x8, 0 = direct).
    pub part_pred: [u8; 4],
    pub ref_idx: [[i8; 4]; 2],
    /// mvd per partition slot: [list][8x8 index * 4 + sub partition].
    pub mvd: [[[i32; 2]; 16]; 2],
    /// Transform coefficient levels: luma 4x4 block r (raster) at r*16 (raster in block) or 8x8
    /// block k at k*64; Cb 4x4 k at 256 + k*16, Cr at 320 + k*16.
    pub coef: [i32; 384],
    /// Luma DC (raster of 4x4 blocks), Cb DC 16..20, Cr DC 20..24.
    pub dc: [i32; 24],
    /// Bits: luma 4x4 raster with coefficients (0..15), chroma AC 16..23.
    pub blk: u32,
    /// Luma 8x8 blocks with coefficients (transform 8x8).
    pub b8: u8,
    pub total: [u8; 24],
    pub cbf: u32,
    pub qp_delta: i32,
}

impl MbData {
    pub(crate) fn new() -> MbData {
        MbData {
            kind: Kind::None,
            i16_mode: 0,
            cbp: 0,
            t8x8: false,
            chroma_mode: 0,
            modes: [2; 16],
            rem: [None; 16],
            sub: [0; 4],
            part_pred: [0; 4],
            ref_idx: [[-1; 4]; 2],
            mvd: [[[0; 2]; 16]; 2],
            coef: [0; 384],
            dc: [0; 24],
            blk: 0,
            b8: 0,
            total: [0; 24],
            cbf: 0,
            qp_delta: 0,
        }
    }
}

/// Map I-slice mb_type (0..=25) to kind / Intra16x16 parameters.
fn set_intra_type(mb: &mut MbData, t: u32) -> Result<(), DecodeError> {
    match t {
        0 => mb.kind = Kind::I4x4,
        1..=24 => {
            mb.kind = Kind::I16x16;
            let v = t - 1;
            mb.i16_mode = (v % 4) as u8;
            let cbpc = (v / 4) % 3;
            let cbpl = if v >= 12 { 15 } else { 0 };
            mb.cbp = (cbpc << 4) as u8 | cbpl;
        }
        25 => mb.kind = Kind::IPcm,
        _ => return Err(inval("mb_type")),
    }
    Ok(())
}

impl<'a> SliceDecoder<'a> {
    fn field_mb(&self) -> bool {
        self.pic.mbs[self.cur].field
    }

    // ------------------------------------------------------------------ CABAC contexts

    pub(crate) fn decode_skip_flag_cabac(&mut self) -> bool {
        let a = self.mb_a();
        let b = self.mb_b();
        let ct = |n: Option<usize>| -> usize {
            match n {
                Some(n) => (!self.pic.mbs[n].kind.is_skip()) as usize,
                None => 0,
            }
        };
        let inc = ct(a) + ct(b);
        let o = if self.s.sh.slice_type == SliceType::B {
            off::MB_SKIP_B
        } else {
            off::MB_SKIP_P
        };
        self.cabac.as_mut().unwrap().decision(o + inc) == 1
    }

    pub(crate) fn decode_field_flag_cabac(&mut self) -> bool {
        let ct = |n: Option<usize>| -> usize {
            match n {
                Some(n) => self.pic.mbs[n].field as usize,
                None => 0,
            }
        };
        let inc = ct(self.nb.a) + ct(self.nb.b);
        self.cabac.as_mut().unwrap().decision(off::FIELD + inc) == 1
    }

    fn cabac_mb_type(&mut self) -> Result<u32, DecodeError> {
        let st = self.s.sh.slice_type;
        let a = self.mb_a();
        let b = self.mb_b();
        Ok(match st {
            SliceType::I => {
                let ct = |n: Option<usize>| match n {
                    Some(n) => !matches!(self.pic.mbs[n].kind, Kind::I4x4 | Kind::I8x8) as usize,
                    None => 0,
                };
                let inc = ct(a) + ct(b);
                self.cabac.as_mut().unwrap().mb_type_i(inc, None)
            }
            SliceType::P | SliceType::SP => self.cabac.as_mut().unwrap().mb_type_p(),
            SliceType::B => {
                let ct = |n: Option<usize>| match n {
                    Some(n) => {
                        !matches!(self.pic.mbs[n].kind, Kind::BSkip | Kind::BDirect) as usize
                    }
                    None => 0,
                };
                let inc = ct(a) + ct(b);
                self.cabac.as_mut().unwrap().mb_type_b(inc)
            }
            SliceType::SI => return Err(DecodeError::Unsupported("SI slices")),
        })
    }

    fn cabac_t8x8(&mut self) -> bool {
        let ct = |n: Option<usize>| match n {
            Some(n) => self.pic.mbs[n].t8x8 as usize,
            None => 0,
        };
        let inc = ct(self.mb_a()) + ct(self.mb_b());
        self.cabac.as_mut().unwrap().decision(off::T8X8 + inc) == 1
    }

    fn cabac_chroma_mode(&mut self) -> u8 {
        let ct = |n: Option<usize>| match n {
            Some(n) => {
                let m = &self.pic.mbs[n];
                (m.kind.is_intra() && m.kind != Kind::IPcm && m.chroma_mode != 0) as usize
            }
            None => 0,
        };
        let inc = ct(self.mb_a()) + ct(self.mb_b());
        self.cabac.as_mut().unwrap().intra_chroma_pred_mode(inc) as u8
    }

    fn cabac_cbp(&mut self) -> u8 {
        let mut cbp = 0u8;
        // Luma prefix: 4 bins, one per 8x8 block.
        for b8 in 0..4usize {
            let x = (b8 & 1) as i32 * 8;
            let y = (b8 >> 1) as i32 * 8;
            let cond = |s: &Self, xn: i32, yn: i32, cbp: u8| -> usize {
                match s.loc(xn, yn, 16, 16) {
                    None => 0,
                    Some((n, xw, yw)) => {
                        let nb8 = ((yw / 8) * 2 + xw / 8) as u8;
                        if n == s.cur {
                            ((cbp >> nb8) & 1 == 0) as usize
                        } else {
                            let m = &s.pic.mbs[n];
                            if m.kind == Kind::IPcm {
                                0
                            } else if m.kind.is_skip() {
                                1
                            } else {
                                ((m.cbp >> nb8) & 1 == 0) as usize
                            }
                        }
                    }
                }
            };
            let ca = cond(self, x - 1, y, cbp);
            let cb = cond(self, x, y - 1, cbp);
            let bin = self
                .cabac
                .as_mut()
                .unwrap()
                .decision(off::CBP_LUMA + ca + 2 * cb);
            cbp |= (bin as u8) << b8;
        }
        if self.s.chroma {
            let a = self.mb_a();
            let b = self.mb_b();
            let cond = |s: &Self, n: Option<usize>, bin: u32| -> usize {
                match n {
                    None => 0,
                    Some(n) => {
                        let m = &s.pic.mbs[n];
                        if m.kind == Kind::IPcm {
                            1
                        } else if m.kind.is_skip() {
                            0
                        } else {
                            let c = m.cbp >> 4;
                            (if bin == 0 { c != 0 } else { c == 2 }) as usize
                        }
                    }
                }
            };
            let inc = cond(self, a, 0) + 2 * cond(self, b, 0);
            if self.cabac.as_mut().unwrap().decision(off::CBP_CHROMA + inc) == 1 {
                let inc = cond(self, a, 1) + 2 * cond(self, b, 1) + 4;
                let v = 1 + self.cabac.as_mut().unwrap().decision(off::CBP_CHROMA + inc) as u8;
                cbp |= v << 4;
            }
        }
        cbp
    }

    fn cabac_qp_delta(&mut self) -> i32 {
        let inc = self.prev_qp_delta_nz as usize;
        self.cabac.as_mut().unwrap().mb_qp_delta(inc)
    }

    /// condTermFlag for coded_block_flag given the neighbour MB and its cbf bit.
    #[inline]
    fn cbf_cond(&self, n: Option<usize>, bit: u32, cur_intra: bool) -> usize {
        match n {
            None => cur_intra as usize,
            Some(n) => {
                let m = &self.pic.mbs[n];
                if m.kind == Kind::IPcm {
                    1
                } else {
                    ((m.cbf & bit) != 0) as usize
                }
            }
        }
    }

    /// coded_block_flag ctxIdxInc for luma 4x4 block at raster (bx, by) (cat 1, 2) using cbf of
    /// the current MB (`cur_cbf`) for in-MB neighbours.
    fn cbf_inc_luma(&self, bx: i32, by: i32, cur_cbf: u32, intra: bool) -> usize {
        let mut inc = 0;
        for (k, (xn, yn)) in [(bx * 4 - 1, by * 4), (bx * 4, by * 4 - 1)]
            .into_iter()
            .enumerate()
        {
            let c = match self.loc(xn, yn, 16, 16) {
                None => intra as usize,
                Some((n, xw, yw)) => {
                    let bit = 1u32 << ((yw / 4) * 4 + xw / 4);
                    if n == self.cur {
                        ((cur_cbf & bit) != 0) as usize
                    } else {
                        self.cbf_cond(Some(n), bit, intra)
                    }
                }
            };
            inc += c << k;
        }
        inc
    }

    fn cbf_inc_chroma_ac(&self, comp: usize, bx: i32, by: i32, cur_cbf: u32, intra: bool) -> usize {
        let mut inc = 0;
        for (k, (xn, yn)) in [(bx * 4 - 1, by * 4), (bx * 4, by * 4 - 1)]
            .into_iter()
            .enumerate()
        {
            let c = match self.loc(xn, yn, 8, 8) {
                None => intra as usize,
                Some((n, xw, yw)) => {
                    let bit = 1u32 << (16 + comp * 4 + ((yw / 4) * 2 + xw / 4) as usize);
                    if n == self.cur {
                        ((cur_cbf & bit) != 0) as usize
                    } else {
                        self.cbf_cond(Some(n), bit, intra)
                    }
                }
            };
            inc += c << k;
        }
        inc
    }

    fn cbf_inc_mb(&self, bit: u32, intra: bool) -> usize {
        self.cbf_cond(self.mb_a(), bit, intra) + 2 * self.cbf_cond(self.mb_b(), bit, intra)
    }

    // ------------------------------------------------------------------ CAVLC nC

    /// nC for a luma 4x4 block (raster bx, by), using current-MB totals for in-MB neighbours.
    fn nc_luma(&self, bx: i32, by: i32) -> i32 {
        if bx > 0 && by > 0 {
            let r = (by * 4 + bx) as usize;
            return (self.mb.total[r - 1] as i32 + self.mb.total[r - 4] as i32 + 1) >> 1;
        }
        let get = |xn: i32, yn: i32| -> Option<i32> {
            let (n, xw, yw) = self.loc(xn, yn, 16, 16)?;
            let r = ((yw / 4) * 4 + xw / 4) as usize;
            if n == self.cur {
                Some(self.mb.total[r] as i32)
            } else {
                Some(self.pic.mbs[n].nz[r] as i32)
            }
        };
        nc_combine(get(bx * 4 - 1, by * 4), get(bx * 4, by * 4 - 1))
    }

    fn nc_chroma(&self, comp: usize, bx: i32, by: i32) -> i32 {
        let get = |xn: i32, yn: i32| -> Option<i32> {
            let (n, xw, yw) = self.loc(xn, yn, 8, 8)?;
            let r = 16 + comp * 4 + ((yw / 4) * 2 + xw / 4) as usize;
            if n == self.cur {
                Some(self.mb.total[r] as i32)
            } else {
                Some(self.pic.mbs[n].nz[r] as i32)
            }
        };
        nc_combine(get(bx * 4 - 1, by * 4), get(bx * 4, by * 4 - 1))
    }

    // ------------------------------------------------------------------ MB layer

    /// Decode a non-skipped macroblock (macroblock_layer()).
    pub(crate) fn decode_coded_mb(&mut self) -> Result<(), DecodeError> {
        let st = self.s.sh.slice_type;
        let cabac = self.s.cabac;
        let mb = &mut self.mb;
        mb.kind = Kind::None;
        mb.cbp = 0;
        mb.t8x8 = false;
        mb.chroma_mode = 0;
        mb.blk = 0;
        mb.b8 = 0;
        mb.total = [0; 24];
        mb.cbf = 0;
        mb.qp_delta = 0;
        mb.ref_idx = [[-1; 4]; 2];
        mb.mvd = [[[0; 2]; 16]; 2];
        mb.part_pred = [0; 4];
        mb.modes = [2; 16];
        mb.rem = [None; 16];
        let mb_type = if cabac {
            self.cabac_mb_type()?
        } else {
            self.br.ue()
        };
        // Map mb_type into kinds.
        match st {
            SliceType::I => set_intra_type(&mut self.mb, mb_type)?,
            SliceType::SI => return Err(DecodeError::Unsupported("SI slices")),
            SliceType::P | SliceType::SP => match mb_type {
                0 => {
                    self.mb.kind = Kind::P16x16;
                    self.mb.part_pred[0] = PRED_L0;
                }
                1 => {
                    self.mb.kind = Kind::P16x8;
                    self.mb.part_pred = [PRED_L0, PRED_L0, 0, 0];
                }
                2 => {
                    self.mb.kind = Kind::P8x16;
                    self.mb.part_pred = [PRED_L0, PRED_L0, 0, 0];
                }
                3 | 4 => {
                    self.mb.kind = Kind::P8x8;
                    self.mb.part_pred = [PRED_L0; 4];
                }
                5..=30 => set_intra_type(&mut self.mb, mb_type - 5)?,
                _ => return Err(inval("P mb_type")),
            },
            SliceType::B => match mb_type {
                0 => self.mb.kind = Kind::BDirect,
                1..=21 => {
                    let (shape, p0, p1) = B_MB_TYPES[mb_type as usize];
                    self.mb.kind = match shape {
                        0 => Kind::B16x16,
                        1 => Kind::B16x8,
                        _ => Kind::B8x16,
                    };
                    self.mb.part_pred = [p0, p1, 0, 0];
                }
                22 => self.mb.kind = Kind::B8x8,
                23..=48 => set_intra_type(&mut self.mb, mb_type - 23)?,
                _ => return Err(inval("B mb_type")),
            },
        }
        let p8x8ref0 = matches!(st, SliceType::P | SliceType::SP) && mb_type == 4;
        if self.mb.kind == Kind::IPcm {
            return self.decode_pcm();
        }
        let kind = self.mb.kind;
        let mut transform_size_8x8_flag = false;
        let mut no_sub_8x8_smaller = true;
        if matches!(kind, Kind::P8x8 | Kind::B8x8) {
            // sub_mb_pred()
            for i in 0..4 {
                let v = if cabac {
                    let c = self.cabac.as_mut().unwrap();
                    if kind == Kind::P8x8 {
                        c.sub_mb_type_p()
                    } else {
                        c.sub_mb_type_b()
                    }
                } else {
                    self.br.ue()
                };
                if (kind == Kind::P8x8 && v > 3) || v > 12 {
                    return Err(inval("sub_mb_type"));
                }
                self.mb.sub[i] = v as u8;
                if kind == Kind::P8x8 {
                    self.mb.part_pred[i] = PRED_L0;
                    if v != 0 {
                        no_sub_8x8_smaller = false;
                    }
                } else {
                    let (shape, pred) = B_SUB_TYPES[v as usize];
                    self.mb.part_pred[i] = pred;
                    if v == 0 {
                        if !self.s.direct_8x8_inference {
                            no_sub_8x8_smaller = false;
                        }
                    } else if shape != 0 {
                        no_sub_8x8_smaller = false;
                    }
                }
            }
            self.parse_ref_mvd_8x8(p8x8ref0)?;
        } else if kind.is_intra() {
            // mb_pred() intra
            if kind == Kind::I4x4 && self.s.t8x8_mode {
                transform_size_8x8_flag = if cabac {
                    self.cabac_t8x8()
                } else {
                    self.br.flag()
                };
                if transform_size_8x8_flag {
                    self.mb.kind = Kind::I8x8;
                }
            }
            let kind = self.mb.kind;
            if kind == Kind::I4x4 {
                for b in 0..16 {
                    self.mb.rem[b] = self.read_intra_mode()?;
                }
            } else if kind == Kind::I8x8 {
                for b in 0..4 {
                    self.mb.rem[b] = self.read_intra_mode()?;
                }
            }
            if self.s.chroma {
                let m = if cabac {
                    self.cabac_chroma_mode()
                } else {
                    self.br.ue() as u8
                };
                if m > 3 {
                    return Err(inval("intra_chroma_pred_mode"));
                }
                self.mb.chroma_mode = m;
            }
        } else if kind != Kind::BDirect {
            self.parse_ref_mvd_parts()?;
        }
        let kind = self.mb.kind;
        if kind != Kind::I16x16 {
            let cbp = if cabac {
                self.cabac_cbp()
            } else {
                let code = self.br.ue();
                let intra = kind.is_intra();
                if self.s.chroma {
                    if code > 47 {
                        return Err(inval("coded_block_pattern"));
                    }
                    CBP_420[code as usize][(!intra) as usize]
                } else {
                    if code > 15 {
                        return Err(inval("coded_block_pattern"));
                    }
                    CBP_400[code as usize][(!intra) as usize]
                }
            };
            self.mb.cbp = cbp;
            if self.mb.cbp & 15 != 0
                && self.s.t8x8_mode
                && !kind.is_intra()
                && (kind != Kind::BDirect || self.s.direct_8x8_inference)
                && no_sub_8x8_smaller
            {
                transform_size_8x8_flag = if cabac {
                    self.cabac_t8x8()
                } else {
                    self.br.flag()
                };
            }
        }
        self.mb.t8x8 = transform_size_8x8_flag;
        if self.mb.cbp != 0 || kind == Kind::I16x16 {
            let dq = if cabac {
                self.cabac_qp_delta()
            } else {
                self.br.se()
            };
            if !(-26..=25).contains(&dq) {
                return Err(inval("mb_qp_delta"));
            }
            self.mb.qp_delta = dq;
            self.qp = (self.qp + dq + 52) % 52;
            self.prev_qp_delta_nz = dq != 0;
            self.parse_residual()?;
        } else {
            self.prev_qp_delta_nz = false;
        }
        self.reconstruct_mb()
    }

    fn read_intra_mode(&mut self) -> Result<Option<u8>, DecodeError> {
        if self.s.cabac {
            Ok(self
                .cabac
                .as_mut()
                .unwrap()
                .intra_pred_mode()
                .map(|v| v as u8))
        } else if self.br.flag() {
            Ok(None)
        } else {
            Ok(Some(self.br.u(3) as u8))
        }
    }

    /// ref_idx / mvd for 16x16, 16x8, 8x16 partitions (mb_pred()).
    fn parse_ref_mvd_parts(&mut self) -> Result<(), DecodeError> {
        let nparts = match self.mb.kind {
            Kind::P16x16 | Kind::B16x16 => 1,
            _ => 2,
        };
        let field_dbl = self.pic.mbaff && self.field_mb();
        for list in 0..2 {
            let nref = self.s.sh.num_ref_idx_active[list] as i32 * (1 + field_dbl as i32);
            for p in 0..nparts {
                let pred = self.mb.part_pred[p];
                if pred & (1 << list) == 0 {
                    continue;
                }
                let r = if nref > 1 || (self.pic.mbaff && self.field_mb() && nref > 1) {
                    if self.s.cabac {
                        let inc = self.ref_idx_inc(list, p);
                        self.cabac.as_mut().unwrap().ref_idx(inc) as i32
                    } else {
                        self.br.te((nref - 1) as u32) as i32
                    }
                } else {
                    0
                };
                if r >= nref {
                    return Err(inval("ref_idx"));
                }
                let r8 = self.part_8x8s(p);
                for &k in r8 {
                    self.mb.ref_idx[list][k] = r as i8;
                }
            }
        }
        for list in 0..2 {
            for p in 0..nparts {
                if self.mb.part_pred[p] & (1 << list) == 0 {
                    continue;
                }
                let (x, y, w, h) = self.part_rect(p);
                let mvd = self.read_mvd(list, x, y)?;
                // Store mvd for all 4x4 blocks of the partition.
                for by in y / 4..(y + h) / 4 {
                    for bx in x / 4..(x + w) / 4 {
                        self.mb.mvd[list][(by * 4 + bx) as usize] = mvd;
                    }
                }
            }
        }
        Ok(())
    }

    /// 8x8 indices covered by partition p of the current (non-8x8) MB type.
    fn part_8x8s(&self, p: usize) -> &'static [usize] {
        match self.mb.kind {
            Kind::P16x16 | Kind::B16x16 => &[0, 1, 2, 3],
            Kind::P16x8 | Kind::B16x8 => {
                if p == 0 {
                    &[0, 1]
                } else {
                    &[2, 3]
                }
            }
            _ => {
                if p == 0 {
                    &[0, 2]
                } else {
                    &[1, 3]
                }
            }
        }
    }

    /// Partition rectangle (x, y, w, h) in luma samples for 16x16/16x8/8x16 types.
    pub(crate) fn part_rect(&self, p: usize) -> (i32, i32, i32, i32) {
        match self.mb.kind {
            Kind::P16x8 | Kind::B16x8 => (0, p as i32 * 8, 16, 8),
            Kind::P8x16 | Kind::B8x16 => (p as i32 * 8, 0, 8, 16),
            _ => (0, 0, 16, 16),
        }
    }

    /// ref_idx / mvd for P_8x8 / B_8x8 (sub_mb_pred()).
    fn parse_ref_mvd_8x8(&mut self, p8x8ref0: bool) -> Result<(), DecodeError> {
        let field_dbl = self.pic.mbaff && self.field_mb();
        let is_b = self.mb.kind == Kind::B8x8;
        for list in 0..2 {
            let nref = self.s.sh.num_ref_idx_active[list] as i32 * (1 + field_dbl as i32);
            for i in 0..4 {
                if is_b && self.mb.sub[i] == 0 {
                    continue;
                }
                if self.mb.part_pred[i] & (1 << list) == 0 {
                    continue;
                }
                let r = if nref > 1 && !p8x8ref0 {
                    if self.s.cabac {
                        let inc = self.ref_idx_inc_8x8(list, i);
                        self.cabac.as_mut().unwrap().ref_idx(inc) as i32
                    } else {
                        self.br.te((nref - 1) as u32) as i32
                    }
                } else {
                    0
                };
                if r >= nref {
                    return Err(inval("ref_idx"));
                }
                self.mb.ref_idx[list][i] = r as i8;
            }
        }
        for list in 0..2 {
            for i in 0..4 {
                if is_b && self.mb.sub[i] == 0 {
                    continue;
                }
                if self.mb.part_pred[i] & (1 << list) == 0 {
                    continue;
                }
                let x0 = (i as i32 & 1) * 8;
                let y0 = (i as i32 >> 1) * 8;
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
                for s in 0..SUB_PARTS[shape as usize] {
                    let (sx, sy) = match shape {
                        0 => (0, 0),
                        1 => (0, s as i32 * 4),
                        2 => (s as i32 * 4, 0),
                        _ => ((s as i32 & 1) * 4, (s as i32 >> 1) * 4),
                    };
                    let x = x0 + sx;
                    let y = y0 + sy;
                    let mvd = self.read_mvd(list, x, y)?;
                    for by in y / 4..(y + sh_) / 4 {
                        for bx in x / 4..(x + sw) / 4 {
                            self.mb.mvd[list][(by * 4 + bx) as usize] = mvd;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// ref_idx ctxIdxInc for partition p (16x16/16x8/8x16).
    fn ref_idx_inc(&self, list: usize, p: usize) -> usize {
        let (x, y, _, _) = self.part_rect(p);
        self.ref_idx_inc_at(list, x, y)
    }

    fn ref_idx_inc_8x8(&self, list: usize, i: usize) -> usize {
        self.ref_idx_inc_at(list, (i as i32 & 1) * 8, (i as i32 >> 1) * 8)
    }

    fn ref_idx_inc_at(&self, list: usize, x: i32, y: i32) -> usize {
        let cur_frame = !self.field_mb();
        let mut inc = 0;
        for (k, (xn, yn)) in [(x - 1, y), (x, y - 1)].into_iter().enumerate() {
            let c = match self.loc(xn, yn, 16, 16) {
                None => 0,
                Some((n, xw, yw)) => {
                    let b8 = ((yw / 8) * 2 + xw / 8) as usize;
                    let (r, direct, skip_or_intra, nfield) = if n == self.cur {
                        (
                            self.mb.ref_idx[list][b8] as i32,
                            self.mb.kind == Kind::B8x8 && self.mb.sub[b8] == 0,
                            false,
                            !cur_frame,
                        )
                    } else {
                        let m = &self.pic.mbs[n];
                        (
                            self.pic.motion.ref_idx[list][n * 4 + b8] as i32,
                            m.direct8 & (1 << b8) != 0,
                            m.kind.is_skip() || m.kind.is_intra(),
                            m.field,
                        )
                    };
                    if skip_or_intra || direct || r < 0 {
                        0
                    } else {
                        let thr = if self.pic.mbaff && cur_frame && nfield {
                            1
                        } else {
                            0
                        };
                        (r > thr) as usize
                    }
                }
            };
            inc += c << k;
        }
        inc
    }

    /// Read mvd_lX for the (sub-)partition with top-left (x, y) (CAVLC se(v) / CABAC UEG3).
    fn read_mvd(&mut self, list: usize, x: i32, y: i32) -> Result<[i32; 2], DecodeError> {
        if !self.s.cabac {
            let a = self.br.se();
            let b = self.br.se();
            return Ok([a, b]);
        }
        let mut out = [0i32; 2];
        for comp in 0..2 {
            let mut sum = 0i32;
            for (xn, yn) in [(x - 1, y), (x, y - 1)] {
                if let Some((n, xw, yw)) = self.loc(xn, yn, 16, 16) {
                    let r = ((yw / 4) * 4 + xw / 4) as usize;
                    let (v, nfield) = if n == self.cur {
                        (
                            self.mb.mvd[list][r][comp].unsigned_abs().min(127) as i32,
                            self.field_mb(),
                        )
                    } else {
                        let m = &self.pic.mbs[n];
                        (m.mvd[list][r][comp] as i32, m.field)
                    };
                    let mut v = v;
                    if comp == 1 && self.pic.mbaff {
                        let cf = self.field_mb();
                        if !cf && nfield {
                            v *= 2;
                        } else if cf && !nfield {
                            v /= 2;
                        }
                    }
                    sum += v;
                }
            }
            let inc = if sum > 32 {
                2
            } else if sum > 2 {
                1
            } else {
                0
            };
            let o = if comp == 0 { off::MVD_X } else { off::MVD_Y };
            out[comp] = self.cabac.as_mut().unwrap().mvd(o, inc);
        }
        Ok(out)
    }

    // ------------------------------------------------------------------ residual

    #[inline(never)]
    fn parse_residual(&mut self) -> Result<(), DecodeError> {
        let kind = self.mb.kind;
        let cbp = self.mb.cbp;
        let field = self.field_mb();
        let scan4: &[u8; 16] = if field { &FIELD4 } else { &ZIGZAG4 };
        let scan8: &[u8; 64] = if field { &FIELD8 } else { &ZIGZAG8 };
        let cabac = self.s.cabac;
        let intra = kind.is_intra();
        let mut levels = [0i32; 64];
        if kind == Kind::I16x16 {
            levels[..16].fill(0);
            let n = if cabac {
                let inc = self.cbf_inc_mb(CBF_LUMA_DC, true);
                let c = self.cabac.as_mut().unwrap();
                let n = if c.decision(off::CBF + CBF_CAT_OFF[0] + inc) == 1 {
                    self.mb.cbf |= CBF_LUMA_DC;
                    c.residual(0, field, 16, &mut levels)
                } else {
                    0
                };
                if n > 0 {
                    for k in 0..16 {
                        self.mb.dc[scan4[k] as usize] = levels[k];
                    }
                }
                n
            } else {
                let nc = self.nc_luma(0, 0);
                cavlc::residual_block(
                    &mut self.br,
                    self.vlc,
                    nc,
                    &mut self.mb.dc[..16],
                    scan4,
                    0,
                    15,
                    16,
                )
                .ok_or(inval("Intra16x16DCLevel"))?
            };
            if n > 0 {
                self.mb.blk |= 1 << 31; // luma DC present marker
            }
        }
        for b8 in 0..4usize {
            if cbp & (1 << b8) == 0 {
                continue;
            }
            if self.mb.t8x8 && cabac {
                levels.fill(0);
                let c = self.cabac.as_mut().unwrap();
                let n = c.residual(5, field, 64, &mut levels);
                let base = b8 * 64;
                for k in 0..64 {
                    self.mb.coef[base + scan8[k] as usize] = levels[k];
                }
                if n > 0 {
                    self.mb.b8 |= 1 << b8;
                }
                let bx = (b8 & 1) * 2;
                let by = (b8 >> 1) * 2;
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let r = (by + dy) * 4 + bx + dx;
                    self.mb.total[r] = n as u8;
                    self.mb.cbf |= 1 << r;
                }
                continue;
            }
            for i4 in 0..4usize {
                let blk_idx = b8 * 4 + i4;
                let r = BLK_TO_RASTER[blk_idx] as usize;
                let bx = (r % 4) as i32;
                let by = (r / 4) as i32;
                if cabac {
                    levels[..16].fill(0);
                    let max = if kind == Kind::I16x16 { 15 } else { 16 };
                    let cat = if kind == Kind::I16x16 { 1 } else { 2 };
                    let inc = self.cbf_inc_luma(bx, by, self.mb.cbf, intra);
                    let c = self.cabac.as_mut().unwrap();
                    let n = if c.decision(off::CBF + CBF_CAT_OFF[cat] + inc) == 1 {
                        self.mb.cbf |= 1 << r;
                        c.residual(cat, field, max, &mut levels)
                    } else {
                        0
                    };
                    self.mb.total[r] = n as u8;
                    if n == 0 {
                        continue;
                    }
                    let base = r * 16;
                    if kind == Kind::I16x16 {
                        for k in 0..15 {
                            self.mb.coef[base + scan4[k + 1] as usize] = levels[k];
                        }
                    } else {
                        for k in 0..16 {
                            self.mb.coef[base + scan4[k] as usize] = levels[k];
                        }
                    }
                    self.mb.blk |= 1 << r;
                    continue;
                }
                let nc = self.nc_luma(bx, by);
                let n = if self.mb.t8x8 {
                    // CAVLC 8x8: interleaved 4x4 blocks (7.3.5.3.2).
                    let map: &[u8; 16] = if field { &FIELD8_I[i4] } else { &ZIGZAG8_I[i4] };
                    let base = b8 * 64;
                    cavlc::residual_block(
                        &mut self.br,
                        self.vlc,
                        nc,
                        &mut self.mb.coef[base..base + 64],
                        map,
                        0,
                        15,
                        16,
                    )
                    .ok_or(inval("residual_block"))?
                } else if kind == Kind::I16x16 {
                    let base = r * 16;
                    cavlc::residual_block(
                        &mut self.br,
                        self.vlc,
                        nc,
                        &mut self.mb.coef[base..base + 16],
                        &scan4[1..],
                        0,
                        14,
                        15,
                    )
                    .ok_or(inval("residual_block"))?
                } else {
                    let base = r * 16;
                    cavlc::residual_block(
                        &mut self.br,
                        self.vlc,
                        nc,
                        &mut self.mb.coef[base..base + 16],
                        scan4,
                        0,
                        15,
                        16,
                    )
                    .ok_or(inval("residual_block"))?
                };
                self.mb.total[r] = n as u8;
                if n == 0 {
                    continue;
                }
                if self.mb.t8x8 {
                    self.mb.b8 |= 1 << b8;
                } else {
                    self.mb.blk |= 1 << r;
                }
            }
        }
        if self.s.chroma && cbp >> 4 != 0 {
            let cbpc = cbp >> 4;
            for comp in 0..2usize {
                levels[..4].fill(0);
                let n = if cabac {
                    let bit = if comp == 0 { CBF_CB_DC } else { CBF_CR_DC };
                    let inc = self.cbf_inc_mb(bit, intra);
                    let c = self.cabac.as_mut().unwrap();
                    if c.decision(off::CBF + CBF_CAT_OFF[3] + inc) == 1 {
                        self.mb.cbf |= bit;
                        c.residual(3, field, 4, &mut levels)
                    } else {
                        0
                    }
                } else {
                    let o = 16 + comp * 4;
                    let n = cavlc::residual_block(
                        &mut self.br,
                        self.vlc,
                        -1,
                        &mut levels[..4],
                        &IDENT4,
                        0,
                        3,
                        4,
                    )
                    .ok_or(inval("ChromaDCLevel"))?;
                    let _ = o;
                    n
                };
                if n > 0 {
                    self.mb.dc[16 + comp * 4..20 + comp * 4].copy_from_slice(&levels[..4]);
                    self.mb.blk |= 1 << (29 + comp);
                }
            }
            if cbpc == 2 {
                for comp in 0..2usize {
                    for k in 0..4usize {
                        let bx = (k & 1) as i32;
                        let by = (k >> 1) as i32;
                        let base = 256 + comp * 64 + k * 16;
                        let n = if cabac {
                            levels[..16].fill(0);
                            let inc = self.cbf_inc_chroma_ac(comp, bx, by, self.mb.cbf, intra);
                            let c = self.cabac.as_mut().unwrap();
                            let n = if c.decision(off::CBF + CBF_CAT_OFF[4] + inc) == 1 {
                                self.mb.cbf |= 1 << (16 + comp * 4 + k);
                                c.residual(4, field, 15, &mut levels)
                            } else {
                                0
                            };
                            if n > 0 {
                                for j in 0..15 {
                                    self.mb.coef[base + scan4[j + 1] as usize] = levels[j];
                                }
                            }
                            n
                        } else {
                            let nc = self.nc_chroma(comp, bx, by);
                            cavlc::residual_block(
                                &mut self.br,
                                self.vlc,
                                nc,
                                &mut self.mb.coef[base..base + 16],
                                &scan4[1..],
                                0,
                                14,
                                15,
                            )
                            .ok_or(inval("ChromaACLevel"))?
                        };
                        self.mb.total[16 + comp * 4 + k] = n as u8;
                        if n > 0 {
                            self.mb.blk |= 1 << (16 + comp * 4 + k);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ I_PCM

    fn decode_pcm(&mut self) -> Result<(), DecodeError> {
        let g = self.geom_of(self.cur);
        self.geom = g;
        let mut samples = [0u8; 384];
        let n = if self.s.chroma { 384 } else { 256 };
        if self.s.cabac {
            let c = self.cabac.as_mut().unwrap();
            let pos = c.pcm_byte_pos();
            let data = self.br.data();
            if pos + n > self.br.len_bits() / 8 + 2 {
                return Err(inval("I_PCM truncated"));
            }
            samples[..n].copy_from_slice(&data[pos..pos + n]);
            c.reinit_at(pos + n);
        } else {
            self.br.align();
            for v in samples[..n].iter_mut() {
                *v = self.br.u(8) as u8;
            }
        }
        let buf = &mut self.pic.buf;
        for y in 0..16 {
            let o = g.off_y + y * g.stride_y;
            buf.y[o..o + 16].copy_from_slice(&samples[y * 16..y * 16 + 16]);
        }
        if self.s.chroma {
            for y in 0..8 {
                let o = g.off_c + y * g.stride_c;
                buf.cb[o..o + 8].copy_from_slice(&samples[256 + y * 8..256 + y * 8 + 8]);
                buf.cr[o..o + 8].copy_from_slice(&samples[320 + y * 8..320 + y * 8 + 8]);
            }
        }
        // mb_qp_delta is not present: QPY unchanged; I_PCM uses qP = 0 for deblocking.
        self.prev_qp_delta_nz = false;
        let cur = self.cur;
        let qpc0 = [self.qpc_for(0, 0) as i8, self.qpc_for(0, 1) as i8];
        let info = &mut self.pic.mbs[cur];
        info.kind = Kind::IPcm;
        info.cbp = 0x2F;
        info.qp = 0;
        info.qpc = qpc0;
        info.nz = [16; 24];
        info.cbf = CBF_ALL;
        info.modes = [2; 16];
        info.nzmask = 0xFFFF;
        info.chroma_mode = 0;
        self.mark_intra_motion();
        Ok(())
    }

    pub(crate) fn qpc_for(&self, qp: i32, comp: usize) -> i32 {
        let qpi = (qp + self.s.chroma_qp_off[comp]).clamp(0, 51);
        QPC_TABLE[qpi as usize] as i32
    }

    /// Store "intra" in the motion arrays for the current MB.
    pub(crate) fn mark_intra_motion(&mut self) {
        let cur = self.cur;
        let m = &mut self.pic.motion;
        for l in 0..2 {
            m.mv[l][cur * 16..cur * 16 + 16].fill([0, 0]);
            m.ref_idx[l][cur * 4..cur * 4 + 4].fill(-1);
            m.ref_id[l][cur * 4..cur * 4 + 4].fill(-1);
        }
        m.mb_flags[cur] = crate::picture::MBF_INTRA
            | if self.pic.mbs[cur].field {
                crate::picture::MBF_FIELD
            } else {
                0
            };
    }

    // ------------------------------------------------------------------ reconstruction

    fn reconstruct_mb(&mut self) -> Result<(), DecodeError> {
        let cur = self.cur;
        self.geom = self.geom_of(cur);
        let kind = self.mb.kind;
        let qp = self.qp;
        {
            let info = &mut self.pic.mbs[cur];
            info.kind = kind;
            info.t8x8 = self.mb.t8x8;
            info.cbp = self.mb.cbp;
            info.qp = qp as i8;
            info.chroma_mode = self.mb.chroma_mode;
            info.nz = self.mb.total;
            info.cbf = self.mb.cbf;
        }
        let qpc = [self.qpc_for(qp, 0), self.qpc_for(qp, 1)];
        self.pic.mbs[cur].qpc = [qpc[0] as i8, qpc[1] as i8];
        if kind.is_intra() {
            self.mark_intra_motion();
            self.pic.mbs[cur].mvd = [[[0; 2]; 16]; 2];
            self.intra_mb()?;
        } else {
            // Store |mvd| for CABAC contexts (only CABAC reads them).
            if self.s.cabac {
                let mut mvd = [[[0u8; 2]; 16]; 2];
                for l in 0..2 {
                    for b in 0..16 {
                        for c in 0..2 {
                            mvd[l][b][c] = self.mb.mvd[l][b][c].unsigned_abs().min(127) as u8;
                        }
                    }
                }
                self.pic.mbs[cur].mvd = mvd;
            }
            self.inter_mb()?;
            self.residual_inter(qp, qpc);
        }
        self.finish_nzmask();
        Ok(())
    }

    #[allow(clippy::needless_range_loop)] // indexed form over parallel fixed-size arrays (mirrors the spec loops, keeps the hot loop vectorisable)
    fn finish_nzmask(&mut self) {
        let cur = self.cur;
        let mut m: u16 = 0;
        if self.mb.t8x8 {
            for b8 in 0..4 {
                let bx = (b8 & 1) * 2;
                let by = (b8 >> 1) * 2;
                let any =
                    (0..2).any(|dy| (0..2).any(|dx| self.mb.total[(by + dy) * 4 + bx + dx] != 0));
                if any {
                    for dy in 0..2 {
                        for dx in 0..2 {
                            m |= 1 << ((by + dy) * 4 + bx + dx);
                        }
                    }
                }
            }
        } else {
            for r in 0..16 {
                if self.mb.total[r] != 0 {
                    m |= 1 << r;
                }
            }
        }
        self.pic.mbs[cur].nzmask = m;
    }

    /// Decode a skipped macroblock (P_Skip / B_Skip).
    pub(crate) fn decode_skip_mb(&mut self) -> Result<(), DecodeError> {
        let cur = self.cur;
        let is_b = self.s.sh.slice_type == SliceType::B;
        let kind = if is_b { Kind::BSkip } else { Kind::PSkip };
        let mb = &mut self.mb;
        mb.kind = kind;
        mb.cbp = 0;
        mb.t8x8 = false;
        mb.blk = 0;
        mb.b8 = 0;
        mb.total = [0; 24];
        mb.cbf = 0;
        mb.ref_idx = [[-1; 4]; 2];
        mb.mvd = [[[0; 2]; 16]; 2];
        mb.part_pred = [0; 4];
        self.geom = self.geom_of(cur);
        self.prev_qp_delta_nz = false;
        let qp = self.qp;
        {
            let info = &mut self.pic.mbs[cur];
            info.kind = kind;
            info.qp = qp as i8;
        }
        let qpc = [self.qpc_for(qp, 0), self.qpc_for(qp, 1)];
        self.pic.mbs[cur].qpc = [qpc[0] as i8, qpc[1] as i8];
        self.inter_mb()?;
        self.pic.mbs[cur].nzmask = 0;
        Ok(())
    }
}

#[inline]
fn nc_combine(a: Option<i32>, b: Option<i32>) -> i32 {
    match (a, b) {
        (Some(a), Some(b)) => (a + b + 1) >> 1,
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => 0,
    }
}
