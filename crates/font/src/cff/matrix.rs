//! The FontMatrix as FreeType applies it: read into 16.16 values sharing one
//! power of ten (`cff_parse_font_matrix`), normalised so that `yy` is one with
//! the rest of the scale moved into units per em (`cff_face_init`), and
//! concatenated with the Top DICT's for Font DICTs. Outlines are transformed
//! in fixed point, so results match FreeType bit for bit.

use super::number::{Operand, POWER_TENS, fixed_dynamic};
use crate::fixed::{ONE, div_fix, mul_div, mul_fix};
use crate::read::Reader;
use triergon_core::{Error, Result};

/// A FontMatrix and the units per em it implies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FontMatrix {
    /// 16.16: `x' = xx x + xy y + dx`, `y' = yx x + yy y + dy`.
    xx: i64,
    xy: i64,
    yx: i64,
    yy: i64,
    /// 16.16 as parsed; whole font units once normalised.
    dx: i64,
    dy: i64,
    units_per_em: i64,
}

impl FontMatrix {
    /// The matrix of a DICT without a FontMatrix.
    pub(super) const fn identity(units_per_em: i64) -> Self {
        Self {
            xx: ONE,
            xy: 0,
            yx: 0,
            yy: ONE,
            dx: 0,
            dy: 0,
            units_per_em,
        }
    }

    /// The FontMatrix of a DICT entry's operands; FreeType reads the first
    /// six. Values with implausible scales or a near-singular matrix read as
    /// the identity with one unit per em, as FreeType falls back.
    pub(super) fn parse(operands: &[Operand<'_>]) -> Result<Self> {
        let mut values = [(0_i64, 0_i64); 6];
        for (value, operand) in values.iter_mut().zip(operands) {
            // Blends only occur in Private DICTs.
            let Operand::Encoded(bytes) = *operand else {
                return Err(Error::Invalid("CFF FontMatrix operands"));
            };
            *value = fixed_dynamic(&mut Reader::new(bytes))?;
        }
        if operands.len() < values.len() {
            return Err(Error::Invalid("CFF FontMatrix operands"));
        }
        let scales = values.iter().filter(|(v, _)| *v != 0).map(|&(_, s)| s);
        let (min, max) = scales.fold((i64::MAX, i64::MIN), |(lo, hi), s| (lo.min(s), hi.max(s)));
        if !(-9..=0).contains(&max) || !(0..=9).contains(&(max - min)) {
            return Ok(Self::identity(1));
        }
        // Rounded to the common power of ten, saturating at the 32-bit
        // `FT_LONG_MIN` and `FT_LONG_MAX` of 32-bit ARM.
        let [xx, yx, xy, yy, dx, dy] = values.map(|(value, scale)| {
            let divisor = POWER_TENS[usize::try_from(max - scale).unwrap_or(0)];
            let half = divisor >> 1;
            let (min, max) = (i64::from(i32::MIN), i64::from(i32::MAX));
            match value {
                0 => 0,
                v if v < 0 && min + half < v => (v - half) / divisor,
                v if v < 0 => min / divisor,
                v if max - half > v => (v + half) / divisor,
                _ => max / divisor,
            }
        });
        let matrix = Self {
            xx,
            xy,
            yx,
            yy,
            dx,
            dy,
            units_per_em: POWER_TENS[usize::try_from(-max).unwrap_or(0)],
        };
        Ok(if matrix.is_regular() {
            matrix
        } else {
            Self::identity(1)
        })
    }

    /// `FT_Matrix_Check` as GCC builds it for 32-bit ARM: nonzero 32-bit
    /// coefficients, shifted right so that the largest has 15 significant
    /// bits (`FT_MSB(val) - 4 * sizeof(FT_ULong) + 2`), whose determinant is
    /// not small next to the squared Frobenius norm, both in 32 bits.
    fn is_regular(&self) -> bool {
        let coefficients = [self.xx, self.xy, self.yx, self.yy];
        let any = coefficients.iter().fold(0, |any, v| any | v.unsigned_abs());
        if any == 0 || any > 0x7fff_ffff {
            return false;
        }
        let shift = (any as u32).ilog2().saturating_sub(14);
        let [xx, xy, yx, yy] = coefficients.map(|v| (v as i32) >> shift);
        let det = xx.wrapping_mul(yy).wrapping_sub(xy.wrapping_mul(yx));
        let frobenius = [xx, xy, yx, yy].iter().fold(0_u32, |sum, v| {
            sum.wrapping_add(v.wrapping_mul(*v).cast_unsigned())
        });
        det.unsigned_abs() > frobenius / 32
    }

    /// Divides by `|yy|` (or `|yx|` when `yy` is zero) and moves that scale into
    /// the units per em; the offsets become whole font units.
    pub(super) fn normalized(mut self) -> Self {
        let scale = if self.yy == 0 { self.yx } else { self.yy }.abs();
        if scale != ONE {
            self.units_per_em = div_fix(self.units_per_em, scale);
            for v in [
                &mut self.xx,
                &mut self.yx,
                &mut self.xy,
                &mut self.yy,
                &mut self.dx,
                &mut self.dy,
            ] {
                *v = div_fix(*v, scale);
            }
        }
        self.dx >>= 16;
        self.dy >>= 16;
        self
    }

    /// A Font DICT's matrix (`self`, as parsed) after the normalised Top DICT
    /// matrix `top`; the Top DICT's offsets do not carry over.
    pub(super) fn after(self, top: &Self) -> Self {
        let scaling = if top.units_per_em > 1 && self.units_per_em > 1 {
            top.units_per_em.min(self.units_per_em)
        } else {
            1
        };
        let val = ONE * scaling;
        let product =
            |a: i64, b: i64, c: i64, d: i64| mul_div(a, b, val).wrapping_add(mul_div(c, d, val));
        Self {
            xx: product(top.xx, self.xx, top.xy, self.yx),
            xy: product(top.xx, self.xy, top.xy, self.yy),
            yx: product(top.yx, self.xx, top.yy, self.yx),
            yy: product(top.yx, self.xy, top.yy, self.yy),
            dx: product(self.dx, top.xx, self.dy, top.xy),
            dy: product(self.dx, top.yx, self.dy, top.yy),
            units_per_em: mul_div(self.units_per_em, top.units_per_em, scaling),
        }
    }

    pub(super) const fn units_per_em(&self) -> i64 {
        self.units_per_em
    }

    const fn is_linear_identity(&self) -> bool {
        self.xx == ONE && self.yy == ONE && self.xy == 0 && self.yx == 0
    }

    /// Whether loading changes anything: a non-identity matrix or an offset.
    pub(super) const fn is_identity(&self) -> bool {
        self.is_linear_identity() && self.dx == 0 && self.dy == 0
    }

    /// A point in font units through the normalised matrix and offset
    /// (`FT_Outline_Transform`, then `FT_Outline_Translate`).
    pub(super) const fn point(&self, (x, y): (i64, i64)) -> (i64, i64) {
        let (x, y) = if self.is_linear_identity() {
            (x, y)
        } else {
            (
                mul_fix(x, self.xx).wrapping_add(mul_fix(y, self.xy)),
                mul_fix(x, self.yx).wrapping_add(mul_fix(y, self.yy)),
            )
        };
        (x.wrapping_add(self.dx), y.wrapping_add(self.dy))
    }

    /// The horizontal advance through the matrix: scaled by `xx`, then moved by
    /// the horizontal offset.
    pub(super) const fn advance(&self, advance: i64) -> i64 {
        let advance = if self.is_linear_identity() {
            advance
        } else {
            mul_fix(advance, self.xx)
        };
        advance.wrapping_add(self.dx)
    }

    /// The vertical advance through the matrix: scaled by `yy`, then moved by
    /// the vertical offset.
    pub(super) const fn vertical_advance(&self, advance: i64) -> i64 {
        let advance = if self.is_linear_identity() {
            advance
        } else {
            mul_fix(advance, self.yy)
        };
        advance.wrapping_add(self.dy)
    }
}

#[cfg(test)]
mod tests {
    use super::super::number::{Operand, number_length};
    use super::{FontMatrix, ONE};
    use triergon_core::Result;

    fn real(text: &[u8]) -> Vec<u8> {
        let mut nibbles: Vec<u8> = text
            .iter()
            .map(|&c| match c {
                b'0'..=b'9' => c - b'0',
                b'.' => 0xa,
                b'E' => 0xb,
                b'e' => 0xc,
                b'-' => 0xe,
                _ => unreachable!(),
            })
            .collect();
        nibbles.push(0xf);
        if nibbles.len() % 2 == 1 {
            nibbles.push(0xf);
        }
        let mut out = vec![30];
        out.extend(nibbles.chunks(2).map(|p| p[0] << 4 | p[1]));
        out
    }

    /// The FontMatrix of `bytes`, one number after another.
    fn parse(bytes: &[u8]) -> Result<FontMatrix> {
        let mut operands = Vec::new();
        let mut rest = bytes;
        while !rest.is_empty() {
            operands.push(Operand::Encoded(rest));
            rest = &rest[number_length(rest).unwrap()..];
        }
        FontMatrix::parse(&operands)
    }

    fn matrix(values: [&[u8]; 6]) -> FontMatrix {
        let bytes: Vec<u8> = values.iter().flat_map(|v| real(v)).collect();
        parse(&bytes).unwrap()
    }

    #[test]
    fn the_default_matrix_normalises_to_the_identity() {
        let m = matrix([b"0.001", b"0", b"0", b"0.001", b"0", b"0"]).normalized();
        assert_eq!(m, FontMatrix::identity(1000));
        // Integers: 1 0 0 1 0 0 at one unit per em.
        let ints = parse(&[140, 139, 139, 140, 139, 139]).unwrap();
        assert_eq!(ints.normalized(), FontMatrix::identity(1));
    }

    #[test]
    fn a_skewed_matrix_keeps_its_rounded_shear() {
        let m = matrix([b"0.0005", b"0", b"0.0001", b"0.0005", b"0", b"0"]).normalized();
        assert_eq!((m.xx, m.xy, m.yx, m.yy), (ONE, 13107, 0, ONE));
        assert_eq!(m.units_per_em(), 2000);
        assert_eq!(m.point((100, 1000)), (100 + 200, 1000));
        assert_eq!(m.advance(500), 500);
    }

    #[test]
    fn implausible_or_singular_matrices_fall_back() {
        let identity = FontMatrix::identity(1);
        assert_eq!(matrix([b"1", b"1", b"1", b"1", b"0", b"0"]), identity);
        assert_eq!(matrix([b"1E-12", b"0", b"0", b"1", b"0", b"0"]), identity);
        assert!(parse(&[139, 139]).is_err());
    }

    #[test]
    fn normalising_and_checking_follow_32_bit_arm() {
        // At a divisor of 10^7, xx's 2147482993 is within half the divisor of
        // FT_LONG_MAX, so FreeType saturates it to FT_LONG_MAX / 10^7, 214,
        // rather than rounding it to 215.
        let m = matrix([b"3.276799e7", b"0", b"0", b"3e7", b"1e4", b"0"]);
        assert_eq!(m.xx, 214);
        assert_ne!(m, FontMatrix::identity(1));
        // FT_Matrix_Check shifts the coefficients to 15 significant bits
        // first: this matrix passes it in 32 bits, though not in 64.
        let skewed = FontMatrix {
            xx: -6420,
            xy: -441_914,
            yx: 14_130_792,
            yy: 0,
            ..FontMatrix::identity(1)
        };
        assert!(skewed.is_regular());
    }

    #[test]
    fn offsets_become_font_units_and_font_dicts_concatenate() {
        let top = matrix([b"0.001", b"0", b"0", b"0.001", b"0.05", b"0"]).normalized();
        // 0.05 is rescaled to 0.001's power of ten, then divided by the rounded
        // 0.001: FreeType's offset ends up one unit short.
        assert_eq!((top.dx, top.dy), (49, 0));
        let sub = matrix([b"2", b"0", b"0", b"1", b"0", b"0"]);
        let both = sub.after(&top).normalized();
        assert_eq!((both.xx, both.yy), (2 * ONE, ONE));
        assert_eq!(both.units_per_em(), 1000);
    }
}
