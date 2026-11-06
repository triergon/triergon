//! Explicit-SIMD (std::simd) row kernels for 8-bit motion compensation. Each function
//! processes the largest multiple of 8 columns and returns that count; the generic code in
//! `inter.rs` (the scalar equivalent) handles the remaining columns and all >8-bit input.
//!
//! Arithmetic is identical to the scalar path: 8-bit filter sums fit i16 (coefficient sign
//! sums of Tables 8-12 / 8-13), 2-D vertical sums and bi-prediction sums use i32.

use std::simd::Simd;
use std::simd::cmp::SimdOrd;
use std::simd::num::{SimdInt, SimdUint};

use crate::inter::PRED_OFFSET;

type I16x8 = Simd<i16, 8>;
type I32x8 = Simd<i32, 8>;
type U8x8 = Simd<u8, 8>;

#[inline(always)]
fn ld(r: &[u8], x: usize) -> I16x8 {
    U8x8::from_slice(&r[x..x + 8]).cast()
}

#[inline(always)]
fn ld16(r: &[i16], x: usize) -> I32x8 {
    I16x8::from_slice(&r[x..x + 8]).cast()
}

/// Default bi-prediction (8-264) for 8-bit in i16 lanes: v is predSamplesL1 and
/// `ot` = predSamplesL0 - PRED_OFFSET (both |.| < 2^15). The sum v + ot is saturated: a
/// saturated value already clips to 0 / 255. floor((t + PRED_OFFSET + 64) / 128) is
/// evaluated as ((t >> 1) + (PRED_OFFSET + 64) / 2) >> 6, which is equal for integer t
/// and stays inside i16.
#[inline(always)]
fn bi16(v: I16x8, ot: I16x8) -> U8x8 {
    let t = v.saturating_add(ot);
    let r =
        ((t >> I16x8::splat(1)) + I16x8::splat(((PRED_OFFSET + 64) / 2) as i16)) >> I16x8::splat(6);
    r.simd_max(I16x8::splat(0))
        .simd_min(I16x8::splat(255))
        .cast()
}

/// Default uni-prediction (8-262) for 8-bit: (v + 32) >> 6 clipped (v < 2^15 - 32).
#[inline(always)]
fn uni16(v: I16x8) -> U8x8 {
    ((v + I16x8::splat(32)) >> I16x8::splat(6))
        .simd_max(I16x8::splat(0))
        .simd_min(I16x8::splat(255))
        .cast()
}

/// Output stage on 8 predicted samples v (see `inter::Out`); v fits i16.
#[inline(always)]
fn put(v: I32x8, x: usize, mode: u8, pred: &mut [i16], dst: &mut [u8], other: &[i16]) {
    put16(v.cast(), x, mode, pred, dst, other);
}

/// 8-bit output stage computed in i16 lanes.
#[inline(always)]
fn put16(v: I16x8, x: usize, mode: u8, pred: &mut [i16], dst: &mut [u8], other: &[i16]) {
    match mode {
        0 => (v - I16x8::splat(PRED_OFFSET as i16)).copy_to_slice(&mut pred[x..x + 8]),
        1 => uni16(v).copy_to_slice(&mut dst[x..x + 8]),
        _ => bi16(v, I16x8::from_slice(&other[x..x + 8])).copy_to_slice(&mut dst[x..x + 8]),
    }
}

#[inline(always)]
pub(super) fn taps8_row(
    r: [&[u8]; 8],
    c: &[i8; 8],
    w: usize,
    mode: u8,
    pred: &mut [i16],
    dst: &mut [u8],
    other: &[i16],
) -> usize {
    let n = w & !7;
    let m = mag8(c);
    let mut x = 0;
    while x < n {
        let acc = sum8(core::array::from_fn(|k| ldb(r[k], x)), &m);
        put16(acc, x, mode, pred, dst, other);
        x += 8;
    }
    n
}

#[inline(always)]
pub(super) fn taps4_row(
    r: [&[u8]; 4],
    c: &[i8; 4],
    w: usize,
    mode: u8,
    pred: &mut [i16],
    dst: &mut [u8],
    other: &[i16],
) -> usize {
    let n = w & !7;
    let m = mag4(c);
    let mut x = 0;
    while x < n {
        let acc = sum4(core::array::from_fn(|k| ldb(r[k], x)), &m);
        put16(acc, x, mode, pred, dst, other);
        x += 8;
    }
    n
}

#[inline(always)]
pub(super) fn hpass8_row(r: [&[u8]; 8], c: &[i8; 8], w: usize, t: &mut [i16]) -> usize {
    let n = w & !7;
    let m = mag8(c);
    let mut x = 0;
    while x < n {
        sum8(core::array::from_fn(|k| ldb(r[k], x)), &m).copy_to_slice(&mut t[x..x + 8]);
        x += 8;
    }
    n
}

#[inline(always)]
pub(super) fn hpass4_row(r: [&[u8]; 4], c: &[i8; 4], w: usize, t: &mut [i16]) -> usize {
    let n = w & !7;
    let m = mag4(c);
    let mut x = 0;
    while x < n {
        sum4(core::array::from_fn(|k| ldb(r[k], x)), &m).copy_to_slice(&mut t[x..x + 8]);
        x += 8;
    }
    n
}

#[inline(always)]
pub(super) fn vpass8_row(
    r: [&[i16]; 8],
    c: &[i8; 8],
    w: usize,
    mode: u8,
    pred: &mut [i16],
    dst: &mut [u8],
    other: &[i16],
) -> usize {
    let n = w & !7;
    let cs: [I32x8; 8] = core::array::from_fn(|k| I32x8::splat(c[k] as i32));
    let mut x = 0;
    while x < n {
        let mut acc = cs[0] * ld16(r[0], x);
        for k in 1..8 {
            acc += cs[k] * ld16(r[k], x);
        }
        put(acc >> I32x8::splat(6), x, mode, pred, dst, other);
        x += 8;
    }
    n
}

#[inline(always)]
pub(super) fn vpass4_row(
    r: [&[i16]; 4],
    c: &[i8; 4],
    w: usize,
    mode: u8,
    pred: &mut [i16],
    dst: &mut [u8],
    other: &[i16],
) -> usize {
    let n = w & !7;
    let cs: [I32x8; 4] = core::array::from_fn(|k| I32x8::splat(c[k] as i32));
    let mut x = 0;
    while x < n {
        let acc = cs[0] * ld16(r[0], x)
            + cs[1] * ld16(r[1], x)
            + cs[2] * ld16(r[2], x)
            + cs[3] * ld16(r[3], x);
        put(acc >> I32x8::splat(6), x, mode, pred, dst, other);
        x += 8;
    }
    n
}

// ----------------------------------------------------------------------------- block kernels
//
// Whole-block 8-bit kernels for widths that are multiples of 8. Horizontal taps are taken
// from one 16-sample load with lane swizzles; vertical taps use a sliding window of rows so
// each source row is loaded once per 8-column strip.

use std::simd::simd_swizzle;

/// Output destination for a block: (pred buffer, stride 64), (dst, offset, stride), other.
pub(super) struct BlockOut<'a> {
    pub pred: &'a mut [i16],
    pub dst: &'a mut [u8],
    pub dst_off: usize,
    pub dst_stride: usize,
    pub other: &'a [i16],
    /// Samples stored to `dst` per 8-lane strip: 8, or 4 for the last strip of a block
    /// whose width is 4 mod 8 (prediction-buffer stores always write 8 lanes).
    pub n: usize,
}

#[inline(always)]
fn put_block16(v: I16x8, x: usize, y: usize, mode: u8, o: &mut BlockOut<'_>) {
    let ps = 64;
    match mode {
        0 => (v - I16x8::splat(PRED_OFFSET as i16))
            .copy_to_slice(&mut o.pred[y * ps + x..y * ps + x + 8]),
        1 => {
            let d = o.dst_off + y * o.dst_stride + x;
            put_bytes(uni16(v), &mut o.dst[d..], o.n);
        }
        _ => {
            let ot = I16x8::from_slice(&o.other[y * ps + x..y * ps + x + 8]);
            let d = o.dst_off + y * o.dst_stride + x;
            put_bytes(bi16(v, ot), &mut o.dst[d..], o.n);
        }
    }
}

/// Store the first n (8 or 4) samples of b.
#[inline(always)]
fn put_bytes(b: U8x8, d: &mut [u8], n: usize) {
    if n == 8 {
        b.copy_to_slice(&mut d[..8]);
    } else {
        d[..4].copy_from_slice(&b.as_array()[..4]);
    }
}

/// 2-D results (acc >> 6) fit i16.
#[inline(always)]
fn put_block32(v: I32x8, x: usize, y: usize, mode: u8, o: &mut BlockOut<'_>) {
    put_block16(v.cast(), x, y, mode, o);
}

type U8x16 = Simd<u8, 16>;
type U16x8 = Simd<u16, 8>;

/// Byte windows w[k] = v[k..k+8] of a 16-byte vector (single-register `ext`).
#[inline(always)]
fn bwin8(v: U8x16) -> [U8x8; 8] {
    [
        simd_swizzle!(v, [0, 1, 2, 3, 4, 5, 6, 7]),
        simd_swizzle!(v, [1, 2, 3, 4, 5, 6, 7, 8]),
        simd_swizzle!(v, [2, 3, 4, 5, 6, 7, 8, 9]),
        simd_swizzle!(v, [3, 4, 5, 6, 7, 8, 9, 10]),
        simd_swizzle!(v, [4, 5, 6, 7, 8, 9, 10, 11]),
        simd_swizzle!(v, [5, 6, 7, 8, 9, 10, 11, 12]),
        simd_swizzle!(v, [6, 7, 8, 9, 10, 11, 12, 13]),
        simd_swizzle!(v, [7, 8, 9, 10, 11, 12, 13, 14]),
    ]
}

/// |coefficient| per tap; tap signs are fixed by Table 8-12: - + - + + - + -.
#[inline(always)]
fn mag8(c: &[i8; 8]) -> [U16x8; 8] {
    core::array::from_fn(|k| U16x8::splat(c[k].unsigned_abs() as u16))
}
/// Table 8-13 chroma tap signs: - + + -.
#[inline(always)]
fn mag4(c: &[i8; 4]) -> [U16x8; 4] {
    core::array::from_fn(|k| U16x8::splat(c[k].unsigned_abs() as u16))
}

/// 8-tap luma sum over byte windows, computed modulo 2^16 (exact: the true sum fits i16).
#[inline(always)]
fn sum8(w: [U8x8; 8], m: &[U16x8; 8]) -> I16x8 {
    let z = |v: U8x8| -> U16x8 { v.cast() };
    let pos = z(w[1]) * m[1] + z(w[3]) * m[3] + z(w[4]) * m[4] + z(w[6]) * m[6];
    let neg = z(w[0]) * m[0] + z(w[2]) * m[2] + z(w[5]) * m[5] + z(w[7]) * m[7];
    (pos - neg).cast()
}

#[inline(always)]
fn sum4(w: [U8x8; 4], m: &[U16x8; 4]) -> I16x8 {
    let z = |v: U8x8| -> U16x8 { v.cast() };
    ((z(w[1]) * m[1] + z(w[2]) * m[2]) - (z(w[0]) * m[0] + z(w[3]) * m[3])).cast()
}

#[inline(always)]
fn h8(src: &[u8], b: usize, m: &[U16x8; 8]) -> I16x8 {
    sum8(bwin8(U8x16::from_slice(&src[b..b + 16])), m)
}

#[inline(always)]
fn h4(src: &[u8], b: usize, m: &[U16x8; 4]) -> I16x8 {
    let w = bwin8(U8x16::from_slice(&src[b..b + 16]));
    sum4([w[0], w[1], w[2], w[3]], m)
}

#[inline(always)]
fn ldb(r: &[u8], x: usize) -> U8x8 {
    U8x8::from_slice(&r[x..x + 8])
}

/// `off` indexes the integer sample position of output (0, 0); `ss` is the source stride.
/// Requires w % 8 == 0 and 16-sample loads to stay inside `src` (callers guarantee the
/// 7 / 3 extra columns of filter support plus slack exist, see `inter::source`).
#[allow(clippy::too_many_arguments)]
pub(super) fn luma_block(
    src: &[u8],
    off: usize,
    ss: usize,
    fx: usize,
    fy: usize,
    w: usize,
    h: usize,
    cx: &[i8; 8],
    cy: &[i8; 8],
    mode: u8,
    o: &mut BlockOut<'_>,
    tmp: &mut [i16],
) {
    let csx = mag8(cx);
    let msy = mag8(cy);
    let csy32: [I32x8; 8] = core::array::from_fn(|k| I32x8::splat(cy[k] as i32));
    let mut x = 0;
    while x < w {
        o.n = (w - x).min(8);
        if fy == 0 {
            for y in 0..h {
                let v = h8(src, off + y * ss + x - 3, &csx);
                put_block16(v, x, y, mode, o);
            }
        } else if fx == 0 {
            // Sliding window over source rows.
            let base = off + x - 3 * ss;
            let mut win: [U8x8; 8] = core::array::from_fn(|k| {
                if k < 7 {
                    ldb(src, base + k * ss)
                } else {
                    U8x8::splat(0)
                }
            });
            for y in 0..h {
                win[7] = ldb(src, base + (y + 7) * ss);
                put_block16(sum8(win, &msy), x, y, mode, o);
                win.copy_within(1..8, 0);
            }
        } else {
            let base = off + x - 3 * ss - 3;
            let mut win: [I32x8; 8] = core::array::from_fn(|k| {
                if k < 7 {
                    h8(src, base + k * ss, &csx).cast()
                } else {
                    I32x8::splat(0)
                }
            });
            for y in 0..h {
                win[7] = h8(src, base + (y + 7) * ss, &csx).cast();
                let mut acc = csy32[0] * win[0];
                for k in 1..8 {
                    acc += csy32[k] * win[k];
                }
                put_block32(acc >> I32x8::splat(6), x, y, mode, o);
                win.copy_within(1..8, 0);
            }
        }
        x += 8;
    }
    let _ = tmp;
}

#[allow(clippy::too_many_arguments)]
pub(super) fn chroma_block(
    src: &[u8],
    off: usize,
    ss: usize,
    fx: usize,
    fy: usize,
    w: usize,
    h: usize,
    cx: &[i8; 4],
    cy: &[i8; 4],
    mode: u8,
    o: &mut BlockOut<'_>,
) {
    let csx = mag4(cx);
    let msy = mag4(cy);
    let csy32: [I32x8; 4] = core::array::from_fn(|k| I32x8::splat(cy[k] as i32));
    let mut x = 0;
    while x < w {
        o.n = (w - x).min(8);
        if fy == 0 {
            for y in 0..h {
                put_block16(h4(src, off + y * ss + x - 1, &csx), x, y, mode, o);
            }
        } else if fx == 0 {
            let base = off + x - ss;
            let mut win: [U8x8; 4] = core::array::from_fn(|k| {
                if k < 3 {
                    ldb(src, base + k * ss)
                } else {
                    U8x8::splat(0)
                }
            });
            for y in 0..h {
                win[3] = ldb(src, base + (y + 3) * ss);
                put_block16(sum4(win, &msy), x, y, mode, o);
                win.copy_within(1..4, 0);
            }
        } else {
            let base = off + x - ss - 1;
            let mut win: [I32x8; 4] = core::array::from_fn(|k| {
                if k < 3 {
                    h4(src, base + k * ss, &csx).cast()
                } else {
                    I32x8::splat(0)
                }
            });
            for y in 0..h {
                win[3] = h4(src, base + (y + 3) * ss, &csx).cast();
                let acc =
                    csy32[0] * win[0] + csy32[1] * win[1] + csy32[2] * win[2] + csy32[3] * win[3];
                put_block32(acc >> I32x8::splat(6), x, y, mode, o);
                win.copy_within(1..4, 0);
            }
        }
        x += 8;
    }
}

/// Integer-position prediction (v = p << 6) for 8-bit with output stage `mode` (0 or 2;
/// mode 1 is a plain copy handled by the caller). w % 8 == 0.
pub(super) fn int_block(
    src: &[u8],
    off: usize,
    ss: usize,
    w: usize,
    h: usize,
    mode: u8,
    o: &mut BlockOut<'_>,
) {
    if w % 16 == 0 {
        match (mode, w) {
            (0, 16) => int_block16::<0, 16>(src, off, ss, h, o),
            (0, 32) => int_block16::<0, 32>(src, off, ss, h, o),
            (0, 64) => int_block16::<0, 64>(src, off, ss, h, o),
            (_, 16) => int_block16::<2, 16>(src, off, ss, h, o),
            (_, 32) => int_block16::<2, 32>(src, off, ss, h, o),
            (_, 64) => int_block16::<2, 64>(src, off, ss, h, o),
            (0, _) => int_block16::<0, 48>(src, off, ss, h, o),
            _ => int_block16::<2, 48>(src, off, ss, h, o),
        }
        return;
    }
    for y in 0..h {
        let mut x = 0;
        while x < w {
            o.n = (w - x).min(8);
            let v = ld(src, off + y * ss + x) << I16x8::splat(6);
            put_block16(v, x, y, mode, o);
            x += 8;
        }
    }
}

/// Integer-position 8-bit prediction on 16 lanes (w % 16 == 0), all in i16: MODE 0 stores
/// (p << 6) - PRED_OFFSET; MODE 2 averages with the stored first prediction. The bi sum
/// (p0 + p1 + 64) uses a saturating add: a saturated sum is already outside the range that
/// clips to 0 / 255 after >> 7, so the result is exact.
#[inline(always)]
fn int_block16<const MODE: u8, const W: usize>(
    src: &[u8],
    off: usize,
    ss: usize,
    h: usize,
    o: &mut BlockOut<'_>,
) {
    let w = W;
    type I16x16 = Simd<i16, 16>;
    let six = I16x16::splat(6);
    for y in 0..h {
        let srow = &src[off + y * ss..off + y * ss + w];
        if MODE == 0 {
            let prow = &mut o.pred[y * 64..y * 64 + w];
            for (s, p) in srow
                .as_chunks::<16>()
                .0
                .iter()
                .zip(prow.as_chunks_mut::<16>().0)
            {
                let v = U8x16::from_slice(s).cast::<i16>() << six;
                (v - I16x16::splat(PRED_OFFSET as i16)).copy_to_slice(p);
            }
        } else {
            let orow = &o.other[y * 64..y * 64 + w];
            let d0 = o.dst_off + y * o.dst_stride;
            let drow = &mut o.dst[d0..d0 + w];
            for ((s, ot), d) in srow
                .as_chunks::<16>()
                .0
                .iter()
                .zip(orow.as_chunks::<16>().0)
                .zip(drow.as_chunks_mut::<16>().0)
            {
                let v = U8x16::from_slice(s).cast::<i16>() << six;
                let t = (v + I16x16::splat(PRED_OFFSET as i16 + 64))
                    .saturating_add(I16x16::from_slice(ot));
                let r = (t >> I16x16::splat(7))
                    .simd_max(I16x16::splat(0))
                    .simd_min(I16x16::splat(255));
                r.cast::<u8>().copy_to_slice(d);
            }
        }
    }
}

// ----------------------------------------------------------------------------- >8-bit blocks
//
// u16 sample kernels (bit depth 9..10). Sums use u32 wrapping arithmetic with |c| and the
// fixed tap signs; the exact sum fits i32, so the wrapped result reinterpreted as i32 is exact.

type U16x8b = Simd<u16, 8>;
type U32x8 = Simd<u32, 8>;

#[inline(always)]
fn lw(r: &[u16], x: usize) -> U32x8 {
    U16x8b::from_slice(&r[x..x + 8]).cast()
}

#[inline(always)]
fn mag8w(c: &[i8; 8]) -> [U32x8; 8] {
    core::array::from_fn(|k| U32x8::splat(c[k].unsigned_abs() as u32))
}

#[inline(always)]
fn mag4w(c: &[i8; 4]) -> [U32x8; 4] {
    core::array::from_fn(|k| U32x8::splat(c[k].unsigned_abs() as u32))
}

#[inline(always)]
fn sum8w(w: &[U32x8; 8], m: &[U32x8; 8]) -> I32x8 {
    let pos = w[1] * m[1] + w[3] * m[3] + w[4] * m[4] + w[6] * m[6];
    let neg = w[0] * m[0] + w[2] * m[2] + w[5] * m[5] + w[7] * m[7];
    (pos - neg).cast()
}

#[inline(always)]
fn sum4w(w: &[U32x8; 4], m: &[U32x8; 4]) -> I32x8 {
    ((w[1] * m[1] + w[2] * m[2]) - (w[0] * m[0] + w[3] * m[3])).cast()
}

pub(super) struct BlockOut16<'a> {
    pub pred: &'a mut [i16],
    pub dst: &'a mut [u16],
    pub dst_off: usize,
    pub dst_stride: usize,
    pub other: &'a [i16],
}

#[derive(Clone, Copy)]
struct Hi {
    sh1: I32x8,
    rnd1: I32x8,
    shu: I32x8,
    rnd2: I32x8,
    shb: I32x8,
    max: I32x8,
}

impl Hi {
    fn new(bd: u32) -> Hi {
        Hi {
            sh1: I32x8::splat((bd - 8) as i32),
            rnd1: I32x8::splat(1 << (13 - bd)),
            shu: I32x8::splat((14 - bd) as i32),
            rnd2: I32x8::splat((1 << (14 - bd)) + PRED_OFFSET),
            shb: I32x8::splat((15 - bd) as i32),
            max: I32x8::splat((1 << bd) - 1),
        }
    }
}

#[inline(always)]
fn put_hi(v: I32x8, x: usize, y: usize, mode: u8, o: &mut BlockOut16<'_>, k: &Hi) {
    let ps = 64;
    match mode {
        0 => {
            let p: I16x8 = (v - I32x8::splat(PRED_OFFSET)).cast();
            p.copy_to_slice(&mut o.pred[y * ps + x..y * ps + x + 8]);
        }
        1 => {
            let p = ((v + k.rnd1) >> k.shu)
                .simd_max(I32x8::splat(0))
                .simd_min(k.max);
            let b: U16x8b = p.cast();
            let d = o.dst_off + y * o.dst_stride + x;
            b.copy_to_slice(&mut o.dst[d..d + 8]);
        }
        _ => {
            let ot: I32x8 = I16x8::from_slice(&o.other[y * ps + x..y * ps + x + 8]).cast();
            let p = ((v + ot + k.rnd2) >> k.shb)
                .simd_max(I32x8::splat(0))
                .simd_min(k.max);
            let b: U16x8b = p.cast();
            let d = o.dst_off + y * o.dst_stride + x;
            b.copy_to_slice(&mut o.dst[d..d + 8]);
        }
    }
}

/// Fractional or integer MC for u16 samples (w % 8 == 0). `ntap` is 8 (luma) or 4 (chroma).
#[allow(clippy::too_many_arguments)]
pub(super) fn block_hi(
    src: &[u16],
    off: usize,
    ss: usize,
    fx: usize,
    fy: usize,
    w: usize,
    h: usize,
    luma: bool,
    bd: u32,
    mode: u8,
    o: &mut BlockOut16<'_>,
) {
    let k = Hi::new(bd);
    if fx == 0 && fy == 0 {
        let sh = I32x8::splat((14 - bd) as i32);
        for y in 0..h {
            let mut x = 0;
            while x < w {
                let v: I32x8 = lw(src, off + y * ss + x).cast::<i32>() << sh;
                put_hi(v, x, y, mode, o, &k);
                x += 8;
            }
        }
        return;
    }
    if luma {
        let (cx, cy) = (
            &crate::tables::LUMA_FILTER[fx],
            &crate::tables::LUMA_FILTER[fy],
        );
        let (mx, my) = (mag8w(cx), mag8w(cy));
        let cy32: [I32x8; 8] = core::array::from_fn(|i| I32x8::splat(cy[i] as i32));
        let mut x = 0;
        while x < w {
            if fy == 0 {
                for y in 0..h {
                    let b = off + y * ss + x - 3;
                    let win: [U32x8; 8] = core::array::from_fn(|i| lw(src, b + i));
                    put_hi(sum8w(&win, &mx) >> k.sh1, x, y, mode, o, &k);
                }
            } else if fx == 0 {
                let base = off + x - 3 * ss;
                let mut win: [U32x8; 8] = core::array::from_fn(|i| {
                    if i < 7 {
                        lw(src, base + i * ss)
                    } else {
                        U32x8::splat(0)
                    }
                });
                for y in 0..h {
                    win[7] = lw(src, base + (y + 7) * ss);
                    put_hi(sum8w(&win, &my) >> k.sh1, x, y, mode, o, &k);
                    win.copy_within(1..8, 0);
                }
            } else {
                let base = off + x - 3 * ss - 3;
                let hrow = |r: usize| -> I32x8 {
                    let b = base + r * ss;
                    let wv: [U32x8; 8] = core::array::from_fn(|i| lw(src, b + i));
                    sum8w(&wv, &mx) >> k.sh1
                };
                let mut win: [I32x8; 8] =
                    core::array::from_fn(|i| if i < 7 { hrow(i) } else { I32x8::splat(0) });
                for y in 0..h {
                    win[7] = hrow(y + 7);
                    let mut acc = cy32[0] * win[0];
                    for i in 1..8 {
                        acc += cy32[i] * win[i];
                    }
                    put_hi(acc >> I32x8::splat(6), x, y, mode, o, &k);
                    win.copy_within(1..8, 0);
                }
            }
            x += 8;
        }
    } else {
        let (cx, cy) = (
            &crate::tables::CHROMA_FILTER[fx],
            &crate::tables::CHROMA_FILTER[fy],
        );
        let (mx, my) = (mag4w(cx), mag4w(cy));
        let cy32: [I32x8; 4] = core::array::from_fn(|i| I32x8::splat(cy[i] as i32));
        let mut x = 0;
        while x < w {
            if fy == 0 {
                for y in 0..h {
                    let b = off + y * ss + x - 1;
                    let win: [U32x8; 4] = core::array::from_fn(|i| lw(src, b + i));
                    put_hi(sum4w(&win, &mx) >> k.sh1, x, y, mode, o, &k);
                }
            } else if fx == 0 {
                let base = off + x - ss;
                let mut win: [U32x8; 4] = core::array::from_fn(|i| {
                    if i < 3 {
                        lw(src, base + i * ss)
                    } else {
                        U32x8::splat(0)
                    }
                });
                for y in 0..h {
                    win[3] = lw(src, base + (y + 3) * ss);
                    put_hi(sum4w(&win, &my) >> k.sh1, x, y, mode, o, &k);
                    win.copy_within(1..4, 0);
                }
            } else {
                let base = off + x - ss - 1;
                let hrow = |r: usize| -> I32x8 {
                    let b = base + r * ss;
                    let wv: [U32x8; 4] = core::array::from_fn(|i| lw(src, b + i));
                    sum4w(&wv, &mx) >> k.sh1
                };
                let mut win: [I32x8; 4] =
                    core::array::from_fn(|i| if i < 3 { hrow(i) } else { I32x8::splat(0) });
                for y in 0..h {
                    win[3] = hrow(y + 3);
                    let acc =
                        cy32[0] * win[0] + cy32[1] * win[1] + cy32[2] * win[2] + cy32[3] * win[3];
                    put_hi(acc >> I32x8::splat(6), x, y, mode, o, &k);
                    win.copy_within(1..4, 0);
                }
            }
            x += 8;
        }
    }
}

// ----------------------------------------------------------------------------- weighted put

/// Explicit weighted uni-prediction (8-275) on i16 prediction rows (stride 64, offset by
/// -PRED_OFFSET). Generic over the output sample type via `store`. w % 8 == 0.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
pub(super) fn wuni<T>(
    dst: &mut [T],
    doff: usize,
    ds: usize,
    src: &[i16],
    w: usize,
    h: usize,
    log2wd: u32,
    wt: i32,
    o: i32,
    max: i32,
    store: impl Fn(I32x8, &mut [T]),
) {
    let (wv, rnd, ov, sh) = (
        I32x8::splat(wt),
        I32x8::splat(1 << (log2wd - 1)),
        I32x8::splat(o),
        I32x8::splat(log2wd as i32),
    );
    let base = I32x8::splat(PRED_OFFSET);
    for y in 0..h {
        let mut x = 0;
        while x < w {
            let v: I32x8 = I16x8::from_slice(&src[y * 64 + x..y * 64 + x + 8]).cast::<i32>() + base;
            let p = (((v * wv + rnd) >> sh) + ov)
                .simd_max(I32x8::splat(0))
                .simd_min(I32x8::splat(max));
            let d = doff + y * ds + x;
            store(p, &mut dst[d..d + 8]);
            x += 8;
        }
    }
}

/// Explicit weighted bi-prediction (8-277).
#[allow(clippy::too_many_arguments)]
#[inline(always)]
pub(super) fn wbi<T>(
    dst: &mut [T],
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
    store: impl Fn(I32x8, &mut [T]),
) {
    let (w0v, w1v, rv, sh) = (
        I32x8::splat(w0),
        I32x8::splat(w1),
        I32x8::splat(rnd),
        I32x8::splat(log2wd as i32 + 1),
    );
    let base = I32x8::splat(PRED_OFFSET);
    for y in 0..h {
        let mut x = 0;
        while x < w {
            let va: I32x8 = I16x8::from_slice(&a[y * 64 + x..y * 64 + x + 8]).cast::<i32>() + base;
            let vb: I32x8 = I16x8::from_slice(&b[y * 64 + x..y * 64 + x + 8]).cast::<i32>() + base;
            let p = ((va * w0v + vb * w1v + rv) >> sh)
                .simd_max(I32x8::splat(0))
                .simd_min(I32x8::splat(max));
            let d = doff + y * ds + x;
            store(p, &mut dst[d..d + 8]);
            x += 8;
        }
    }
}

#[inline(always)]
pub(super) fn store_u8(p: I32x8, d: &mut [u8]) {
    let b: U8x8 = p.cast();
    b.copy_to_slice(d);
}

#[inline(always)]
pub(super) fn store_u16(p: I32x8, d: &mut [u16]) {
    let b: Simd<u16, 8> = p.cast();
    b.copy_to_slice(d);
}

/// Fused integer-position explicit weighted uni-prediction: v = p << shift3, then 8-275.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
pub(super) fn wcopy<T: Copy>(
    dst: &mut [T],
    doff: usize,
    ds: usize,
    src: &[T],
    soff: usize,
    ss: usize,
    w: usize,
    h: usize,
    shift3: u32,
    log2wd: u32,
    wt: i32,
    o: i32,
    max: i32,
    load: impl Fn(&[T]) -> I32x8,
    store: impl Fn(I32x8, &mut [T]),
) {
    let (wv, rnd, ov, sh, s3) = (
        I32x8::splat(wt),
        I32x8::splat(1 << (log2wd - 1)),
        I32x8::splat(o),
        I32x8::splat(log2wd as i32),
        I32x8::splat(shift3 as i32),
    );
    for y in 0..h {
        let mut x = 0;
        while x < w {
            let v = load(&src[soff + y * ss + x..soff + y * ss + x + 8]) << s3;
            let p = (((v * wv + rnd) >> sh) + ov)
                .simd_max(I32x8::splat(0))
                .simd_min(I32x8::splat(max));
            let d = doff + y * ds + x;
            store(p, &mut dst[d..d + 8]);
            x += 8;
        }
    }
}

#[inline(always)]
pub(super) fn load_u8(s: &[u8]) -> I32x8 {
    U8x8::from_slice(s).cast()
}

#[inline(always)]
pub(super) fn load_u16(s: &[u16]) -> I32x8 {
    Simd::<u16, 8>::from_slice(s).cast()
}
