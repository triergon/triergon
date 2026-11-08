//! Alpha bitmaps and the blends libass combines them with.

use triergon_core::{Budget, BudgetVec, Error, Result};

use crate::geometry::Vector;
use crate::limits::{Limits, MAX_BITMAP_BYTES};

/// Row alignment: strides are multiples of 16 bytes, as libass's C engine (and
/// its NEON and SSE2 engines) lay bitmaps out.
pub(crate) const ALIGNMENT: usize = 16;

/// An 8-bit coverage bitmap placed at `position`. Rows are [`Bitmap::stride`]
/// bytes apart. The bytes past the width are zero, and every operation keeps
/// them so, as libass's C engine does.
#[derive(Debug)]
pub struct Bitmap {
    /// The first pixel's column and row (libass's `left` and `top`), whole
    /// pixels, y down, relative to the origin the bitmap is placed against: the
    /// glyph's position, or the run's for a composite.
    pub position: Vector,
    width: usize,
    height: usize,
    stride: usize,
    data: BudgetVec<u8>,
}

impl Bitmap {
    /// A zeroed `width` × `height` bitmap at the origin.
    ///
    /// # Errors
    ///
    /// `Limit` when the bitmap would exceed
    /// [`MAX_BITMAP_BYTES`](crate::MAX_BITMAP_BYTES), libass's bound; `Policy`
    /// when it exceeds `limits.bitmap_bytes`; and the budget's errors.
    pub fn new(budget: &Budget, width: usize, height: usize, limits: &Limits) -> Result<Self> {
        let too_large = Error::Limit {
            requested: width.saturating_mul(height),
            maximum: MAX_BITMAP_BYTES,
        };
        let stride = width.checked_next_multiple_of(ALIGNMENT).ok_or(too_large)?;
        // libass's test, with its rounding: the stride against the bound per row.
        if stride > MAX_BITMAP_BYTES / height.max(1) {
            return Err(too_large);
        }
        let bytes = stride * height;
        if bytes > limits.bitmap_bytes {
            return Err(Error::Policy("bitmap size"));
        }
        let mut data = BudgetVec::new(budget);
        data.try_resize(bytes, 0)?;
        Ok(Self {
            position: Vector::new(0, 0),
            width,
            height,
            stride,
            data,
        })
    }

    /// [`Bitmap::new`], or `None` for a bitmap beyond libass's bound, which
    /// libass does not draw.
    pub(crate) fn drawable(
        budget: &Budget,
        width: usize,
        height: usize,
        limits: &Limits,
    ) -> Result<Option<Self>> {
        match Self::new(budget, width, height, limits) {
            Ok(bitmap) => Ok(Some(bitmap)),
            Err(Error::Limit { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The width in pixels.
    pub fn width(&self) -> usize {
        self.width
    }

    /// The height in pixels: the number of rows.
    pub fn height(&self) -> usize {
        self.height
    }

    /// Bytes from one row to the next: the width rounded up to 16.
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// All rows, `stride × height` bytes.
    pub fn data(&self) -> &[u8] {
        self.data.as_slice()
    }

    /// Row `y`, `width` bytes.
    pub fn row(&self, y: usize) -> &[u8] {
        &self.data.as_slice()[y * self.stride..][..self.width]
    }

    /// The rows, each `width` bytes.
    pub fn rows(&self) -> impl ExactSizeIterator<Item = &[u8]> + '_ {
        let width = self.width;
        self.data
            .as_slice()
            .chunks_exact(self.stride.max(1))
            .take(self.height)
            .map(move |row| &row[..width])
    }

    /// All rows, mutable, including the zero padding.
    pub(crate) fn data_mut(&mut self) -> &mut [u8] {
        self.data.as_mut_slice()
    }

    /// The rows, each `width` bytes, mutable.
    pub fn rows_mut(&mut self) -> impl Iterator<Item = &mut [u8]> + '_ {
        let width = self.width;
        let height = self.height;
        self.data
            .as_mut_slice()
            .chunks_exact_mut(self.stride.max(1))
            .take(height)
            .map(move |row| &mut row[..width])
    }

    /// A copy charged to `budget`.
    pub fn try_clone(&self, budget: &Budget) -> Result<Self> {
        let mut data = BudgetVec::new(budget);
        data.try_extend_from_slice(self.data())?;
        Ok(Self { data, ..*self })
    }

    /// Adds `src` with its top-left corner at (`x`, `y`) of this bitmap,
    /// saturating at 255. The part outside this bitmap is ignored.
    pub fn add(&mut self, src: &Self, x: i32, y: i32) {
        self.blend(src, x, y, |dst, src| {
            for (d, &s) in dst.iter_mut().zip(src) {
                *d = d.saturating_add(s);
            }
        });
    }

    /// Multiplies by the inverse of `src` placed at (`x`, `y`):
    /// `d · (255 − s) + 255 >> 8`, the inverse vector clip.
    pub fn multiply_inverse(&mut self, src: &Self, x: i32, y: i32) {
        self.blend(src, x, y, |dst, src| {
            for (d, &s) in dst.iter_mut().zip(src) {
                *d = ((u16::from(*d) * (255 - u16::from(s)) + 255) >> 8) as u8;
            }
        });
    }

    /// Multiplies by `src` placed at (`x`, `y`): `d · s + 255 >> 8`, the vector
    /// clip.
    pub fn multiply(&mut self, src: &Self, x: i32, y: i32) {
        self.blend(src, x, y, |dst, src| {
            for (d, &s) in dst.iter_mut().zip(src) {
                *d = ((u16::from(*d) * u16::from(s) + 255) >> 8) as u8;
            }
        });
    }

    /// Applies `op` to the overlapping row spans of this bitmap and `src` at
    /// (`x`, `y`).
    fn blend(&mut self, src: &Self, x: i32, y: i32, op: impl Fn(&mut [u8], &[u8])) {
        let Some((dst_x, src_x, width)) = overlap(x, src.width, self.width) else {
            return;
        };
        let Some((dst_y, src_y, height)) = overlap(y, src.height, self.height) else {
            return;
        };
        let stride = self.stride;
        for row in 0..height {
            let d = &mut self.data.as_mut_slice()[(dst_y + row) * stride + dst_x..][..width];
            let s = &src.data()[(src_y + row) * src.stride + src_x..][..width];
            op(d, s);
        }
    }

    /// Takes the fill out of this border bitmap, as libass does where the fill
    /// shows through: where the border covers more than `fill` it keeps
    /// `border − fill / 2`, elsewhere 0. Only the overlap with `fill`, by their
    /// positions, changes; rows overlap over their whole strides, as libass
    /// computes it.
    pub fn subtract_fill(&mut self, glyph: &Self) {
        let (sl, st) = (i64::from(self.position.x), i64::from(self.position.y));
        let (gl, gt) = (i64::from(glyph.position.x), i64::from(glyph.position.y));
        let (l, t) = (sl.max(gl), st.max(gt));
        let r = (sl + self.stride as i64).min(gl + glyph.stride as i64);
        let b = (st + self.height as i64).min(gt + glyph.height as i64);
        if r <= l || b <= t {
            return;
        }
        let width = (r - l) as usize;
        let (ox, oy) = ((l - sl) as usize, (t - st) as usize);
        let (gx, gy) = ((l - gl) as usize, (t - gt) as usize);
        let stride = self.stride;
        for row in 0..(b - t) as usize {
            let o = &mut self.data.as_mut_slice()[(oy + row) * stride + ox..][..width];
            let g = &glyph.data()[(gy + row) * glyph.stride + gx..][..width];
            for (o, &g) in o.iter_mut().zip(g) {
                *o = if *o > g { *o - g / 2 } else { 0 };
            }
        }
    }

    /// Moves the bitmap by the 26.6 `offset`, as libass moves shadows: its
    /// position by the whole pixels (rounding down), and its coverage right and
    /// down by the remaining 64ths, each pixel passing the fraction
    /// `v · shift >> 6` of itself to its neighbour, from the far edge so moved
    /// coverage is not moved twice.
    pub fn shift_by(&mut self, offset: Vector) {
        self.position.x = self.position.x.wrapping_add(offset.x >> 6);
        self.position.y = self.position.y.wrapping_add(offset.y >> 6);
        let (shift_x, shift_y) = ((offset.x & 63) as u32, (offset.y & 63) as u32);
        if shift_x != 0 {
            for row in self.rows_mut() {
                for x in (1..row.len()).rev() {
                    let moved = ((u32::from(row[x - 1]) * shift_x) >> 6) as u8;
                    row[x - 1] -= moved;
                    row[x] = row[x].wrapping_add(moved);
                }
            }
        }
        if shift_y != 0 && self.height > 1 {
            let (width, stride) = (self.width, self.stride);
            let data = self.data.as_mut_slice();
            for y in (1..self.height).rev() {
                let (above, below) = data.split_at_mut(y * stride);
                let above = &mut above[(y - 1) * stride..][..width];
                for (a, b) in above.iter_mut().zip(&mut below[..width]) {
                    let moved = ((u32::from(*a) * shift_y) >> 6) as u8;
                    *a -= moved;
                    *b = b.wrapping_add(moved);
                }
            }
        }
    }
}

/// The overlap of a span of `len` at `offset` with `0..extent`, as
/// (start in the extent, start in the span, length).
fn overlap(offset: i32, len: usize, extent: usize) -> Option<(usize, usize, usize)> {
    let start = i64::from(offset).max(0);
    let end = (i64::from(offset) + len as i64).min(extent as i64);
    (end > start).then(|| {
        (
            start as usize,
            (start - i64::from(offset)) as usize,
            (end - start) as usize,
        )
    })
}
