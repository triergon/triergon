//! CFF DICT data (Adobe Technical Note 5176, section 4; OpenType CFF2 DICTs).

use super::number::shared_integer;
use crate::read::Reader;
use crate::work::charge;
use triergon_core::{Error, Result, WorkBudget};

/// The DICT operand stack ceilings: 48 in CFF, 513 in CFF2.
pub(crate) const CFF_OPERANDS: usize = 48;
pub(crate) const CFF2_OPERANDS: usize = 513;

/// A DICT operator: one byte, or 12 followed by a second byte as `0x0c00 | byte`.
pub(crate) mod op {
    pub(crate) const PRIVATE: u16 = 18;
    pub(crate) const SUBRS: u16 = 19;
    pub(crate) const DEFAULT_WIDTH_X: u16 = 20;
    pub(crate) const NOMINAL_WIDTH_X: u16 = 21;
    pub(crate) const VSINDEX: u16 = 22;
    pub(crate) const BLEND: u16 = 23;
    pub(crate) const VSTORE: u16 = 24;
    pub(crate) const CHARSET: u16 = 15;
    pub(crate) const CHAR_STRINGS: u16 = 17;
    pub(crate) const CHARSTRING_TYPE: u16 = 0x0c06;
    pub(crate) const FONT_MATRIX: u16 = 0x0c07;
    pub(crate) const SYNTHETIC_BASE: u16 = 0x0c14;
    pub(crate) const ROS: u16 = 0x0c1e;
    pub(crate) const FD_ARRAY: u16 = 0x0c24;
    pub(crate) const FD_SELECT: u16 = 0x0c25;
}

/// An operand stack: DICT numbers as `f64`, charstring numbers as 16.16.
pub(crate) struct Stack<T, const N: usize> {
    values: [T; N],
    len: usize,
}

impl<T: Copy + Default, const N: usize> Stack<T, N> {
    pub(crate) fn new() -> Self {
        Self {
            values: [T::default(); N],
            len: 0,
        }
    }

    pub(crate) fn push(&mut self, value: T) -> Result<()> {
        let slot = self
            .values
            .get_mut(self.len)
            .ok_or(Error::Invalid("CFF operand stack overflow"))?;
        *slot = value;
        self.len += 1;
        Ok(())
    }

    pub(crate) fn pop(&mut self) -> Result<T> {
        self.len = self
            .len
            .checked_sub(1)
            .ok_or(Error::Invalid("CFF operand stack underflow"))?;
        Ok(self.values[self.len])
    }

    pub(crate) fn as_slice(&self) -> &[T] {
        &self.values[..self.len]
    }

    /// Removes the bottom operand.
    pub(crate) fn shift(&mut self) {
        if self.len > 0 {
            self.values.copy_within(1..self.len, 0);
            self.len -= 1;
        }
    }

    pub(crate) const fn clear(&mut self) {
        self.len = 0;
    }

    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    /// Replaces the top `count × (1 + regions)` operands with `count` values:
    /// each default `mix`ed with its `regions` deltas.
    pub(crate) fn blend(
        &mut self,
        count: usize,
        regions: usize,
        mix: impl Fn(T, &[T]) -> T,
    ) -> Result<()> {
        let size = count
            .checked_mul(regions + 1)
            .ok_or(Error::Invalid("CFF2 blend operands"))?;
        if size > self.len {
            return Err(Error::Invalid("CFF2 blend operands"));
        }
        let start = self.len - size;
        let (defaults, deltas) = self.values[start..self.len].split_at_mut(count);
        if regions > 0 {
            for (value, deltas) in defaults.iter_mut().zip(deltas.chunks_exact(regions)) {
                *value = mix(*value, deltas);
            }
        }
        self.len = start + count;
        Ok(())
    }
}

impl<const N: usize> Stack<f64, N> {
    /// Blends at the default instance: each default plus its deltas weighted by
    /// the region scalars.
    pub(crate) fn blend_scalars(&mut self, count: usize, scalars: &[f64]) -> Result<()> {
        self.blend(count, scalars.len(), |value, deltas| {
            value + scalars.iter().zip(deltas).map(|(s, d)| s * d).sum::<f64>()
        })
    }
}

/// Calls `apply` for each operator with its operands. In a CFF2 Private DICT,
/// `blend` is applied in place using `scalars(vsindex)`.
pub(crate) fn parse<'s, const N: usize>(
    bytes: &[u8],
    work: &mut WorkBudget,
    scalars: &dyn Fn(usize) -> Result<&'s [f64]>,
    mut apply: impl FnMut(u16, &[f64]) -> Result<()>,
) -> Result<()> {
    let mut r = Reader::new(bytes);
    let mut stack = Stack::<f64, N>::new();
    let mut vsindex = 0;
    while !r.at_end() {
        charge(work, 1)?;
        let byte = r.u8()?;
        let operator = match byte {
            12 => 0x0c00 | u16::from(r.u8()?),
            0..=27 => u16::from(byte),
            _ => {
                stack.push(number(byte, &mut r)?)?;
                continue;
            }
        };
        match operator {
            op::VSINDEX if N == CFF2_OPERANDS => {
                vsindex = integer(stack.pop()?)?;
                scalars(vsindex)?;
            }
            op::BLEND if N == CFF2_OPERANDS => {
                let count = integer(stack.pop()?)?;
                stack.blend_scalars(count, scalars(vsindex)?)?;
                continue;
            }
            _ => apply(operator, stack.as_slice())?,
        }
        stack.clear();
    }
    Ok(())
}

/// A number in DICT data (TN 5176 Table 3).
fn number(byte: u8, r: &mut Reader<'_>) -> Result<f64> {
    match byte {
        29 => Ok(f64::from(r.i32()?)),
        30 => real(r),
        _ => shared_integer(byte, r)?
            .map(f64::from)
            .ok_or(Error::Invalid("reserved CFF DICT byte")),
    }
}

/// A real number: nibbles of digits, point, exponent and sign (TN 5176 Table 5).
fn real(r: &mut Reader<'_>) -> Result<f64> {
    const MAX: usize = 64;
    let mut text = [0_u8; MAX];
    let mut len = 0;
    loop {
        let byte = r.u8()?;
        for nibble in [byte >> 4, byte & 0xf] {
            let chars: &[u8] = match nibble {
                0..=9 => &[b'0' + nibble],
                0xa => b".",
                0xb => b"E",
                0xc => b"E-",
                0xe => b"-",
                0xf => {
                    let text = std::str::from_utf8(&text[..len]).unwrap_or_default();
                    // An empty or malformed real reads as zero, as FreeType does.
                    return Ok(text
                        .parse::<f64>()
                        .ok()
                        .filter(|n| n.is_finite())
                        .unwrap_or(0.0));
                }
                _ => return Err(Error::Invalid("reserved CFF real nibble")),
            };
            text.get_mut(len..len + chars.len())
                .ok_or(Error::Invalid("CFF real number too long"))?
                .copy_from_slice(chars);
            len += chars.len();
        }
    }
}

/// A nonnegative integral operand used as a count, index or offset.
pub(crate) fn integer(value: f64) -> Result<usize> {
    if value < 0.0 || value.fract() != 0.0 || value > f64::from(u32::MAX) {
        return Err(Error::Invalid("CFF operand is not an index"));
    }
    usize::try_from(value as u32).map_err(|_| Error::Overflow)
}

/// The single operand of an offset or count operator.
pub(crate) fn single(operands: &[f64]) -> Result<usize> {
    match operands {
        [value] => integer(*value),
        _ => Err(Error::Invalid("CFF DICT operand count")),
    }
}

#[cfg(test)]
mod tests {
    use super::{CFF_OPERANDS, CFF2_OPERANDS, Stack, op, parse, real};
    use crate::read::Reader;
    use triergon_core::{Result, WorkBudget};

    fn entries<const N: usize>(bytes: &[u8], scalars: &[f64]) -> Result<Vec<(u16, Vec<f64>)>> {
        let mut out = Vec::new();
        parse::<N>(
            bytes,
            &mut WorkBudget::new(1000),
            &|_| Ok(scalars),
            |operator, operands| {
                out.push((operator, operands.to_vec()));
                Ok(())
            },
        )?;
        Ok(out)
    }

    #[test]
    fn numbers_and_operators() {
        // 100 17 (CharStrings); -1000 1131 -0.5 12 7 (FontMatrix, partial).
        let bytes = [239, 17, 254, 124, 250, 255, 30, 0xe0, 0xa5, 0xff, 12, 7];
        let parsed = entries::<CFF_OPERANDS>(&bytes, &[]).unwrap();
        assert_eq!(parsed[0], (op::CHAR_STRINGS, vec![100.0]));
        assert_eq!(parsed[1], (op::FONT_MATRIX, vec![-1000.0, 1131.0, -0.5]));
        assert_eq!(real(&mut Reader::new(&[0x1a, 0x25, 0xff])), Ok(1.25));
        assert_eq!(real(&mut Reader::new(&[0x2b, 0x3f])), Ok(2000.0));
        assert_eq!(real(&mut Reader::new(&[0x2c, 0x1f])), Ok(0.2));
        assert!(real(&mut Reader::new(&[0xd0])).is_err());
        assert!(entries::<CFF_OPERANDS>(&[255], &[]).is_err());
    }

    #[test]
    fn cff2_blends_at_the_default_instance() {
        // 10 20 (deltas for one value over two regions) 1 blend 19 (Subrs).
        let bytes = [149, 159, 169, 140, 23, 19];
        let parsed = entries::<CFF2_OPERANDS>(&bytes, &[0.0, 1.0]).unwrap();
        assert_eq!(parsed, [(op::SUBRS, vec![40.0])]);
        let mut stack = Stack::<f64, 4>::new();
        for v in [1.0, 2.0, 3.0, 4.0] {
            stack.push(v).unwrap();
        }
        assert!(stack.push(5.0).is_err());
        assert!(stack.blend_scalars(2, &[1.0]).is_ok());
        assert_eq!(stack.as_slice(), [4.0, 6.0]);
        assert!(stack.blend_scalars(3, &[]).is_err());
    }
}
