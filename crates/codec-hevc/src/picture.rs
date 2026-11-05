//! Sample planes, frames and per-picture side information.

/// Sample storage type: u8 for 8-bit streams, u16 for 9..10-bit streams.
pub trait Pixel: Copy + Default + Send + Sync + 'static + PartialEq + core::fmt::Debug {
    fn to_i32(self) -> i32;
    fn from_i32(v: i32) -> Self;
    const IS_U8: bool;
    fn view(f: &AnyFrame) -> Option<&Frame<Self>>;
    // Optional explicit-SIMD row kernels (see mc_simd.rs). Each returns the number of
    // leading columns it produced; the generic scalar code finishes the row.
    #[inline(always)]
    fn simd_taps8(
        _r: [&[Self]; 8],
        _c: &[i8; 8],
        _w: usize,
        _mode: u8,
        _p: &mut [i16],
        _d: &mut [Self],
        _o: &[i16],
    ) -> usize {
        0
    }
    #[inline(always)]
    fn simd_taps4(
        _r: [&[Self]; 4],
        _c: &[i8; 4],
        _w: usize,
        _mode: u8,
        _p: &mut [i16],
        _d: &mut [Self],
        _o: &[i16],
    ) -> usize {
        0
    }
    #[inline(always)]
    fn simd_hpass8(_r: [&[Self]; 8], _c: &[i8; 8], _w: usize, _t: &mut [i16]) -> usize {
        0
    }
    #[inline(always)]
    fn simd_hpass4(_r: [&[Self]; 4], _c: &[i8; 4], _w: usize, _t: &mut [i16]) -> usize {
        0
    }
    #[inline(always)]
    fn simd_vpass8(
        _r: [&[i16]; 8],
        _c: &[i8; 8],
        _w: usize,
        _mode: u8,
        _p: &mut [i16],
        _d: &mut [Self],
        _o: &[i16],
    ) -> usize {
        0
    }
    #[inline(always)]
    fn simd_vpass4(
        _r: [&[i16]; 4],
        _c: &[i8; 4],
        _w: usize,
        _mode: u8,
        _p: &mut [i16],
        _d: &mut [Self],
        _o: &[i16],
    ) -> usize {
        0
    }
    /// Explicit weighted uni / bi combine (w % 8 == 0). Returns false if not handled.
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn simd_wuni(
        _d: &mut [Self],
        _doff: usize,
        _ds: usize,
        _s: &[i16],
        _w: usize,
        _h: usize,
        _log2wd: u32,
        _wt: i32,
        _o: i32,
        _max: i32,
    ) -> bool {
        false
    }
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn simd_wbi(
        _d: &mut [Self],
        _doff: usize,
        _ds: usize,
        _a: &[i16],
        _b: &[i16],
        _w: usize,
        _h: usize,
        _log2wd: u32,
        _w0: i32,
        _w1: i32,
        _rnd: i32,
        _max: i32,
    ) -> bool {
        false
    }
    /// Fused integer-position explicit weighted uni-prediction (w % 8 == 0).
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn simd_wcopy(
        _d: &mut [Self],
        _doff: usize,
        _ds: usize,
        _s: &[Self],
        _soff: usize,
        _ss: usize,
        _w: usize,
        _h: usize,
        _shift3: u32,
        _log2wd: u32,
        _wt: i32,
        _o: i32,
        _max: i32,
    ) -> bool {
        false
    }
    /// Integer-position prediction (output modes 0 / 2), w % 8 == 0. Returns false if not handled.
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn simd_int(
        _src: &[Self],
        _off: usize,
        _ss: usize,
        _w: usize,
        _h: usize,
        _bd: u32,
        _mode: u8,
        _p: &mut [i16],
        _d: &mut [Self],
        _doff: usize,
        _ds: usize,
        _o: &[i16],
    ) -> bool {
        false
    }
    /// The SIMD in-loop filter kernels below are available.
    const SIMD_FILTERS: bool = false;
    /// Horizontal luma deblocking edge over 16 columns (see filter_simd.rs).
    #[inline(always)]
    fn simd_luma_h16(
        _d: &mut [Self],
        _pos: usize,
        _stride: usize,
        _prm: &crate::deblock::LumaLanes,
        _max: i32,
    ) {
    }
    /// Vertical luma deblocking edge over 16 rows (see filter_simd.rs).
    #[inline(always)]
    fn simd_luma_v16(
        _d: &mut [Self],
        _pos: usize,
        _stride: usize,
        _prm: &crate::deblock::LumaLanes,
        _max: i32,
    ) {
    }
    /// Chroma deblocking edge over 16 lines (see filter_simd.rs); `vert` selects EDGE_VER.
    #[inline(always)]
    fn simd_chroma16(
        _vert: bool,
        _d: &mut [Self],
        _pos: usize,
        _stride: usize,
        _prm: &crate::deblock::LumaLanes,
        _max: i32,
    ) {
    }
    /// SAO edge offset on a row (see filter_simd.rs); returns the leading samples done.
    #[inline(always)]
    fn simd_sao_eo(_c: &[Self], _a: &[Self], _b: &[Self], _d: &mut [Self], _t: &[i16; 4]) -> usize {
        0
    }
    /// SAO band offset on a row; returns the leading samples done.
    #[inline(always)]
    fn simd_sao_bo(_s: &[Self], _d: &mut [Self], _band_pos: u8, _o: &[i16; 4]) -> usize {
        0
    }
    /// >8-bit SAO edge offset (max = (1 << bitDepth) - 1); returns the samples done.
    #[inline(always)]
    fn simd_sao_eo_hi(
        _c: &[Self],
        _a: &[Self],
        _b: &[Self],
        _d: &mut [Self],
        _t: &[i16; 4],
        _max: i16,
    ) -> usize {
        0
    }
    /// >8-bit SAO band offset with bandShift `shift`; returns the samples done.
    #[inline(always)]
    fn simd_sao_bo_hi(
        _s: &[Self],
        _d: &mut [Self],
        _shift: u32,
        _band_pos: u8,
        _o: &[i16; 4],
        _max: i16,
    ) -> usize {
        0
    }
    /// Whole-block fractional luma / chroma MC (w % 8 == 0). Returns false if not handled.
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn simd_block(
        _luma: bool,
        _src: &[Self],
        _off: usize,
        _ss: usize,
        _fx: usize,
        _fy: usize,
        _w: usize,
        _h: usize,
        _bd: u32,
        _mode: u8,
        _p: &mut [i16],
        _d: &mut [Self],
        _doff: usize,
        _ds: usize,
        _o: &[i16],
    ) -> bool {
        false
    }
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
    #[cfg(feature = "simd")]
    const SIMD_FILTERS: bool = true;
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_luma_h16(
        d: &mut [u8],
        pos: usize,
        stride: usize,
        prm: &crate::deblock::LumaLanes,
        _max: i32,
    ) {
        crate::filter_simd::luma_h16_u8(d, pos, stride, prm)
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_luma_v16(
        d: &mut [u8],
        pos: usize,
        stride: usize,
        prm: &crate::deblock::LumaLanes,
        _max: i32,
    ) {
        crate::filter_simd::luma_v16_u8(d, pos, stride, prm)
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_chroma16(
        vert: bool,
        d: &mut [u8],
        pos: usize,
        stride: usize,
        prm: &crate::deblock::LumaLanes,
        _max: i32,
    ) {
        if vert {
            crate::filter_simd::chroma_v16_u8(d, pos, stride, prm)
        } else {
            crate::filter_simd::chroma_h16_u8(d, pos, stride, prm)
        }
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_sao_eo(c: &[u8], a: &[u8], b: &[u8], d: &mut [u8], t: &[i16; 4]) -> usize {
        crate::filter_simd::sao_eo(c, a, b, d, t)
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_sao_bo(s: &[u8], d: &mut [u8], band_pos: u8, o: &[i16; 4]) -> usize {
        crate::filter_simd::sao_bo(s, d, band_pos, o)
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_wuni(
        d: &mut [u8],
        doff: usize,
        ds: usize,
        s: &[i16],
        w: usize,
        h: usize,
        log2wd: u32,
        wt: i32,
        o: i32,
        max: i32,
    ) -> bool {
        if w % 8 != 0 {
            return false;
        }
        crate::mc_simd::wuni(
            d,
            doff,
            ds,
            s,
            w,
            h,
            log2wd,
            wt,
            o,
            max,
            crate::mc_simd::store_u8,
        );
        true
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_wbi(
        d: &mut [u8],
        doff: usize,
        ds: usize,
        a: &[i16],
        b: &[i16],
        w: usize,
        h: usize,
        log2wd: u32,
        w0: i32,
        w1: i32,
        rnd: i32,
        max: i32,
    ) -> bool {
        if w % 8 != 0 {
            return false;
        }
        crate::mc_simd::wbi(
            d,
            doff,
            ds,
            a,
            b,
            w,
            h,
            log2wd,
            w0,
            w1,
            rnd,
            max,
            crate::mc_simd::store_u8,
        );
        true
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_wcopy(
        d: &mut [u8],
        doff: usize,
        ds: usize,
        s: &[u8],
        soff: usize,
        ss: usize,
        w: usize,
        h: usize,
        shift3: u32,
        log2wd: u32,
        wt: i32,
        o: i32,
        max: i32,
    ) -> bool {
        if w % 8 != 0 {
            return false;
        }
        crate::mc_simd::wcopy(
            d,
            doff,
            ds,
            s,
            soff,
            ss,
            w,
            h,
            shift3,
            log2wd,
            wt,
            o,
            max,
            crate::mc_simd::load_u8,
            crate::mc_simd::store_u8,
        );
        true
    }
    fn view(f: &AnyFrame) -> Option<&Frame<Self>> {
        match f {
            AnyFrame::U8(f) => Some(f),
            _ => None,
        }
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_taps8(
        r: [&[u8]; 8],
        c: &[i8; 8],
        w: usize,
        mode: u8,
        p: &mut [i16],
        d: &mut [u8],
        o: &[i16],
    ) -> usize {
        crate::mc_simd::taps8_row(r, c, w, mode, p, d, o)
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_taps4(
        r: [&[u8]; 4],
        c: &[i8; 4],
        w: usize,
        mode: u8,
        p: &mut [i16],
        d: &mut [u8],
        o: &[i16],
    ) -> usize {
        crate::mc_simd::taps4_row(r, c, w, mode, p, d, o)
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_hpass8(r: [&[u8]; 8], c: &[i8; 8], w: usize, t: &mut [i16]) -> usize {
        crate::mc_simd::hpass8_row(r, c, w, t)
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_hpass4(r: [&[u8]; 4], c: &[i8; 4], w: usize, t: &mut [i16]) -> usize {
        crate::mc_simd::hpass4_row(r, c, w, t)
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_vpass8(
        r: [&[i16]; 8],
        c: &[i8; 8],
        w: usize,
        mode: u8,
        p: &mut [i16],
        d: &mut [u8],
        o: &[i16],
    ) -> usize {
        crate::mc_simd::vpass8_row(r, c, w, mode, p, d, o)
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_vpass4(
        r: [&[i16]; 4],
        c: &[i8; 4],
        w: usize,
        mode: u8,
        p: &mut [i16],
        d: &mut [u8],
        o: &[i16],
    ) -> usize {
        crate::mc_simd::vpass4_row(r, c, w, mode, p, d, o)
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_int(
        src: &[u8],
        off: usize,
        ss: usize,
        w: usize,
        h: usize,
        _bd: u32,
        mode: u8,
        p: &mut [i16],
        d: &mut [u8],
        doff: usize,
        ds: usize,
        o: &[i16],
    ) -> bool {
        // 8-sample loads: a 4-wide last strip reads 4 samples past the block.
        if w % 4 != 0 || mode == 1 || off + (h - 1) * ss + w.next_multiple_of(8) > src.len() {
            return false;
        }
        let mut bo = crate::mc_simd::BlockOut {
            pred: p,
            dst: d,
            dst_off: doff,
            dst_stride: ds,
            other: o,
            n: 8,
        };
        crate::mc_simd::int_block(src, off, ss, w, h, mode, &mut bo);
        true
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_block(
        luma: bool,
        src: &[u8],
        off: usize,
        ss: usize,
        fx: usize,
        fy: usize,
        w: usize,
        h: usize,
        _bd: u32,
        mode: u8,
        p: &mut [i16],
        d: &mut [u8],
        doff: usize,
        ds: usize,
        o: &[i16],
    ) -> bool {
        // 16-sample loads may read up to 13 samples past the filter support.
        let rows = if luma { h + 7 } else { h + 3 };
        let start = off - if luma { 3 * ss + 3 } else { ss + 1 };
        if w % 4 != 0 || w > 16 || start + (rows - 1) * ss + w + 16 > src.len() {
            return false;
        }
        let mut bo = crate::mc_simd::BlockOut {
            pred: p,
            dst: d,
            dst_off: doff,
            dst_stride: ds,
            other: o,
            n: 8,
        };
        if luma {
            let (cx, cy) = (
                &crate::tables::LUMA_FILTER[fx],
                &crate::tables::LUMA_FILTER[fy],
            );
            crate::mc_simd::luma_block(src, off, ss, fx, fy, w, h, cx, cy, mode, &mut bo, &mut []);
        } else {
            let (cx, cy) = (
                &crate::tables::CHROMA_FILTER[fx],
                &crate::tables::CHROMA_FILTER[fy],
            );
            crate::mc_simd::chroma_block(src, off, ss, fx, fy, w, h, cx, cy, mode, &mut bo);
        }
        true
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
    #[cfg(feature = "simd")]
    const SIMD_FILTERS: bool = true;
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_luma_h16(
        d: &mut [u16],
        pos: usize,
        stride: usize,
        prm: &crate::deblock::LumaLanes,
        max: i32,
    ) {
        crate::filter_simd::luma_h16_u16(d, pos, stride, prm, max as i16)
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_luma_v16(
        d: &mut [u16],
        pos: usize,
        stride: usize,
        prm: &crate::deblock::LumaLanes,
        max: i32,
    ) {
        crate::filter_simd::luma_v16_u16(d, pos, stride, prm, max as i16)
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_sao_eo_hi(
        c: &[u16],
        a: &[u16],
        b: &[u16],
        d: &mut [u16],
        t: &[i16; 4],
        max: i16,
    ) -> usize {
        crate::filter_simd::sao_eo_u16(c, a, b, d, t, max)
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_sao_bo_hi(
        s: &[u16],
        d: &mut [u16],
        shift: u32,
        band_pos: u8,
        o: &[i16; 4],
        max: i16,
    ) -> usize {
        crate::filter_simd::sao_bo_u16(s, d, shift, band_pos, o, max)
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_chroma16(
        vert: bool,
        d: &mut [u16],
        pos: usize,
        stride: usize,
        prm: &crate::deblock::LumaLanes,
        max: i32,
    ) {
        if vert {
            crate::filter_simd::chroma_v16_u16(d, pos, stride, prm, max as i16)
        } else {
            crate::filter_simd::chroma_h16_u16(d, pos, stride, prm, max as i16)
        }
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_wuni(
        d: &mut [u16],
        doff: usize,
        ds: usize,
        s: &[i16],
        w: usize,
        h: usize,
        log2wd: u32,
        wt: i32,
        o: i32,
        max: i32,
    ) -> bool {
        if w % 8 != 0 {
            return false;
        }
        crate::mc_simd::wuni(
            d,
            doff,
            ds,
            s,
            w,
            h,
            log2wd,
            wt,
            o,
            max,
            crate::mc_simd::store_u16,
        );
        true
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_wbi(
        d: &mut [u16],
        doff: usize,
        ds: usize,
        a: &[i16],
        b: &[i16],
        w: usize,
        h: usize,
        log2wd: u32,
        w0: i32,
        w1: i32,
        rnd: i32,
        max: i32,
    ) -> bool {
        if w % 8 != 0 {
            return false;
        }
        crate::mc_simd::wbi(
            d,
            doff,
            ds,
            a,
            b,
            w,
            h,
            log2wd,
            w0,
            w1,
            rnd,
            max,
            crate::mc_simd::store_u16,
        );
        true
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_wcopy(
        d: &mut [u16],
        doff: usize,
        ds: usize,
        s: &[u16],
        soff: usize,
        ss: usize,
        w: usize,
        h: usize,
        shift3: u32,
        log2wd: u32,
        wt: i32,
        o: i32,
        max: i32,
    ) -> bool {
        if w % 8 != 0 {
            return false;
        }
        crate::mc_simd::wcopy(
            d,
            doff,
            ds,
            s,
            soff,
            ss,
            w,
            h,
            shift3,
            log2wd,
            wt,
            o,
            max,
            crate::mc_simd::load_u16,
            crate::mc_simd::store_u16,
        );
        true
    }
    fn view(f: &AnyFrame) -> Option<&Frame<Self>> {
        match f {
            AnyFrame::U16(f) => Some(f),
            _ => None,
        }
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_int(
        src: &[u16],
        off: usize,
        ss: usize,
        w: usize,
        h: usize,
        bd: u32,
        mode: u8,
        p: &mut [i16],
        d: &mut [u16],
        doff: usize,
        ds: usize,
        o: &[i16],
    ) -> bool {
        if w % 8 != 0 || mode == 1 {
            return false;
        }
        let mut bo = crate::mc_simd::BlockOut16 {
            pred: p,
            dst: d,
            dst_off: doff,
            dst_stride: ds,
            other: o,
        };
        crate::mc_simd::block_hi(src, off, ss, 0, 0, w, h, true, bd, mode, &mut bo);
        true
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn simd_block(
        luma: bool,
        src: &[u16],
        off: usize,
        ss: usize,
        fx: usize,
        fy: usize,
        w: usize,
        h: usize,
        bd: u32,
        mode: u8,
        p: &mut [i16],
        d: &mut [u16],
        doff: usize,
        ds: usize,
        o: &[i16],
    ) -> bool {
        if w % 8 != 0 {
            return false;
        }
        let mut bo = crate::mc_simd::BlockOut16 {
            pred: p,
            dst: d,
            dst_off: doff,
            dst_stride: ds,
            other: o,
        };
        crate::mc_simd::block_hi(src, off, ss, fx, fy, w, h, luma, bd, mode, &mut bo);
        true
    }
}

#[derive(Clone)]
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

#[derive(Clone)]
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
#[derive(Clone, Copy, Default)]
pub struct ColMv {
    pub mv: [Mv; 2],
    pub ref_poc: [i32; 2],
    /// bit0/1: predFlagL0/L1, bit2/3: L0/L1 reference was long-term.
    pub flags: u8,
}

#[allow(dead_code)]
pub struct ColMotion {
    pub w16: usize,
    pub h16: usize,
    pub data: Vec<ColMv>,
}
