//! `\blur` and `\be`: the cascade Gaussian blur and the 3×3 box blur.
//!
//! The Gaussian blur works in 16-bit fixed point (0 to 0x4000): it halves the
//! bitmap `level` times with the kernel [1, 5, 10, 10, 5, 1], applies a
//! symmetric kernel of radius 4 to 8, then doubles back with [5, 10, 1] and
//! [1, 10, 5]: vertically then horizontally on the way down, in the opposite
//! order on the way up. Every stage is a convolution of the zero-extended
//! image, so the bitmap grows by the stages' support.

mod coeff;

use triergon_core::{Budget, BudgetVec, Result, WorkBudget};

use crate::bitmap::Bitmap;
use crate::limits::{Limits, charge};
use coeff::Method;

/// libass's `\blur` and `\be`, with scratch storage reused across bitmaps.
#[derive(Debug)]
pub struct Blur {
    planes: [BudgetVec<i16>; 2],
    row: BudgetVec<i16>,
    halves: BudgetVec<i16>,
    zeros: BudgetVec<i16>,
    acc: BudgetVec<i32>,
    lines: BudgetVec<u16>,
    limits: Limits,
}

/// Zeros on either side of a row for the horizontal filters: the widest reach,
/// 2 × 8 taps.
const PAD: usize = 16;

impl Blur {
    /// A blur whose scratch is charged to `budget`, each operation bounded by
    /// `limits`.
    pub fn new(budget: &Budget, limits: Limits) -> Self {
        Self {
            limits,
            planes: [BudgetVec::new(budget), BudgetVec::new(budget)],
            row: BudgetVec::new(budget),
            halves: BudgetVec::new(budget),
            zeros: BudgetVec::new(budget),
            acc: BudgetVec::new(budget),
            lines: BudgetVec::new(budget),
        }
    }

    /// Bytes of scratch storage currently charged.
    pub fn retained_bytes(&self) -> usize {
        (self.planes[0].capacity()
            + self.planes[1].capacity()
            + self.row.capacity()
            + self.halves.capacity()
            + self.zeros.capacity())
            * 2
            + self.acc.capacity() * 4
            + self.lines.capacity() * 2
    }

    /// Blurs `bitmap` as libass does (`ass_synth_blur`): with the Gaussian of
    /// `variance` (x and y, in pixels²) when either exceeds 0.001, which
    /// replaces the bitmap by a larger one covering the blur's reach, then with
    /// `be` passes of the 3×3 box blur in place.
    ///
    /// # Errors
    ///
    /// `Policy` when the limits run out, and the budget's errors; the bitmap is
    /// then unchanged or blurred by the Gaussian only.
    pub fn apply(&mut self, bitmap: &mut Bitmap, be: u32, variance: [f64; 2]) -> Result<()> {
        let mut work = self.limits.work();
        let [r2x, r2y] = variance;
        if r2x > 0.001 || r2y > 0.001 {
            self.gaussian(bitmap, r2x, r2y, &mut work)?;
        }
        self.box_blur(bitmap, be, &mut work)
    }

    /// Blurs with the kernel [1, 2, 1] × [1, 2, 1] / 16 `times` times in place.
    /// Repeated passes run on coverage scaled to 0..64, as libass (and
    /// VSFilter) do; the last pass runs at full range. Each pass covers the
    /// bitmap's width, as libass's C engine does, so the row padding stays zero.
    fn box_blur(&mut self, bitmap: &mut Bitmap, times: u32, work: &mut WorkBudget) -> Result<()> {
        if times == 0 || bitmap.width() == 0 || bitmap.height() == 0 {
            return Ok(());
        }
        // One unit per row of each pass; a row is bounded by the bitmap's charge.
        charge(
            work,
            u64::from(times).saturating_mul(bitmap.height() as u64),
        )?;
        self.lines.clear();
        self.lines.try_resize(3 * bitmap.stride(), 0)?;
        if times > 1 {
            for row in bitmap.rows_mut() {
                for v in row {
                    *v = ((*v >> 1) + 1) >> 1;
                }
            }
            for _ in 1..times {
                box_pass(bitmap, self.lines.as_mut_slice());
            }
            for row in bitmap.rows_mut() {
                for v in row {
                    *v = (*v << 2).wrapping_sub(u8::from(*v > 32));
                }
            }
        }
        box_pass(bitmap, self.lines.as_mut_slice());
        Ok(())
    }

    /// The cascade Gaussian blur of variances (`r2x`, `r2y`) in pixels². The
    /// bitmap is replaced by a larger one covering the blur's reach.
    fn gaussian(
        &mut self,
        bitmap: &mut Bitmap,
        r2x: f64,
        r2y: f64,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let mx = Method::new(r2x);
        #[allow(clippy::float_cmp)] // libass reuses the method for equal variances
        let my = if r2y == r2x { mx } else { Method::new(r2y) };
        let (w, h) = (bitmap.width(), bitmap.height());
        if w == 0 || h == 0 {
            return Ok(());
        }
        let end = |size: usize, m: &Method| {
            let offset = (2 * m.radius + 9)
                .checked_shl(m.level)
                .filter(|o| *o < 1 << 31)?
                - 5;
            Some(((size + offset) & !((1usize << m.level) - 1)) - 4)
        };
        // libass leaves the bitmap unblurred beyond 2²⁹ intermediate samples.
        let Some((end_w, end_h)) = end(w, &mx).zip(end(h, &my)) else {
            return Ok(());
        };
        if end_w.next_multiple_of(8).saturating_mul(end_h) > i32::MAX as usize / 4 {
            return Ok(());
        }
        let stages = u64::from(mx.level + my.level) * 2 + 2;
        // One unit per output row of each stage, at most `end_h`.
        charge(work, stages.saturating_mul(end_h as u64))?;
        for plane in &mut self.planes {
            plane.clear();
            plane.try_resize(end_w * end_h, 0)?;
        }
        self.row.clear();
        self.row.try_resize(end_w + 2 * PAD, 0)?;
        self.halves.clear();
        self.halves.try_resize(end_w + 2 * PAD, 0)?;
        self.zeros.clear();
        self.zeros.try_resize(end_w, 0)?;
        self.acc.clear();
        self.acc.try_resize(end_w, 0)?;
        let budget = self.row.budget().clone();

        let [a, b] = &mut self.planes;
        let mut planes = Planes {
            src: a.as_mut_slice(),
            dst: b.as_mut_slice(),
            size: (w, h),
        };
        unpack(bitmap, planes.src);
        let (row, acc) = (self.row.as_mut_slice(), self.acc.as_mut_slice());
        let zeros = self.zeros.as_slice();
        let halves = self.halves.as_mut_slice();
        for _ in 0..my.level {
            planes.step(|s, d, z| shrink_vert(s, d, z, zeros));
        }
        for _ in 0..mx.level {
            planes.step(|s, d, z| shrink_horz(s, d, z, row, halves));
        }
        planes.step(|s, d, z| blur_horz(s, d, z, &mx, row, acc));
        planes.step(|s, d, z| blur_vert(s, d, z, &my, acc, zeros));
        for _ in 0..mx.level {
            planes.step(|s, d, z| expand_horz(s, d, z, row));
        }
        for _ in 0..my.level {
            planes.step(|s, d, z| expand_vert(s, d, z, zeros));
        }
        debug_assert_eq!(planes.size, (end_w, end_h));

        let mut out = Bitmap::new(&budget, end_w, end_h, &self.limits)?;
        out.left = bitmap
            .left
            .wrapping_sub((((mx.radius + 4) << mx.level) - 4) as i32);
        out.top = bitmap
            .top
            .wrapping_sub((((my.radius + 4) << my.level) - 4) as i32);
        pack(&mut out, planes.src);
        *bitmap = out;
        Ok(())
    }
}

/// The two planes the stages alternate between, and the current image size.
struct Planes<'a> {
    src: &'a mut [i16],
    dst: &'a mut [i16],
    size: (usize, usize),
}

impl Planes<'_> {
    /// Runs one stage from `src` into `dst` and makes its output the source.
    fn step(&mut self, stage: impl FnOnce(&[i16], &mut [i16], (usize, usize)) -> (usize, usize)) {
        self.size = stage(self.src, self.dst, self.size);
        std::mem::swap(&mut self.src, &mut self.dst);
    }
}

/// One box blur pass in place over the bitmap's width, zero beyond its edges, as
/// libass's C engine runs it; `lines` holds three rows of horizontal sums.
fn box_pass(bitmap: &mut Bitmap, lines: &mut [u16]) {
    let (width, height, stride) = (bitmap.width(), bitmap.height(), bitmap.stride());
    let data = bitmap.data_mut();
    // [1, 2, 1] along the row, zero beyond both ends.
    let horizontal = |row: &[u8], out: &mut [u16]| {
        let row = &row[..width];
        let out = &mut out[..width];
        if width == 1 {
            out[0] = 2 * u16::from(row[0]);
            return;
        }
        out[0] = 2 * u16::from(row[0]) + u16::from(row[1]);
        for (o, w) in out[1..width - 1].iter_mut().zip(row.windows(3)) {
            *o = u16::from(w[0]) + 2 * u16::from(w[1]) + u16::from(w[2]);
        }
        out[width - 1] = u16::from(row[width - 2]) + 2 * u16::from(row[width - 1]);
    };
    let (mut above, rest) = lines.split_at_mut(width);
    let (mut centre, mut below) = rest.split_at_mut(width);
    above.fill(0);
    horizontal(&data[..width], centre);
    for y in 0..height {
        if y + 1 < height {
            horizontal(&data[(y + 1) * stride..][..width], below);
        } else {
            below.fill(0);
        }
        let out = &mut data[y * stride..][..width];
        for (((o, &a), &c), &b) in out.iter_mut().zip(&*above).zip(&*centre).zip(&*below) {
            *o = ((a + 2 * c + b) >> 4) as u8;
        }
        // The centre row becomes the row above, the row below the centre.
        std::mem::swap(&mut above, &mut centre);
        std::mem::swap(&mut centre, &mut below);
    }
}

/// 8-bit coverage to 0..0x4000: `(0x4000 · v + 127) / 255`.
fn unpack(bitmap: &Bitmap, plane: &mut [i16]) {
    let w = bitmap.width();
    for (row, out) in bitmap.rows().zip(plane.chunks_exact_mut(w)) {
        for (o, &v) in out.iter_mut().zip(row) {
            let v = u16::from(v);
            *o = ((((v << 7) | (v >> 1)) + 1) >> 1) as i16;
        }
    }
}

/// 0..0x4000 back to 8-bit coverage with a 2×2 ordered dither.
fn pack(bitmap: &mut Bitmap, plane: &[i16]) {
    const DITHER: [[i32; 2]; 2] = [[8, 40], [56, 24]];
    let w = bitmap.width();
    for (y, (out, row)) in bitmap.rows_mut().zip(plane.chunks_exact(w)).enumerate() {
        let dither = DITHER[y & 1];
        for (x, (o, &v)) in out.iter_mut().zip(row).enumerate() {
            let v = i32::from(v);
            *o = (((v - (v >> 8) + dither[x & 1]) as u16) >> 6) as u8;
        }
    }
}

/// Copies `src` into the middle of `row` with `PAD` zeros on either side.
fn padded<'r>(row: &'r mut [i16], src: &[i16]) -> &'r [i16] {
    let row = &mut row[..src.len() + 2 * PAD];
    row[..PAD].fill(0);
    row[PAD..PAD + src.len()].copy_from_slice(src);
    row[PAD + src.len()..].fill(0);
    row
}

/// Row `index − back` of a `w` × `h` plane, or `zeros` outside it.
#[inline(always)]
fn line<'a>(
    src: &'a [i16],
    (w, h): (usize, usize),
    index: usize,
    back: usize,
    zeros: &'a [i16],
) -> &'a [i16] {
    match index.checked_sub(back) {
        Some(y) if y < h => &src[y * w..][..w],
        _ => &zeros[..w],
    }
}

/// `(p1p + 5 p1n + 10 z0p + 10 z0n + 5 n1p + n1n + 16) >> 5`, in libass's
/// shift cascade.
#[inline(always)]
fn shrink_func(p: [i16; 6]) -> i16 {
    let [p1p, p1n, z0p, z0n, n1p, n1n] = p.map(i32::from);
    let r = (p1p + p1n + n1p + n1n) >> 1;
    let r = (r + z0p + z0n) >> 1;
    let r = (r + p1n + n1p) >> 1;
    ((r + z0p + z0n + 2) >> 2) as i16
}

/// The two samples doubled from three: `(5 p1 + 10 z0 + n1 + 8) >> 4` and its
/// mirror, in 16-bit unsigned steps.
#[inline(always)]
fn expand_func(p1: i16, z0: i16, n1: i16) -> (i16, i16) {
    let (p1, z0, n1) = (p1 as u16, z0 as u16, n1 as u16);
    let r = ((p1.wrapping_add(n1) >> 1).wrapping_add(z0)) >> 1;
    let rp = ((r.wrapping_add(p1) >> 1).wrapping_add(z0).wrapping_add(1)) >> 1;
    let rn = ((r.wrapping_add(n1) >> 1).wrapping_add(z0).wrapping_add(1)) >> 1;
    (rp as i16, rn as i16)
}

fn shrink_horz(
    src: &[i16],
    dst: &mut [i16],
    (w, h): (usize, usize),
    row: &mut [i16],
    halves: &mut [i16],
) -> (usize, usize) {
    let dw = (w + 5) >> 1;
    let (even, odd) = halves[..2 * (dw + 2)].split_at_mut(dw + 2);
    for (y, out) in dst[..h * dw].chunks_exact_mut(dw).enumerate() {
        let s = padded(row, &src[y * w..][..w]);
        // Output x reads source columns 2x - 4 ..= 2x + 1: split the row into
        // its even and odd columns from -4 so every tap is a plain slice.
        for ((e, o), pair) in even
            .iter_mut()
            .zip(odd.iter_mut())
            .zip(s[PAD - 4..].chunks_exact(2))
        {
            (*e, *o) = (pair[0], pair[1]);
        }
        let (e0, o0, e1, o1, e2, o2) = (
            &even[..dw],
            &odd[..dw],
            &even[1..],
            &odd[1..],
            &even[2..],
            &odd[2..],
        );
        for x in 0..dw {
            out[x] = shrink_func([e0[x], o0[x], e1[x], o1[x], e2[x], o2[x]]);
        }
    }
    (dw, h)
}

fn shrink_vert(
    src: &[i16],
    dst: &mut [i16],
    (w, h): (usize, usize),
    zeros: &[i16],
) -> (usize, usize) {
    let dh = (h + 5) >> 1;
    for (y, out) in dst[..dh * w].chunks_exact_mut(w).enumerate() {
        // Source rows 2y − 4 ..= 2y + 1.
        let [a, b, c, d, e, f]: [&[i16]; 6] =
            std::array::from_fn(|k| line(src, (w, h), 2 * y + k, 4, zeros));
        for x in 0..w {
            out[x] = shrink_func([a[x], b[x], c[x], d[x], e[x], f[x]]);
        }
    }
    (w, dh)
}

fn expand_horz(
    src: &[i16],
    dst: &mut [i16],
    (w, h): (usize, usize),
    row: &mut [i16],
) -> (usize, usize) {
    let dw = 2 * w + 4;
    for (y, out) in dst[..h * dw].chunks_exact_mut(dw).enumerate() {
        let s = padded(row, &src[y * w..][..w]);
        // Output pair x reads source columns x - 2 ..= x.
        let (p1, z0, n1) = (&s[PAD - 2..], &s[PAD - 1..], &s[PAD..]);
        for (x, pair) in out.chunks_exact_mut(2).enumerate() {
            (pair[0], pair[1]) = expand_func(p1[x], z0[x], n1[x]);
        }
    }
    (dw, h)
}

fn expand_vert(
    src: &[i16],
    dst: &mut [i16],
    (w, h): (usize, usize),
    zeros: &[i16],
) -> (usize, usize) {
    for (y, pair) in dst[..(2 * h + 4) * w].chunks_exact_mut(2 * w).enumerate() {
        // Source rows y − 2 ..= y.
        let [p1, z0, n1]: [&[i16]; 3] = std::array::from_fn(|k| line(src, (w, h), y + k, 2, zeros));
        let (top, bottom) = pair.split_at_mut(w);
        for x in 0..w {
            (top[x], bottom[x]) = expand_func(p1[x], z0[x], n1[x]);
        }
    }
    (w, 2 * h + 4)
}

/// Adds `p · (side − centre)` to every accumulator, the differences wrapping
/// to 16 bits as in libass.
#[inline(always)]
fn accumulate(acc: &mut [i32], side: &[i16], centre: &[i16], p: i32) {
    for ((a, &v), &c) in acc.iter_mut().zip(side).zip(centre) {
        *a = a.wrapping_add(i32::from(v.wrapping_sub(c)) * p);
    }
}

/// The symmetric kernel `centre + Σᵢ cᵢ · ((leftᵢ − centre) + (rightᵢ − centre))`
/// with taps in 1/65536, rounded; `tap(k)` is the row of samples k − n away.
/// The 32-bit sums wrap, so their order does not matter.
fn convolve<'a>(out: &mut [i16], acc: &mut [i32], m: &Method, tap: impl Fn(usize) -> &'a [i16]) {
    let n = m.radius;
    let centre = tap(n);
    let acc = &mut acc[..out.len()];
    acc.fill(0x8000);
    for i in (1..=n).rev() {
        let p = i32::from(m.coeff[i - 1]);
        accumulate(acc, tap(n - i), centre, p);
        accumulate(acc, tap(n + i), centre, p);
    }
    for ((d, &c), &a) in out.iter_mut().zip(centre).zip(acc.iter()) {
        *d = c.wrapping_add((a >> 16) as i16);
    }
}

fn blur_horz(
    src: &[i16],
    dst: &mut [i16],
    (w, h): (usize, usize),
    m: &Method,
    row: &mut [i16],
    acc: &mut [i32],
) -> (usize, usize) {
    let n = m.radius;
    let dw = w + 2 * n;
    for (y, out) in dst[..h * dw].chunks_exact_mut(dw).enumerate() {
        let s = padded(row, &src[y * w..][..w]);
        // Output x is centred on source column x - n.
        convolve(out, acc, m, |k| &s[PAD - 2 * n + k..][..dw]);
    }
    (dw, h)
}

fn blur_vert(
    src: &[i16],
    dst: &mut [i16],
    (w, h): (usize, usize),
    m: &Method,
    acc: &mut [i32],
    zeros: &[i16],
) -> (usize, usize) {
    let n = m.radius;
    let dh = h + 2 * n;
    for (y, out) in dst[..dh * w].chunks_exact_mut(w).enumerate() {
        // Output row y is centred on source row y - n.
        convolve(out, acc, m, |k| line(src, (w, h), y + k, 2 * n, zeros));
    }
    (w, dh)
}
