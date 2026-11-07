//! Sample planes, frames and per-picture side information.

use std::sync::{Arc, Mutex, Weak};

use triergon_codec::{Budget, BudgetVec, StorageError};

/// Sample storage type: u8 for 8-bit streams, u16 for 9..10-bit streams.
pub trait Pixel:
    crate::kernels::Kernels + Copy + Default + Send + Sync + 'static + PartialEq + core::fmt::Debug
{
    fn to_i32(self) -> i32;
    fn from_i32(v: i32) -> Self;
    const IS_U8: bool;
    fn view(f: &AnyFrame) -> Option<&Frame<Self>>;
    fn slot(pool: &FramePool) -> &Mutex<Option<Frame<Self>>>;
}

impl Pixel for u8 {
    #[inline(always)]
    fn to_i32(self) -> i32 {
        self as i32
    }
    #[inline(always)]
    fn from_i32(v: i32) -> Self {
        v as u8
    }
    const IS_U8: bool = true;
    fn view(f: &AnyFrame) -> Option<&Frame<Self>> {
        match f {
            AnyFrame::U8(f) => Some(f),
            _ => None,
        }
    }
    fn slot(pool: &FramePool) -> &Mutex<Option<Frame<Self>>> {
        &pool.u8
    }
}

impl Pixel for u16 {
    #[inline(always)]
    fn to_i32(self) -> i32 {
        self as i32
    }
    #[inline(always)]
    fn from_i32(v: i32) -> Self {
        v as u16
    }
    const IS_U8: bool = false;
    fn view(f: &AnyFrame) -> Option<&Frame<Self>> {
        match f {
            AnyFrame::U16(f) => Some(f),
            _ => None,
        }
    }
    fn slot(pool: &FramePool) -> &Mutex<Option<Frame<Self>>> {
        &pool.u16
    }
}

#[derive(Debug)]
pub struct Plane<P> {
    pub data: BudgetVec<P>,
    pub stride: usize,
    pub width: usize,
    pub height: usize,
}

impl<P: Pixel> Plane<P> {
    pub fn try_new(
        budget: &Budget,
        width: usize,
        height: usize,
        fill: P,
    ) -> Result<Self, StorageError> {
        let stride = (width + 31) & !31;
        let len = stride.checked_mul(height).ok_or(StorageError::Overflow)?;
        Ok(Plane {
            data: BudgetVec::try_filled(budget, fill, len)?,
            stride,
            width,
            height,
        })
    }
}

#[cfg(test)]
impl<P: Pixel> Plane<P> {
    /// A plane on its own unlimited pool, for the micro-benchmarks.
    pub fn new(width: usize, height: usize, fill: P) -> Self {
        Self::try_new(&Budget::new(usize::MAX).unwrap(), width, height, fill).unwrap()
    }
}

/// Three sample planes. They carry their charge, so a frame stays charged wherever it
/// is kept: DPB, frame pool or a caller's picture.
#[derive(Debug)]
pub struct Frame<P> {
    pub planes: [Plane<P>; 3],
}

impl<P: Pixel> Frame<P> {
    /// A 4:2:0 frame.
    pub fn try_new(
        budget: &Budget,
        width: usize,
        height: usize,
        fill: P,
    ) -> Result<Self, StorageError> {
        let chroma = (width / 2, height / 2);
        Self::with_planes(budget, [(width, height), chroma, chroma], fill)
    }
    /// A frame of planes of the given (width, height).
    pub fn with_planes(
        budget: &Budget,
        sizes: [(usize, usize); 3],
        fill: P,
    ) -> Result<Self, StorageError> {
        let [(w0, h0), (w1, h1), (w2, h2)] = sizes;
        Ok(Frame {
            planes: [
                Plane::try_new(budget, w0, h0, fill)?,
                Plane::try_new(budget, w1, h1, fill)?,
                Plane::try_new(budget, w2, h2, fill)?,
            ],
        })
    }
    pub fn same_geometry(&self, w: usize, h: usize) -> bool {
        self.planes[0].width == w && self.planes[0].height == h
    }
}

/// A decoded frame of either sample type.
#[derive(Debug)]
pub enum AnyFrame {
    U8(Frame<u8>),
    U16(Frame<u16>),
}

/// A decoder's spare frames, one per sample type, as in a buffer pool: a frame returns
/// here when its last holder (the DPB or a caller's picture) drops it.
#[derive(Debug, Default)]
pub struct FramePool {
    u8: Mutex<Option<Frame<u8>>>,
    u16: Mutex<Option<Frame<u16>>>,
}

impl FramePool {
    /// The spare frame of this sample type if it has this luma size; a spare of another
    /// size is released.
    pub fn take<P: Pixel>(&self, width: usize, height: usize) -> Option<Frame<P>> {
        P::slot(self)
            .lock()
            .ok()?
            .take()
            .filter(|f| f.same_geometry(width, height))
    }

    fn put(&self, frame: AnyFrame) {
        fn keep<P>(slot: &Mutex<Option<Frame<P>>>, frame: Frame<P>) {
            if let Ok(mut spare) = slot.lock()
                && spare.is_none()
            {
                *spare = Some(frame);
            }
        }
        match frame {
            AnyFrame::U8(f) => keep(&self.u8, f),
            AnyFrame::U16(f) => keep(&self.u16, f),
        }
    }
}

/// A frame shared by the DPB and the output pictures that show it.
#[derive(Debug)]
pub struct SharedFrame {
    /// Taken only by `drop`.
    frame: Option<AnyFrame>,
    pool: Weak<FramePool>,
}

impl SharedFrame {
    pub fn new(frame: AnyFrame, pool: &Arc<FramePool>) -> Arc<Self> {
        Arc::new(SharedFrame {
            frame: Some(frame),
            pool: Arc::downgrade(pool),
        })
    }
}

impl core::ops::Deref for SharedFrame {
    type Target = AnyFrame;

    #[inline]
    fn deref(&self) -> &AnyFrame {
        #[allow(clippy::expect_used)] // Only `drop` takes the frame.
        self.frame.as_ref().expect("frame present until dropped")
    }
}

impl Drop for SharedFrame {
    fn drop(&mut self) {
        if let (Some(frame), Some(pool)) = (self.frame.take(), self.pool.upgrade()) {
            pool.put(frame);
        }
    }
}

/// Motion vector in quarter luma samples.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Mv {
    pub x: i16,
    pub y: i16,
}

impl Mv {
    pub const ZERO: Mv = Mv { x: 0, y: 0 };
    #[inline(always)]
    pub fn new(x: i32, y: i32) -> Mv {
        Mv {
            x: x as i16,
            y: y as i16,
        }
    }
}

/// Motion of one 4x4 luma block. `ref_idx[l] < 0` means list l is unused; the matching mv is zero.
#[derive(Clone, Copy, Eq, Debug)]
#[repr(C)]
pub struct PuMotion {
    pub mv: [Mv; 2],
    pub ref_idx: [i8; 2],
}

impl PartialEq for PuMotion {
    /// Field-wise equality evaluated on packed words (two compares instead of six).
    #[inline(always)]
    fn eq(&self, o: &Self) -> bool {
        self.key() == o.key()
    }
}

impl Default for PuMotion {
    fn default() -> Self {
        PuMotion {
            mv: [Mv::ZERO; 2],
            ref_idx: [-1, -1],
        }
    }
}

impl PuMotion {
    #[inline(always)]
    fn key(&self) -> (u64, u16) {
        let m = |v: Mv| (v.x as u16 as u64) | ((v.y as u16 as u64) << 16);
        (
            m(self.mv[0]) | (m(self.mv[1]) << 32),
            (self.ref_idx[0] as u8 as u16) | ((self.ref_idx[1] as u8 as u16) << 8),
        )
    }
    #[inline(always)]
    pub fn is_inter(&self) -> bool {
        self.ref_idx[0] >= 0 || self.ref_idx[1] >= 0
    }
}

/// Collocated motion (16x16 granularity) kept with a reference picture for TMVP.
#[derive(Debug, Clone, Copy, Default)]
pub struct ColMv {
    pub mv: [Mv; 2],
    pub ref_poc: [i32; 2],
    /// bit0/1: predFlagL0/L1, bit2/3: L0/L1 reference was long-term.
    pub flags: u8,
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct ColMotion {
    pub w16: usize,
    pub h16: usize,
    pub data: BudgetVec<ColMv>,
}

impl ColMotion {
    /// Default (unused) motion for a picture of this luma size.
    pub fn try_new(budget: &Budget, width: usize, height: usize) -> Result<Self, StorageError> {
        let w16 = width.div_ceil(16);
        let h16 = height.div_ceil(16);
        let len = w16.checked_mul(h16).ok_or(StorageError::Overflow)?;
        Ok(ColMotion {
            w16,
            h16,
            data: BudgetVec::try_filled(budget, ColMv::default(), len)?,
        })
    }
}
