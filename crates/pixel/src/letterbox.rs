//! Letterbox one decoded 4:2:0 picture into the 1920x1080 RGBA
//! canvas, reproducing the reference chain `scale=1920:1080:
//! force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,
//! format=rgba` bit for bit.
//!
//! Every rule below was recovered by black-box observation of the reference
//! build (crafted impulses, ramps and constant fields through its public filter
//! API).
//!
//! * Geometry: the fitted size is the display size scaled to fit, rounded to
//!   nearest; the padding offset is `(canvas - fitted) / 2` rounded down (odd
//!   offsets included); padding is opaque black.
//! * Same-size 8-bit pictures take a direct converter: luma at 15 fractional
//!   bits, each chroma product floored to 14 bits, one final rounding, chroma
//!   shared by each 2x2 block (`Direct`).
//! * Everything else (any resampling, and every 10-bit picture) takes the
//!   separable path: a horizontal pass to 15-bit intermediates (14-bit taps,
//!   `>> (bit_depth - 1)`, clamped above at 32767), a vertical pass with
//!   12-bit taps rounded to 8 bits, chroma resampled as its own plane to half
//!   the fitted width and the full fitted height, then luma-domain table
//!   offsets and one grey ramp per range (`Conversion`). Taps come from
//!   `Filter::new`.
//! * An odd fitted width switches to the `Wide` stage (chroma at full
//!   width, 13-bit coefficients, wrapping 32-bit sum).
//! * The picture is first widened by the part of a left crop the reference
//!   decoder keeps for alignment (`reference_crop`).
//!
//! Tap weights use the fixed-point bicubic stages of the selected receiver
//! profile. The tap and complete picture comparisons are separate checks.
use crate::view::{Colour, PictureView, Plane};

pub const CANVAS_WIDTH: usize = 1920;
pub const CANVAS_HEIGHT: usize = 1080;
pub const CANVAS_BYTES: usize = CANVAS_WIDTH * CANVAS_HEIGHT * 4;
const BLACK: [u8; 4] = [0, 0, 0, 255];

/// Fitted picture size and its offset inside the canvas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fit {
    pub width: usize,
    pub height: usize,
    pub x: usize,
    pub y: usize,
}

pub fn fit(width: usize, height: usize) -> Fit {
    let (ow, oh) = (CANVAS_WIDTH as u64, CANVAS_HEIGHT as u64);
    let (iw, ih) = (width.max(1) as u64, height.max(1) as u64);
    let rescale = |a: u64, b: u64, c: u64| (a * b + c / 2) / c;
    let w = rescale(oh, iw, ih).clamp(1, ow) as usize;
    let h = rescale(ow, ih, iw).clamp(1, oh) as usize;
    Fit {
        width: w,
        height: h,
        x: (CANVAS_WIDTH - w) / 2,
        y: (CANVAS_HEIGHT - h) / 2,
    }
}

/// One resampling pass: `len` taps per output sample, all inside the source
/// (`start[i] + len <= source length`), summing to the pass's unit.
#[derive(Clone, Debug, Default)]
struct Filter {
    len: usize,
    start: Vec<u32>,
    coef: Vec<i16>,
}

/// Keys cubic with C = 3/5, evaluated in the receiver's fixed-point domain.
/// Rounding a single exact cubic at the end puts some taps on the wrong
/// side of a tie. Quantise the distance, powers and C before blending.
fn cubic_weight(distance: i128) -> i128 {
    const Q30: i128 = 1 << 30;
    const Q24: i128 = 1 << 24;
    const SHARPNESS: i128 = 3 * Q24 / 5;
    if distance >= 2 * Q30 {
        return 0;
    }
    let square = (distance * distance) >> 30;
    let cube = (square * distance) >> 30;
    let weight = if distance < Q30 {
        let smooth = Q30 - 3 * square + 2 * cube;
        let edge = square - cube;
        Q24 * smooth + SHARPNESS * edge
    } else {
        SHARPNESS * (4 * Q30 - 8 * distance + 5 * square - cube)
    };
    6 * weight
}

impl Filter {
    /// The reference's taps for `src -> dst` samples summing to `one`.
    ///
    /// Positions: step `inc = round(src * 2^16 / dst)`, output `x` centred
    /// at `c = (x * inc + (inc - 2^16) / 2) / 2^16` source samples. Kernel:
    /// Keys cubic at `d / s`, `s = max(src / dst, 1)`, over a window of
    /// `size = 4` (upscale) or `ceil(4 s)` taps starting at
    /// `floor(c - (size - 1) / 2 + 1 / 2)`, or `ceil(c - (size - 1) / 2)` when
    /// that is negative. Leading taps are dropped, last output first, while
    /// their summed magnitude stays within 1/3000 (raw kernel units) and the
    /// start stays below the next output's. Taps outside the source fold onto
    /// the edge sample. Both passes use the same fixed-point cubic weights.
    /// The final 14- or 12-bit taps are rounded to nearest, carrying the
    /// signed residual to the next tap in the row.
    fn new(src: usize, dst: usize, one: i32) -> Self {
        if src == dst {
            return Self {
                len: 1,
                start: (0..src as u32).collect(),
                coef: vec![one as i16; src],
            };
        }
        let (src_i, dst_i) = (src as i64, dst as i64);
        let inc = ((src_i << 16) + dst_i / 2) / dst_i;
        let down = inc > 65536;
        let size = if down { (4 * src).div_ceil(dst) } else { 4 };
        let half = ((size as i64) - 1) << 16;
        let headroom = (src / dst).checked_ilog2().unwrap_or(0).min(8);
        let scale = 1_i128 << headroom;
        let unit = (1_i128 << 54) / scale;
        let mut windows: Vec<(i64, Vec<i128>)> = (0..dst_i)
            .map(|x| {
                let c2 = 2 * x * inc + inc - 65536;
                let mut s = (c2 - half + (1 << 16)).div_euclid(1 << 17);
                if s < 0 {
                    s = -((half - c2).div_euclid(1 << 17));
                }
                let w = (0..size as i64)
                    .map(|j| {
                        let mut distance = i128::from(((s + j) << 17) - c2).abs() << 13;
                        if down {
                            distance = distance * i128::from(dst_i) / i128::from(src_i);
                        }
                        cubic_weight(distance) / scale
                    })
                    .collect();
                (s, w)
            })
            .collect();
        for i in (0..windows.len()).rev() {
            let next = windows.get(i + 1).map(|w| w.0);
            let (s, w) = &mut windows[i];
            let mut cut = 0_i128;
            let mut drop = 0;
            while drop + 1 < w.len() {
                cut += w[drop].abs();
                // Weights carry the factor of six from `cubic_weight`.
                if cut * 500 > unit || next.is_some_and(|n| *s >= n) {
                    break;
                }
                drop += 1;
                *s += 1;
            }
            w.drain(..drop);
        }
        let mut folded: Vec<(usize, Vec<i32>)> = Vec::with_capacity(dst);
        let mut len = 1;
        for (s, w) in &windows {
            let lo = (*s).max(0) as usize;
            let hi = ((*s + w.len() as i64) as usize).min(src);
            let mut f = vec![0_i128; hi - lo];
            for (j, &v) in w.iter().enumerate() {
                let at = (*s + j as i64).clamp(0, src_i - 1) as usize;
                f[at - lo] += v;
            }
            let total: i128 = f.iter().sum();
            let one = i128::from(one);
            let step = ((total + one / 2) / one).max(1);
            let mut carry = 0_i128;
            let taps = f
                .iter()
                .map(|&v| {
                    let acc = v + carry;
                    let r = acc.signum() * ((acc.abs() + step / 2) / step);
                    carry = acc - r * step;
                    r as i32
                })
                .collect::<Vec<_>>();
            len = len.max(taps.len());
            folded.push((lo, taps));
        }
        let mut start = Vec::with_capacity(dst);
        let mut coef = vec![0_i16; dst * len];
        for (i, (lo, taps)) in folded.iter().enumerate() {
            let at = (*lo).min(src - len);
            start.push(at as u32);
            let row = &mut coef[i * len..][..len];
            for (j, &t) in taps.iter().enumerate() {
                row[lo - at + j] = t as i16;
            }
        }
        debug_assert!(start.windows(2).all(|p| p[0] <= p[1]));
        Self { len, start, coef }
    }
}

trait Sample: Copy {
    fn get(self) -> i32;
}
impl Sample for u8 {
    #[inline(always)]
    fn get(self) -> i32 {
        i32::from(self)
    }
}
impl Sample for u16 {
    #[inline(always)]
    fn get(self) -> i32 {
        i32::from(self)
    }
}

/// Horizontal pass of one source row into 15-bit intermediates.
fn hscale<T: Sample>(src: &[T], f: &Filter, shift: u32, out: &mut [i16]) {
    match f.len {
        1 => {
            let up = 14 - shift as i32;
            for (o, &s) in out.iter_mut().zip(src) {
                *o = ((s.get() << up.max(0)) >> (-up).max(0)).min(32767) as i16;
            }
        }
        4 => hscale_fixed::<T, 4>(src, f, shift, out),
        8 => hscale_fixed::<T, 8>(src, f, shift, out),
        _ => {
            for ((o, &s), c) in out.iter_mut().zip(&f.start).zip(f.coef.chunks_exact(f.len)) {
                let taps = &src[s as usize..][..f.len];
                let acc: i32 = c
                    .iter()
                    .zip(taps)
                    .map(|(&c, &t)| i32::from(c) * t.get())
                    .sum();
                *o = (acc >> shift).min(32767) as i16;
            }
        }
    }
}

#[inline(always)]
fn hscale_fixed<T: Sample, const L: usize>(src: &[T], f: &Filter, shift: u32, out: &mut [i16]) {
    for ((o, &s), c) in out.iter_mut().zip(&f.start).zip(f.coef.chunks_exact(L)) {
        let taps: &[T; L] = src[s as usize..][..L]
            .try_into()
            .expect("filter inside source");
        let mut acc = 0_i32;
        for j in 0..L {
            acc += i32::from(c[j]) * taps[j].get();
        }
        *o = (acc >> shift).min(32767) as i16;
    }
}

/// Vertical pass: 12-bit taps over `len` ring rows into `acc` (with the
/// final rounding bias added).
fn vscale(ring: &[i16], width: usize, cap: usize, first: usize, coef: &[i16], acc: &mut [i32]) {
    let row = |k: usize| &ring[((first + k) % cap) * width..][..width];
    match coef.len() {
        1 => vfixed::<1>(std::array::from_fn(row), coef, acc),
        2 => vfixed::<2>(std::array::from_fn(row), coef, acc),
        3 => vfixed::<3>(std::array::from_fn(row), coef, acc),
        4 => vfixed::<4>(std::array::from_fn(row), coef, acc),
        5 => vfixed::<5>(std::array::from_fn(row), coef, acc),
        6 => vfixed::<6>(std::array::from_fn(row), coef, acc),
        _ => {
            // Long (strong downscale) filters: four rows per sweep over `acc`.
            for (i, c) in coef.chunks(4).enumerate() {
                let rows = std::array::from_fn(|k| row(4 * i + k.min(c.len() - 1)));
                if i == 0 {
                    vfixed_into::<false>(c, rows, acc);
                } else {
                    vfixed_into::<true>(c, rows, acc);
                }
            }
        }
    }
}

#[inline(always)]
fn vfixed<const L: usize>(rows: [&[i16]; L], coef: &[i16], acc: &mut [i32]) {
    let c: [i32; L] = std::array::from_fn(|k| i32::from(coef[k]));
    let n = acc.len();
    let rows: [&[i16]; L] = rows.map(|r| &r[..n]);
    for (x, a) in acc.iter_mut().enumerate() {
        let mut sum = 1 << 18;
        for k in 0..L {
            sum += c[k] * i32::from(rows[k][x]);
        }
        *a = sum;
    }
}

/// Up to four taps of a long filter, added to (or starting) `acc`.
fn vfixed_into<const ADD: bool>(coef: &[i16], rows: [&[i16]; 4], acc: &mut [i32]) {
    let n = acc.len();
    let c: [i32; 4] = std::array::from_fn(|k| coef.get(k).map_or(0, |&c| i32::from(c)));
    let rows = rows.map(|r| &r[..n]);
    for (x, a) in acc.iter_mut().enumerate() {
        let sum = c[0] * i32::from(rows[0][x])
            + c[1] * i32::from(rows[1][x])
            + c[2] * i32::from(rows[2][x])
            + c[3] * i32::from(rows[3][x]);
        *a = if ADD { *a + sum } else { sum + (1 << 18) };
    }
}

/// A plane resampled row by row through a ring of horizontally scaled rows.
#[derive(Debug, Default)]
struct Pass {
    horizontal: Filter,
    vertical: Filter,
    width: usize,
    ring: Vec<i16>,
    cap: usize,
    next: usize,
    acc: Vec<i32>,
}

impl Pass {
    fn new(src: (usize, usize), dst: (usize, usize)) -> Self {
        let vertical = Filter::new(src.1, dst.1, 4096);
        let cap = vertical.len;
        Self {
            horizontal: Filter::new(src.0, dst.0, 16384),
            vertical,
            width: dst.0,
            ring: vec![0; cap * dst.0],
            cap,
            next: 0,
            acc: vec![0; dst.0],
        }
    }

    /// Output row `y` (before the final `>> 19`) into `self.acc`.
    fn row(&mut self, plane: &Plane<'_>, src_width: usize, shift: u32, y: usize) {
        let first = self.vertical.start[y] as usize;
        let need = first + self.vertical.len;
        if self.next < first {
            self.next = first;
        }
        while self.next < need {
            let out = &mut self.ring[(self.next % self.cap) * self.width..][..self.width];
            match *plane {
                Plane::U8 {
                    data,
                    stride,
                    offset,
                } => hscale(
                    &data[offset + self.next * stride..][..src_width],
                    &self.horizontal,
                    shift,
                    out,
                ),
                Plane::U16 {
                    data,
                    stride,
                    offset,
                } => hscale(
                    &data[offset + self.next * stride..][..src_width],
                    &self.horizontal,
                    shift,
                    out,
                ),
            }
            self.next += 1;
        }
        let coef = &self.vertical.coef[y * self.vertical.len..][..self.vertical.len];
        vscale(&self.ring, self.width, self.cap, first, coef, &mut self.acc);
    }
}

/// Per-geometry state of the separable path.
#[derive(Debug, Default)]
struct Plan {
    luma: Pass,
    chroma: [Pass; 2],
    terms: Terms,
    /// Odd fitted width: chroma at full width and the [`Wide`] output stage.
    wide: bool,
}

/// Scaled-path conversion constants: chroma offsets in luma steps,
/// `floor(c * inc / 2^16) - floor(128 * inc / 2^16)` for chroma code `c`, and
/// the grey ramp `clamp(floor((i * cy + oy) / 2^16), 0, 255)` applied to luma
/// plus offset. The 16.16 increments are fitted to exhaustive constant-field
/// observations; any value in each fitted range
/// yields the same tables, the middle one is used.
#[derive(Clone, Copy)]
struct Conversion {
    /// rv, gu, gv, bu.
    inc: [i32; 4],
    full: bool,
}

impl Conversion {
    fn new(colour: Colour) -> Self {
        // Matrices the reference refuses (8, 10, 11+) fall back to BT.601.
        let (tv, pc) = match colour.matrix {
            1 => (
                [100902, -12002, -29994, 118894],
                [103205, -12280, -30680, 121608],
            ),
            4 => (
                [89702, -21264, -45612, 114052],
                [91725, -21748, -46650, 116651],
            ),
            7 => (
                [100981, -14519, -30540, 116998],
                [103286, -14850, -31236, 119668],
            ),
            9 => (
                [94483, -10545, -36608, 120546],
                [96640, -10783, -37431, 123299],
            ),
            _ => (
                [89831, -22048, -45755, 113537],
                [91881, -22552, -46793, 116128],
            ),
        };
        Self {
            inc: if colour.full_range { pc } else { tv },
            full: colour.full_range,
        }
    }

    #[inline(always)]
    fn offset(inc: i32, c: i32) -> i32 {
        ((c * inc) >> 16) - ((128 * inc) >> 16)
    }
}

const GREY_CY: i32 = 76309;
const GREY_OY: i32 = -1_304_880;

/// Chroma terms of one output row, one value per pixel (each chroma sample
/// written to its horizontal pair) so the packing loops vectorise.
#[derive(Debug, Default)]
struct Terms {
    c: [Vec<i16>; 3],
}

impl Terms {
    fn resize(&mut self, width: usize) {
        for c in &mut self.c {
            c.resize(width, 0);
        }
    }

    /// Writes `term(u, v)` for each chroma sample to its pixel pair.
    #[inline(always)]
    fn spread<S: Copy>(&mut self, u: &[S], v: &[S], term: impl Fn(S, S) -> [i16; 3]) {
        let [r, g, b] = &mut self.c;
        let width = r.len();
        let pairs = r
            .chunks_exact_mut(2)
            .zip(g.chunks_exact_mut(2))
            .zip(b.chunks_exact_mut(2));
        for (((r, g), b), (&u, &v)) in pairs.zip(u.iter().zip(v)) {
            let [tr, tg, tb] = term(u, v);
            r[0] = tr;
            r[1] = tr;
            g[0] = tg;
            g[1] = tg;
            b[0] = tb;
            b[1] = tb;
        }
        if width % 2 == 1 {
            let i = width / 2;
            [r[width - 1], g[width - 1], b[width - 1]] = term(u[i], v[i]);
        }
    }
}

impl Plan {
    fn new(width: usize, height: usize, f: Fit) -> Self {
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        let wide = f.width % 2 == 1;
        let dst = (if wide { f.width } else { f.width / 2 }, f.height);
        Self {
            luma: Pass::new((width, height), (f.width, f.height)),
            chroma: [Pass::new((cw, ch), dst), Pass::new((cw, ch), dst)],
            terms: Terms::default(),
            wide,
        }
    }

    fn run(&mut self, picture: &PictureView<'_>, f: Fit, rgba: &mut [u8]) {
        let shift = u32::from(picture.bit_depth.clamp(8, 16)) - 1;
        let k = Conversion::new(picture.colour);
        let cw = picture.width.div_ceil(2);
        self.luma.next = 0;
        for pass in &mut self.chroma {
            pass.next = 0;
        }
        if self.wide {
            let k = Wide::new(picture.colour);
            for y in 0..f.height {
                self.luma.row(&picture.planes[0], picture.width, shift, y);
                self.chroma[0].row(&picture.planes[1], cw, shift, y);
                self.chroma[1].row(&picture.planes[2], cw, shift, y);
                let out = &mut rgba[((f.y + y) * CANVAS_WIDTH + f.x) * 4..][..f.width * 4];
                k.row(
                    &self.luma.acc,
                    &self.chroma[0].acc,
                    &self.chroma[1].acc,
                    out,
                );
            }
            return;
        }
        let t = &mut self.terms;
        t.resize(f.width);
        for y in 0..f.height {
            self.luma.row(&picture.planes[0], picture.width, shift, y);
            self.chroma[0].row(&picture.planes[1], cw, shift, y);
            self.chroma[1].row(&picture.planes[2], cw, shift, y);
            let (u, v) = (&self.chroma[0].acc, &self.chroma[1].acc);
            t.spread(u, v, |u, v| {
                let (u, v) = ((u >> 19).clamp(0, 255), (v >> 19).clamp(0, 255));
                [
                    Conversion::offset(k.inc[0], v) as i16,
                    (Conversion::offset(k.inc[1], u) + Conversion::offset(k.inc[2], v)) as i16,
                    Conversion::offset(k.inc[3], u) as i16,
                ]
            });
            let out = &mut rgba[((f.y + y) * CANVAS_WIDTH + f.x) * 4..][..f.width * 4];
            if k.full {
                pack::<true>(&self.luma.acc, &t.c, out);
            } else {
                pack::<false>(&self.luma.acc, &t.c, out);
            }
        }
    }
}

/// The grey ramp on `i = luma + offset`: the identity (clamped) for full
/// range, `clamp(floor((i * 76309 - 1304880) / 2^16))` for limited range,
/// written as `i - 20 + floor((i * 10773 + 5840) / 2^16)` to stay in 16-bit
/// lanes.
#[inline(always)]
fn grey<const FULL: bool>(i: i16) -> u8 {
    if FULL {
        i.clamp(0, 255) as u8
    } else {
        let corr = ((i32::from(i) * (GREY_CY - 65536) + (GREY_OY + 20 * 65536)) >> 16) as i16;
        (i - 20 + corr).clamp(0, 255) as u8
    }
}

fn pack<const FULL: bool>(acc: &[i32], [r, g, b]: &[Vec<i16>; 3], out: &mut [u8]) {
    let rows = out.chunks_exact_mut(4).zip(acc).zip(r).zip(g).zip(b);
    for ((((px, &a), &r), &g), &b) in rows {
        let l = (a >> 19) as i16;
        px[0] = grey::<FULL>(l + r);
        px[1] = grey::<FULL>(l + g);
        px[2] = grey::<FULL>(l + b);
        px[3] = 255;
    }
}

/// The reference's output stage for an odd fitted width (observed black-box):
/// chroma is resampled to the full width, the
/// vertical sums keep 9 fractional bits, `v = (A + 2^9) >> 10` (A as before
/// the `>> 19`), and `out = clamp(wrap32(y' * cy + chroma terms + 2^21) >> 22)`
/// with `y' = v_y - 16 * 2^9` (limited) or `v_y`, `u' = v_u - 128 * 2^9`,
/// `v' = v_v - 128 * 2^9` and 13-bit coefficients. The sum is a wrapping
/// 32-bit integer: values of 512 levels and above come out black, as the
/// reference's do.
struct Wide {
    /// cy, crv, cbu, cgu, cgv.
    k: [i32; 5],
    offset: i32,
}

impl Wide {
    fn new(colour: Colour) -> Self {
        let (tv, pc) = match colour.matrix {
            1 => (
                [9539, 14686, 17305, 1747, 4366],
                [8192, 12901, 15201, 1534, 3835],
            ),
            4 => (
                [9539, 13056, 16600, 3095, 6639],
                [8192, 11469, 14582, 2719, 5831],
            ),
            7 => (
                [9539, 14697, 17029, 2113, 4445],
                [8192, 12911, 14959, 1856, 3904],
            ),
            9 => (
                [9539, 13752, 17545, 1535, 5328],
                [8192, 12080, 15412, 1348, 4680],
            ),
            _ => (
                [9539, 13075, 16525, 3209, 6660],
                [8192, 11485, 14516, 2819, 5850],
            ),
        };
        if colour.full_range {
            Self { k: pc, offset: 0 }
        } else {
            Self {
                k: tv,
                offset: 16 << 9,
            }
        }
    }

    fn row(&self, y: &[i32], u: &[i32], v: &[i32], out: &mut [u8]) {
        let [cy, crv, cbu, cgu, cgv] = self.k;
        // The pass sums carry the 2^18 rounding bias of the table path.
        let fine = |a: i32| (a - (1 << 18) + (1 << 9)) >> 10;
        let pixels = out.chunks_exact_mut(4).zip(y).zip(u.iter().zip(v));
        for ((px, &y), (&u, &v)) in pixels {
            let l = (fine(y) - self.offset)
                .wrapping_mul(cy)
                .wrapping_add(1 << 21);
            let (u, v) = (fine(u) - (128 << 9), fine(v) - (128 << 9));
            let clip = |t: i32| (l.wrapping_add(t) >> 22).clamp(0, 255) as u8;
            px[0] = clip(v.wrapping_mul(crv));
            px[1] = clip(
                u.wrapping_mul(cgu)
                    .wrapping_neg()
                    .wrapping_sub(v.wrapping_mul(cgv)),
            );
            px[2] = clip(u.wrapping_mul(cbu));
            px[3] = 255;
        }
    }
}

#[derive(Debug, Default)]
pub struct Scaler {
    key: Option<(usize, usize, u8)>,
    plan: Option<Plan>,
    rows: [Vec<u8>; 3],
    terms: Terms,
}

impl Scaler {
    /// Writes the complete canvas: black padding and the fitted picture.
    pub fn letterbox(&mut self, picture: &PictureView<'_>, rgba: &mut [u8]) {
        assert_eq!(rgba.len(), CANVAS_BYTES);
        let picture = &reference_crop(picture);
        let f = fit(picture.width, picture.height);
        pad(f, rgba);
        if picture.bit_depth <= 8 && (f.width, f.height) == (picture.width, picture.height) {
            self.direct(picture, f, rgba);
            return;
        }
        let key = (picture.width, picture.height, picture.bit_depth);
        if self.key != Some(key) || self.plan.is_none() {
            self.key = Some(key);
            self.plan = Some(Plan::new(picture.width, picture.height, f));
        }
        self.plan.as_mut().expect("plan").run(picture, f, rgba);
    }

    fn direct(&mut self, picture: &PictureView<'_>, f: Fit, rgba: &mut [u8]) {
        let k = Direct::new(picture.colour);
        let cw = f.width.div_ceil(2);
        let t = &mut self.terms;
        t.resize(f.width);
        let base = (1 << (SHIFT - 1)) - k.offset * k.cy;
        for y in 0..f.height {
            let [ly, lu, lv] = &mut self.rows;
            // Chroma terms change every second row only.
            if y % 2 == 0 {
                let cb = row_u8(&picture.planes[1], y / 2, cw, lu);
                let cr = row_u8(&picture.planes[2], y / 2, cw, lv);
                t.spread(cb, cr, |u, v| k.terms(u, v));
            }
            let luma = row_u8(&picture.planes[0], y, f.width, ly);
            let out = &mut rgba[((f.y + y) * CANVAS_WIDTH + f.x) * 4..][..f.width * 4];
            let [r, g, b] = &t.c;
            for ((((px, &l), &r), &g), &b) in out.chunks_exact_mut(4).zip(luma).zip(r).zip(g).zip(b)
            {
                // l = ((y - offset) * cy + 2^14) >> 14, then out = (l + term) >> 1.
                let l = ((i32::from(l) * k.cy + base) >> (SHIFT - 1)) as i16;
                px[0] = ((l + r) >> 1).clamp(0, 255) as u8;
                px[1] = ((l + g) >> 1).clamp(0, 255) as u8;
                px[2] = ((l + b) >> 1).clamp(0, 255) as u8;
                px[3] = 255;
            }
        }
    }
}

fn plane_layout(plane: &Plane<'_>) -> (usize, usize) {
    match *plane {
        Plane::U8 { stride, offset, .. } | Plane::U16 { stride, offset, .. } => (stride, offset),
    }
}

fn shifted(plane: Plane<'_>, by: usize) -> Plane<'_> {
    match plane {
        Plane::U8 {
            data,
            stride,
            offset,
        } => Plane::U8 {
            data,
            stride,
            offset: offset - by,
        },
        Plane::U16 {
            data,
            stride,
            offset,
        } => Plane::U16 {
            data,
            stride,
            offset: offset - by,
        },
    }
}

/// The picture the reference's filter graph actually receives. With its
/// default `apply_cropping`, the reference decoder applies a left crop only as
/// far as the plane pointers stay 32-byte aligned (`AVCodecContext.
/// apply_cropping` docs; the exact rule below is fitted to crafted streams):
/// with per-plane byte offsets `o_p` of the
/// cropped origin and `m = min ctz(o_p)`, a left crop `l` with `m < 5` becomes
/// `l & !((1 << (5 + ctz(l) - m)) - 1)`. The remaining columns of the left
/// border stay in the picture. Top, right and bottom crops are applied in
/// full.
///
/// The crop is read from the plane layout: `offset = top * stride + left` in
/// the whole decoded plane (see [`Plane`]). The reference's row pitch is taken
/// as `stride` rounded up to 32 bytes (observed for 176..1920-sample-wide
/// 8-bit and 1920-wide 10-bit pictures).
fn reference_crop<'a>(picture: &PictureView<'a>) -> PictureView<'a> {
    let (stride, offset) = plane_layout(&picture.planes[0]);
    let (cstride, coffset) = plane_layout(&picture.planes[1]);
    if stride == 0 || cstride == 0 {
        return *picture;
    }
    let (top, left) = (offset / stride, offset % stride);
    // Only a consistent 4:2:0 layout (whole planes, even crop) is adjusted.
    if left == 0
        || left % 2 == 1
        || top % 2 == 1
        || coffset != (top / 2) * cstride + left / 2
        || plane_layout(&picture.planes[2]) != (cstride, coffset)
    {
        return *picture;
    }
    let bytes = if picture.bit_depth > 8 { 2 } else { 1 };
    let pitch = (stride * bytes).next_multiple_of(32);
    let origins = [
        top * pitch + left * bytes,
        (top / 2) * (pitch / 2) + (left / 2) * bytes,
    ];
    let align = origins
        .iter()
        .map(|&o| if o == 0 { 64 } else { o.trailing_zeros() })
        .min()
        .unwrap_or(64);
    let kept = if align < 5 {
        let bits = 5 + left.trailing_zeros() - align;
        left & !((1_usize << bits) - 1)
    } else {
        left
    };
    let extra = left - kept;
    if extra == 0 {
        return *picture;
    }
    let [y, u, v] = picture.planes;
    PictureView {
        width: picture.width + extra,
        planes: [
            shifted(y, extra),
            shifted(u, extra / 2),
            shifted(v, extra / 2),
        ],
        ..*picture
    }
}

/// One row of an 8-bit plane, borrowed when stored as bytes.
fn row_u8<'a>(plane: &'a Plane<'_>, y: usize, width: usize, scratch: &'a mut Vec<u8>) -> &'a [u8] {
    match *plane {
        Plane::U8 {
            data,
            stride,
            offset,
        } => &data[offset + y * stride..][..width],
        Plane::U16 {
            data,
            stride,
            offset,
        } => {
            scratch.clear();
            scratch.extend(
                data[offset + y * stride..][..width]
                    .iter()
                    .map(|&s| s.min(255) as u8),
            );
            scratch
        }
    }
}

/// Opaque black outside the fitted rectangle.
fn pad(f: Fit, rgba: &mut [u8]) {
    let fill = |px: &mut [u8]| {
        for p in px.chunks_exact_mut(4) {
            p.copy_from_slice(&BLACK);
        }
    };
    let row = CANVAS_WIDTH * 4;
    fill(&mut rgba[..f.y * row]);
    fill(&mut rgba[(f.y + f.height) * row..]);
    if f.width < CANVAS_WIDTH {
        for line in rgba[f.y * row..(f.y + f.height) * row].chunks_exact_mut(row) {
            fill(&mut line[..f.x * 4]);
            fill(&mut line[(f.x + f.width) * 4..]);
        }
    }
}

const SHIFT: u32 = 15;

/// The same-size 8-bit converter: luma at 15 fractional bits, each chroma
/// product floored to half an output step (14 bits) before a single
/// round-to-nearest, chroma shared by each 2x2 block. Constants per H.273
/// MatrixCoefficients and range, recovered exactly from random full frames.
struct Direct {
    cy: i32,
    offset: i32,
    crv: i32,
    cbu: i32,
    cgu: i32,
    cgv: i32,
}

impl Direct {
    fn new(colour: Colour) -> Self {
        let full = colour.full_range;
        // [crv, cbu, cgu, cgv] for limited, then full range.
        let (limited, full_set): ([i32; 4], [i32; 4]) = match colour.matrix {
            1 => ([58745, 69219, 6988, 17462], [51603, 60804, 6138, 15339]),
            4 => ([52224, 66399, 12380, 26555], [45875, 58328, 10876, 23326]),
            7 => ([58789, 68115, 8451, 17779], [51642, 59834, 7424, 15616]),
            9 | 10 => ([55007, 70179, 6138, 21305], [48320, 61650, 5392, 18722]),
            _ => ([52299, 66097, 12835, 26639], [45941, 58060, 11277, 23401]),
        };
        let [crv, cbu, cgu, cgv] = if full { full_set } else { limited };
        Self {
            cy: if full { 32768 } else { 38156 },
            offset: if full { 0 } else { 16 },
            crv,
            cbu,
            cgu,
            cgv,
        }
    }

    /// Chroma terms in half output steps: with `l = ((y - offset) * cy +
    /// 2^14) >> 14`, `out = (l + term) >> 1` equals the single rounding of
    /// the 15-bit sum (every chroma product is a multiple of 2^14).
    #[inline(always)]
    fn terms(&self, u: u8, v: u8) -> [i16; 3] {
        let half = |product: i32| product >> (SHIFT - 1);
        let (u, v) = (i32::from(u) - 128, i32::from(v) - 128);
        [
            half(v * self.crv),
            half(-u * self.cgu) + half(-v * self.cgv),
            half(u * self.cbu),
        ]
        .map(|t| t as i16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_follows_the_decrease_rule_and_floor_offsets() {
        assert_eq!(
            fit(1920, 1080),
            Fit {
                width: 1920,
                height: 1080,
                x: 0,
                y: 0
            }
        );
        assert_eq!(
            fit(3840, 2160),
            Fit {
                width: 1920,
                height: 1080,
                x: 0,
                y: 0
            }
        );
        assert_eq!(
            fit(1280, 720),
            Fit {
                width: 1920,
                height: 1080,
                x: 0,
                y: 0
            }
        );
        assert_eq!(
            fit(1440, 1080),
            Fit {
                width: 1440,
                height: 1080,
                x: 240,
                y: 0
            }
        );
        assert_eq!(
            fit(1920, 800),
            Fit {
                width: 1920,
                height: 800,
                x: 0,
                y: 140
            }
        );
        // Observed: odd offsets are kept (pad runs on RGBA).
        assert_eq!(
            fit(720, 576),
            Fit {
                width: 1350,
                height: 1080,
                x: 285,
                y: 0
            }
        );
        assert_eq!(
            fit(1920, 1088),
            Fit {
                width: 1906,
                height: 1080,
                x: 7,
                y: 0
            }
        );
        assert_eq!(
            fit(412, 236),
            Fit {
                width: 1885,
                height: 1080,
                x: 17,
                y: 0
            }
        );
        assert_eq!(
            fit(300, 168),
            Fit {
                width: 1920,
                height: 1075,
                x: 0,
                y: 2
            }
        );
    }

    fn row(f: &Filter, i: usize) -> (u32, Vec<i16>) {
        (f.start[i], f.coef[i * f.len..][..f.len].to_vec())
    }

    #[test]
    fn taps_match_reference_measurements() {
        // Measured with impulses through the reference.
        let f = Filter::new(176, 1320, 16384);
        assert_eq!(f.len, 4);
        assert_eq!(row(&f, 4), (0, vec![15218, 1254, -88, 0]));
        assert_eq!(row(&f, 8), (0, vec![6126, 11634, -1376, 0]));
        assert_eq!(row(&f, 660), (86, vec![-1050, 7961, 10839, -1366]));
        assert_eq!(row(&f, 1310), (172, vec![0, -1342, 14573, 3153]));
        // Near-ties the exact kernel would round the other way (2^-30 rounding).
        assert_eq!(row(&f, 37), (3, vec![-1229, 9423, 9419, -1229]));
        let f = Filter::new(88, 660, 16384);
        assert_eq!(row(&f, 112), (13, vec![-1229, 9426, 9415, -1228]));
        let f = Filter::new(1056, 135, 16384);
        let (s, c) = row(&f, 86);
        let nz: Vec<i16> = c.into_iter().filter(|&v| v != 0).collect();
        assert_eq!(
            (s, &nz[nz.len() - 6..]),
            (661, &[-180, -182, -151, -102, -49, -11][..])
        );
        let f = Filter::new(1280, 1920, 16384);
        assert_eq!(row(&f, 7), (3, vec![-1229, 9420, 9422, -1229]));
        // Leading-tap trim (vertical 12-bit taps).
        let f = Filter::new(360, 1080, 4096);
        assert_eq!(row(&f, 199), (65, vec![3, 4095, -2, 0]));
        assert_eq!(row(&f, 910), (302, vec![12, 4095, -11, 0]));
        let f = Filter::new(120, 1080, 4096);
        assert_eq!(row(&f, 418), (45, vec![-3, 4096, 3, 0]));
        // Downscale: 2x, the s=1.1 left edge, and 1056 -> 135 (32 taps).
        let f = Filter::new(3840, 1920, 16384);
        assert_eq!(f.len, 8);
        assert_eq!(row(&f, 0), (0, vec![8192, 7142, 1972, -692, -230, 0, 0, 0]));
        let f = Filter::new(264, 240, 4096);
        assert_eq!(row(&f, 0).1[..3], [3773, 411, -88]);
        assert_eq!(row(&f, 1).1[..4], [-92, 3553, 788, -153]);
        let f = Filter::new(1056, 135, 16384);
        let (s, c) = row(&f, 50);
        let nz: Vec<i16> = c.into_iter().filter(|&v| v != 0).collect();
        assert_eq!(s, 380);
        assert_eq!(&nz[..6], &[-22, -68, -120, -165, -186, -167]);
        assert_eq!(nz.iter().map(|&v| i32::from(v)).sum::<i32>(), 16384);
    }

    #[test]
    fn horizontal_near_ties_round_after_the_fixed_point_kernel() {
        for (src, dst, x, tap_at, expected) in [
            (1056, 135, 121, 960, [-184, -177]),
            (1920, 1906, 1268, 1276, [-1435, 13234]),
            (960, 1906, 1668, 840, [15904, -881]),
            (206, 1885, 1840, 201, [12283, -1447]),
        ] {
            let (start, taps) = row(&Filter::new(src, dst, 16384), x);
            let offset = tap_at - start as usize;
            assert_eq!(taps[offset..offset + 2], expected, "{src}->{dst}, x={x}");
        }
    }

    #[test]
    fn left_crop_follows_the_reference_alignment_rule() {
        // (coded width, bits, crop left, crop top) -> left crop the reference keeps,
        // observed on crafted and conformance streams.
        let cases = [
            (352, 8, 26, 60, 0), // CVFC1_Sony_C
            (416, 8, 2, 2, 0),   // CONFWIN_A_Sony_1
            (352, 8, 96, 0, 64), // h264_metadata crop_left=96
            (352, 8, 160, 0, 128),
            (352, 8, 64, 2, 0), // the top crop misaligns chroma
            (352, 8, 64, 4, 64),
            (1920, 10, 48, 0, 32),
            (1920, 10, 34, 0, 32),
            (1920, 10, 16, 0, 0),
        ];
        for (coded, bits, left, top, kept) in cases {
            let cs = coded / 2;
            let (w, h) = (coded - left - 8, 64);
            let luma = vec![0_u16; coded * (top + h)];
            let chroma = vec![0_u16; cs * (top + h) / 2];
            let plane = |data, stride, offset| Plane::U16 {
                data,
                stride,
                offset,
            };
            let view = PictureView {
                width: w,
                height: h,
                bit_depth: bits,
                planes: [
                    plane(&luma[..], coded, top * coded + left),
                    plane(&chroma[..], cs, top / 2 * cs + left / 2),
                    plane(&chroma[..], cs, top / 2 * cs + left / 2),
                ],
                pts: None,
                sar: (1, 1),
                colour: Colour {
                    matrix: 1,
                    full_range: false,
                },
            };
            let got = reference_crop(&view);
            assert_eq!(got.width, w + left - kept, "{coded} {bits} {left} {top}");
        }
    }

    #[test]
    fn every_filter_is_normalised_and_inside_the_source() {
        for (src, dst) in [
            (176, 1320),
            (88, 660),
            (720, 1080),
            (2160, 1080),
            (8440, 1080),
            (1056, 135),
            (3, 1080),
            (1, 960),
        ] {
            for one in [16384, 4096] {
                let f = Filter::new(src, dst, one);
                for i in 0..dst {
                    let (s, c) = row(&f, i);
                    assert!(s as usize + f.len <= src);
                    assert_eq!(
                        c.iter().map(|&v| i32::from(v)).sum::<i32>(),
                        one,
                        "{src}->{dst} row {i}"
                    );
                }
            }
        }
    }
}
