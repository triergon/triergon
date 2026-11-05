//! Slice segment data decoding (clause 7.3.8): CTU / coding quadtree / coding unit /
//! prediction unit / transform tree parsing with CABAC (clause 9.3), and the per-block
//! reconstruction (intra 8.4, inter 8.5, scaling/transform 8.6).

use crate::bits::BitReader;
use crate::cabac::{Cabac, ctx, init_contexts};
use crate::error::{DecodeError, Result};
use crate::inter::{self, McScratch, PRED_STRIDE};
use crate::intra::{self, MAX_REF};
use crate::picture::{ColMotion, Frame, Mv, Pixel, PuMotion};
use crate::ps::{Layout, Pps, Sps};
use crate::slice::{SLICE_B, SLICE_I, SLICE_P, SliceHeader};
use crate::state::*;
use crate::tables::{
    CTX_IDX_MAP_SCAN, LEVEL_SCALE, SCAN_INV_4X4, SCAN_POS_4X4, SIG_CTX_SCAN, qpc_from_qpi,
    sub_block_scan, sub_block_scan_inv,
};
use crate::transform;

/// Z-order (Morton) index of 4x4 units inside a 64x64 CTB, [y4 * 16 + x4] (6.5.2 within a CTB).
const Z4: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut y = 0;
    while y < 16 {
        let mut x = 0;
        while x < 16 {
            let mut z = 0usize;
            let mut b = 0;
            while b < 4 {
                z |= (((x >> b) & 1) << (2 * b)) | (((y >> b) & 1) << (2 * b + 1));
                b += 1;
            }
            t[y * 16 + x] = z as u8;
            x += 1;
        }
        y += 1;
    }
    t
};

pub const PART_2NX2N: u8 = 0;
pub const PART_2NXN: u8 = 1;
pub const PART_NX2N: u8 = 2;
pub const PART_NXN: u8 = 3;
pub const PART_2NXNU: u8 = 4;
pub const PART_2NXND: u8 = 5;
pub const PART_NLX2N: u8 = 6;
pub const PART_NRX2N: u8 = 7;

#[derive(Clone, Copy)]
pub struct RefEntry<'a, P> {
    pub frame: Option<&'a Frame<P>>,
    pub poc: i32,
    pub lt: bool,
}

pub struct RefLists<'a, P> {
    pub list: [[RefEntry<'a, P>; 16]; 2],
    pub num: [usize; 2],
    pub col: Option<&'a ColMotion>,
    pub col_poc: i32,
    pub no_backward_pred: bool,
}

pub struct Scratch<P> {
    pub coeffs: Vec<i32>,
    pub res: Vec<i32>,
    pub pred: [Vec<i16>; 2],
    pub mc: McScratch<P>,
    pub tr: transform::TransformScratch,
    /// SAO source band (deblocked samples of one CTB row plus a line above and below) and
    /// the saved last deblocked line of the previous row, per component.
    pub band: Option<Frame<P>>,
    pub saved: [Vec<P>; 3],
}

impl<P: Pixel> Scratch<P> {
    pub fn new() -> Self {
        Scratch {
            coeffs: vec![0; 32 * 32],
            res: vec![0; 32 * 32],
            pred: [vec![0; PRED_STRIDE * 64], vec![0; PRED_STRIDE * 64]],
            mc: McScratch::new(),
            tr: transform::TransformScratch::new(),
            band: None,
            saved: Default::default(),
        }
    }
}

impl<P: Pixel> Default for Scratch<P> {
    fn default() -> Self {
        Self::new()
    }
}

/// Coding-unit level info passed down the transform tree.
#[derive(Clone, Copy)]
struct Cu {
    x0: u32,
    y0: u32,
    log2: u32,
    intra: bool,
    part: u8,
    bypass: bool,
    max_trafo_depth: u32,
    intra_split: bool,
    chroma_mode: u32,
}

pub struct SliceDecoder<'a, P: Pixel> {
    sps: &'a Sps,
    pps: &'a Pps,
    sh: &'a SliceHeader,
    layout: &'a Layout,
    frame: &'a mut Frame<P>,
    st: &'a mut PicState,
    refs: &'a RefLists<'a, P>,
    sc: &'a mut Scratch<P>,
    data: &'a [u8],
    cabac: Cabac<'a>,
    c: [u8; ctx::NUM],
    slice_idx: u16,
    poc: i32,
    // constants
    width: u32,
    height: u32,
    log2_ctb: u32,
    ctb_size: u32,
    min_cb: u32,
    log2_min_tb: u32,
    min_tb_mask_w: u32,
    log2_min_qg: u32,
    bd_y: u32,
    bd_c: u32,
    qp_bd_off: i32,
    init_type: usize,
    // state
    ctb_rs: u32,
    ctb_ts: u32,
    ctb_xi: i32,
    ctb_yi: i32,
    /// Availability of the left, up-left, up and up-right CTBs.
    nb_ok: [bool; 4],
    qp_y: i32,
    qg_pred: i32,
    last_qp_y: i32,
    is_cu_qp_delta_coded: bool,
    cu_qp_delta: i32,
    ipm: [u32; 4],
    /// The current CU has interior transform edges marked for deblocking.
    inner_edges: bool,
}

impl<'a, P: Pixel> SliceDecoder<'a, P> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sps: &'a Sps,
        pps: &'a Pps,
        sh: &'a SliceHeader,
        layout: &'a Layout,
        frame: &'a mut Frame<P>,
        st: &'a mut PicState,
        refs: &'a RefLists<'a, P>,
        sc: &'a mut Scratch<P>,
        data: &'a [u8],
        slice_idx: u16,
        poc: i32,
    ) -> Self {
        let init_type = match sh.slice_type {
            SLICE_I => 0,
            SLICE_P => {
                if sh.cabac_init {
                    2
                } else {
                    1
                }
            }
            _ => {
                if sh.cabac_init {
                    1
                } else {
                    2
                }
            }
        };
        let start = sh.data_offset;
        SliceDecoder {
            sps,
            pps,
            sh,
            layout,
            frame,
            st,
            refs,
            sc,
            data,
            cabac: Cabac::new(data, start.min(data.len())),
            c: [0; ctx::NUM],
            slice_idx,
            poc,
            width: sps.width,
            height: sps.height,
            log2_ctb: sps.log2_ctb,
            ctb_size: sps.ctb_size,
            min_cb: sps.log2_min_cb,
            log2_min_tb: sps.log2_min_tb,
            min_tb_mask_w: layout.min_tb_w,
            log2_min_qg: sps.log2_ctb - pps.diff_cu_qp_delta_depth,
            bd_y: sps.bit_depth_luma,
            bd_c: sps.bit_depth_chroma,
            qp_bd_off: 6 * (sps.bit_depth_luma as i32 - 8),
            init_type,
            ctb_rs: 0,
            ctb_ts: 0,
            ctb_xi: 0,
            ctb_yi: 0,
            nb_ok: [false; 4],
            qp_y: sh.slice_qp,
            qg_pred: sh.slice_qp,
            last_qp_y: sh.slice_qp,
            is_cu_qp_delta_coded: false,
            cu_qp_delta: 0,
            ipm: [1; 4],
            inner_edges: false,
        }
    }

    // ------------------------------------------------------------------ availability

    #[inline(always)]
    fn zs(&self, x: u32, y: u32) -> u32 {
        self.layout.min_tb_addr_zs
            [((y >> self.log2_min_tb) * self.min_tb_mask_w + (x >> self.log2_min_tb)) as usize]
    }

    #[inline(always)]
    fn ctb_of(&self, x: u32, y: u32) -> usize {
        ((y >> self.log2_ctb) * self.sps.pic_w_ctb + (x >> self.log2_ctb)) as usize
    }

    /// Clause 6.4.1 z-scan order block availability (general form).
    fn avail_general(&self, xc: u32, yc: u32, xn: i32, yn: i32) -> bool {
        if xn < 0 || yn < 0 || xn >= self.width as i32 || yn >= self.height as i32 {
            return false;
        }
        let (xn, yn) = (xn as u32, yn as u32);
        if self.zs(xn, yn) > self.zs(xc, yc) {
            return false;
        }
        let cn = self.ctb_of(xn, yn);
        let cc = self.ctb_of(xc, yc);
        if cn == cc {
            return true;
        }
        if self.st.ctb_slice_addr[cn] != self.st.ctb_slice_addr[cc] {
            return false;
        }
        let ts = &self.layout.ctb_addr_rs_to_ts;
        self.layout.tile_id[ts[cn] as usize] == self.layout.tile_id[ts[cc] as usize]
    }

    /// Clause 6.4.1 for a current location inside the current CTB: same-CTB neighbours are
    /// compared in z-order (4x4 granularity orders identically to MinTb granularity for any
    /// neighbour outside the current block); other CTBs use flags computed at CTB start.
    #[inline(always)]
    fn avail(&self, xc: u32, yc: u32, xn: i32, yn: i32) -> bool {
        if xn < 0 || yn < 0 || xn >= self.width as i32 || yn >= self.height as i32 {
            return false;
        }
        let dx = (xn >> self.log2_ctb) - self.ctb_xi;
        let dy = (yn >> self.log2_ctb) - self.ctb_yi;
        if dx == 0 && dy == 0 {
            let m = self.ctb_size - 1;
            let zn = Z4[(((yn as u32 & m) >> 2) * 16 + ((xn as u32 & m) >> 2)) as usize];
            let zc = Z4[(((yc & m) >> 2) * 16 + ((xc & m) >> 2)) as usize];
            return zn <= zc;
        }
        match (dx, dy) {
            (-1, 0) => self.nb_ok[0],
            (-1, -1) => self.nb_ok[1],
            (0, -1) => self.nb_ok[2],
            (1, -1) => self.nb_ok[3],
            _ => false,
        }
    }

    #[inline(always)]
    fn blk(&self, x: u32, y: u32) -> &BlkInfo {
        &self.st.blk[(y >> 2) as usize * self.st.w4 + (x >> 2) as usize]
    }

    #[inline(always)]
    fn motion_at(&self, x: u32, y: u32) -> PuMotion {
        self.st.motion[(y >> 2) as usize * self.st.w4 + (x >> 2) as usize]
    }

    /// Store `v` for every 4x4 block of the area.
    fn fill_blk(&mut self, x0: u32, y0: u32, w: u32, h: u32, v: BlkInfo) {
        let w4 = self.st.w4;
        let (xa, xb) = ((x0 >> 2) as usize, ((x0 + w) >> 2) as usize);
        for y in (y0 >> 2) as usize..((y0 + h) >> 2) as usize {
            self.st.blk[y * w4 + xa..y * w4 + xb].fill(v);
        }
    }

    fn set_blk_area(&mut self, x0: u32, y0: u32, w: u32, h: u32, f: impl Fn(&mut BlkInfo)) {
        let w4 = self.st.w4;
        for y in (y0 >> 2)..((y0 + h) >> 2) {
            let row = &mut self.st.blk[y as usize * w4..];
            for x in (x0 >> 2)..((x0 + w) >> 2) {
                f(&mut row[x as usize]);
            }
        }
    }

    // ------------------------------------------------------------------ slice data

    fn init_ctx(&mut self) {
        init_contexts(&mut self.c, self.sh.slice_qp, self.init_type);
    }

    pub fn decode(&mut self) -> Result<()> {
        let sps = self.sps;
        let pps = self.pps;
        let layout = self.layout;
        let pic_w_ctb = sps.pic_w_ctb;
        let num_ctb = sps.pic_size_ctb();
        self.ctb_rs = self.sh.segment_address;
        self.ctb_ts = layout.ctb_addr_rs_to_ts[self.ctb_rs as usize];
        let mut first = true;
        if self.sh.dependent_slice_segment {
            self.last_qp_y = self.st.ds_last_qp;
        }
        loop {
            let rs = self.ctb_rs;
            let ts = self.ctb_ts;
            let tile = layout.tile_id[ts as usize];
            let first_in_tile = ts == 0 || layout.tile_id[ts as usize - 1] != tile;
            let ctb_x = rs % pic_w_ctb;
            let first_in_row_of_tile = ctb_x == 0
                || layout.tile_id[layout.ctb_addr_rs_to_ts[rs as usize - 1] as usize] != tile;
            let x_ctb = ctb_x << self.log2_ctb;
            let y_ctb = (rs / pic_w_ctb) << self.log2_ctb;
            // Record the slice before any availability lookup inside this CTB.
            self.st.ctb_slice_addr[rs as usize] = self.sh.slice_addr_rs;
            self.st.ctb_slice_idx[rs as usize] = self.slice_idx;
            self.ctb_xi = ctb_x as i32;
            self.ctb_yi = (rs / pic_w_ctb) as i32;
            let cs = self.ctb_size as i32;
            let (xi, yi) = (x_ctb as i32, y_ctb as i32);
            self.nb_ok = [
                self.avail_general(x_ctb, y_ctb, xi - 1, yi),
                self.avail_general(x_ctb, y_ctb, xi - 1, yi - 1),
                self.avail_general(x_ctb, y_ctb, xi, yi - 1),
                self.avail_general(x_ctb, y_ctb, xi + cs, yi - 1),
            ];
            // Context initialization (9.3.1 / 9.3.2.1).
            if first || first_in_tile || (pps.entropy_coding_sync && first_in_row_of_tile) {
                if first_in_tile {
                    self.init_ctx();
                } else if pps.entropy_coding_sync && first_in_row_of_tile {
                    let tr_avail = self.nb_ok[3];
                    if tr_avail && self.st.ctx_wpp_valid {
                        self.c = self.st.ctx_wpp;
                    } else {
                        self.init_ctx();
                    }
                } else if first && self.sh.dependent_slice_segment && self.st.ctx_ds_valid {
                    self.c = self.st.ctx_ds;
                } else {
                    self.init_ctx();
                }
            }
            if first && !self.sh.dependent_slice_segment {
                self.last_qp_y = self.sh.slice_qp;
            }
            if first_in_tile || (pps.entropy_coding_sync && first_in_row_of_tile) {
                self.last_qp_y = self.sh.slice_qp;
            }
            first = false;
            // coding_tree_unit()
            if self.sh.sao_luma || self.sh.sao_chroma {
                self.sao_syntax(ctb_x, rs / pic_w_ctb)?;
            }
            self.coding_quadtree(x_ctb, y_ctb, self.log2_ctb, 0)?;
            self.st.decoded_ctbs += 1;
            let ry = (rs / pic_w_ctb) as usize;
            self.st.row_done[ry] = self.st.row_done[ry].saturating_add(1);
            if self.st.row_done[ry] as u32 >= pic_w_ctb {
                let Scratch { band, saved, .. } = &mut *self.sc;
                crate::decoder::filter_ready(
                    self.frame, self.st, sps, pps, layout, band, saved, false,
                );
            }
            if self.cabac.overrun() {
                return Err(DecodeError::InvalidData("slice data overrun"));
            }
            // WPP storage (9.3.1).
            if pps.entropy_coding_sync
                && (ctb_x == 1
                    || (rs > 1
                        && layout.tile_id[layout.ctb_addr_rs_to_ts[rs as usize - 2] as usize]
                            != tile))
            {
                self.st.ctx_wpp = self.c;
                self.st.ctx_wpp_valid = true;
            }
            let end = self.cabac.terminate();
            self.ctb_ts += 1;
            if end == 1 {
                if pps.dependent_slice_segments_enabled {
                    self.st.ctx_ds = self.c;
                    self.st.ctx_ds_valid = true;
                    self.st.ds_last_qp = self.last_qp_y;
                }
                return Ok(());
            }
            if self.ctb_ts >= num_ctb {
                return Err(DecodeError::InvalidData("slice runs past end of picture"));
            }
            self.ctb_rs = layout.ctb_addr_ts_to_rs[self.ctb_ts as usize];
            let ntile = layout.tile_id[self.ctb_ts as usize];
            let nx = self.ctb_rs % pic_w_ctb;
            let new_row = pps.entropy_coding_sync
                && (nx == 0
                    || layout.tile_id[layout.ctb_addr_rs_to_ts[self.ctb_rs as usize - 1] as usize]
                        != ntile);
            if (pps.tiles_enabled && ntile != tile) || new_row {
                let one = self.cabac.terminate();
                if one != 1 {
                    return Err(DecodeError::InvalidData("end_of_subset_one_bit"));
                }
                let pos = self.cabac.aligned_byte_pos();
                self.cabac.reinit(pos);
            }
        }
    }

    // ------------------------------------------------------------------ SAO syntax

    #[inline(never)]
    fn sao_syntax(&mut self, rx: u32, ry: u32) -> Result<()> {
        let pic_w_ctb = self.sps.pic_w_ctb;
        let rs = self.ctb_rs;
        let layout = self.layout;
        let tile = layout.tile_id[self.ctb_ts as usize];
        let mut merge_left = false;
        let mut merge_up = false;
        if rx > 0 {
            let left_in_slice = rs > self.sh.slice_addr_rs;
            let left_in_tile =
                layout.tile_id[layout.ctb_addr_rs_to_ts[rs as usize - 1] as usize] == tile;
            if left_in_slice && left_in_tile {
                merge_left = self.cabac.decode(&mut self.c[ctx::SAO_MERGE]) != 0;
            }
        }
        if ry > 0 && !merge_left {
            let up = rs - pic_w_ctb;
            let up_in_slice = up >= self.sh.slice_addr_rs;
            let up_in_tile = layout.tile_id[layout.ctb_addr_rs_to_ts[up as usize] as usize] == tile;
            if up_in_slice && up_in_tile {
                merge_up = self.cabac.decode(&mut self.c[ctx::SAO_MERGE]) != 0;
            }
        }
        let mut p = SaoParams::default();
        if merge_left {
            p = self.st.sao[rs as usize - 1];
        } else if merge_up {
            p = self.st.sao[(rs - pic_w_ctb) as usize];
        } else {
            for c_idx in 0..3usize {
                let enabled = if c_idx == 0 {
                    self.sh.sao_luma
                } else {
                    self.sh.sao_chroma
                };
                if !enabled {
                    continue;
                }
                if c_idx == 0 || c_idx == 1 {
                    let t = if self.cabac.decode(&mut self.c[ctx::SAO_TYPE]) == 0 {
                        0
                    } else if self.cabac.bypass() == 0 {
                        1
                    } else {
                        2
                    };
                    p.type_idx[c_idx] = t;
                } else {
                    p.type_idx[2] = p.type_idx[1];
                    p.eo_class[2] = p.eo_class[1];
                }
                let t = p.type_idx[c_idx];
                if t == 0 {
                    continue;
                }
                let bd = if c_idx == 0 { self.bd_y } else { self.bd_c };
                let cmax = (1u32 << (bd.min(10) - 5)) - 1;
                let mut abs = [0i16; 4];
                for a in abs.iter_mut() {
                    let mut v = 0;
                    while v < cmax && self.cabac.bypass() == 1 {
                        v += 1;
                    }
                    *a = v as i16;
                }
                if t == 1 {
                    for a in abs.iter_mut() {
                        if *a != 0 && self.cabac.bypass() == 1 {
                            *a = -*a;
                        }
                    }
                    p.band_pos[c_idx] = self.cabac.bypass_bits(5) as u8;
                    p.offset[c_idx] = abs;
                } else {
                    p.offset[c_idx] = [abs[0], abs[1], -abs[2], -abs[3]];
                    if c_idx == 0 {
                        p.eo_class[0] = self.cabac.bypass_bits(2) as u8;
                    } else if c_idx == 1 {
                        p.eo_class[1] = self.cabac.bypass_bits(2) as u8;
                    }
                }
            }
        }
        if !self.sh.sao_luma {
            p.type_idx[0] = 0;
        }
        if !self.sh.sao_chroma {
            p.type_idx[1] = 0;
            p.type_idx[2] = 0;
        }
        self.st.sao[rs as usize] = p;
        Ok(())
    }

    // ------------------------------------------------------------------ coding quadtree

    fn coding_quadtree(&mut self, x0: u32, y0: u32, log2: u32, depth: u32) -> Result<()> {
        let size = 1 << log2;
        let split = if x0 + size <= self.width && y0 + size <= self.height && log2 > self.min_cb {
            let mut inc = 0;
            if self.avail(x0, y0, x0 as i32 - 1, y0 as i32)
                && self.blk(x0 - 1, y0).depth as u32 > depth
            {
                inc += 1;
            }
            if self.avail(x0, y0, x0 as i32, y0 as i32 - 1)
                && self.blk(x0, y0 - 1).depth as u32 > depth
            {
                inc += 1;
            }
            self.cabac.decode(&mut self.c[ctx::SPLIT_CU + inc]) != 0
        } else {
            log2 > self.min_cb
        };
        if log2 >= self.log2_min_qg {
            if self.pps.cu_qp_delta_enabled {
                self.is_cu_qp_delta_coded = false;
                self.cu_qp_delta = 0;
            }
            self.start_qg(x0, y0);
        }
        if split {
            let h = size >> 1;
            let x1 = x0 + h;
            let y1 = y0 + h;
            self.coding_quadtree(x0, y0, log2 - 1, depth + 1)?;
            if x1 < self.width {
                self.coding_quadtree(x1, y0, log2 - 1, depth + 1)?;
            }
            if y1 < self.height {
                self.coding_quadtree(x0, y1, log2 - 1, depth + 1)?;
            }
            if x1 < self.width && y1 < self.height {
                self.coding_quadtree(x1, y1, log2 - 1, depth + 1)?;
            }
            Ok(())
        } else {
            self.coding_unit(x0, y0, log2, depth)
        }
    }

    /// Start of a quantization group: derive qPY_PRED (8.6.1).
    fn start_qg(&mut self, xq: u32, yq: u32) {
        let prev = self.last_qp_y;
        let mask = self.ctb_size - 1;
        let qa = if xq & mask != 0 {
            self.blk(xq - 1, yq).qp as i32
        } else {
            prev
        };
        let qb = if yq & mask != 0 {
            self.blk(xq, yq - 1).qp as i32
        } else {
            prev
        };
        self.qg_pred = (qa + qb + 1) >> 1;
        self.update_qp();
    }

    #[inline]
    fn update_qp(&mut self) {
        let off = self.qp_bd_off;
        self.qp_y = ((self.qg_pred + self.cu_qp_delta + 52 + 2 * off) % (52 + off)) - off;
    }

    // ------------------------------------------------------------------ coding unit

    fn coding_unit(&mut self, x0: u32, y0: u32, log2: u32, depth: u32) -> Result<()> {
        let n = 1u32 << log2;
        self.inner_edges = false;
        let bypass = if self.pps.transquant_bypass {
            self.cabac.decode(&mut self.c[ctx::CU_TRANSQUANT_BYPASS]) != 0
        } else {
            false
        };
        let mut skip = false;
        if self.sh.slice_type != SLICE_I {
            let mut inc = 0;
            if self.avail(x0, y0, x0 as i32 - 1, y0 as i32)
                && self.blk(x0 - 1, y0).flags & F_SKIP != 0
            {
                inc += 1;
            }
            if self.avail(x0, y0, x0 as i32, y0 as i32 - 1)
                && self.blk(x0, y0 - 1).flags & F_SKIP != 0
            {
                inc += 1;
            }
            skip = self.cabac.decode(&mut self.c[ctx::CU_SKIP + inc]) != 0;
        }
        self.update_qp();
        let d = depth as u8;
        let mut cu = Cu {
            x0,
            y0,
            log2,
            intra: false,
            part: PART_2NX2N,
            bypass,
            max_trafo_depth: 0,
            intra_split: false,
            chroma_mode: 0,
        };
        let mut merge_2nx2n = false;
        let mut pcm = false;
        // QpY is stored with the block info now and rewritten at the end of the CU only if
        // a cu_qp_delta in this CU changed it.
        let qp_start = self.qp_y;
        let info = |flags: u8| BlkInfo {
            flags,
            depth: d,
            ipm: 1,
            qp: qp_start as i8,
        };
        if skip {
            self.fill_blk(x0, y0, n, n, info(F_SKIP));
            self.prediction_unit(&cu, x0, y0, n, n, 0, true)?;
        } else {
            let intra = if self.sh.slice_type != SLICE_I {
                self.cabac.decode(&mut self.c[ctx::PRED_MODE]) != 0
            } else {
                true
            };
            cu.intra = intra;
            // part_mode
            if !intra || log2 == self.min_cb {
                cu.part = self.parse_part_mode(intra, log2);
            }
            cu.intra_split = intra && cu.part == PART_NXN;
            let flags = if intra { F_INTRA } else { 0 };
            self.fill_blk(x0, y0, n, n, info(flags));
            if intra {
                // Intra blocks have no motion.
                self.clear_motion(x0, y0, n, n);
                if cu.part == PART_2NX2N
                    && self.sps.pcm
                    && log2 >= self.sps.log2_min_pcm
                    && log2 <= self.sps.log2_max_pcm
                {
                    pcm = self.cabac.terminate() != 0;
                }
                if pcm {
                    self.pcm_sample(x0, y0, log2)?;
                    let f = if self.sps.pcm_loop_filter_disabled {
                        self.st.any_nofilter = true;
                        F_NOFILTER | F_PCM
                    } else {
                        F_PCM
                    };
                    self.set_blk_area(x0, y0, n, n, |b| b.flags |= f);
                } else {
                    self.parse_intra_modes(&mut cu)?;
                }
            } else {
                match cu.part {
                    PART_2NX2N => {
                        merge_2nx2n = self.prediction_unit(&cu, x0, y0, n, n, 0, false)?;
                    }
                    PART_2NXN => {
                        self.prediction_unit(&cu, x0, y0, n, n / 2, 0, false)?;
                        self.prediction_unit(&cu, x0, y0 + n / 2, n, n / 2, 1, false)?;
                    }
                    PART_NX2N => {
                        self.prediction_unit(&cu, x0, y0, n / 2, n, 0, false)?;
                        self.prediction_unit(&cu, x0 + n / 2, y0, n / 2, n, 1, false)?;
                    }
                    PART_2NXNU => {
                        self.prediction_unit(&cu, x0, y0, n, n / 4, 0, false)?;
                        self.prediction_unit(&cu, x0, y0 + n / 4, n, n * 3 / 4, 1, false)?;
                    }
                    PART_2NXND => {
                        self.prediction_unit(&cu, x0, y0, n, n * 3 / 4, 0, false)?;
                        self.prediction_unit(&cu, x0, y0 + n * 3 / 4, n, n / 4, 1, false)?;
                    }
                    PART_NLX2N => {
                        self.prediction_unit(&cu, x0, y0, n / 4, n, 0, false)?;
                        self.prediction_unit(&cu, x0 + n / 4, y0, n * 3 / 4, n, 1, false)?;
                    }
                    PART_NRX2N => {
                        self.prediction_unit(&cu, x0, y0, n * 3 / 4, n, 0, false)?;
                        self.prediction_unit(&cu, x0 + n * 3 / 4, y0, n / 4, n, 1, false)?;
                    }
                    _ => {
                        let h = n / 2;
                        self.prediction_unit(&cu, x0, y0, h, h, 0, false)?;
                        self.prediction_unit(&cu, x0 + h, y0, h, h, 1, false)?;
                        self.prediction_unit(&cu, x0, y0 + h, h, h, 2, false)?;
                        self.prediction_unit(&cu, x0 + h, y0 + h, h, h, 3, false)?;
                    }
                }
            }
            if !pcm {
                let rqt_root_cbf = if !intra && !(cu.part == PART_2NX2N && merge_2nx2n) {
                    self.cabac.decode(&mut self.c[ctx::RQT_ROOT_CBF]) != 0
                } else {
                    true
                };
                if rqt_root_cbf {
                    cu.max_trafo_depth = if intra {
                        self.sps.max_th_depth_intra + cu.intra_split as u32
                    } else {
                        self.sps.max_th_depth_inter
                    };
                    self.transform_tree(&cu, x0, y0, x0, y0, log2, 0, 0, false, false)?;
                }
            }
        }
        if bypass {
            self.st.any_nofilter = true;
            self.set_blk_area(x0, y0, n, n, |b| b.flags |= F_NOFILTER);
        }
        // Final QpY for this CU (deblocking, prediction of later groups).
        if self.qp_y != qp_start {
            let qp = self.qp_y as i8;
            self.set_blk_area(x0, y0, n, n, |b| b.qp = qp);
        }
        self.last_qp_y = self.qp_y;
        if !self.sh.deblocking_disabled {
            self.deblock_cu_edges(&cu);
        }
        Ok(())
    }

    fn clear_motion(&mut self, x0: u32, y0: u32, w: u32, h: u32) {
        self.fill_motion(x0, y0, w, h, PuMotion::default());
    }

    fn fill_motion(&mut self, x0: u32, y0: u32, w: u32, h: u32, m: PuMotion) {
        let w4 = self.st.w4;
        let (xa, xb) = ((x0 >> 2) as usize, ((x0 + w) >> 2) as usize);
        for y in (y0 >> 2) as usize..((y0 + h) >> 2) as usize {
            self.st.motion[y * w4 + xa..y * w4 + xb].fill(m);
        }
    }

    fn parse_part_mode(&mut self, intra: bool, log2: u32) -> u8 {
        if self.cabac.decode(&mut self.c[ctx::PART_MODE]) != 0 {
            return PART_2NX2N;
        }
        if intra {
            return PART_NXN;
        }
        if log2 == self.min_cb {
            if self.cabac.decode(&mut self.c[ctx::PART_MODE + 1]) != 0 {
                return PART_2NXN;
            }
            if log2 == 3 {
                return PART_NX2N;
            }
            if self.cabac.decode(&mut self.c[ctx::PART_MODE + 2]) != 0 {
                return PART_NX2N;
            }
            return PART_NXN;
        }
        let horizontal = self.cabac.decode(&mut self.c[ctx::PART_MODE + 1]) != 0;
        if !self.sps.amp {
            return if horizontal { PART_2NXN } else { PART_NX2N };
        }
        if horizontal {
            if self.cabac.decode(&mut self.c[ctx::PART_MODE + 3]) != 0 {
                return PART_2NXN;
            }
            if self.cabac.bypass() == 0 {
                PART_2NXNU
            } else {
                PART_2NXND
            }
        } else {
            if self.cabac.decode(&mut self.c[ctx::PART_MODE + 3]) != 0 {
                return PART_NX2N;
            }
            if self.cabac.bypass() == 0 {
                PART_NLX2N
            } else {
                PART_NRX2N
            }
        }
    }

    fn pcm_sample(&mut self, x0: u32, y0: u32, log2: u32) -> Result<()> {
        let pos = self.cabac.aligned_byte_pos();
        if pos > self.data.len() {
            return Err(DecodeError::InvalidData("PCM data overrun"));
        }
        let mut r = BitReader::new(&self.data[pos..]);
        let n = 1usize << log2;
        let bd_y = self.sps.pcm_bit_depth_luma;
        let bd_c = self.sps.pcm_bit_depth_chroma;
        let sy = self.bd_y - bd_y;
        let scy = self.bd_c - bd_c;
        {
            let pl = &mut self.frame.planes[0];
            let stride = pl.stride;
            for j in 0..n {
                for i in 0..n {
                    let v = r.u(bd_y)? as i32;
                    pl.data[(y0 as usize + j) * stride + x0 as usize + i] = P::from_i32(v << sy);
                }
            }
        }
        let nc = n / 2;
        for pl in self.frame.planes[1..].iter_mut() {
            let stride = pl.stride;
            for j in 0..nc {
                for i in 0..nc {
                    let v = r.u(bd_c)? as i32;
                    pl.data[(y0 as usize / 2 + j) * stride + x0 as usize / 2 + i] =
                        P::from_i32(v << scy);
                }
            }
        }
        let consumed = r.bit_pos().div_ceil(8);
        self.cabac.reinit(pos + consumed);
        Ok(())
    }

    #[inline(never)]
    fn parse_intra_modes(&mut self, cu: &mut Cu) -> Result<()> {
        let n = 1u32 << cu.log2;
        let nparts = if cu.part == PART_NXN { 4 } else { 1 };
        let pb = if nparts == 4 { n / 2 } else { n };
        let mut prev = [false; 4];
        for p in prev.iter_mut().take(nparts) {
            *p = self.cabac.decode(&mut self.c[ctx::PREV_INTRA_LUMA_PRED]) != 0;
        }
        for i in 0..nparts {
            let xp = cu.x0 + (i as u32 & 1) * pb;
            let yp = cu.y0 + (i as u32 >> 1) * pb;
            // Candidate list (8.4.2).
            let cand_a = self.cand_intra_mode(xp, yp, xp as i32 - 1, yp as i32, false);
            let cand_b = self.cand_intra_mode(xp, yp, xp as i32, yp as i32 - 1, true);
            let mut list = if cand_a == cand_b {
                if cand_a < 2 {
                    [0, 1, 26]
                } else {
                    [
                        cand_a,
                        2 + ((cand_a + 29) % 32),
                        2 + ((cand_a - 2 + 1) % 32),
                    ]
                }
            } else {
                let third = if cand_a != 0 && cand_b != 0 {
                    0
                } else if cand_a != 1 && cand_b != 1 {
                    1
                } else {
                    26
                };
                [cand_a, cand_b, third]
            };
            let mode = if prev[i] {
                let idx = if self.cabac.bypass() == 0 {
                    0
                } else if self.cabac.bypass() == 0 {
                    1
                } else {
                    2
                };
                list[idx]
            } else {
                let mut m = self.cabac.bypass_bits(5);
                if list[0] > list[1] {
                    list.swap(0, 1);
                }
                if list[0] > list[2] {
                    list.swap(0, 2);
                }
                if list[1] > list[2] {
                    list.swap(1, 2);
                }
                for &c in list.iter() {
                    if m >= c {
                        m += 1;
                    }
                }
                m
            };
            self.ipm[i] = mode;
            let mb = mode as u8;
            self.set_blk_area(xp, yp, pb, pb, |b| b.ipm = mb);
        }
        // intra_chroma_pred_mode (4:2:0: one per CU), Table 8-2.
        let icpm = if self.cabac.decode(&mut self.c[ctx::INTRA_CHROMA_PRED_MODE]) == 0 {
            4
        } else {
            self.cabac.bypass_bits(2)
        };
        let ym = self.ipm[0];
        cu.chroma_mode = match icpm {
            4 => ym,
            k => {
                let m = [0u32, 26, 10, 1][k as usize];
                if m == ym { 34 } else { m }
            }
        };
        if nparts == 1 {
            self.ipm[1] = self.ipm[0];
            self.ipm[2] = self.ipm[0];
            self.ipm[3] = self.ipm[0];
        }
        Ok(())
    }

    fn cand_intra_mode(&self, xp: u32, yp: u32, xn: i32, yn: i32, is_b: bool) -> u32 {
        if !self.avail(xp, yp, xn, yn) {
            return 1;
        }
        let b = self.blk(xn as u32, yn as u32);
        if b.flags & F_INTRA == 0 || b.flags & F_PCM != 0 {
            return 1;
        }
        if is_b && (yn as u32) < ((yp >> self.log2_ctb) << self.log2_ctb) {
            return 1;
        }
        b.ipm as u32
    }

    // ------------------------------------------------------------------ prediction unit

    /// Parses a prediction unit, derives its motion, stores it and performs motion
    /// compensation. Returns merge_flag.
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    fn prediction_unit(
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
    fn eg_k(&mut self, mut k: u32) -> u32 {
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
            if self.sh.slice_type == SLICE_B {
                if let Some(mv) = self.temporal_mv(xp, yp, w, h, 0, 1) {
                    col.mv[1] = mv;
                    col.ref_idx[1] = 0;
                    avail = true;
                }
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
        if (yp >> self.log2_ctb) == (ybr >> self.log2_ctb) && ybr < self.height && xbr < self.width
        {
            if let Some(mv) = self.col_mv(col, xbr, ybr, ref_idx, lx) {
                return Some(mv);
            }
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
        if i < 2 && !(avail_a && avail_b && mv_a != mv_b) {
            if let Some(mv) = self.temporal_mv(xp, yp, w, h, ref_idx, lx) {
                list[i] = mv;
                i += 1;
            }
        }
        let _ = i;
        list[flag as usize]
    }

    // ------------------------------------------------------------------ motion compensation

    #[inline(never)]
    fn motion_compensate(&mut self, xp: u32, yp: u32, w: u32, h: u32, m: &PuMotion) -> Result<()> {
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

    // ------------------------------------------------------------------ transform tree

    #[allow(clippy::too_many_arguments)]
    fn transform_tree(
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
                .decode(&mut self.c[ctx::SPLIT_TRANSFORM + (5 - log2) as usize])
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
                    .decode(&mut self.c[ctx::CBF_CHROMA + depth as usize])
                    != 0;
            }
            if depth == 0 || parent_cr {
                cbf_cr = self
                    .cabac
                    .decode(&mut self.c[ctx::CBF_CHROMA + depth as usize])
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
                    .decode(&mut self.c[ctx::CBF_LUMA + if depth == 0 { 1 } else { 0 }])
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
                if self.cabac.decode(&mut self.c[ctx::CU_QP_DELTA_ABS + inc]) == 0 {
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

    // ------------------------------------------------------------------ residual coding

    fn residual(
        &mut self,
        cu: &Cu,
        x0: u32,
        y0: u32,
        log2: u32,
        c_idx: usize,
        pred_mode_intra: u32,
    ) -> Result<()> {
        let n = 1usize << log2;
        let mut cab = self.cabac.clone();
        let c = &mut self.c;
        let mut transform_skip = false;
        if self.pps.transform_skip && !cu.bypass && log2 <= 2 {
            transform_skip = cab.decode(&mut c[ctx::TRANSFORM_SKIP + (c_idx > 0) as usize]) != 0;
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
            && cab.decode(&mut c[ctx::LAST_X_PREFIX + (ctx_off + (px >> ctx_shift)) as usize]) != 0
        {
            px += 1;
        }
        let mut py = 0u32;
        while py < max_prefix
            && cab.decode(&mut c[ctx::LAST_Y_PREFIX + (ctx_off + (py >> ctx_shift)) as usize]) != 0
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
        if scan_idx == 2 {
            core::mem::swap(&mut last_x, &mut last_y);
        }
        if last_x as usize >= n || last_y as usize >= n {
            return Err(DecodeError::InvalidData(
                "last significant coefficient outside block",
            ));
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
                cab.decode(&mut c[csbf_base + inc])
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
                let b = cab.decode(&mut sig_ctx[tab[nn] as usize]);
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
                    if cab.decode(s0) != 0 {
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
                let b = cab.decode(&mut gt1_ctx[greater1_ctx.min(3) as usize]);
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
            if gt1_first < 16 && cab.decode(&mut c[gt2_base + ctx_set]) != 0 {
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
        Ok(())
    }

    // ------------------------------------------------------------------ deblocking edges

    #[inline(never)]
    fn deblock_cu_edges(&mut self, cu: &Cu) {
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
