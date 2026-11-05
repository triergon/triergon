//! Per-macroblock state kept for the whole current picture (neighbour contexts, deblocking).

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub(crate) enum Kind {
    #[default]
    None = 0,
    I4x4,
    I8x8,
    I16x16,
    IPcm,
    PSkip,
    P16x16,
    P16x8,
    P8x16,
    P8x8,
    BSkip,
    BDirect,
    B16x16,
    B16x8,
    B8x16,
    B8x8,
}

impl Kind {
    #[inline(always)]
    pub(crate) fn is_intra(self) -> bool {
        matches!(self, Kind::I4x4 | Kind::I8x8 | Kind::I16x16 | Kind::IPcm)
    }
    #[inline(always)]
    pub(crate) fn is_skip(self) -> bool {
        matches!(self, Kind::PSkip | Kind::BSkip)
    }
    #[inline(always)]
    pub(crate) fn is_inter(self) -> bool {
        !self.is_intra() && self != Kind::None
    }
}

/// CABAC coded_block_flag bits stored per macroblock.
pub(crate) const CBF_LUMA_DC: u32 = 1 << 24;
pub(crate) const CBF_CB_DC: u32 = 1 << 25;
pub(crate) const CBF_CR_DC: u32 = 1 << 26;
/// Bits 0..15: luma 4x4 (raster); 16..19 Cb AC (raster 2x2); 20..23 Cr AC.
pub(crate) const CBF_ALL: u32 = 0x07FF_FFFF;

#[derive(Clone, Copy)]
pub(crate) struct MbInfo {
    /// Slice number + 1 within the picture; 0 = not (yet) decoded.
    pub slice: u16,
    pub kind: Kind,
    pub field: bool,
    pub t8x8: bool,
    /// coded_block_pattern (luma bits 0..3, chroma in bits 4..5). I_PCM stores 0x2F.
    pub cbp: u8,
    pub qp: i8,
    pub qpc: [i8; 2],
    pub chroma_mode: u8,
    /// Bit k set: 8x8 partition k is predicted in direct mode (B_Skip / B_Direct / B_Direct_8x8).
    pub direct8: u8,
    /// Intra 4x4 / 8x8 prediction modes per raster 4x4 block (2 = DC for other MB kinds).
    pub modes: [i8; 16],
    /// TotalCoeff per 4x4 block: luma raster 0..15, Cb 16..19, Cr 20..23.
    pub nz: [u8; 24],
    pub cbf: u32,
    /// |mvd| per raster 4x4 block, [list][blk][comp], saturated at 127.
    pub mvd: [[[u8; 2]; 16]; 2],
    /// Non-zero transform coefficient flags per luma 4x4 (for deblocking bS = 2).
    pub nzmask: u16,
}

impl Default for MbInfo {
    fn default() -> Self {
        MbInfo {
            slice: 0,
            kind: Kind::None,
            field: false,
            t8x8: false,
            cbp: 0,
            qp: 0,
            qpc: [0; 2],
            chroma_mode: 0,
            direct8: 0,
            modes: [2; 16],
            nz: [0; 24],
            cbf: 0,
            mvd: [[[0; 2]; 16]; 2],
            nzmask: 0,
        }
    }
}

/// Per-slice parameters needed after the slice is parsed (deblocking).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SliceDbk {
    pub disable_idc: u32,
    pub alpha_off: i32,
    pub beta_off: i32,
}
