//! Per-picture decoding state shared by the slices of a picture and the in-loop filters.

use crate::cabac::ctx;
use crate::picture::PuMotion;
use triergon_codec::{Budget, BudgetVec, StorageError};

pub const F_INTRA: u8 = 1;
pub const F_SKIP: u8 = 2;
/// Samples must not be modified by the in-loop filters (PCM with pcm_loop_filter_disabled_flag,
/// or cu_transquant_bypass_flag).
pub const F_NOFILTER: u8 = 4;
pub const F_PCM: u8 = 8;
/// The luma transform block covering this 4x4 block has non-zero coefficients.
pub const F_NZ: u8 = 16;

#[derive(Debug, Clone, Copy, Default)]
pub struct BlkInfo {
    pub flags: u8,
    pub depth: u8,
    pub ipm: u8,
    pub qp: i8,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SaoParams {
    pub type_idx: [u8; 3],
    pub band_pos: [u8; 3],
    pub eo_class: [u8; 3],
    pub offset: [[i16; 4]; 3],
}

/// Information about a slice segment of the current picture needed after parsing.
#[derive(Debug, Clone)]
#[allow(dead_code)] // informative fields kept for diagnostics
pub struct SliceInfo {
    pub slice_addr_rs: u32,
    pub deblocking_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub lf_across_slices: bool,
    pub sao_luma: bool,
    pub sao_chroma: bool,
    pub ref_poc: [[i32; 16]; 2],
    pub ref_lt: [[bool; 16]; 2],
}

/// The per-picture maps are `BudgetVec`s sized when a picture starts and reused (grown,
/// never shrunk) for later pictures.
#[derive(Debug)]
pub struct PicState {
    pub w4: usize,
    pub h4: usize,
    pub blk: BudgetVec<BlkInfo>,
    pub motion: BudgetVec<PuMotion>,
    /// Vertical edge marks / boundary strengths at x multiple of 8, per 4 rows.
    pub bs_v: BudgetVec<u8>,
    /// Horizontal edge marks / boundary strengths at y multiple of 8, per 4 columns.
    pub bs_h: BudgetVec<u8>,
    pub bs_v_stride: usize,
    pub bs_h_stride: usize,
    pub ctb_slice_addr: BudgetVec<u32>,
    pub ctb_slice_idx: BudgetVec<u16>,
    pub sao: BudgetVec<SaoParams>,
    pub slices: BudgetVec<SliceInfo>,
    pub ctx_wpp: [u8; ctx::NUM],
    pub ctx_ds: [u8; ctx::NUM],
    pub ctx_wpp_valid: bool,
    pub ctx_ds_valid: bool,
    /// qPY_PREV carried into a dependent slice segment.
    pub ds_last_qp: i32,
    pub any_deblock: bool,
    pub any_sao: bool,
    pub any_nofilter: bool,
    pub decoded_ctbs: usize,
    /// Decoded CTBs per CTB row, and the next rows to deblock / to SAO (in-loop filters
    /// run row by row during decoding, see decoder::filter_ready).
    pub row_done: BudgetVec<u16>,
    pub filt_next: usize,
    pub sao_next: usize,
}

impl PicState {
    pub fn new(budget: &Budget) -> Self {
        PicState {
            w4: 0,
            h4: 0,
            blk: BudgetVec::new(budget),
            motion: BudgetVec::new(budget),
            bs_v: BudgetVec::new(budget),
            bs_h: BudgetVec::new(budget),
            bs_v_stride: 0,
            bs_h_stride: 0,
            ctb_slice_addr: BudgetVec::new(budget),
            ctb_slice_idx: BudgetVec::new(budget),
            sao: BudgetVec::new(budget),
            slices: BudgetVec::new(budget),
            ctx_wpp: [0; ctx::NUM],
            ctx_ds: [0; ctx::NUM],
            ctx_wpp_valid: false,
            ctx_ds_valid: false,
            ds_last_qp: 0,
            any_deblock: false,
            any_sao: false,
            any_nofilter: false,
            decoded_ctbs: 0,
            row_done: BudgetVec::new(budget),
            filt_next: 0,
            sao_next: 0,
        }
    }

    /// Prepare for a new picture of the given luma size, CTB count and CTB rows. Every
    /// map is sized before any of them changes, so a refused charge leaves the state as
    /// it was.
    pub fn reset(
        &mut self,
        width: usize,
        height: usize,
        num_ctb: usize,
        ctb_rows: usize,
    ) -> Result<(), StorageError> {
        let w4 = width / 4;
        let h4 = height / 4;
        let (bs_v_stride, bs_h_stride) = (width / 8 + 1, w4);
        let blk = resized(&self.blk, w4 * h4, BlkInfo::default())?;
        let motion = resized(&self.motion, w4 * h4, PuMotion::default())?;
        let bs_v = resized(&self.bs_v, bs_v_stride * h4, 0)?;
        let bs_h = resized(&self.bs_h, bs_h_stride * (height / 8 + 1), 0)?;
        let ctb_slice_addr = resized(&self.ctb_slice_addr, num_ctb, u32::MAX)?;
        let ctb_slice_idx = resized(&self.ctb_slice_idx, num_ctb, 0)?;
        let sao = resized(&self.sao, num_ctb, SaoParams::default())?;
        let row_done = resized(&self.row_done, ctb_rows, 0)?;
        if self.w4 != w4 || self.h4 != h4 {
            self.w4 = w4;
            self.h4 = h4;
            self.bs_v_stride = bs_v_stride;
            self.bs_h_stride = bs_h_stride;
            refill(&mut self.blk, blk, BlkInfo::default());
            refill(&mut self.motion, motion, PuMotion::default());
        }
        // blk / motion entries are always written (in decoding order) before any read
        // within a picture: neighbour reads are gated by availability, the loop filters
        // read only decoded CTBs. The edge marks and CTB maps are cleared.
        refill(&mut self.bs_v, bs_v, 0);
        refill(&mut self.bs_h, bs_h, 0);
        refill(&mut self.ctb_slice_addr, ctb_slice_addr, u32::MAX);
        refill(&mut self.ctb_slice_idx, ctb_slice_idx, 0);
        refill(&mut self.sao, sao, SaoParams::default());
        refill(&mut self.row_done, row_done, 0);
        self.slices.clear();
        self.ctx_wpp_valid = false;
        self.ctx_ds_valid = false;
        self.any_deblock = false;
        self.any_sao = false;
        self.any_nofilter = false;
        self.decoded_ctbs = 0;
        self.filt_next = 0;
        self.sao_next = 0;
        Ok(())
    }

    /// Make room for one more slice segment's `SliceInfo` (doubling), so that it is
    /// charged before the slice header changes any picture state.
    pub fn reserve_slice(&mut self) -> Result<(), StorageError> {
        if self.slices.len() < self.slices.capacity() {
            return Ok(());
        }
        self.slices.try_reserve_exact(self.slices.len().max(4))
    }
}

/// New storage of `n` copies of `value` when `v` does not hold `n` elements, so a size
/// change in either direction moves the charge; `None` keeps `v` (the steady state).
fn resized<T: Clone>(
    v: &BudgetVec<T>,
    n: usize,
    value: T,
) -> Result<Option<BudgetVec<T>>, StorageError> {
    if v.len() == n {
        return Ok(None);
    }
    BudgetVec::try_filled(v.budget(), value, n).map(Some)
}

/// Fill `v` with `value`, or replace it with the storage `resized` made.
fn refill<T: Clone>(v: &mut BudgetVec<T>, resized: Option<BudgetVec<T>>, value: T) {
    match resized {
        Some(new) => *v = new,
        None => v.fill(value),
    }
}
