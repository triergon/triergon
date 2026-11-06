//! Optional explicit-SIMD kernels behind the `simd` feature (nightly `std::simd`).
//!
//! Every hook defaults to "not handled" (0 leading samples or `false`), so the generic
//! scalar code in `inter`, `deblock` and `sao` produces the whole output. With `simd`
//! the u8/u16 implementations produce a leading part or the whole block; the scalar
//! code finishes the rest. Both builds must decode bit-identically; the conformance
//! tests run under both configurations.
#[cfg(feature = "simd")]
mod filter;
#[cfg(feature = "simd")]
mod mc;

/// Row and block kernels used by the generic decoder through `Pixel`.
pub trait Kernels: Sized {
    // Optional explicit-SIMD row kernels (see kernels/mc.rs). Each returns the number of
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
    /// Horizontal luma deblocking edge over 16 columns (see kernels/filter.rs).
    #[inline(always)]
    fn simd_luma_h16(
        _d: &mut [Self],
        _pos: usize,
        _stride: usize,
        _prm: &crate::deblock::LumaLanes,
        _max: i32,
    ) {
    }
    /// Vertical luma deblocking edge over 16 rows (see kernels/filter.rs).
    #[inline(always)]
    fn simd_luma_v16(
        _d: &mut [Self],
        _pos: usize,
        _stride: usize,
        _prm: &crate::deblock::LumaLanes,
        _max: i32,
    ) {
    }
    /// Chroma deblocking edge over 16 lines (see kernels/filter.rs); `vert` selects EDGE_VER.
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
    /// SAO edge offset on a row (see kernels/filter.rs); returns the leading samples done.
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

#[cfg(not(feature = "simd"))]
impl Kernels for u8 {}
#[cfg(not(feature = "simd"))]
impl Kernels for u16 {}

#[cfg(feature = "simd")]
impl Kernels for u8 {
    const SIMD_FILTERS: bool = true;
    #[inline(always)]
    fn simd_luma_h16(
        d: &mut [u8],
        pos: usize,
        stride: usize,
        prm: &crate::deblock::LumaLanes,
        _max: i32,
    ) {
        filter::luma_h16_u8(d, pos, stride, prm)
    }
    #[inline(always)]
    fn simd_luma_v16(
        d: &mut [u8],
        pos: usize,
        stride: usize,
        prm: &crate::deblock::LumaLanes,
        _max: i32,
    ) {
        filter::luma_v16_u8(d, pos, stride, prm)
    }
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
            filter::chroma_v16_u8(d, pos, stride, prm)
        } else {
            filter::chroma_h16_u8(d, pos, stride, prm)
        }
    }
    #[inline(always)]
    fn simd_sao_eo(c: &[u8], a: &[u8], b: &[u8], d: &mut [u8], t: &[i16; 4]) -> usize {
        filter::sao_eo(c, a, b, d, t)
    }
    #[inline(always)]
    fn simd_sao_bo(s: &[u8], d: &mut [u8], band_pos: u8, o: &[i16; 4]) -> usize {
        filter::sao_bo(s, d, band_pos, o)
    }
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
        mc::wuni(d, doff, ds, s, w, h, log2wd, wt, o, max, mc::store_u8);
        true
    }
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
        mc::wbi(
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
            mc::store_u8,
        );
        true
    }
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
        mc::wcopy(
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
            mc::load_u8,
            mc::store_u8,
        );
        true
    }
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
        mc::taps8_row(r, c, w, mode, p, d, o)
    }
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
        mc::taps4_row(r, c, w, mode, p, d, o)
    }
    #[inline(always)]
    fn simd_hpass8(r: [&[u8]; 8], c: &[i8; 8], w: usize, t: &mut [i16]) -> usize {
        mc::hpass8_row(r, c, w, t)
    }
    #[inline(always)]
    fn simd_hpass4(r: [&[u8]; 4], c: &[i8; 4], w: usize, t: &mut [i16]) -> usize {
        mc::hpass4_row(r, c, w, t)
    }
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
        mc::vpass8_row(r, c, w, mode, p, d, o)
    }
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
        mc::vpass4_row(r, c, w, mode, p, d, o)
    }
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
        let mut bo = mc::BlockOut {
            pred: p,
            dst: d,
            dst_off: doff,
            dst_stride: ds,
            other: o,
            n: 8,
        };
        mc::int_block(src, off, ss, w, h, mode, &mut bo);
        true
    }
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
        let mut bo = mc::BlockOut {
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
            mc::luma_block(src, off, ss, fx, fy, w, h, cx, cy, mode, &mut bo, &mut []);
        } else {
            let (cx, cy) = (
                &crate::tables::CHROMA_FILTER[fx],
                &crate::tables::CHROMA_FILTER[fy],
            );
            mc::chroma_block(src, off, ss, fx, fy, w, h, cx, cy, mode, &mut bo);
        }
        true
    }
}

#[cfg(feature = "simd")]
impl Kernels for u16 {
    const SIMD_FILTERS: bool = true;
    #[inline(always)]
    fn simd_luma_h16(
        d: &mut [u16],
        pos: usize,
        stride: usize,
        prm: &crate::deblock::LumaLanes,
        max: i32,
    ) {
        filter::luma_h16_u16(d, pos, stride, prm, max as i16)
    }
    #[inline(always)]
    fn simd_luma_v16(
        d: &mut [u16],
        pos: usize,
        stride: usize,
        prm: &crate::deblock::LumaLanes,
        max: i32,
    ) {
        filter::luma_v16_u16(d, pos, stride, prm, max as i16)
    }
    #[inline(always)]
    fn simd_sao_eo_hi(
        c: &[u16],
        a: &[u16],
        b: &[u16],
        d: &mut [u16],
        t: &[i16; 4],
        max: i16,
    ) -> usize {
        filter::sao_eo_u16(c, a, b, d, t, max)
    }
    #[inline(always)]
    fn simd_sao_bo_hi(
        s: &[u16],
        d: &mut [u16],
        shift: u32,
        band_pos: u8,
        o: &[i16; 4],
        max: i16,
    ) -> usize {
        filter::sao_bo_u16(s, d, shift, band_pos, o, max)
    }
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
            filter::chroma_v16_u16(d, pos, stride, prm, max as i16)
        } else {
            filter::chroma_h16_u16(d, pos, stride, prm, max as i16)
        }
    }
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
        mc::wuni(d, doff, ds, s, w, h, log2wd, wt, o, max, mc::store_u16);
        true
    }
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
        mc::wbi(
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
            mc::store_u16,
        );
        true
    }
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
        mc::wcopy(
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
            mc::load_u16,
            mc::store_u16,
        );
        true
    }
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
        let mut bo = mc::BlockOut16 {
            pred: p,
            dst: d,
            dst_off: doff,
            dst_stride: ds,
            other: o,
        };
        mc::block_hi(src, off, ss, 0, 0, w, h, true, bd, mode, &mut bo);
        true
    }
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
        let mut bo = mc::BlockOut16 {
            pred: p,
            dst: d,
            dst_off: doff,
            dst_stride: ds,
            other: o,
        };
        mc::block_hi(src, off, ss, fx, fy, w, h, luma, bd, mode, &mut bo);
        true
    }
}
