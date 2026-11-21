//! Per-picture decoding state shared by the slices of a picture and the in-loop filters.

use crate::cabac::ctx;
use crate::picture::PuMotion;
use triergon_codec::{Budget, BudgetVec, StorageError};

pub(crate) const F_INTRA: u8 = 1;
pub(crate) const F_SKIP: u8 = 2;
/// Samples must not be modified by the in-loop filters (PCM with pcm_loop_filter_disabled_flag,
/// or cu_transquant_bypass_flag).
pub(crate) const F_NOFILTER: u8 = 4;
pub(crate) const F_PCM: u8 = 8;
/// The luma transform block covering this 4x4 block has non-zero coefficients.
pub(crate) const F_NZ: u8 = 16;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct BlkInfo {
    pub(crate) flags: u8,
    pub(crate) depth: u8,
    pub(crate) ipm: u8,
    pub(crate) qp: i8,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SaoParams {
    pub(crate) type_idx: [u8; 3],
    pub(crate) band_pos: [u8; 3],
    pub(crate) eo_class: [u8; 3],
    pub(crate) offset: [[i16; 4]; 3],
}

/// Information about a slice segment of the current picture needed after parsing.
#[derive(Debug, Clone)]
pub(crate) struct SliceInfo {
    pub(crate) beta_offset_div2: i32,
    pub(crate) tc_offset_div2: i32,
    pub(crate) lf_across_slices: bool,
    pub(crate) ref_poc: [[i32; 16]; 2],
    pub(crate) ref_lt: [[bool; 16]; 2],
}

/// Per-picture block, motion, edge and CTB maps, sized when a picture starts and reused
/// while the picture size stays the same.
#[derive(Debug)]
pub(crate) struct PicState {
    pub(crate) w4: usize,
    pub(crate) h4: usize,
    pub(crate) blk: BudgetVec<BlkInfo>,
    pub(crate) motion: BudgetVec<PuMotion>,
    /// Vertical edge marks / boundary strengths at x multiple of 8, per 4 rows.
    pub(crate) bs_v: BudgetVec<u8>,
    /// Horizontal edge marks / boundary strengths at y multiple of 8, per 4 columns.
    pub(crate) bs_h: BudgetVec<u8>,
    pub(crate) bs_v_stride: usize,
    pub(crate) bs_h_stride: usize,
    pub(crate) ctb_slice_addr: BudgetVec<u32>,
    pub(crate) ctb_slice_idx: BudgetVec<u16>,
    pub(crate) sao: BudgetVec<SaoParams>,
    pub(crate) slices: BudgetVec<SliceInfo>,
    pub(crate) ctx_wpp: [u8; ctx::NUM],
    pub(crate) ctx_ds: [u8; ctx::NUM],
    pub(crate) ctx_wpp_valid: bool,
    pub(crate) ctx_ds_valid: bool,
    /// qPY_PREV carried into a dependent slice segment.
    pub(crate) ds_last_qp: i32,
    pub(crate) any_deblock: bool,
    pub(crate) any_sao: bool,
    pub(crate) any_nofilter: bool,
    pub(crate) decoded_ctbs: usize,
    /// Decoded CTBs per CTB row, and the next rows to deblock / to SAO (in-loop filters
    /// run row by row during decoding, see decoder::filter_ready).
    pub(crate) row_done: BudgetVec<u16>,
    pub(crate) filt_next: usize,
    pub(crate) sao_next: usize,
}

impl PicState {
    pub(crate) fn new(budget: &Budget) -> Self {
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
    pub(crate) fn reset(
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
    pub(crate) fn reserve_slice(&mut self) -> Result<(), StorageError> {
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
