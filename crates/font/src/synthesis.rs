//! Synthetic oblique and bold for faces that lack the style, as libass applies
//! them to scaled outlines: FreeType's `FT_Outline_Transform` and
//! `FT_Outline_EmboldenXY`, bit for bit.

use crate::Font;
use crate::fixed::{mul_div, mul_fix, norm_len};
use crate::glyf::narrow;
use crate::outline::{Outline, Point};
use triergon_core::Result;

/// The fill rule direction of an outline's contours, from the signed area of
/// its control polygon (`FT_Outline_Get_Orientation`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    /// Filled contours run clockwise (y up), as TrueType draws them; also the
    /// answer for an outline without points.
    TrueType,
    /// Filled contours run counter-clockwise, as CFF draws them.
    PostScript,
    /// Zero area.
    None,
}

/// Which synthetic styles libass applies to a glyph of a face.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Synthesis {
    /// Slant the outline (see [`Synthesis::shear`]).
    pub oblique: bool,
    /// Embolden the outline by the size's
    /// [`embolden_strength`](crate::SizedFont::embolden_strength).
    pub embolden: bool,
}

impl Synthesis {
    /// The 16.16 horizontal shear libass applies for `oblique`: tan 10° for
    /// PostScript (CFF) faces, and 0x5700, as GDI slants, for the others.
    pub fn shear(font: &Font) -> i32 {
        if font.is_postscript() { 0x2d24 } else { 0x5700 }
    }
}

impl Font {
    /// The synthetic styles libass applies for a requested weight and slant (as
    /// [`Request::weight`](crate::Request::weight) and
    /// [`Request::slant`](crate::Request::slant) give them): oblique when the
    /// face is not italic and the slant exceeds 55, bold when the face is not
    /// bold and the weight exceeds [`Font::weight`] by more than 150. Italic and
    /// bold come from `OS/2.fsSelection` bits 0 and 5, or FreeType's style
    /// without `OS/2`.
    pub fn synthesis(&self, weight: u32, slant: u32) -> Synthesis {
        let (italic, bold) = match self.os2() {
            Some(os2) => (os2.selection.italic(), os2.selection.bold()),
            None => (self.style().italic, self.style().bold),
        };
        Synthesis {
            oblique: !italic && slant > 55,
            embolden: !bold && weight > self.weight() + 150,
        }
    }
}

impl Outline {
    /// The orientation of the contours, by the shoelace sum over every point,
    /// control points included, wrapping in 64 bits as FreeType's `FT_Int64`
    /// sum does; a zero sum is [`Orientation::None`].
    pub fn orientation(&self) -> Orientation {
        if self.is_empty() {
            return Orientation::TrueType;
        }
        let mut area = 0_i64;
        for contour in self.contours() {
            let Some(last) = contour.last() else {
                continue;
            };
            let mut prev = last.point;
            for p in contour {
                let (x0, y0) = (i64::from(prev.x), i64::from(prev.y));
                let (x1, y1) = (i64::from(p.point.x), i64::from(p.point.y));
                area = area.wrapping_add((y1 - y0).wrapping_mul(x1 + x0));
                prev = p.point;
            }
        }
        match area.signum() {
            1 => Orientation::PostScript,
            -1 => Orientation::TrueType,
            _ => Orientation::None,
        }
    }

    /// Applies the 16.16 matrix `[xx, xy, yx, yy]` to every point: `x' = xx x +
    /// xy y`, `y' = yx x + yy y`, each product rounded.
    ///
    /// # Errors
    ///
    /// `Overflow` when a coordinate leaves `i32`.
    pub fn transform(&mut self, [xx, xy, yx, yy]: [i32; 4]) -> Result<()> {
        let [xx, xy, yx, yy] = [xx, xy, yx, yy].map(i64::from);
        for p in self.points_mut() {
            let (x, y) = (i64::from(p.point.x), i64::from(p.point.y));
            p.point = Point::new(
                narrow(mul_fix(x, xx).wrapping_add(mul_fix(y, xy)))?,
                narrow(mul_fix(x, yx).wrapping_add(mul_fix(y, yy)))?,
            );
        }
        Ok(())
    }

    /// Emboldens the outline by `strength` 26.6 units in total, half on each
    /// side of every contour, moving points along the bisectors of their
    /// corners. An outline of zero area is left as it is (FreeType refuses it).
    ///
    /// # Errors
    ///
    /// `Overflow` when a coordinate leaves `i32`.
    pub fn embolden(&mut self, strength: i32) -> Result<()> {
        let half = i64::from(strength) / 2;
        if half == 0 {
            return Ok(());
        }
        let truetype = match self.orientation() {
            Orientation::None => return Ok(()),
            o => o == Orientation::TrueType,
        };
        let mut first = 0;
        for c in 0..self.contour_ends().len() {
            let end = self.contour_ends()[c] as usize;
            if end > first {
                embolden_contour(&mut self.points_mut()[first..end], half, truetype)?;
            }
            first = end;
        }
        Ok(())
    }
}

/// One contour of `FT_Outline_EmboldenXY` with equal strengths.
fn embolden_contour(
    points: &mut [crate::outline::OutlinePoint],
    strength: i64,
    truetype: bool,
) -> Result<()> {
    let last = points.len() - 1;
    let next = |j: usize| if j < last { j + 1 } else { 0 };
    let at = |points: &[crate::outline::OutlinePoint], i: usize| {
        (i64::from(points[i].point.x), i64::from(points[i].point.y))
    };
    let (mut incoming, mut l_in) = ((0, 0), 0);
    let (mut anchor, mut l_anchor) = ((0, 0), 0);
    // `j` visits the points; `i` advances only when points move; the anchor `k`
    // is the first moved point.
    let (mut i, mut j, mut k) = (last, 0, None);
    while j != i && Some(i) != k {
        let (outgoing, l_out) = if Some(j) == k {
            (anchor, l_anchor)
        } else {
            let (pj, pi) = (at(points, j), at(points, i));
            let mut out = (pj.0 - pi.0, pj.1 - pi.1);
            let l_out = norm_len(&mut out);
            if l_out == 0 {
                j = next(j);
                continue;
            }
            (out, l_out)
        };
        if l_in != 0 {
            if k.is_none() {
                k = Some(i);
                anchor = incoming;
                l_anchor = l_in;
            }
            let (a, b) = (incoming, outgoing);
            let mut d = mul_fix(a.0, b.0) + mul_fix(a.1, b.1);
            // Shift only where the contour turns by less than about 160°.
            let shift = if d > -0xF000 {
                d += 0x1_0000;
                let mut shift = (a.1 + b.1, a.0 + b.0);
                let mut q = mul_fix(b.0, a.1) - mul_fix(b.1, a.0);
                if truetype {
                    shift.0 = -shift.0;
                    q = -q;
                } else {
                    shift.1 = -shift.1;
                }
                // Restrict the shift on collapsing segments.
                let l = l_in.min(l_out);
                let limit = |s: i64| {
                    if mul_fix(strength, q) <= mul_fix(l, d) {
                        mul_div(s, strength, d)
                    } else {
                        mul_div(s, l, q)
                    }
                };
                (limit(shift.0), limit(shift.1))
            } else {
                (0, 0)
            };
            while i != j {
                let p = &mut points[i].point;
                p.x = narrow(i64::from(p.x) + strength + shift.0)?;
                p.y = narrow(i64::from(p.y) + strength + shift.1)?;
                i = next(i);
            }
        } else {
            i = j;
        }
        incoming = outgoing;
        l_in = l_out;
        j = next(j);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Orientation;
    use crate::outline::{Outline, Point, PointKind};
    use triergon_core::Budget;

    fn square(budget: &Budget, clockwise: bool) -> Outline {
        let mut o = Outline::new(budget);
        o.begin(usize::MAX);
        let mut corners = [(0, 0), (0, 640), (640, 640), (640, 0)];
        if !clockwise {
            corners.reverse();
        }
        for (x, y) in corners {
            o.push_point(Point::new(x, y), PointKind::On).unwrap();
        }
        o.end_contour().unwrap();
        o
    }

    #[test]
    fn orientation_and_emboldening_grow_the_square() {
        let budget = Budget::new(1 << 16).unwrap();
        let mut o = square(&budget, true);
        assert_eq!(o.orientation(), Orientation::TrueType);
        o.embolden(64).unwrap();
        let xs: Vec<_> = o.points().iter().map(|p| p.point).collect();
        assert_eq!(
            xs,
            [
                Point::new(0, 0),
                Point::new(0, 704),
                Point::new(704, 704),
                Point::new(704, 0)
            ]
        );
        let ccw = square(&budget, false);
        assert_eq!(ccw.orientation(), Orientation::PostScript);
        let mut sheared = square(&budget, true);
        sheared.transform([0x1_0000, 0x5700, 0, 0x1_0000]).unwrap();
        assert_eq!(sheared.points()[1].point, Point::new(218, 640));
        assert_eq!(Outline::new(&budget).orientation(), Orientation::TrueType);
    }
}
