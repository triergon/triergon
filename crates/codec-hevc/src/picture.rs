//! Sample planes, frames and per-picture side information.

/// Sample storage type: u8 for 8-bit streams, u16 for 9..10-bit streams.
pub trait Pixel:
    crate::kernels::Kernels + Copy + Default + Send + Sync + 'static + PartialEq + core::fmt::Debug
{
    fn to_i32(self) -> i32;
    fn from_i32(v: i32) -> Self;
    const IS_U8: bool;
    fn view(f: &AnyFrame) -> Option<&Frame<Self>>;
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
}

#[derive(Debug, Clone)]
pub struct Plane<P> {
    pub data: Vec<P>,
    pub stride: usize,
    pub width: usize,
    pub height: usize,
}

impl<P: Pixel> Plane<P> {
    pub fn new(width: usize, height: usize, fill: P) -> Self {
        let stride = (width + 31) & !31;
        Plane {
            data: vec![fill; stride * height],
            stride,
            width,
            height,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Frame<P> {
    pub planes: [Plane<P>; 3],
}

impl<P: Pixel> Frame<P> {
    pub fn new(width: usize, height: usize, fill: P) -> Self {
        Frame {
            planes: [
                Plane::new(width, height, fill),
                Plane::new(width / 2, height / 2, fill),
                Plane::new(width / 2, height / 2, fill),
            ],
        }
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
    pub data: Vec<ColMv>,
}
