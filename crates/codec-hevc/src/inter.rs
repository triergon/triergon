//! Fractional sample interpolation (clause 8.5.3.3.3) and weighted sample prediction
//! (clause 8.5.3.3.4).
//!
//! Prediction samples (the spec's 14-bit predSamplesLX) are stored as `v - PRED_OFFSET` in
//! i16 so that even the extreme 2-D filter results fit; every consumer adds the offset back
//! (folded into its rounding constant). Reference coordinates are clamped to the picture
//! (8-222, 8-223); blocks whose filter support crosses the border are first copied into an
//! edge-emulation buffer so the inner loops never clamp. Inner loops run over pre-sliced
//! rows of exactly `w` samples, which lets LLVM drop bounds checks and vectorize.

use crate::picture::{Pixel, Plane};
use crate::tables::{CHROMA_FILTER, LUMA_FILTER};

pub const PRED_STRIDE: usize = 64;
pub const PRED_OFFSET: i32 = 8192;
/// Edge buffer: (64 + 7) x (64 + 7) samples.
pub const EDGE_STRIDE: usize = 80;
pub const EDGE_SIZE: usize = EDGE_STRIDE * 72;

pub struct McScratch<P> {
    pub edge: Vec<P>,
    pub tmp: Vec<i16>,
}

impl<P: Pixel> McScratch<P> {
    pub fn new() -> Self {
        McScratch {
            edge: vec![P::default(); EDGE_SIZE],
            tmp: vec![0; PRED_STRIDE * 72],
        }
    }
}

impl<P: Pixel> Default for McScratch<P> {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns (samples, index of sample (x, y), stride) covering [x - before, x + w + after) x
/// [y - before, y + h + after), using edge emulation when needed.
#[inline]
#[allow(clippy::too_many_arguments)]
fn source<'a, P: Pixel>(
    plane: &'a Plane<P>,
    edge: &'a mut [P],
    x: i32,
    y: i32,
    w: usize,
    h: usize,
    before: i32,
    after: i32,
) -> (&'a [P], usize, usize) {
    let pw = plane.width as i32;
    let ph = plane.height as i32;
    let x0 = x - before;
    let y0 = y - before;
    let x1 = x + w as i32 + after; // exclusive
    let y1 = y + h as i32 + after;
    if x0 >= 0 && y0 >= 0 && x1 <= pw && y1 <= ph {
        let off = y as usize * plane.stride + x as usize;
        return (&plane.data[..], off, plane.stride);
    }
    let bw = (x1 - x0) as usize;
    let bh = (y1 - y0) as usize;
    // Columns [l, r) of the block lie inside the picture; the rest replicate the edge.
    let l = (-x0).max(0).min(bw as i32) as usize;
    let r = (pw - x0).max(0).min(bw as i32) as usize;
    for j in 0..bh {
        let sy = (y0 + j as i32).max(0).min(ph - 1) as usize;
        let srow = &plane.data[sy * plane.stride..sy * plane.stride + plane.width];
        let drow = &mut edge[j * EDGE_STRIDE..j * EDGE_STRIDE + bw];
        if l < r {
            let sx0 = (x0 + l as i32) as usize;
            drow[l..r].copy_from_slice(&srow[sx0..sx0 + (r - l)]);
            drow[..l].fill(srow[0]);
            drow[r..].fill(srow[plane.width - 1]);
        } else if r == 0 && x0 >= pw {
            drow.fill(srow[plane.width - 1]);
        } else if r == 0 {
            // block entirely left of the picture, or right
            let v = if x0 < 0 && x1 <= 0 {
                srow[0]
            } else {
                srow[plane.width - 1]
            };
            drow.fill(v);
        } else {
            drow.fill(srow[0]);
        }
    }
    let off = before as usize * EDGE_STRIDE + before as usize;
    (&edge[..], off, EDGE_STRIDE)
}

#[inline(always)]
fn row<P>(s: &[P], start: usize, w: usize) -> &[P] {
    &s[start..start + w]
}

/// Output stage applied to each predicted sample v (the spec's 14-bit predSampleLX):
/// MODE 0 stores v - PRED_OFFSET into an i16 prediction buffer, MODE 1 writes the default
/// uni-prediction pixel (8-262), MODE 2 writes the default bi-prediction pixel (8-264)
/// combining with an earlier MODE 0 buffer.
pub struct Out<'a, P> {
    pub pred: &'a mut [i16],
    pub dst: &'a mut [P],
    pub dst_off: usize,
    pub dst_stride: usize,
    pub other: &'a [i16],
}

#[derive(Clone, Copy)]
struct Epi {
    rnd: i32,
    sh: u32,
    max: i32,
}

impl Epi {
    fn new<const MODE: u8>(bd: u32) -> Epi {
        match MODE {
            1 => Epi {
                rnd: 1 << (13 - bd),
                sh: 14 - bd,
                max: (1 << bd) - 1,
            },
            2 => Epi {
                rnd: (1 << (14 - bd)) + PRED_OFFSET,
                sh: 15 - bd,
                max: (1 << bd) - 1,
            },
            _ => Epi {
                rnd: 0,
                sh: 0,
                max: 0,
            },
        }
    }
}

#[inline(always)]
fn emit<P: Pixel, const MODE: u8>(
    v: i32,
    x: usize,
    pred: &mut [i16],
    dst: &mut [P],
    other: &[i16],
    e: Epi,
) {
    match MODE {
        0 => pred[x] = (v - PRED_OFFSET) as i16,
        1 => dst[x] = P::from_i32(((v + e.rnd) >> e.sh).max(0).min(e.max)),
        _ => dst[x] = P::from_i32(((v + other[x] as i32 + e.rnd) >> e.sh).max(0).min(e.max)),
    }
}

/// Row views for the output stage of output row y.
#[inline(always)]
fn out_rows<'b, P: Pixel, const MODE: u8>(
    o: &'b mut Out<'_, P>,
    y: usize,
    w: usize,
) -> (&'b mut [i16], &'b mut [P], &'b [i16]) {
    let pred: &mut [i16] = if MODE == 0 {
        &mut o.pred[y * PRED_STRIDE..y * PRED_STRIDE + w]
    } else {
        &mut []
    };
    let dst: &mut [P] = if MODE != 0 {
        let b = o.dst_off + y * o.dst_stride;
        &mut o.dst[b..b + w]
    } else {
        &mut []
    };
    let other: &[i16] = if MODE == 2 {
        &o.other[y * PRED_STRIDE..y * PRED_STRIDE + w]
    } else {
        &[]
    };
    (pred, dst, other)
}

/// 8-tap filter over rows given by `starts`; v = (sum) >> shift. For 8-bit input every
/// partial sum lies in [-24*255, 88*255] (sign sums of Table 8-12 coefficients) so the sum is
/// formed in i16 lanes.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn taps8<P: Pixel, const MODE: u8>(
    src: &[P],
    starts: [usize; 8],
    c: &[i8; 8],
    w: usize,
    shift: u32,
    o: &mut Out<'_, P>,
    y: usize,
    e: Epi,
) {
    let (r0, r1, r2, r3) = (
        row(src, starts[0], w),
        row(src, starts[1], w),
        row(src, starts[2], w),
        row(src, starts[3], w),
    );
    let (r4, r5, r6, r7) = (
        row(src, starts[4], w),
        row(src, starts[5], w),
        row(src, starts[6], w),
        row(src, starts[7], w),
    );
    let (pred, dst, other) = out_rows::<P, MODE>(o, y, w);
    let x0 = if shift == 0 {
        P::simd_taps8(
            [r0, r1, r2, r3, r4, r5, r6, r7],
            c,
            w,
            MODE,
            pred,
            dst,
            other,
        )
    } else {
        0
    };
    if P::IS_U8 && shift == 0 {
        let c: [i16; 8] = [
            c[0] as i16,
            c[1] as i16,
            c[2] as i16,
            c[3] as i16,
            c[4] as i16,
            c[5] as i16,
            c[6] as i16,
            c[7] as i16,
        ];
        if MODE == 1 {
            // 8-bit uni: (v + 32) >> 6 clipped, all within i16.
            for x in x0..w {
                let acc = c[0] * r0[x].to_i32() as i16
                    + c[1] * r1[x].to_i32() as i16
                    + c[2] * r2[x].to_i32() as i16
                    + c[3] * r3[x].to_i32() as i16
                    + c[4] * r4[x].to_i32() as i16
                    + c[5] * r5[x].to_i32() as i16
                    + c[6] * r6[x].to_i32() as i16
                    + c[7] * r7[x].to_i32() as i16;
                dst[x] = P::from_i32(((acc + 32) >> 6).clamp(0, 255) as i32);
            }
            return;
        }
        for x in x0..w {
            let acc = c[0] * r0[x].to_i32() as i16
                + c[1] * r1[x].to_i32() as i16
                + c[2] * r2[x].to_i32() as i16
                + c[3] * r3[x].to_i32() as i16
                + c[4] * r4[x].to_i32() as i16
                + c[5] * r5[x].to_i32() as i16
                + c[6] * r6[x].to_i32() as i16
                + c[7] * r7[x].to_i32() as i16;
            emit::<P, MODE>(acc as i32, x, pred, dst, other, e);
        }
        return;
    }
    let c: [i32; 8] = [
        c[0] as i32,
        c[1] as i32,
        c[2] as i32,
        c[3] as i32,
        c[4] as i32,
        c[5] as i32,
        c[6] as i32,
        c[7] as i32,
    ];
    for x in 0..w {
        let acc = c[0] * r0[x].to_i32()
            + c[1] * r1[x].to_i32()
            + c[2] * r2[x].to_i32()
            + c[3] * r3[x].to_i32()
            + c[4] * r4[x].to_i32()
            + c[5] * r5[x].to_i32()
            + c[6] * r6[x].to_i32()
            + c[7] * r7[x].to_i32();
        emit::<P, MODE>(acc >> shift, x, pred, dst, other, e);
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn taps4<P: Pixel, const MODE: u8>(
    src: &[P],
    starts: [usize; 4],
    c: &[i8; 4],
    w: usize,
    shift: u32,
    o: &mut Out<'_, P>,
    y: usize,
    e: Epi,
) {
    let (r0, r1, r2, r3) = (
        row(src, starts[0], w),
        row(src, starts[1], w),
        row(src, starts[2], w),
        row(src, starts[3], w),
    );
    let (pred, dst, other) = out_rows::<P, MODE>(o, y, w);
    let x0 = if shift == 0 {
        P::simd_taps4([r0, r1, r2, r3], c, w, MODE, pred, dst, other)
    } else {
        0
    };
    if P::IS_U8 && shift == 0 {
        // Chroma filter sign sums: positive <= 72, negative >= -10 (Table 8-13).
        let c: [i16; 4] = [c[0] as i16, c[1] as i16, c[2] as i16, c[3] as i16];
        if MODE == 1 {
            for x in x0..w {
                let acc = c[0] * r0[x].to_i32() as i16
                    + c[1] * r1[x].to_i32() as i16
                    + c[2] * r2[x].to_i32() as i16
                    + c[3] * r3[x].to_i32() as i16;
                dst[x] = P::from_i32(((acc + 32) >> 6).clamp(0, 255) as i32);
            }
            return;
        }
        for x in x0..w {
            let acc = c[0] * r0[x].to_i32() as i16
                + c[1] * r1[x].to_i32() as i16
                + c[2] * r2[x].to_i32() as i16
                + c[3] * r3[x].to_i32() as i16;
            emit::<P, MODE>(acc as i32, x, pred, dst, other, e);
        }
        return;
    }
    let c: [i32; 4] = [c[0] as i32, c[1] as i32, c[2] as i32, c[3] as i32];
    for x in 0..w {
        let acc = c[0] * r0[x].to_i32()
            + c[1] * r1[x].to_i32()
            + c[2] * r2[x].to_i32()
            + c[3] * r3[x].to_i32();
        emit::<P, MODE>(acc >> shift, x, pred, dst, other, e);
    }
}

/// Raw horizontal pass for 2-D filtering: tmp = (sum) >> shift (fits i16).
#[inline(always)]
fn hpass8<P: Pixel>(src: &[P], b: usize, c: &[i8; 8], w: usize, shift: u32, t: &mut [i16]) {
    let (r0, r1, r2, r3) = (
        row(src, b, w),
        row(src, b + 1, w),
        row(src, b + 2, w),
        row(src, b + 3, w),
    );
    let (r4, r5, r6, r7) = (
        row(src, b + 4, w),
        row(src, b + 5, w),
        row(src, b + 6, w),
        row(src, b + 7, w),
    );
    let t = &mut t[..w];
    if P::IS_U8 && shift == 0 {
        let x0 = P::simd_hpass8([r0, r1, r2, r3, r4, r5, r6, r7], c, w, t);
        let c: [i16; 8] = [
            c[0] as i16,
            c[1] as i16,
            c[2] as i16,
            c[3] as i16,
            c[4] as i16,
            c[5] as i16,
            c[6] as i16,
            c[7] as i16,
        ];
        for x in x0..w {
            t[x] = c[0] * r0[x].to_i32() as i16
                + c[1] * r1[x].to_i32() as i16
                + c[2] * r2[x].to_i32() as i16
                + c[3] * r3[x].to_i32() as i16
                + c[4] * r4[x].to_i32() as i16
                + c[5] * r5[x].to_i32() as i16
                + c[6] * r6[x].to_i32() as i16
                + c[7] * r7[x].to_i32() as i16;
        }
        return;
    }
    let c: [i32; 8] = [
        c[0] as i32,
        c[1] as i32,
        c[2] as i32,
        c[3] as i32,
        c[4] as i32,
        c[5] as i32,
        c[6] as i32,
        c[7] as i32,
    ];
    for x in 0..w {
        let acc = c[0] * r0[x].to_i32()
            + c[1] * r1[x].to_i32()
            + c[2] * r2[x].to_i32()
            + c[3] * r3[x].to_i32()
            + c[4] * r4[x].to_i32()
            + c[5] * r5[x].to_i32()
            + c[6] * r6[x].to_i32()
            + c[7] * r7[x].to_i32();
        t[x] = (acc >> shift) as i16;
    }
}

#[inline(always)]
fn hpass4<P: Pixel>(src: &[P], b: usize, c: &[i8; 4], w: usize, shift: u32, t: &mut [i16]) {
    let (r0, r1, r2, r3) = (
        row(src, b, w),
        row(src, b + 1, w),
        row(src, b + 2, w),
        row(src, b + 3, w),
    );
    let t = &mut t[..w];
    let x0 = if shift == 0 {
        P::simd_hpass4([r0, r1, r2, r3], c, w, t)
    } else {
        0
    };
    let c: [i32; 4] = [c[0] as i32, c[1] as i32, c[2] as i32, c[3] as i32];
    for x in x0..w {
        let acc = c[0] * r0[x].to_i32()
            + c[1] * r1[x].to_i32()
            + c[2] * r2[x].to_i32()
            + c[3] * r3[x].to_i32();
        t[x] = (acc >> shift) as i16;
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn vpass8<P: Pixel, const MODE: u8>(
    t: &[i16],
    b: usize,
    c: &[i8; 8],
    w: usize,
    o: &mut Out<'_, P>,
    y: usize,
    e: Epi,
) {
    let s = PRED_STRIDE;
    let (r0, r1, r2, r3) = (
        row(t, b, w),
        row(t, b + s, w),
        row(t, b + 2 * s, w),
        row(t, b + 3 * s, w),
    );
    let (r4, r5, r6, r7) = (
        row(t, b + 4 * s, w),
        row(t, b + 5 * s, w),
        row(t, b + 6 * s, w),
        row(t, b + 7 * s, w),
    );
    let (pred, dst, other) = out_rows::<P, MODE>(o, y, w);
    let x0 = P::simd_vpass8(
        [r0, r1, r2, r3, r4, r5, r6, r7],
        c,
        w,
        MODE,
        pred,
        dst,
        other,
    );
    let c: [i32; 8] = [
        c[0] as i32,
        c[1] as i32,
        c[2] as i32,
        c[3] as i32,
        c[4] as i32,
        c[5] as i32,
        c[6] as i32,
        c[7] as i32,
    ];
    for x in x0..w {
        let acc = c[0] * r0[x] as i32
            + c[1] * r1[x] as i32
            + c[2] * r2[x] as i32
            + c[3] * r3[x] as i32
            + c[4] * r4[x] as i32
            + c[5] * r5[x] as i32
            + c[6] * r6[x] as i32
            + c[7] * r7[x] as i32;
        emit::<P, MODE>(acc >> 6, x, pred, dst, other, e);
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn vpass4<P: Pixel, const MODE: u8>(
    t: &[i16],
    b: usize,
    c: &[i8; 4],
    w: usize,
    o: &mut Out<'_, P>,
    y: usize,
    e: Epi,
) {
    let s = PRED_STRIDE;
    let (r0, r1, r2, r3) = (
        row(t, b, w),
        row(t, b + s, w),
        row(t, b + 2 * s, w),
        row(t, b + 3 * s, w),
    );
    let (pred, dst, other) = out_rows::<P, MODE>(o, y, w);
    let x0 = P::simd_vpass4([r0, r1, r2, r3], c, w, MODE, pred, dst, other);
    let c: [i32; 4] = [c[0] as i32, c[1] as i32, c[2] as i32, c[3] as i32];
    for x in x0..w {
        let acc =
            c[0] * r0[x] as i32 + c[1] * r1[x] as i32 + c[2] * r2[x] as i32 + c[3] * r3[x] as i32;
        emit::<P, MODE>(acc >> 6, x, pred, dst, other, e);
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn luma_impl<P: Pixel, const MODE: u8>(
    plane: &Plane<P>,
    sc: &mut McScratch<P>,
    xi: i32,
    yi: i32,
    fx: usize,
    fy: usize,
    w: usize,
    h: usize,
    bit_depth: u32,
    o: &mut Out<'_, P>,
) {
    let shift1 = bit_depth - 8;
    let shift3 = 14 - bit_depth;
    let e = Epi::new::<MODE>(bit_depth);
    let int = fx == 0 && fy == 0;
    let (before, after) = if int { (0, 0) } else { (3, 4) };
    let McScratch { edge, tmp } = sc;
    let (src, off, ss) = source(plane, edge, xi, yi, w, h, before, after);
    if int
        && P::simd_int(
            src,
            off,
            ss,
            w,
            h,
            bit_depth,
            MODE,
            o.pred,
            o.dst,
            o.dst_off,
            o.dst_stride,
            o.other,
        )
    {
        return;
    }
    if !int
        && P::simd_block(
            true,
            src,
            off,
            ss,
            fx,
            fy,
            w,
            h,
            bit_depth,
            MODE,
            o.pred,
            o.dst,
            o.dst_off,
            o.dst_stride,
            o.other,
        )
    {
        return;
    }
    if int {
        for y in 0..h {
            let s = row(src, off + y * ss, w);
            let (pred, dst, other) = out_rows::<P, MODE>(o, y, w);
            for (x, sv) in s.iter().enumerate() {
                emit::<P, MODE>(sv.to_i32() << shift3, x, pred, dst, other, e);
            }
        }
    } else if fy == 0 {
        let c = &LUMA_FILTER[fx];
        for y in 0..h {
            let b = off + y * ss - 3;
            taps8::<P, MODE>(
                src,
                [b, b + 1, b + 2, b + 3, b + 4, b + 5, b + 6, b + 7],
                c,
                w,
                shift1,
                o,
                y,
                e,
            );
        }
    } else if fx == 0 {
        let c = &LUMA_FILTER[fy];
        for y in 0..h {
            let b = off + y * ss - 3 * ss;
            taps8::<P, MODE>(
                src,
                [
                    b,
                    b + ss,
                    b + 2 * ss,
                    b + 3 * ss,
                    b + 4 * ss,
                    b + 5 * ss,
                    b + 6 * ss,
                    b + 7 * ss,
                ],
                c,
                w,
                shift1,
                o,
                y,
                e,
            );
        }
    } else {
        let ch = &LUMA_FILTER[fx];
        for j in 0..h + 7 {
            hpass8(
                src,
                off + j * ss - 3 * ss - 3,
                ch,
                w,
                shift1,
                &mut tmp[j * PRED_STRIDE..],
            );
        }
        let cv = &LUMA_FILTER[fy];
        let t: &[i16] = tmp;
        for y in 0..h {
            vpass8::<P, MODE>(t, y * PRED_STRIDE, cv, w, o, y, e);
        }
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn chroma_impl<P: Pixel, const MODE: u8>(
    plane: &Plane<P>,
    sc: &mut McScratch<P>,
    xi: i32,
    yi: i32,
    fx: usize,
    fy: usize,
    w: usize,
    h: usize,
    bit_depth: u32,
    o: &mut Out<'_, P>,
) {
    let shift1 = bit_depth - 8;
    let shift3 = 14 - bit_depth;
    let e = Epi::new::<MODE>(bit_depth);
    let int = fx == 0 && fy == 0;
    let (before, after) = if int { (0, 0) } else { (1, 2) };
    let McScratch { edge, tmp } = sc;
    let (src, off, ss) = source(plane, edge, xi, yi, w, h, before, after);
    if int
        && P::simd_int(
            src,
            off,
            ss,
            w,
            h,
            bit_depth,
            MODE,
            o.pred,
            o.dst,
            o.dst_off,
            o.dst_stride,
            o.other,
        )
    {
        return;
    }
    if !int
        && P::simd_block(
            false,
            src,
            off,
            ss,
            fx,
            fy,
            w,
            h,
            bit_depth,
            MODE,
            o.pred,
            o.dst,
            o.dst_off,
            o.dst_stride,
            o.other,
        )
    {
        return;
    }
    if int {
        for y in 0..h {
            let s = row(src, off + y * ss, w);
            let (pred, dst, other) = out_rows::<P, MODE>(o, y, w);
            for (x, sv) in s.iter().enumerate() {
                emit::<P, MODE>(sv.to_i32() << shift3, x, pred, dst, other, e);
            }
        }
    } else if fy == 0 {
        let c = &CHROMA_FILTER[fx];
        for y in 0..h {
            let b = off + y * ss - 1;
            taps4::<P, MODE>(src, [b, b + 1, b + 2, b + 3], c, w, shift1, o, y, e);
        }
    } else if fx == 0 {
        let c = &CHROMA_FILTER[fy];
        for y in 0..h {
            let b = off + y * ss - ss;
            taps4::<P, MODE>(
                src,
                [b, b + ss, b + 2 * ss, b + 3 * ss],
                c,
                w,
                shift1,
                o,
                y,
                e,
            );
        }
    } else {
        let ch = &CHROMA_FILTER[fx];
        for j in 0..h + 3 {
            hpass4(
                src,
                off + j * ss - ss - 1,
                ch,
                w,
                shift1,
                &mut tmp[j * PRED_STRIDE..],
            );
        }
        let cv = &CHROMA_FILTER[fy];
        let t: &[i16] = tmp;
        for y in 0..h {
            vpass4::<P, MODE>(t, y * PRED_STRIDE, cv, w, o, y, e);
        }
    }
}

/// Luma interpolation (8.5.3.3.3.2) with output stage `mode` (see [`Out`]).
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub fn mc_luma<P: Pixel>(
    plane: &Plane<P>,
    sc: &mut McScratch<P>,
    xi: i32,
    yi: i32,
    fx: usize,
    fy: usize,
    w: usize,
    h: usize,
    bd: u32,
    mode: u8,
    o: &mut Out<'_, P>,
) {
    match mode {
        0 => luma_impl::<P, 0>(plane, sc, xi, yi, fx, fy, w, h, bd, o),
        1 => luma_impl::<P, 1>(plane, sc, xi, yi, fx, fy, w, h, bd, o),
        _ => luma_impl::<P, 2>(plane, sc, xi, yi, fx, fy, w, h, bd, o),
    }
}

/// Chroma interpolation (8.5.3.3.3.3) with output stage `mode` (see [`Out`]).
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub fn mc_chroma<P: Pixel>(
    plane: &Plane<P>,
    sc: &mut McScratch<P>,
    xi: i32,
    yi: i32,
    fx: usize,
    fy: usize,
    w: usize,
    h: usize,
    bd: u32,
    mode: u8,
    o: &mut Out<'_, P>,
) {
    match mode {
        0 => chroma_impl::<P, 0>(plane, sc, xi, yi, fx, fy, w, h, bd, o),
        1 => chroma_impl::<P, 1>(plane, sc, xi, yi, fx, fy, w, h, bd, o),
        _ => chroma_impl::<P, 2>(plane, sc, xi, yi, fx, fy, w, h, bd, o),
    }
}

/// Integer-position uni-prediction with default weights is an exact copy
/// ((p << shift3 + offset1) >> shift1 == p).
#[allow(clippy::too_many_arguments)]
#[inline(never)]
pub fn copy_block<P: Pixel>(
    dst: &mut Plane<P>,
    x0: usize,
    y0: usize,
    src: &Plane<P>,
    sc: &mut McScratch<P>,
    xi: i32,
    yi: i32,
    w: usize,
    h: usize,
) {
    let (s, off, ss) = source(src, &mut sc.edge, xi, yi, w, h, 0, 0);
    let ds = dst.stride;
    let d0 = y0 * ds + x0;
    // Fixed row widths compile to inline vector moves.
    match w {
        64 => copy_rows::<P, 64>(&mut dst.data, d0, ds, s, off, ss, h),
        32 => copy_rows::<P, 32>(&mut dst.data, d0, ds, s, off, ss, h),
        16 => copy_rows::<P, 16>(&mut dst.data, d0, ds, s, off, ss, h),
        8 => copy_rows::<P, 8>(&mut dst.data, d0, ds, s, off, ss, h),
        4 => copy_rows::<P, 4>(&mut dst.data, d0, ds, s, off, ss, h),
        _ => {
            for y in 0..h {
                dst.data[d0 + y * ds..d0 + y * ds + w]
                    .copy_from_slice(&s[off + y * ss..off + y * ss + w]);
            }
        }
    }
}

#[inline(always)]
fn copy_rows<P: Pixel, const W: usize>(
    d: &mut [P],
    doff: usize,
    ds: usize,
    s: &[P],
    soff: usize,
    ss: usize,
    h: usize,
) {
    for y in 0..h {
        let dr: &mut [P; W] = (&mut d[doff + y * ds..doff + y * ds + W])
            .try_into()
            .unwrap();
        let sr: &[P; W] = s[soff + y * ss..soff + y * ss + W].try_into().unwrap();
        *dr = *sr;
    }
}

/// Integer-position explicit weighted uni-prediction fused with the copy. Returns false if
/// the SIMD kernel does not cover the block (caller falls back to the two-pass path).
#[allow(clippy::too_many_arguments)]
pub fn weighted_copy<P: Pixel>(
    dst: &mut Plane<P>,
    x0: usize,
    y0: usize,
    src: &Plane<P>,
    sc: &mut McScratch<P>,
    xi: i32,
    yi: i32,
    w: usize,
    h: usize,
    bit_depth: u32,
    log2_denom: u32,
    weight: i32,
    offset: i32,
) -> bool {
    if w % 8 != 0 {
        return false;
    }
    let (s, off, ss) = source(src, &mut sc.edge, xi, yi, w, h, 0, 0);
    let shift3 = 14 - bit_depth;
    let log2wd = log2_denom + shift3;
    let ds = dst.stride;
    P::simd_wcopy(
        &mut dst.data,
        y0 * ds + x0,
        ds,
        s,
        off,
        ss,
        w,
        h,
        shift3,
        log2wd,
        weight,
        offset << (bit_depth - 8),
        (1 << bit_depth) - 1,
    )
}

/// Explicit weighted prediction, uni-directional (8-275).
#[allow(clippy::too_many_arguments)]
#[inline(never)]
pub fn put_weighted_uni<P: Pixel>(
    plane: &mut Plane<P>,
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    src: &[i16],
    bit_depth: u32,
    log2_denom: u32,
    weight: i32,
    offset: i32,
) {
    let shift1 = 14 - bit_depth;
    let log2wd = log2_denom + shift1;
    let rnd = 1 << (log2wd - 1);
    let o = offset << (bit_depth - 8);
    let max = (1 << bit_depth) - 1;
    let stride = plane.stride;
    if P::simd_wuni(
        &mut plane.data,
        y0 * stride + x0,
        stride,
        src,
        w,
        h,
        log2wd,
        weight,
        o,
        max,
    ) {
        return;
    }
    for y in 0..h {
        let d = &mut plane.data[(y0 + y) * stride + x0..(y0 + y) * stride + x0 + w];
        let s = &src[y * PRED_STRIDE..y * PRED_STRIDE + w];
        for x in 0..w {
            let v = s[x] as i32 + PRED_OFFSET;
            d[x] = P::from_i32((((v * weight + rnd) >> log2wd) + o).max(0).min(max));
        }
    }
}

/// Explicit weighted prediction, bi-directional (8-277).
#[allow(clippy::too_many_arguments)]
#[inline(never)]
pub fn put_weighted_bi<P: Pixel>(
    plane: &mut Plane<P>,
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    a: &[i16],
    b: &[i16],
    bit_depth: u32,
    log2_denom: u32,
    w0: i32,
    w1: i32,
    o0: i32,
    o1: i32,
) {
    let shift1 = 14 - bit_depth;
    let log2wd = log2_denom + shift1;
    let o0 = o0 << (bit_depth - 8);
    let o1 = o1 << (bit_depth - 8);
    let rnd = (o0 + o1 + 1) << log2wd;
    let max = (1 << bit_depth) - 1;
    let stride = plane.stride;
    if P::simd_wbi(
        &mut plane.data,
        y0 * stride + x0,
        stride,
        a,
        b,
        w,
        h,
        log2wd,
        w0,
        w1,
        rnd,
        max,
    ) {
        return;
    }
    for y in 0..h {
        let d = &mut plane.data[(y0 + y) * stride + x0..(y0 + y) * stride + x0 + w];
        let sa = &a[y * PRED_STRIDE..y * PRED_STRIDE + w];
        let sb = &b[y * PRED_STRIDE..y * PRED_STRIDE + w];
        for x in 0..w {
            let va = sa[x] as i32 + PRED_OFFSET;
            let vb = sb[x] as i32 + PRED_OFFSET;
            d[x] = P::from_i32(((va * w0 + vb * w1 + rnd) >> (log2wd + 1)).max(0).min(max));
        }
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// Development micro-benchmark (not evidence).
    #[test]
    #[ignore]
    fn bench_mc() {
        let mut plane = Plane::<u8>::new(256, 256, 0);
        let mut seed = 5u32;
        for v in plane.data.iter_mut() {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            *v = (seed >> 16) as u8;
        }
        let mut dst = Plane::<u8>::new(256, 256, 0);
        let mut sc = McScratch::<u8>::new();
        let mut pred = vec![0i16; PRED_STRIDE * 64];
        for (w, h) in [(4usize, 4usize), (8, 8), (16, 16), (32, 32), (64, 64)] {
            for (fx, fy, name) in [
                (0usize, 0usize, "int"),
                (2, 0, "h"),
                (0, 2, "v"),
                (1, 3, "hv"),
            ] {
                for mode in [0u8, 1] {
                    let iters = 400_000 / (w * h) + 100;
                    let mut best = f64::MAX;
                    for _ in 0..5 {
                        let t = std::time::Instant::now();
                        for i in 0..iters {
                            let mut o = Out {
                                pred: &mut pred,
                                dst: &mut dst.data,
                                dst_off: 64 * 256 + 64,
                                dst_stride: 256,
                                other: &[],
                            };
                            mc_luma(
                                &plane,
                                &mut sc,
                                50 + (i & 7) as i32,
                                60,
                                fx,
                                fy,
                                w,
                                h,
                                8,
                                mode,
                                &mut o,
                            );
                        }
                        best = best.min(t.elapsed().as_secs_f64() / iters as f64);
                    }
                    eprintln!(
                        "luma {w}x{h} {name} mode{mode}: {:.1} ns ({:.2} ns/px)",
                        best * 1e9,
                        best * 1e9 / (w * h) as f64
                    );
                }
            }
        }
        for (w, h) in [(2usize, 2usize), (4, 4), (8, 8), (16, 16), (32, 32)] {
            for (fx, fy, name) in [(3usize, 0usize, "h"), (0, 5, "v"), (3, 5, "hv")] {
                let iters = 400_000 / (w * h) + 100;
                let mut best = f64::MAX;
                for _ in 0..5 {
                    let t = std::time::Instant::now();
                    for i in 0..iters {
                        let mut o = Out {
                            pred: &mut pred,
                            dst: &mut dst.data,
                            dst_off: 64 * 256 + 64,
                            dst_stride: 256,
                            other: &[],
                        };
                        mc_chroma(
                            &plane,
                            &mut sc,
                            50 + (i & 7) as i32,
                            60,
                            fx,
                            fy,
                            w,
                            h,
                            8,
                            1,
                            &mut o,
                        );
                    }
                    best = best.min(t.elapsed().as_secs_f64() / iters as f64);
                }
                eprintln!(
                    "chroma {w}x{h} {name} uni: {:.1} ns ({:.2} ns/px)",
                    best * 1e9,
                    best * 1e9 / (w * h) as f64
                );
            }
        }
    }
}

#[cfg(test)]
mod bench_big {
    use super::*;

    #[test]
    #[ignore]
    fn bench_mc_big() {
        let (pw, ph) = (2560usize, 1600usize);
        let mut plane = Plane::<u8>::new(pw, ph, 0);
        let mut seed = 5u32;
        for v in plane.data.iter_mut() {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            *v = (seed >> 16) as u8;
        }
        let mut dst = Plane::<u8>::new(pw, ph, 0);
        let mut sc = McScratch::<u8>::new();
        let mut pred = vec![0i16; PRED_STRIDE * 64];
        for (w, h) in [(8usize, 8usize), (16, 16), (32, 32)] {
            for hot in [true, false] {
                let iters = 200_000;
                let mut best = f64::MAX;
                for _ in 0..3 {
                    let t = std::time::Instant::now();
                    let mut s = 1u32;
                    for i in 0..iters {
                        s = s.wrapping_mul(1103515245).wrapping_add(12345);
                        let (x, y) = if hot {
                            (100 + (i & 7), 100)
                        } else {
                            (
                                ((s >> 8) % (pw as u32 - 80)) as i32 + 8,
                                ((s >> 4) % (ph as u32 - 80)) as i32 + 8,
                            )
                        };
                        let mut o = Out {
                            pred: &mut pred,
                            dst: &mut dst.data,
                            dst_off: 64 * pw + 64,
                            dst_stride: pw,
                            other: &[],
                        };
                        mc_luma(&plane, &mut sc, x, y, 1, 3, w, h, 8, 0, &mut o);
                    }
                    best = best.min(t.elapsed().as_secs_f64() / iters as f64);
                }
                eprintln!(
                    "luma {w}x{h} hv {}: {:.1} ns",
                    if hot { "hot" } else { "scattered" },
                    best * 1e9
                );
            }
        }
    }
}
