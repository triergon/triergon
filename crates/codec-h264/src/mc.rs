//! Fractional sample interpolation (clause 8.4.2.2) and weighted sample prediction (8.4.2.3).
//!
//! Reference access: when the source window of a block lies inside the reference plane the
//! samples are read in place; otherwise a small edge-emulated window is built by clamping
//! coordinates (equations 8-228 / 8-229 / 8-237 / 8-238), so no per-sample clamping happens on
//! the common path.

/// A view of one reference plane: a frame, or one field of a frame (stride doubled).
#[derive(Clone, Copy)]
pub(crate) struct PlaneRef<'a> {
    pub data: &'a [u8],
    pub base: usize,
    pub stride: usize,
    pub w: i32,
    pub h: i32,
}

impl<'a> PlaneRef<'a> {
    pub(crate) fn new(data: &'a [u8], width: usize, height: usize, parity: u8) -> Self {
        match parity {
            0 => PlaneRef {
                data,
                base: 0,
                stride: width,
                w: width as i32,
                h: height as i32,
            },
            1 => PlaneRef {
                data,
                base: 0,
                stride: width * 2,
                w: width as i32,
                h: (height / 2) as i32,
            },
            _ => PlaneRef {
                data,
                base: width,
                stride: width * 2,
                w: width as i32,
                h: (height / 2) as i32,
            },
        }
    }
}

const WIN: usize = 24; // max window (16 + 5) rounded up

/// Fetch a `ww` x `wh` window at (x0, y0) (may lie outside) into `out` with stride WIN,
/// replicating edge samples (equations 8-228 / 8-229, 8-237 / 8-238).
#[inline(never)]
fn fetch(r: &PlaneRef, x0: i32, y0: i32, ww: usize, wh: usize, out: &mut [u8; WIN * WIN]) {
    // Columns [xa, xb) are inside the plane.
    let xa = (-x0).clamp(0, ww as i32) as usize;
    let xb = (r.w - x0).clamp(xa as i32, ww as i32) as usize;
    for y in 0..wh {
        let yy = (y0 + y as i32).clamp(0, r.h - 1) as usize;
        let row = r.base + yy * r.stride;
        let o = &mut out[y * WIN..y * WIN + ww];
        if xb > xa {
            let s = (x0 + xa as i32) as usize;
            o[xa..xb].copy_from_slice(&r.data[row + s..row + s + (xb - xa)]);
        }
        let left = r.data[row];
        let right = r.data[row + r.w as usize - 1];
        o[..xa].fill(left);
        o[xb..].fill(right);
    }
}

/// Luma prediction of a w x h block at integer position (xi, yi) with fraction (fx, fy),
/// written to `dst` with stride `ds` (8.4.2.2.1).
#[allow(clippy::too_many_arguments)]
pub(crate) fn luma_mc(
    r: &PlaneRef,
    xi: i32,
    yi: i32,
    fx: u32,
    fy: u32,
    w: usize,
    h: usize,
    dst: &mut [u8],
    ds: usize,
) {
    match w {
        16 => luma_mc_w::<16>(r, xi, yi, fx, fy, h, dst, ds),
        8 => luma_mc_w::<8>(r, xi, yi, fx, fy, h, dst, ds),
        _ => luma_mc_w::<4>(r, xi, yi, fx, fy, h, dst, ds),
    }
}

/// Clip1((v + 16) >> 5) of a 6-tap intermediate; every first-stage intermediate (b1 / h1) lies
/// in -2550 ..= 10710, so 16-bit lanes suffice.
#[inline(always)]
#[allow(clippy::manual_clamp)] // Ord::clamp asserts min <= max at run time, which blocks vectorisation
fn clip_h(v: i16) -> u8 {
    ((v + 16) >> 5).max(0).min(255) as u8
}

/// Horizontal 6-tap intermediates b1 for one row starting at column x - 2 (`src[o..]`): six
/// overlapping W-sample windows combined lane by lane.
#[inline(always)]
fn h6<const W: usize>(src: &[u8], o: usize) -> [i16; W] {
    let win = &src[o..o + W + 5];
    let t = |k: usize| -> &[u8; W] { win[k..k + W].try_into().unwrap() };
    let (s0, s1, s2, s3, s4, s5) = (t(0), t(1), t(2), t(3), t(4), t(5));
    let mut out = [0i16; W];
    for x in 0..W {
        let outer = s0[x] as i16 + s5[x] as i16;
        let mid = s1[x] as i16 + s4[x] as i16;
        let inner = s2[x] as i16 + s3[x] as i16;
        out[x] = outer - 5 * mid + 20 * inner;
    }
    out
}

#[inline(always)]
fn row16(src: &[u8], o: usize) -> &[u8; 16] {
    src[o..o + 16].try_into().unwrap()
}

/// Vertical 6-tap intermediates h1 for one output row from 6 source rows.
#[inline(always)]
fn v6<const W: usize>(src: &[u8], o: usize, st: usize) -> [i16; W] {
    let (r0, r1, r2) = (row16(src, o), row16(src, o + st), row16(src, o + 2 * st));
    let (r3, r4, r5) = (
        row16(src, o + 3 * st),
        row16(src, o + 4 * st),
        row16(src, o + 5 * st),
    );
    let mut out = [0i16; W];
    for x in 0..W {
        let outer = r0[x] as i16 + r5[x] as i16;
        let mid = r1[x] as i16 + r4[x] as i16;
        let inner = r2[x] as i16 + r3[x] as i16;
        out[x] = outer - 5 * mid + 20 * inner;
    }
    out
}

/// Second-stage 6-tap over horizontal intermediates (the j position, 8-246 / 8-247).
#[inline(always)]
fn j6<const W: usize>(b: &[[i16; W]], y: usize) -> [u8; W] {
    let (r0, r1, r2, r3, r4, r5) = (&b[y], &b[y + 1], &b[y + 2], &b[y + 3], &b[y + 4], &b[y + 5]);
    let mut out = [0u8; W];
    for x in 0..W {
        let outer = r0[x] as i32 + r5[x] as i32;
        let mid = r1[x] as i32 + r4[x] as i32;
        let inner = r2[x] as i32 + r3[x] as i32;
        let v = outer - 5 * mid + 20 * inner;
        out[x] = ((v + 512) >> 10).clamp(0, 255) as u8;
    }
    out
}

/// Copy an h-row block of W samples.
#[inline(always)]
fn copy_block<const W: usize>(
    src: &[u8],
    so: usize,
    st: usize,
    h: usize,
    dst: &mut [u8],
    ds: usize,
) {
    for y in 0..h {
        let s: &[u8; W] = src[so + y * st..so + y * st + W].try_into().unwrap();
        let d: &mut [u8; W] = (&mut dst[y * ds..y * ds + W]).try_into().unwrap();
        *d = *s;
    }
}

#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn luma_mc_w<const W: usize>(
    r: &PlaneRef,
    xi: i32,
    yi: i32,
    fx: u32,
    fy: u32,
    h: usize,
    dst: &mut [u8],
    ds: usize,
) {
    if fx == 0 && fy == 0 && xi >= 0 && yi >= 0 && xi + W as i32 <= r.w && yi + h as i32 <= r.h {
        // Full-sample position (8-239): a plain block copy.
        let o = r.base + yi as usize * r.stride + xi as usize;
        copy_block::<W>(r.data, o, r.stride, h, dst, ds);
        return;
    }
    // The kernels read fixed 21-byte rows from x - 2 (16-byte rows from x - 2 / x - 1 for the
    // vertical filter), so the in-place path needs 19 columns of margin on the right.
    let inside = xi - 2 >= 0 && yi - 2 >= 0 && xi + 19 <= r.w && yi + h as i32 + 3 <= r.h;
    if inside {
        // Source origin at (x - 2, y - 2).
        let base = r.base + (yi - 2) as usize * r.stride + (xi - 2) as usize;
        luma_kernel::<W>(r.data, base, r.stride, fx, fy, h, dst, ds);
    } else {
        let mut win = [0u8; WIN * WIN];
        fetch(r, xi - 2, yi - 2, W + 5, h + 5, &mut win);
        luma_kernel::<W>(&win, 0, WIN, fx, fy, h, dst, ds);
    }
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::needless_range_loop)] // indexed form over parallel fixed-size arrays (mirrors the spec loops, keeps the hot loop vectorisable)
fn luma_kernel<const W: usize>(
    src: &[u8],
    base: usize,
    st: usize,
    fx: u32,
    fy: u32,
    h: usize,
    dst: &mut [u8],
    ds: usize,
) {
    let g = |y: usize| base + (y + 2) * st + 2; // offset of G(0, y)
    match (fx, fy) {
        (0, 0) => {
            for y in 0..h {
                let o = g(y);
                dst[y * ds..y * ds + W].copy_from_slice(&src[o..o + W]);
            }
        }
        (_, 0) => {
            for y in 0..h {
                let o = base + (y + 2) * st;
                let b1 = h6::<W>(src, o);
                let gr = &src[o + 2 + (fx == 3) as usize..o + 2 + (fx == 3) as usize + W];
                let d = &mut dst[y * ds..y * ds + W];
                if fx == 2 {
                    for x in 0..W {
                        d[x] = clip_h(b1[x]);
                    }
                } else {
                    for x in 0..W {
                        d[x] = ((clip_h(b1[x]) as u16 + gr[x] as u16 + 1) >> 1) as u8;
                    }
                }
            }
        }
        (0, _) => {
            for y in 0..h {
                let h1 = v6::<W>(src, base + y * st + 2, st);
                let gr = &src[g(y + (fy == 3) as usize)..g(y + (fy == 3) as usize) + W];
                let d = &mut dst[y * ds..y * ds + W];
                if fy == 2 {
                    for x in 0..W {
                        d[x] = clip_h(h1[x]);
                    }
                } else {
                    for x in 0..W {
                        d[x] = ((clip_h(h1[x]) as u16 + gr[x] as u16 + 1) >> 1) as u8;
                    }
                }
            }
        }
        (2, _) | (_, 2) => {
            // Horizontal intermediates for rows -2 .. h + 3.
            let mut b1 = [[0i16; W]; 21];
            for k in 0..h + 5 {
                b1[k] = h6::<W>(src, base + k * st);
            }
            for y in 0..h {
                let j = j6::<W>(&b1, y);
                let d = &mut dst[y * ds..y * ds + W];
                match (fx, fy) {
                    (2, 2) => d.copy_from_slice(&j),
                    (2, 1) | (2, 3) => {
                        let br = &b1[y + 2 + (fy == 3) as usize];
                        for x in 0..W {
                            d[x] = ((j[x] as u16 + clip_h(br[x]) as u16 + 1) >> 1) as u8;
                        }
                    }
                    _ => {
                        let h1 = v6::<W>(src, base + y * st + 2 + (fx == 3) as usize, st);
                        for x in 0..W {
                            d[x] = ((j[x] as u16 + clip_h(h1[x]) as u16 + 1) >> 1) as u8;
                        }
                    }
                }
            }
        }
        _ => {
            // Diagonal quarter positions e, g, p, r: average of b (row y or y + 1) and h
            // (column x or x + 1).
            for y in 0..h {
                let ob = base + (y + 2 + (fy == 3) as usize) * st;
                let b1 = h6::<W>(src, ob);
                let h1 = v6::<W>(src, base + y * st + 2 + (fx == 3) as usize, st);
                let d = &mut dst[y * ds..y * ds + W];
                for x in 0..W {
                    d[x] = ((clip_h(b1[x]) as u16 + clip_h(h1[x]) as u16 + 1) >> 1) as u8;
                }
            }
        }
    }
}

/// Chroma prediction (8.4.2.2.2) of a w x h block at integer (xi, yi), eighth fraction (fx, fy).
#[allow(clippy::too_many_arguments)]
pub(crate) fn chroma_mc(
    r: &PlaneRef,
    xi: i32,
    yi: i32,
    fx: u32,
    fy: u32,
    w: usize,
    h: usize,
    dst: &mut [u8],
    ds: usize,
) {
    match w {
        8 => chroma_mc_w::<8>(r, xi, yi, fx, fy, h, dst, ds),
        4 => chroma_mc_w::<4>(r, xi, yi, fx, fy, h, dst, ds),
        _ => chroma_mc_w::<2>(r, xi, yi, fx, fy, h, dst, ds),
    }
}

#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn chroma_mc_w<const W: usize>(
    r: &PlaneRef,
    xi: i32,
    yi: i32,
    fx: u32,
    fy: u32,
    h: usize,
    dst: &mut [u8],
    ds: usize,
) {
    if fx == 0 && fy == 0 && xi >= 0 && yi >= 0 && xi + W as i32 <= r.w && yi + h as i32 <= r.h {
        let o = r.base + yi as usize * r.stride + xi as usize;
        copy_block::<W>(r.data, o, r.stride, h, dst, ds);
        return;
    }
    let inside = xi >= 0 && yi >= 0 && xi + (W as i32) < r.w && yi + (h as i32) < r.h;
    if inside {
        chroma_kernel::<W>(
            r.data,
            r.base + yi as usize * r.stride + xi as usize,
            r.stride,
            fx,
            fy,
            h,
            dst,
            ds,
        );
    } else {
        let mut win = [0u8; WIN * WIN];
        fetch(r, xi, yi, W + 1, h + 1, &mut win);
        chroma_kernel::<W>(&win, 0, WIN, fx, fy, h, dst, ds);
    }
}

#[allow(clippy::too_many_arguments)]
fn chroma_kernel<const W: usize>(
    src: &[u8],
    base: usize,
    st: usize,
    fx: u32,
    fy: u32,
    h: usize,
    dst: &mut [u8],
    ds: usize,
) {
    let (fx, fy) = (fx as u16, fy as u16);
    if fx == 0 && fy == 0 {
        for y in 0..h {
            let r0: &[u8; W] = src[base + y * st..base + y * st + W].try_into().unwrap();
            let d: &mut [u8; W] = (&mut dst[y * ds..y * ds + W]).try_into().unwrap();
            d.copy_from_slice(r0);
        }
        return;
    }
    if fy == 0 {
        // Horizontal-only interpolation ((8 - xF) * A + xF * B + 4) >> 3 == equation 8-266 with yF = 0.
        let (a, b) = (8 - fx, fx);
        for y in 0..h {
            let r0 = &src[base + y * st..base + y * st + W + 1];
            let d: &mut [u8; W] = (&mut dst[y * ds..y * ds + W]).try_into().unwrap();
            for x in 0..W {
                d[x] = (((a * r0[x] as u16 + b * r0[x + 1] as u16) * 8 + 32) >> 6) as u8;
            }
        }
        return;
    }
    let c00 = (8 - fx) * (8 - fy);
    let c10 = fx * (8 - fy);
    let c01 = (8 - fx) * fy;
    let c11 = fx * fy;
    let mut r0 = &src[base..base + W + 1];
    for y in 0..h {
        let r1 = &src[base + (y + 1) * st..base + (y + 1) * st + W + 1];
        let d: &mut [u8; W] = (&mut dst[y * ds..y * ds + W]).try_into().unwrap();
        for x in 0..W {
            let v = c00 * r0[x] as u16
                + c10 * r0[x + 1] as u16
                + c01 * r1[x] as u16
                + c11 * r1[x + 1] as u16
                + 32;
            d[x] = (v >> 6) as u8;
        }
        r0 = r1;
    }
}

/// Default weighted sample prediction (8-273): `d` = rounded average of `a` and `b`.
#[allow(clippy::too_many_arguments)]
#[inline]
pub(crate) fn avg2(
    d: &mut [u8],
    ds: usize,
    a: &[u8],
    sa: usize,
    b: &[u8],
    sb: usize,
    w: usize,
    h: usize,
) {
    match w {
        16 => avg_w::<16>(d, ds, a, sa, b, sb, h),
        8 => avg_w::<8>(d, ds, a, sa, b, sb, h),
        4 => avg_w::<4>(d, ds, a, sa, b, sb, h),
        _ => avg_w::<2>(d, ds, a, sa, b, sb, h),
    }
}

#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn avg_w<const W: usize>(
    d: &mut [u8],
    ds: usize,
    a: &[u8],
    sa: usize,
    b: &[u8],
    sb: usize,
    h: usize,
) {
    for y in 0..h {
        let rd: &mut [u8; W] = (&mut d[y * ds..y * ds + W]).try_into().unwrap();
        let ra: &[u8; W] = a[y * sa..y * sa + W].try_into().unwrap();
        let rb: &[u8; W] = b[y * sb..y * sb + W].try_into().unwrap();
        for x in 0..W {
            rd[x] = ((ra[x] as u16 + rb[x] as u16 + 1) >> 1) as u8;
        }
    }
}

/// Explicit weighted prediction, single list (8-270, 8-271).
#[inline]
pub(crate) fn weight_uni(
    a: &mut [u8],
    sa: usize,
    w: usize,
    h: usize,
    log_wd: u32,
    wt: i32,
    o: i32,
) {
    if o == 0 && wt == 1 << log_wd {
        return; // default weights: identity
    }
    match w {
        16 => weight_uni_w::<16>(a, sa, h, log_wd, wt, o),
        8 => weight_uni_w::<8>(a, sa, h, log_wd, wt, o),
        4 => weight_uni_w::<4>(a, sa, h, log_wd, wt, o),
        _ => weight_uni_w::<2>(a, sa, h, log_wd, wt, o),
    }
}

#[inline(always)]
fn weight_uni_w<const W: usize>(a: &mut [u8], sa: usize, h: usize, log_wd: u32, wt: i32, o: i32) {
    // ((v*w + 2^(logWD-1)) >> logWD) + o, with o folded into the rounding term (8-270 / 8-271).
    let round = (if log_wd >= 1 { 1 << (log_wd - 1) } else { 0 }) + (o << log_wd);
    for y in 0..h {
        let ra: &mut [u8; W] = (&mut a[y * sa..y * sa + W]).try_into().unwrap();
        for v in ra.iter_mut() {
            *v = ((*v as i32 * wt + round) >> log_wd).clamp(0, 255) as u8;
        }
    }
}

/// Bi-predictive weighted prediction (8-272) of `a` and `b` into `d`.
#[allow(clippy::too_many_arguments)]
#[inline]
pub(crate) fn weight_bi(
    d: &mut [u8],
    ds: usize,
    a: &[u8],
    sa: usize,
    b: &[u8],
    sb: usize,
    w: usize,
    h: usize,
    log_wd: u32,
    w0: i32,
    w1: i32,
    o: i32,
) {
    if o == 0 && w0 == 1 << log_wd && w1 == w0 {
        return avg2(d, ds, a, sa, b, sb, w, h); // equal default weights: (a + b + 1) >> 1
    }
    let round = (1 << log_wd) + (o << (log_wd + 1));
    if 255 * (w0.abs() + w1.abs()) + round.abs() <= i16::MAX as i32 {
        // Every intermediate of 8-272 fits in 16 bits (always true for implicit weights, whose
        // sum is 64): same result with twice the lanes per vector.
        let (w0, w1, round) = (w0 as i16, w1 as i16, round as i16);
        return match w {
            16 => weight_bi_w::<16, i16>(d, ds, a, sa, b, sb, h, log_wd, w0, w1, round),
            8 => weight_bi_w::<8, i16>(d, ds, a, sa, b, sb, h, log_wd, w0, w1, round),
            4 => weight_bi_w::<4, i16>(d, ds, a, sa, b, sb, h, log_wd, w0, w1, round),
            _ => weight_bi_w::<2, i16>(d, ds, a, sa, b, sb, h, log_wd, w0, w1, round),
        };
    }
    match w {
        16 => weight_bi_w::<16, i32>(d, ds, a, sa, b, sb, h, log_wd, w0, w1, round),
        8 => weight_bi_w::<8, i32>(d, ds, a, sa, b, sb, h, log_wd, w0, w1, round),
        4 => weight_bi_w::<4, i32>(d, ds, a, sa, b, sb, h, log_wd, w0, w1, round),
        _ => weight_bi_w::<2, i32>(d, ds, a, sa, b, sb, h, log_wd, w0, w1, round),
    }
}

/// Lane type of the bi-prediction weighting: i32 in general, i16 when the caller has checked
/// that 255 * (|w0| + |w1|) + |round| fits.
trait WLane:
    Copy
    + Ord
    + From<u8>
    + std::ops::Mul<Output = Self>
    + std::ops::Add<Output = Self>
    + std::ops::Shr<u32, Output = Self>
{
    const ZERO: Self;
    const MAX8: Self;
    fn to_u8(self) -> u8;
}
impl WLane for i16 {
    const ZERO: Self = 0;
    const MAX8: Self = 255;
    #[inline(always)]
    fn to_u8(self) -> u8 {
        self as u8
    }
}
impl WLane for i32 {
    const ZERO: Self = 0;
    const MAX8: Self = 255;
    #[inline(always)]
    fn to_u8(self) -> u8 {
        self as u8
    }
}

/// ((a*w0 + b*w1 + 2^logWD) >> (logWD + 1)) + o with o folded into `round` (8-272).
#[allow(clippy::too_many_arguments)]
#[inline(always)]
#[allow(clippy::manual_clamp)] // Ord::clamp asserts min <= max at run time, which blocks vectorisation
fn weight_bi_w<const W: usize, T: WLane>(
    d: &mut [u8],
    ds: usize,
    a: &[u8],
    sa: usize,
    b: &[u8],
    sb: usize,
    h: usize,
    log_wd: u32,
    w0: T,
    w1: T,
    round: T,
) {
    let sh = log_wd + 1;
    for y in 0..h {
        let rd: &mut [u8; W] = (&mut d[y * ds..y * ds + W]).try_into().unwrap();
        let ra: &[u8; W] = a[y * sa..y * sa + W].try_into().unwrap();
        let rb: &[u8; W] = b[y * sb..y * sb + W].try_into().unwrap();
        for x in 0..W {
            let r = (T::from(ra[x]) * w0 + T::from(rb[x]) * w1 + round) >> sh;
            rd[x] = r.max(T::ZERO).min(T::MAX8).to_u8();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weight_bi_matches_equation_8_272() {
        let mut seed = 0x1234_5678u32;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let b: Vec<u8> = (0..256).map(|_| rnd() as u8).collect();
        for case in 0..4000 {
            let log_wd = rnd() % 8;
            let (w0, w1, o) = if case % 2 == 0 {
                let w1 = (rnd() % 193) as i32 - 64; // implicit: w0 + w1 = 64
                (64 - w1, w1, 0)
            } else {
                (
                    (rnd() % 256) as i32 - 128,
                    (rnd() % 256) as i32 - 128,
                    (rnd() % 256) as i32 - 128,
                )
            };
            let a: Vec<u8> = (0..256).map(|_| rnd() as u8).collect();
            let mut x = vec![0u8; 256];
            weight_bi(&mut x, 16, &a, 16, &b, 16, 16, 16, log_wd, w0, w1, o);
            let want: Vec<u8> = (0..256)
                .map(|i| {
                    let v =
                        ((a[i] as i32 * w0 + b[i] as i32 * w1 + (1 << log_wd)) >> (log_wd + 1)) + o;
                    v.clamp(0, 255) as u8
                })
                .collect();
            assert_eq!(x, want, "{log_wd} {w0} {w1} {o}");
        }
    }

    #[test]
    fn flat_plane_interpolates_flat() {
        let data = vec![77u8; 32 * 32];
        let r = PlaneRef::new(&data, 32, 32, 0);
        for fx in 0..4 {
            for fy in 0..4 {
                let mut d = [0u8; 256];
                luma_mc(&r, -5, 30, fx, fy, 16, 16, &mut d, 16);
                assert!(d.iter().all(|&v| v == 77), "{fx} {fy}");
            }
        }
        let mut d = [0u8; 64];
        chroma_mc(&r, 40, -3, 3, 5, 8, 8, &mut d, 8);
        assert!(d.iter().all(|&v| v == 77));
    }
}
