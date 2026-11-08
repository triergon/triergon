//! Integer points and rectangles, y down: 26.6 fixed point (1/64 pixel) for
//! outlines, whole pixels for bitmap positions.

/// The largest coordinate magnitude an outline point may have: 2²⁸ − 1, so that
/// cubic subdivision (`8 · max + 4`) stays within 32 bits.
pub const OUTLINE_MAX: i32 = (1 << 28) - 1;

/// libass's `double_to_d6`: pixels to 26.6 fixed point, rounding half to
/// even. Beyond the `i32` range it saturates and NaN is 0, as libass gives
/// them on 32-bit ARM with GCC.
pub fn pixels_26_6(pixels: f64) -> i32 {
    triergon_core::round_ties_even_i32(pixels * 64.0)
}

/// A point or offset: in 26.6 fixed point for outlines, matrices and offsets of
/// coverage, in whole pixels for bitmap and clip positions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Vector {
    /// The horizontal coordinate, increasing to the right.
    pub x: i32,
    /// The vertical coordinate, increasing downwards.
    pub y: i32,
}

impl Vector {
    /// The point (`x`, `y`).
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    /// Whether both coordinates are within ±[`OUTLINE_MAX`].
    pub const fn in_outline_range(self) -> bool {
        self.x.unsigned_abs() <= OUTLINE_MAX as u32 && self.y.unsigned_abs() <= OUTLINE_MAX as u32
    }
}

/// The closed rectangle `x_min ≤ x ≤ x_max`, `y_min ≤ y ≤ y_max`. It bounds
/// points (an outline's control box in 26.6, both corners included) or whole
/// pixels (an area in frame pixels: columns `x_min..x_max` and rows
/// `y_min..y_max`, whose squares it covers). [`Rect::EMPTY`] is the identity
/// of [`Rect::include`] and reports [`Rect::is_empty`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rect {
    /// The left edge.
    pub x_min: i32,
    /// The top edge.
    pub y_min: i32,
    /// The right edge, included.
    pub x_max: i32,
    /// The bottom edge, included.
    pub y_max: i32,
}

impl Rect {
    /// The rectangle covering nothing: minima at `i32::MAX`, maxima at
    /// `i32::MIN`, as libass's `rectangle_reset` leaves one.
    pub const EMPTY: Self = Self {
        x_min: i32::MAX,
        y_min: i32::MAX,
        x_max: i32::MIN,
        y_max: i32::MIN,
    };

    /// The rectangle from (`x_min`, `y_min`) to (`x_max`, `y_max`), both
    /// corners included.
    pub const fn new(x_min: i32, y_min: i32, x_max: i32, y_max: i32) -> Self {
        Self {
            x_min,
            y_min,
            x_max,
            y_max,
        }
    }

    /// Grows the rectangle to cover `other`.
    #[inline]
    pub fn include(&mut self, other: Self) {
        self.x_min = self.x_min.min(other.x_min);
        self.y_min = self.y_min.min(other.y_min);
        self.x_max = self.x_max.max(other.x_max);
        self.y_max = self.y_max.max(other.y_max);
    }

    /// Grows the rectangle to cover `point`.
    #[inline]
    pub fn include_point(&mut self, point: Vector) {
        self.include(Self::new(point.x, point.y, point.x, point.y));
    }

    /// Whether the rectangle covers nothing: a minimum exceeds its maximum.
    pub const fn is_empty(&self) -> bool {
        self.x_min > self.x_max || self.y_min > self.y_max
    }
}

/// A point in floating point, for normals and transforms.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct DVector {
    pub(crate) x: f64,
    pub(crate) y: f64,
}

impl DVector {
    pub(crate) const ZERO: Self = Self::new(0.0, 0.0);

    pub(crate) const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// `a.x * b.x + a.y * b.y`.
    #[inline(always)]
    pub(crate) fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y
    }

    /// `a.x * b.y - a.y * b.x`.
    #[inline(always)]
    pub(crate) fn cross(self, other: Self) -> f64 {
        self.x * other.y - self.y * other.x
    }

    #[inline(always)]
    pub(crate) fn len(self) -> f64 {
        self.dot(self).sqrt()
    }

    #[inline(always)]
    pub(crate) fn scale(self, factor: f64) -> Self {
        Self::new(self.x * factor, self.y * factor)
    }

    /// `self / 2`.
    #[inline(always)]
    pub(crate) fn halved(self) -> Self {
        Self::new(self.x / 2.0, self.y / 2.0)
    }

    /// `(self + other) / 2`.
    #[inline(always)]
    pub(crate) fn mean(self, other: Self) -> Self {
        (self + other).halved()
    }
}

impl std::ops::Add for DVector {
    type Output = Self;

    #[inline(always)]
    fn add(self, other: Self) -> Self {
        Self::new(self.x + other.x, self.y + other.y)
    }
}

/// The halving of a quadratic spline at t = ½: five points sharing the middle
/// one, rounded as libass rounds them.
pub(crate) fn split_quadratic(pt: &[Vector; 3]) -> [Vector; 5] {
    let half = |a: i32, b: i32, c: i32| {
        let (ab, bc) = (a + b, b + c);
        [a, ab >> 1, (ab + bc + 2) >> 2, bc >> 1, c]
    };
    let x = half(pt[0].x, pt[1].x, pt[2].x);
    let y = half(pt[0].y, pt[1].y, pt[2].y);
    std::array::from_fn(|i| Vector::new(x[i], y[i]))
}

/// The halving of a cubic spline at t = ½: seven points sharing the middle one.
/// Within ±[`OUTLINE_MAX`] the sums stay below 2³¹.
pub(crate) fn split_cubic(pt: &[Vector; 4]) -> [Vector; 7] {
    let half = |a: i32, b: i32, c: i32, d: i32| {
        let (ab, centre, cd) = (a + b, b + c + 2, c + d);
        let (abc, bcd) = (ab + centre, centre + cd);
        [
            a,
            ab >> 1,
            abc >> 2,
            (abc + bcd - 1) >> 3,
            bcd >> 2,
            cd >> 1,
            d,
        ]
    };
    let x = half(pt[0].x, pt[1].x, pt[2].x, pt[3].x);
    let y = half(pt[0].y, pt[1].y, pt[2].y, pt[3].y);
    std::array::from_fn(|i| Vector::new(x[i], y[i]))
}

#[cfg(test)]
mod tests {
    use super::pixels_26_6;

    #[test]
    fn pixels_round_to_26_6_half_to_even() {
        assert_eq!([pixels_26_6(256.0), pixels_26_6(0.5 / 64.0)], [16384, 0]);
        assert_eq!(pixels_26_6(1.5 / 64.0), 2);
        assert_eq!(pixels_26_6(f64::INFINITY), i32::MAX);
        assert_eq!(pixels_26_6(f64::NAN), 0);
    }
}
