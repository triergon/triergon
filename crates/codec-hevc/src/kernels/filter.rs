//! Explicit-SIMD (std::simd) kernels for the 8-bit in-loop filters. Each kernel processes
//! the largest multiple of 16 samples and returns that count; the scalar code in `sao.rs`
//! finishes the row. Results are identical to the scalar path (8-bit SaoOffsetVal fits
//! i8, and saturating u8 arithmetic is the clip to [0, 255]).

use std::simd::Simd;
use std::simd::cmp::{SimdOrd, SimdPartialEq, SimdPartialOrd};
use std::simd::num::{SimdInt, SimdUint};

type U8x16 = Simd<u8, 16>;
type I8x16 = Simd<i8, 16>;

/// v + off clipped to [0, 255], off in -128..=127 (only small offsets occur).
#[inline(always)]
fn add_clip(v: U8x16, off: I8x16) -> U8x16 {
    let pos = off.simd_max(I8x16::splat(0)).cast::<u8>();
    let neg = (-off).simd_max(I8x16::splat(0)).cast::<u8>();
    v.saturating_add(pos).saturating_sub(neg)
}

/// SAO edge offset (8.7.3.2) on n samples: cur / a / b are the sample and its two
/// neighbours along the edge class; t = SaoOffsetVal[1..=4].
#[inline(always)]
pub(super) fn sao_eo(cur: &[u8], a: &[u8], b: &[u8], dst: &mut [u8], t: &[i16; 4]) -> usize {
    let len = cur.len().min(a.len()).min(b.len()).min(dst.len());
    if len < 16 {
        return 0;
    }
    let mut tab = [0u8; 16];
    // edgeIdx e = sgn(v - a) + sgn(v - b) in -2..=2, looked up at e + 2.
    tab[0] = t[0] as i8 as u8;
    tab[1] = t[1] as i8 as u8;
    tab[3] = t[2] as i8 as u8;
    tab[4] = t[3] as i8 as u8;
    let tab = U8x16::from_array(tab);
    // The source rows are separate from dst, so the last vector may overlap the previous
    // one (recomputing identical values) instead of leaving a scalar tail.
    let mut x = 0;
    loop {
        let x16 = x.min(len - 16);
        let v = U8x16::from_slice(&cur[x16..x16 + 16]);
        let va = U8x16::from_slice(&a[x16..x16 + 16]);
        let vb = U8x16::from_slice(&b[x16..x16 + 16]);
        // Mask lanes are -1 when true, so this is -(sgn(v - a) + sgn(v - b)).
        let e = va.simd_lt(v).to_simd() - va.simd_gt(v).to_simd() + vb.simd_lt(v).to_simd()
            - vb.simd_gt(v).to_simd();
        // Table index = 2 + edge sum = 2 - e.
        let idx = (I8x16::splat(2) - e).cast::<u8>();
        let off = tab.swizzle_dyn(idx).cast::<i8>();
        add_clip(v, off).copy_to_slice(&mut dst[x16..x16 + 16]);
        x += 16;
        if x >= len {
            return len;
        }
    }
}

/// SAO band offset (8.7.3.3) for 8-bit samples: bandTable lookup of (v >> 3) - band_pos.
#[inline(always)]
pub(super) fn sao_bo(src: &[u8], dst: &mut [u8], band_pos: u8, o: &[i16; 4]) -> usize {
    let n = src.len().min(dst.len()) & !15;
    let mut tab = [0u8; 16];
    for k in 0..4 {
        tab[k] = o[k] as i8 as u8;
    }
    let tab = U8x16::from_array(tab);
    let bp = U8x16::splat(band_pos);
    let mut x = 0;
    while x < n {
        let v = U8x16::from_slice(&src[x..x + 16]);
        // k = ((v >> 3) - bandPos) & 31; entries 4..31 are zero (indices >= 16 give 0).
        let k = ((v >> U8x16::splat(3)) - bp) & U8x16::splat(31);
        let off = tab.swizzle_dyn(k).cast::<i8>();
        add_clip(v, off).copy_to_slice(&mut dst[x..x + 16]);
        x += 16;
    }
    n
}

// ------------------------------------------------------------------------- luma deblocking

use std::simd::{Mask, Select, simd_swizzle};

type I16x16 = Simd<i16, 16>;
type M16 = Mask<i16, 16>;

use crate::deblock::LumaLanes;

/// Broadcast line 0 / line 3 of each 4-line segment to the segment's lanes.
#[inline(always)]
fn line0(v: I16x16) -> I16x16 {
    simd_swizzle!(v, [0, 0, 0, 0, 4, 4, 4, 4, 8, 8, 8, 8, 12, 12, 12, 12])
}
#[inline(always)]
fn line3(v: I16x16) -> I16x16 {
    simd_swizzle!(v, [3, 3, 3, 3, 7, 7, 7, 7, 11, 11, 11, 11, 15, 15, 15, 15])
}

#[inline(always)]
fn clip(v: I16x16, lo: I16x16, hi: I16x16) -> I16x16 {
    v.simd_max(lo).simd_min(hi)
}

/// Luma edge decisions and filters (8.7.2.5.3, 8.7.2.5.7) on 16 lines; s = p3..q3 with one
/// lane per line. Returns the new p2, p1, p0, q0, q1, q2, or None if no lane changes.
#[inline(always)]
fn luma_core(s: &[I16x16; 8], prm: &LumaLanes, max: i16) -> Option<[I16x16; 6]> {
    let [p3, p2, p1, p0, q0, q1, q2, q3] = *s;
    let beta = I16x16::from_array(prm.beta);
    let tc = I16x16::from_array(prm.tc);
    let z = I16x16::splat(0);
    let dp = (p2 - p1 - p1 + p0).abs();
    let dq = (q2 - q1 - q1 + q0).abs();
    let (dp0, dp3, dq0, dq3) = (line0(dp), line3(dp), line0(dq), line3(dq));
    let (dpq0, dpq3) = (dp0 + dq0, dp3 + dq3);
    let on = (dpq0 + dpq3).simd_lt(beta) & tc.simd_gt(z);
    if !on.any() {
        return None;
    }
    let one = I16x16::splat(1);
    let tc5 = (tc * I16x16::splat(5) + one) >> one;
    let lane_ok = (((p3 - p0).abs() + (q0 - q3).abs()).simd_lt(beta >> I16x16::splat(3))
        & (p0 - q0).abs().simd_lt(tc5))
    .to_simd();
    let b2 = beta >> I16x16::splat(2);
    let strong = on
        & line0(lane_ok).simd_ne(z)
        & line3(lane_ok).simd_ne(z)
        & (dpq0 + dpq0).simd_lt(b2)
        & (dpq3 + dpq3).simd_lt(b2);
    let mp: M16 = I16x16::from_array(prm.mp).simd_ne(z);
    let mq: M16 = I16x16::from_array(prm.mq).simd_ne(z);
    let (c2, c3, c4) = (I16x16::splat(2), I16x16::splat(3), I16x16::splat(4));
    // Strong filter (8-353 .. 8-358).
    let tc2 = tc + tc;
    let sp0 = clip(
        (p2 + p1 * c2 + p0 * c2 + q0 * c2 + q1 + c4) >> c3,
        p0 - tc2,
        p0 + tc2,
    );
    let sp1 = clip((p2 + p1 + p0 + q0 + c2) >> c2, p1 - tc2, p1 + tc2);
    let sp2 = clip(
        (p3 * c2 + p2 * c3 + p1 + p0 + q0 + c4) >> c3,
        p2 - tc2,
        p2 + tc2,
    );
    let sq0 = clip(
        (p1 + p0 * c2 + q0 * c2 + q1 * c2 + q2 + c4) >> c3,
        q0 - tc2,
        q0 + tc2,
    );
    let sq1 = clip((p0 + q0 + q1 + q2 + c2) >> c2, q1 - tc2, q1 + tc2);
    let sq2 = clip(
        (p0 + q0 + q1 + q2 * c3 + q3 * c2 + c4) >> c3,
        q2 - tc2,
        q2 + tc2,
    );
    // Weak filter (8-359 .. 8-366).
    let side = (beta + (beta >> one)) >> c3;
    let dep = (dp0 + dp3).simd_lt(side);
    let deq = (dq0 + dq3).simd_lt(side);
    let delta = ((q0 - p0) * I16x16::splat(9) - (q1 - p1) * c3 + I16x16::splat(8)) >> c4;
    let weak = on & !strong & delta.abs().simd_lt(tc * I16x16::splat(10));
    let delta = clip(delta, -tc, tc);
    let mx = I16x16::splat(max);
    let wp0 = clip(p0 + delta, z, mx);
    let wq0 = clip(q0 - delta, z, mx);
    let tch = tc >> one;
    let dpv = clip(((((p2 + p0 + one) >> one) - p1) + delta) >> one, -tch, tch);
    let dqv = clip(((((q2 + q0 + one) >> one) - q1) - delta) >> one, -tch, tch);
    let wp1 = clip(p1 + dpv, z, mx);
    let wq1 = clip(q1 + dqv, z, mx);
    let (sp, sq) = (strong & mp, strong & mq);
    let (wp, wq) = (weak & mp, weak & mq);
    Some([
        sp.select(sp2, p2),
        sp.select(sp1, (wp & dep).select(wp1, p1)),
        sp.select(sp0, wp.select(wp0, p0)),
        sq.select(sq0, wq.select(wq0, q0)),
        sq.select(sq1, (wq & deq).select(wq1, q1)),
        sq.select(sq2, q2),
    ])
}

/// Horizontal luma edge, 16 consecutive columns starting at `pos` (q0 row).
#[inline(always)]
pub(super) fn luma_h16_u8(d: &mut [u8], pos: usize, stride: usize, prm: &LumaLanes) {
    let base = pos - 4 * stride;
    let s: [I16x16; 8] = core::array::from_fn(|i| {
        let o = base + i * stride;
        U8x16::from_slice(&d[o..o + 16]).cast()
    });
    if let Some(r) = luma_core(&s, prm, 255) {
        for (i, v) in r.iter().enumerate() {
            let o = base + (i + 1) * stride;
            v.cast::<u8>().copy_to_slice(&mut d[o..o + 16]);
        }
    }
}

/// Horizontal luma edge for >8-bit samples.
#[inline(always)]
pub(super) fn luma_h16_u16(d: &mut [u16], pos: usize, stride: usize, prm: &LumaLanes, max: i16) {
    let base = pos - 4 * stride;
    let s: [I16x16; 8] = core::array::from_fn(|i| {
        let o = base + i * stride;
        Simd::<u16, 16>::from_slice(&d[o..o + 16]).cast()
    });
    if let Some(r) = luma_core(&s, prm, max) {
        for (i, v) in r.iter().enumerate() {
            let o = base + (i + 1) * stride;
            v.cast::<u16>().copy_to_slice(&mut d[o..o + 16]);
        }
    }
}

// ----------------------------------------------------------------- 16x8 <-> 8x16 transposes
//
// Vertical edges have the samples across the edge (p3..q3) contiguous in each row. Sixteen
// rows of 8 samples are transposed into 8 vectors of 16 lines with three two-input shuffle
// stages (pairs of rows, then 4 rows, then 16 rows per column) and back after filtering.

use std::simd::{SimdElement, Swizzle};

struct F1;
impl Swizzle<16> for F1 {
    // S[2c + t] = row t, column c.
    const INDEX: [usize; 16] = {
        let mut a = [0; 16];
        let mut i = 0;
        while i < 16 {
            a[i] = (i % 2) * 8 + i / 2;
            i += 1;
        }
        a
    };
}
struct F2<const G: usize>;
impl<const G: usize> Swizzle<16> for F2<G> {
    // V[4c' + u] = row u (of 4), column c' + 4G.
    const INDEX: [usize; 16] = {
        let mut a = [0; 16];
        let mut i = 0;
        while i < 16 {
            let (cp, u) = (i / 4, i % 4);
            let c = cp + 4 * G;
            a[i] = if u < 2 { 2 * c + u } else { 16 + 2 * c + u - 2 };
            i += 1;
        }
        a
    };
}
struct F3a<const HALF: usize>;
impl<const HALF: usize> Swizzle<16> for F3a<HALF> {
    // Y[8m + v] = row v (of 8), column 2 HALF + m (within the group of 4).
    const INDEX: [usize; 16] = {
        let mut a = [0; 16];
        let mut i = 0;
        while i < 16 {
            let (m, v) = (i / 8, i % 8);
            let cp = 2 * HALF + m;
            a[i] = if v < 4 {
                4 * cp + v
            } else {
                16 + 4 * cp + v - 4
            };
            i += 1;
        }
        a
    };
}
struct F3b<const M: usize>;
impl<const M: usize> Swizzle<16> for F3b<M> {
    // C[r] = row r (of 16).
    const INDEX: [usize; 16] = {
        let mut a = [0; 16];
        let mut r = 0;
        while r < 16 {
            a[r] = if r < 8 { 8 * M + r } else { 16 + 8 * M + r - 8 };
            r += 1;
        }
        a
    };
}
struct B3b<const H: usize>;
impl<const H: usize> Swizzle<16> for B3b<H> {
    const INDEX: [usize; 16] = {
        let mut a = [0; 16];
        let mut i = 0;
        while i < 16 {
            let (m, v) = (i / 8, i % 8);
            a[i] = m * 16 + 8 * H + v;
            i += 1;
        }
        a
    };
}
struct B3a<const T: usize>;
impl<const T: usize> Swizzle<16> for B3a<T> {
    const INDEX: [usize; 16] = {
        let mut a = [0; 16];
        let mut i = 0;
        while i < 16 {
            let (cp, v) = (i / 4, i % 4);
            a[i] = (cp / 2) * 16 + 8 * (cp % 2) + 4 * T + v;
            i += 1;
        }
        a
    };
}
struct B2<const T: usize>;
impl<const T: usize> Swizzle<16> for B2<T> {
    const INDEX: [usize; 16] = {
        let mut a = [0; 16];
        let mut i = 0;
        while i < 16 {
            let (c, u) = (i / 2, i % 2);
            a[i] = (c / 4) * 16 + 4 * (c % 4) + 2 * T + u;
            i += 1;
        }
        a
    };
}
struct B1<const T: usize>;
impl<const T: usize> Swizzle<8> for B1<T> {
    const INDEX: [usize; 8] = {
        let mut a = [0; 8];
        let mut c = 0;
        while c < 8 {
            a[c] = 2 * c + T;
            c += 1;
        }
        a
    };
}

/// c[i][r] = rows[r][i].
#[inline(always)]
fn transpose_16x8<T: SimdElement>(r: &[Simd<T, 8>; 16]) -> [Simd<T, 16>; 8] {
    let s: [Simd<T, 16>; 8] = core::array::from_fn(|j| F1::concat_swizzle(r[2 * j], r[2 * j + 1]));
    let v: [[Simd<T, 16>; 2]; 4] = core::array::from_fn(|k| {
        [
            F2::<0>::concat_swizzle(s[2 * k], s[2 * k + 1]),
            F2::<1>::concat_swizzle(s[2 * k], s[2 * k + 1]),
        ]
    });
    let y = |g: usize, h: usize| {
        [
            F3a::<0>::concat_swizzle(v[2 * h][g], v[2 * h + 1][g]),
            F3a::<1>::concat_swizzle(v[2 * h][g], v[2 * h + 1][g]),
        ]
    };
    let (y00, y01, y10, y11) = (y(0, 0), y(0, 1), y(1, 0), y(1, 1));
    [
        F3b::<0>::concat_swizzle(y00[0], y01[0]),
        F3b::<1>::concat_swizzle(y00[0], y01[0]),
        F3b::<0>::concat_swizzle(y00[1], y01[1]),
        F3b::<1>::concat_swizzle(y00[1], y01[1]),
        F3b::<0>::concat_swizzle(y10[0], y11[0]),
        F3b::<1>::concat_swizzle(y10[0], y11[0]),
        F3b::<0>::concat_swizzle(y10[1], y11[1]),
        F3b::<1>::concat_swizzle(y10[1], y11[1]),
    ]
}

/// Inverse of `transpose_16x8`.
#[inline(always)]
fn transpose_8x16<T: SimdElement>(c: &[Simd<T, 16>; 8]) -> [Simd<T, 8>; 16] {
    // y[g][h][half]
    let y = |g: usize, h: usize, half: usize| {
        let (a, b) = (c[4 * g + 2 * half], c[4 * g + 2 * half + 1]);
        if h == 0 {
            B3b::<0>::concat_swizzle(a, b)
        } else {
            B3b::<1>::concat_swizzle(a, b)
        }
    };
    // v[k][g] for k = 2h + t
    let v = |k: usize, g: usize| {
        let (h, t) = (k / 2, k % 2);
        let (a, b) = (y(g, h, 0), y(g, h, 1));
        if t == 0 {
            B3a::<0>::concat_swizzle(a, b)
        } else {
            B3a::<1>::concat_swizzle(a, b)
        }
    };
    let vv: [[Simd<T, 16>; 2]; 4] = core::array::from_fn(|k| [v(k, 0), v(k, 1)]);
    let s: [Simd<T, 16>; 8] = core::array::from_fn(|j| {
        let (k, t) = (j / 2, j % 2);
        if t == 0 {
            B2::<0>::concat_swizzle(vv[k][0], vv[k][1])
        } else {
            B2::<1>::concat_swizzle(vv[k][0], vv[k][1])
        }
    });
    core::array::from_fn(|r| {
        if r % 2 == 0 {
            B1::<0>::swizzle(s[r / 2])
        } else {
            B1::<1>::swizzle(s[r / 2])
        }
    })
}

/// Vertical luma edge over 16 rows; `pos` is q0 of the first row.
#[inline(always)]
pub(super) fn luma_v16_u8(d: &mut [u8], pos: usize, stride: usize, prm: &LumaLanes) {
    let rows: [Simd<u8, 8>; 16] = core::array::from_fn(|r| {
        let o = pos + r * stride - 4;
        Simd::<u8, 8>::from_slice(&d[o..o + 8])
    });
    let mut c = transpose_16x8(&rows);
    let s: [I16x16; 8] = core::array::from_fn(|i| c[i].cast());
    if let Some(n) = luma_core(&s, prm, 255) {
        for i in 0..6 {
            c[i + 1] = n[i].cast();
        }
        let rows = transpose_8x16(&c);
        for (r, v) in rows.iter().enumerate() {
            let o = pos + r * stride - 4;
            v.copy_to_slice(&mut d[o..o + 8]);
        }
    }
}

/// Vertical luma edge for >8-bit samples.
#[inline(always)]
pub(super) fn luma_v16_u16(d: &mut [u16], pos: usize, stride: usize, prm: &LumaLanes, max: i16) {
    let rows: [Simd<u16, 8>; 16] = core::array::from_fn(|r| {
        let o = pos + r * stride - 4;
        Simd::<u16, 8>::from_slice(&d[o..o + 8])
    });
    let mut c = transpose_16x8(&rows);
    let s: [I16x16; 8] = core::array::from_fn(|i| c[i].cast());
    if let Some(n) = luma_core(&s, prm, max) {
        for i in 0..6 {
            c[i + 1] = n[i].cast();
        }
        let rows = transpose_8x16(&c);
        for (r, v) in rows.iter().enumerate() {
            let o = pos + r * stride - 4;
            v.copy_to_slice(&mut d[o..o + 8]);
        }
    }
}

// ----------------------------------------------------------------------- chroma deblocking

/// Chroma edge filter (8.7.2.5.5) on 16 lines; returns new p0, q0. Lanes with tc = 0 are
/// unchanged (delta clips to 0).
#[inline(always)]
fn chroma_core(s: [I16x16; 4], prm: &LumaLanes, max: i16) -> [I16x16; 2] {
    let [p1, p0, q0, q1] = s;
    let tc = I16x16::from_array(prm.tc);
    let z = I16x16::splat(0);
    let delta = clip(
        (((q0 - p0) << I16x16::splat(2)) + p1 - q1 + I16x16::splat(4)) >> I16x16::splat(3),
        -tc,
        tc,
    );
    let mx = I16x16::splat(max);
    let mp: M16 = I16x16::from_array(prm.mp).simd_ne(z);
    let mq: M16 = I16x16::from_array(prm.mq).simd_ne(z);
    [
        mp.select(clip(p0 + delta, z, mx), p0),
        mq.select(clip(q0 - delta, z, mx), q0),
    ]
}

/// Horizontal chroma edge over 16 columns starting at `pos` (q0 row).
#[inline(always)]
pub(super) fn chroma_h16_u8(d: &mut [u8], pos: usize, stride: usize, prm: &LumaLanes) {
    let base = pos - 2 * stride;
    let s: [I16x16; 4] = core::array::from_fn(|i| {
        let o = base + i * stride;
        U8x16::from_slice(&d[o..o + 16]).cast()
    });
    let r = chroma_core(s, prm, 255);
    for (i, v) in r.iter().enumerate() {
        let o = base + (i + 1) * stride;
        v.cast::<u8>().copy_to_slice(&mut d[o..o + 16]);
    }
}

#[inline(always)]
pub(super) fn chroma_h16_u16(d: &mut [u16], pos: usize, stride: usize, prm: &LumaLanes, max: i16) {
    let base = pos - 2 * stride;
    let s: [I16x16; 4] = core::array::from_fn(|i| {
        let o = base + i * stride;
        Simd::<u16, 16>::from_slice(&d[o..o + 16]).cast()
    });
    let r = chroma_core(s, prm, max);
    for (i, v) in r.iter().enumerate() {
        let o = base + (i + 1) * stride;
        v.cast::<u16>().copy_to_slice(&mut d[o..o + 16]);
    }
}

/// Vertical chroma edge over 16 rows; `pos` is q0 of the first row (8 samples p3..q3 of
/// each row are transposed; only p0 / q0 change).
#[inline(always)]
pub(super) fn chroma_v16_u8(d: &mut [u8], pos: usize, stride: usize, prm: &LumaLanes) {
    let rows: [Simd<u8, 8>; 16] = core::array::from_fn(|r| {
        let o = pos + r * stride - 4;
        Simd::<u8, 8>::from_slice(&d[o..o + 8])
    });
    let mut c = transpose_16x8(&rows);
    let s: [I16x16; 4] = core::array::from_fn(|i| c[i + 2].cast());
    let r = chroma_core(s, prm, 255);
    c[3] = r[0].cast();
    c[4] = r[1].cast();
    let rows = transpose_8x16(&c);
    for (r, v) in rows.iter().enumerate() {
        let o = pos + r * stride - 4;
        v.copy_to_slice(&mut d[o..o + 8]);
    }
}

#[inline(always)]
pub(super) fn chroma_v16_u16(d: &mut [u16], pos: usize, stride: usize, prm: &LumaLanes, max: i16) {
    let rows: [Simd<u16, 8>; 16] = core::array::from_fn(|r| {
        let o = pos + r * stride - 4;
        Simd::<u16, 8>::from_slice(&d[o..o + 8])
    });
    let mut c = transpose_16x8(&rows);
    let s: [I16x16; 4] = core::array::from_fn(|i| c[i + 2].cast());
    let r = chroma_core(s, prm, max);
    c[3] = r[0].cast();
    c[4] = r[1].cast();
    let rows = transpose_8x16(&c);
    for (r, v) in rows.iter().enumerate() {
        let o = pos + r * stride - 4;
        v.copy_to_slice(&mut d[o..o + 8]);
    }
}

// --------------------------------------------------------------------------- >8-bit SAO

/// t[k] where idx == k (k = 0..3), else 0.
#[inline(always)]
fn pick4(idx: I16x16, t: &[i16; 4]) -> I16x16 {
    let z = I16x16::splat(0);
    let mut off = z;
    for (k, &tk) in t.iter().enumerate() {
        off = idx
            .simd_eq(I16x16::splat(k as i16))
            .select(I16x16::splat(tk), off);
    }
    off
}

/// SAO edge offset for >8-bit samples (see `sao_eo`); max = (1 << bitDepth) - 1.
#[inline(always)]
pub(super) fn sao_eo_u16(
    cur: &[u16],
    a: &[u16],
    b: &[u16],
    dst: &mut [u16],
    t: &[i16; 4],
    max: i16,
) -> usize {
    let len = cur.len().min(a.len()).min(b.len()).min(dst.len());
    if len < 16 {
        return 0;
    }
    let ld = |s: &[u16], x: usize| -> I16x16 { Simd::<u16, 16>::from_slice(&s[x..x + 16]).cast() };
    let mut x = 0;
    loop {
        let x16 = x.min(len - 16);
        let v = ld(cur, x16);
        let (va, vb) = (ld(a, x16), ld(b, x16));
        // -(sgn(v - a) + sgn(v - b)) from -1/0 mask lanes; index 0..3 for edgeIdx 1,2,3,4.
        let e = va.simd_lt(v).to_simd() - va.simd_gt(v).to_simd() + vb.simd_lt(v).to_simd()
            - vb.simd_gt(v).to_simd();
        // e = -2 -> t[0], -1 -> t[1], 1 -> t[2], 2 -> t[3] (sum = -e).
        let s = -e;
        let idx = s + I16x16::splat(2)
            - s.simd_gt(I16x16::splat(0))
                .select(I16x16::splat(1), I16x16::splat(0));
        let off = s
            .simd_ne(I16x16::splat(0))
            .select(pick4(idx, t), I16x16::splat(0));
        clip(v + off, I16x16::splat(0), I16x16::splat(max))
            .cast::<u16>()
            .copy_to_slice(&mut dst[x16..x16 + 16]);
        x += 16;
        if x >= len {
            return len;
        }
    }
}

/// SAO band offset for >8-bit samples: bandTable lookup of (v >> shift) - band_pos.
#[inline(always)]
pub(super) fn sao_bo_u16(
    src: &[u16],
    dst: &mut [u16],
    shift: u32,
    band_pos: u8,
    o: &[i16; 4],
    max: i16,
) -> usize {
    let n = src.len().min(dst.len()) & !15;
    let mut x = 0;
    while x < n {
        let v: I16x16 = Simd::<u16, 16>::from_slice(&src[x..x + 16]).cast();
        let k = ((v >> I16x16::splat(shift as i16)) - I16x16::splat(band_pos as i16))
            & I16x16::splat(31);
        let off = pick4(k, o);
        clip(v + off, I16x16::splat(0), I16x16::splat(max))
            .cast::<u16>()
            .copy_to_slice(&mut dst[x..x + 16]);
        x += 16;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transposes_invert() {
        let rows: [Simd<u8, 8>; 16] =
            core::array::from_fn(|r| Simd::from_array(core::array::from_fn(|i| (r * 8 + i) as u8)));
        let c = transpose_16x8(&rows);
        for i in 0..8 {
            for r in 0..16 {
                assert_eq!(c[i][r], rows[r][i], "column {i} row {r}");
            }
        }
        assert_eq!(transpose_8x16(&c), rows);
    }
}
