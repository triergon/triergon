//! Per-picture decoding state shared by the slices of a picture and the in-loop filters.

use crate::cabac::ctx;
use crate::picture::PuMotion;

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

#[derive(Debug)]
pub struct PicState {
    pub w4: usize,
    pub h4: usize,
    pub blk: Vec<BlkInfo>,
    pub motion: Vec<PuMotion>,
    /// Vertical edge marks / boundary strengths at x multiple of 8, per 4 rows.
    pub bs_v: Vec<u8>,
    /// Horizontal edge marks / boundary strengths at y multiple of 8, per 4 columns.
    pub bs_h: Vec<u8>,
    pub bs_v_stride: usize,
    pub bs_h_stride: usize,
    pub ctb_slice_addr: Vec<u32>,
    pub ctb_slice_idx: Vec<u16>,
    pub sao: Vec<SaoParams>,
    pub slices: Vec<SliceInfo>,
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
    pub row_done: Vec<u16>,
    pub filt_next: usize,
    pub sao_next: usize,
}

impl PicState {
    pub fn new() -> Self {
        PicState {
            w4: 0,
            h4: 0,
            blk: Vec::new(),
            motion: Vec::new(),
            bs_v: Vec::new(),
            bs_h: Vec::new(),
            bs_v_stride: 0,
            bs_h_stride: 0,
            ctb_slice_addr: Vec::new(),
            ctb_slice_idx: Vec::new(),
            sao: Vec::new(),
            slices: Vec::new(),
            ctx_wpp: [0; ctx::NUM],
            ctx_ds: [0; ctx::NUM],
            ctx_wpp_valid: false,
            ctx_ds_valid: false,
            ds_last_qp: 0,
            any_deblock: false,
            any_sao: false,
            any_nofilter: false,
            decoded_ctbs: 0,
            row_done: Vec::new(),
            filt_next: 0,
            sao_next: 0,
        }
    }

    /// Prepare for a new picture of the given luma size and CTB count.
    pub fn reset(&mut self, width: usize, height: usize, num_ctb: usize) {
        let w4 = width / 4;
        let h4 = height / 4;
        if self.w4 != w4 || self.h4 != h4 {
            self.w4 = w4;
            self.h4 = h4;
            self.blk = vec![BlkInfo::default(); w4 * h4];
            self.motion = vec![PuMotion::default(); w4 * h4];
            self.bs_v_stride = width / 8 + 1;
            self.bs_h_stride = w4;
            self.bs_v = vec![0; self.bs_v_stride * h4];
            self.bs_h = vec![0; self.bs_h_stride * (height / 8 + 1)];
        } else {
            // blk / motion entries are always written (in decoding order) before any read
            // within a picture: neighbour reads are gated by availability, the loop filters
            // read only decoded CTBs. Only the edge marks need clearing.
            self.bs_v.fill(0);
            self.bs_h.fill(0);
        }
        self.ctb_slice_addr.clear();
        self.ctb_slice_addr.resize(num_ctb, u32::MAX);
        self.ctb_slice_idx.clear();
        self.ctb_slice_idx.resize(num_ctb, 0);
        self.sao.clear();
        self.sao.resize(num_ctb, SaoParams::default());
        self.slices.clear();
        self.ctx_wpp_valid = false;
        self.ctx_ds_valid = false;
        self.any_deblock = false;
        self.any_sao = false;
        self.any_nofilter = false;
        self.decoded_ctbs = 0;
        self.filt_next = 0;
        self.sao_next = 0;
    }
}

impl Default for PicState {
    fn default() -> Self {
        Self::new()
    }
}
