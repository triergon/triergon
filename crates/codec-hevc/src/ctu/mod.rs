//! Slice segment data decoding (clause 7.3.8): CTU / coding quadtree / coding unit /
//! prediction unit / transform tree parsing with CABAC (clause 9.3), and the per-block
//! reconstruction (intra 8.4, inter 8.5, scaling/transform 8.6).

mod coding_tree;
mod deblock_edges;
mod inter_pred;
mod prediction_unit;
mod residual;
mod transform_tree;

use crate::cabac::{Bins, Cabac, ctx, init_contexts};
use crate::error::{DecodeError, Result};
use crate::inter::{McScratch, PRED_STRIDE};
use crate::picture::{ColMotion, Frame, Pixel, PuMotion};
use crate::ps::{Layout, Pps, Sps};
use crate::slice::{SLICE_I, SLICE_P, SliceHeader};
use crate::state::*;
use crate::transform;
use triergon_codec::{Budget, BudgetVec};

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

#[derive(Debug)]
pub struct Scratch<P> {
    pub coeffs: Vec<i32>,
    pub res: Vec<i32>,
    pub pred: [Vec<i16>; 2],
    pub mc: McScratch<P>,
    pub tr: transform::TransformScratch,
    /// SAO source band (deblocked samples of one CTB row plus a line above and below) and
    /// the saved last deblocked line of the previous row, per component.
    pub band: Option<Frame<P>>,
    pub saved: [BudgetVec<P>; 3],
}

impl<P: Pixel> Scratch<P> {
    pub fn new(budget: &Budget) -> Self {
        Scratch {
            coeffs: vec![0; 32 * 32],
            res: vec![0; 32 * 32],
            pred: [vec![0; PRED_STRIDE * 64], vec![0; PRED_STRIDE * 64]],
            mc: McScratch::new(),
            tr: transform::TransformScratch::new(),
            band: None,
            saved: core::array::from_fn(|_| BudgetVec::new(budget)),
        }
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
                let pos = self.cabac.aligned_byte_position();
                self.cabac.restart(pos);
            }
        }
    }
}
