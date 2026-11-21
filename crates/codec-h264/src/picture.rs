//! Frame buffers, per-picture motion storage, the frame stores of the decoded picture
//! buffer, and fixed-size views of their rows.

use triergon_codec::{Budget, BudgetVec, StorageError};

/// The `N` elements of `s` from `at`; a range past the end panics as indexing does.
#[inline(always)]
#[expect(
    clippy::unwrap_used,
    reason = "the range has length N; `as_chunks` compiles worse"
)]
pub(crate) fn array<T, const N: usize>(s: &[T], at: usize) -> &[T; N] {
    s[at..at + N].try_into().unwrap()
}

/// A copy of the `N` elements of `s` from `at`; a range past the end panics as indexing does.
#[inline(always)]
#[expect(
    clippy::unwrap_used,
    reason = "the range has length N; `as_chunks` compiles worse"
)]
pub(crate) fn copied<T: Copy, const N: usize>(s: &[T], at: usize) -> [T; N] {
    s[at..at + N].try_into().unwrap()
}

/// The `N` elements of `s` from `at`, mutably; a range past the end panics as indexing does.
#[inline(always)]
#[expect(
    clippy::unwrap_used,
    reason = "the range has length N; `as_chunks` compiles worse"
)]
pub(crate) fn array_mut<T, const N: usize>(s: &mut [T], at: usize) -> &mut [T; N] {
    (&mut s[at..at + N]).try_into().unwrap()
}

/// Planar 4:2:0 8-bit frame buffer (luma dimensions are multiples of 16). The planes carry
/// their charge, so output pictures sharing the buffer keep it charged.
#[derive(Debug)]
pub(crate) struct FrameBuf {
    pub(crate) y: BudgetVec<u8>,
    pub(crate) cb: BudgetVec<u8>,
    pub(crate) cr: BudgetVec<u8>,
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) cwidth: usize,
    pub(crate) cheight: usize,
}

impl FrameBuf {
    /// A buffer without samples.
    pub(crate) fn empty(budget: &Budget) -> FrameBuf {
        FrameBuf {
            y: BudgetVec::new(budget),
            cb: BudgetVec::new(budget),
            cr: BudgetVec::new(budget),
            width: 0,
            height: 0,
            cwidth: 0,
            cheight: 0,
        }
    }

    /// Monochrome streams keep the 128-filled chroma planes.
    pub(crate) fn new(
        width: usize,
        height: usize,
        budget: &Budget,
    ) -> Result<FrameBuf, StorageError> {
        let (cw, ch) = (width / 2, height / 2);
        Ok(FrameBuf {
            y: BudgetVec::try_filled(budget, 0, width * height)?,
            cb: BudgetVec::try_filled(budget, 128, cw * ch)?,
            cr: BudgetVec::try_filled(budget, 128, cw * ch)?,
            width,
            height,
            cwidth: cw,
            cheight: ch,
        })
    }

    pub(crate) fn try_clone(&self) -> Result<FrameBuf, StorageError> {
        Ok(FrameBuf {
            y: self.y.try_clone()?,
            cb: self.cb.try_clone()?,
            cr: self.cr.try_clone()?,
            ..*self
        })
    }
}

/// Motion data of one coded picture (frame, or one field). Indexed MB-major:
/// mv: `mb * 16 + raster4x4` (`mb * 4 + 8x8 index` when `corners`), ref_idx/ref_id:
/// `mb * 4 + 8x8 index`.
#[derive(Debug)]
pub(crate) struct Motion {
    pub(crate) mv: [BudgetVec<[i16; 2]>; 2],
    /// Only the corner 4x4 block of each 8x8 is kept (a stored reference of a frame-only
    /// stream with direct_8x8_inference_flag, whose co-located blocks are always corners).
    pub(crate) corners: bool,
    pub(crate) ref_idx: [BudgetVec<i8>; 2],
    pub(crate) ref_id: [BudgetVec<i32>; 2],
    /// Per MB: bit 0 intra, bit 1 field macroblock.
    pub(crate) mb_flags: BudgetVec<u8>,
    pub(crate) mbs: usize,
}

pub(crate) const MBF_INTRA: u8 = 1;
pub(crate) const MBF_FIELD: u8 = 2;

impl Motion {
    /// Motion of no macroblocks.
    pub(crate) fn empty(budget: &Budget) -> Motion {
        Motion {
            mv: [BudgetVec::new(budget), BudgetVec::new(budget)],
            corners: false,
            ref_idx: [BudgetVec::new(budget), BudgetVec::new(budget)],
            ref_id: [BudgetVec::new(budget), BudgetVec::new(budget)],
            mb_flags: BudgetVec::new(budget),
            mbs: 0,
        }
    }

    /// Arrays for `mbs` macroblocks, all blocks unreferenced. The current arrays are freed
    /// before the new ones are charged.
    fn reset(&mut self, mbs: usize, corners: bool) -> Result<(), StorageError> {
        let budget = self.mb_flags.budget().clone();
        *self = Motion::empty(&budget);
        let (mvs, blocks) = (mbs * if corners { 4 } else { 16 }, mbs * 4);
        *self = Motion {
            mv: [
                BudgetVec::try_filled(&budget, [0; 2], mvs)?,
                BudgetVec::try_filled(&budget, [0; 2], mvs)?,
            ],
            corners,
            ref_idx: [
                BudgetVec::try_filled(&budget, -1, blocks)?,
                BudgetVec::try_filled(&budget, -1, blocks)?,
            ],
            ref_id: [
                BudgetVec::try_filled(&budget, -1, blocks)?,
                BudgetVec::try_filled(&budget, -1, blocks)?,
            ],
            mb_flags: BudgetVec::try_filled(&budget, 0, mbs)?,
            mbs,
        };
        Ok(())
    }

    pub(crate) fn alloc(&mut self, mbs: usize) -> Result<(), StorageError> {
        if self.mbs != mbs || self.corners {
            self.reset(mbs, false)?;
        }
        Ok(())
    }
}

impl Motion {
    /// Motion vector of 4x4 block `b4` of MB `mb` (with `corners`, b4 must be a corner block).
    #[inline(always)]
    pub(crate) fn mv_at(&self, l: usize, mb: usize, b4: usize) -> [i16; 2] {
        debug_assert!(!self.corners || matches!(b4, 0 | 3 | 12 | 15));
        if self.corners {
            self.mv[l][mb * 4 + (b4 / 8) * 2 + (b4 % 4) / 2]
        } else {
            self.mv[l][mb * 16 + b4]
        }
    }

    /// Store `full` into `self` keeping only corner motion vectors (allocation reused).
    pub(crate) fn compact_from(&mut self, full: &Motion) -> Result<(), StorageError> {
        if !self.corners || self.mbs != full.mbs {
            self.reset(full.mbs, true)?;
        }
        for l in 0..2 {
            for (d, s) in self.mv[l]
                .chunks_exact_mut(4)
                .zip(full.mv[l].chunks_exact(16))
            {
                d[0] = s[0];
                d[1] = s[3];
                d[2] = s[12];
                d[3] = s[15];
            }
            self.ref_idx[l].copy_from_slice(&full.ref_idx[l]);
            self.ref_id[l].copy_from_slice(&full.ref_id[l]);
        }
        self.mb_flags.copy_from_slice(&full.mb_flags);
        Ok(())
    }
}

/// How a frame store's content was coded (Table 8-7 PicCodingStruct).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum Coding {
    #[default]
    Frame,
    Mbaff,
    Fields,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct OutputInfo {
    pub(crate) crop: (u32, u32, u32, u32),
    pub(crate) sar: (u32, u32),
    pub(crate) colour: crate::ColourDescription,
}

/// The contents of a frame buffer of the decoded picture buffer: a frame, a complementary
/// field pair or a single field. Its reference marking and output state are kept by the
/// buffer (`triergon_framing::picture_output::DecodedPictures`) that stores it.
#[derive(Debug)]
pub(crate) struct FrameStore {
    /// The frame buffer's number. A picture takes the free frame buffer with the lowest
    /// number, so buffers, and the samples they keep, are reused in a fixed order.
    pub(crate) number: usize,
    /// Shared with output pictures; the decoder writes only into an unshared buffer. None
    /// before first use and while the store's picture is being decoded.
    pub(crate) buf: Option<std::sync::Arc<FrameBuf>>,
    /// Frame motion in `motion[0]`, or top / bottom field motion in `motion[0]` / `motion[1]`.
    pub(crate) motion: [Motion; 2],
    pub(crate) uid: u32,
    /// Decoded fields: bit 0 top, bit 1 bottom (3 = frame or both fields).
    pub(crate) fields: u8,
    pub(crate) coding: Coding,
    pub(crate) frame_num: u32,
    pub(crate) long_term_frame_idx: u32,
    pub(crate) poc: [i32; 2],
    pub(crate) pts: Option<i64>,
    pub(crate) non_existing: bool,
    pub(crate) info: OutputInfo,
    pub(crate) idr: bool,
}

impl FrameStore {
    /// Frame buffer `number`, empty and without samples.
    pub(crate) fn new(number: usize, budget: &Budget) -> FrameStore {
        FrameStore {
            number,
            buf: None,
            motion: [Motion::empty(budget), Motion::empty(budget)],
            uid: 0,
            fields: 0,
            coding: Coding::default(),
            frame_num: 0,
            long_term_frame_idx: 0,
            poc: [0; 2],
            pts: None,
            non_existing: false,
            info: OutputInfo::default(),
            idr: false,
        }
    }
    /// PicOrderCnt of the frame / complementary field pair (8-1): min of the decoded fields.
    pub(crate) fn frame_poc(&self) -> i32 {
        match self.fields {
            1 => self.poc[0],
            2 => self.poc[1],
            _ => self.poc[0].min(self.poc[1]),
        }
    }
}

/// An entry of RefPicList0/1: a frame (parity 0) or a field (1 top, 2 bottom) of a DPB entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RefPic {
    pub(crate) idx: usize,
    pub(crate) parity: u8,
    pub(crate) poc: i32,
    pub(crate) long: bool,
    /// Identity used to compare reference pictures (uid << 2 | structure code).
    pub(crate) id: i32,
}

pub(crate) fn ref_id(uid: u32, parity: u8) -> i32 {
    ((uid as i32) << 2) | if parity == 0 { 3 } else { parity as i32 }
}
