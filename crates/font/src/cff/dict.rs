//! CFF DICT data (Adobe Technical Note 5176, section 4; OpenType CFF2 DICTs).

use super::number::{Operand, number_length};
use super::path::Fixed;
use crate::fixed::mul_fix;
use crate::read::Reader;
use crate::work::charge;
use triergon_core::{Error, Result, WorkBudget};

/// The DICT operand stack ceilings: 48 in CFF, 513 in CFF2.
pub(crate) const CFF_OPERANDS: usize = 48;
pub(crate) const CFF2_OPERANDS: usize = 513;

/// A DICT operator: one byte, or 12 followed by a second byte as `0x0c00 | byte`.
pub(crate) mod op {
    pub(crate) const BLUE_VALUES: u16 = 6;
    pub(crate) const OTHER_BLUES: u16 = 7;
    pub(crate) const FAMILY_BLUES: u16 = 8;
    pub(crate) const FAMILY_OTHER_BLUES: u16 = 9;
    pub(crate) const BLUE_SCALE: u16 = 0x0c09;
    pub(crate) const BLUE_SHIFT: u16 = 0x0c0a;
    pub(crate) const BLUE_FUZZ: u16 = 0x0c0b;
    pub(crate) const LANGUAGE_GROUP: u16 = 0x0c11;
    pub(crate) const PRIVATE: u16 = 18;
    pub(crate) const SUBRS: u16 = 19;
    pub(crate) const DEFAULT_WIDTH_X: u16 = 20;
    pub(crate) const NOMINAL_WIDTH_X: u16 = 21;
    pub(crate) const VSINDEX: u16 = 22;
    pub(crate) const BLEND: u16 = 23;
    pub(crate) const VSTORE: u16 = 24;
    pub(crate) const ENCODING: u16 = 16;
    pub(crate) const CHARSET: u16 = 15;
    pub(crate) const CHAR_STRINGS: u16 = 17;
    pub(crate) const CHARSTRING_TYPE: u16 = 0x0c06;
    pub(crate) const FONT_MATRIX: u16 = 0x0c07;
    pub(crate) const SYNTHETIC_BASE: u16 = 0x0c14;
    pub(crate) const ROS: u16 = 0x0c1e;
    pub(crate) const FD_ARRAY: u16 = 0x0c24;
    pub(crate) const FD_SELECT: u16 = 0x0c25;
}

/// An operand stack: DICT operands, or charstring numbers as 16.16.
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

/// One DICT entry: an operator and its operands as FreeType's parser holds
/// them.
pub(crate) struct Entry<'a, 'b> {
    pub(crate) operator: u16,
    pub(crate) operands: &'b [Operand<'a>],
}

/// Calls `apply` for each entry. In a CFF2 DICT, `blend` replaces its operands
/// with their blends in place, as `cff_blend_doBlend` does: each default in
/// 16.16 plus its deltas times the 16.16 region scalars of `scalars(vsindex)`.
pub(crate) fn parse<'a, 's, const N: usize>(
    bytes: &'a [u8],
    work: &mut WorkBudget,
    scalars: &dyn Fn(usize) -> Result<&'s [Fixed]>,
    mut apply: impl FnMut(Entry<'a, '_>) -> Result<()>,
) -> Result<()> {
    let mut r = Reader::new(bytes);
    let mut stack = Stack::<Operand<'a>, N>::new();
    let mut vsindex = 0;
    while !r.at_end() {
        charge(work, 1)?;
        let at = r.offset();
        let byte = r.u8()?;
        let operator = match byte {
            12 => 0x0c00 | u16::from(r.u8()?),
            0..=27 => u16::from(byte),
            _ => {
                let number = &bytes[at..];
                let len = number_length(number).ok_or(Error::Invalid("CFF DICT number"))?;
                r.skip(len - 1)?;
                stack.push(Operand::Encoded(number))?;
                continue;
            }
        };
        match operator {
            op::VSINDEX if N == CFF2_OPERANDS => {
                vsindex = stack.pop()?.index()?;
                scalars(vsindex)?;
            }
            op::BLEND if N == CFF2_OPERANDS => {
                let count = stack.pop()?.index()?;
                let scalars = scalars(vsindex)?;
                stack.blend(count, scalars.len(), |default, deltas| {
                    let sum = deltas
                        .iter()
                        .zip(scalars)
                        .fold(default.fixed(0), |sum, (d, &s)| {
                            sum.wrapping_add(mul_fix(d.fixed(0), i64::from(s)))
                        });
                    Operand::Blended(sum)
                })?;
                continue;
            }
            _ => apply(Entry {
                operator,
                operands: stack.as_slice(),
            })?,
        }
        stack.clear();
    }
    Ok(())
}

/// The single operand of an offset or count operator.
pub(crate) fn single(operands: &[Operand<'_>]) -> Result<usize> {
    match operands {
        [value] => value.index(),
        _ => Err(Error::Invalid("CFF DICT operand count")),
    }
}

/// The first operand of an entry that needs one.
pub(crate) fn first<'a>(operands: &[Operand<'a>]) -> Result<Operand<'a>> {
    operands
        .first()
        .copied()
        .ok_or(Error::Invalid("CFF DICT operand count"))
}

#[cfg(test)]
mod tests {
    use super::{CFF_OPERANDS, CFF2_OPERANDS, Stack, op, parse};
    use triergon_core::{Result, WorkBudget};

    /// Each entry's operator and operands in 16.16.
    fn entries<const N: usize>(bytes: &[u8], scalars: &[i32]) -> Result<Vec<(u16, Vec<i64>)>> {
        let mut out = Vec::new();
        parse::<N>(
            bytes,
            &mut WorkBudget::new(1000),
            &|_| Ok(scalars),
            |entry| {
                out.push((
                    entry.operator,
                    entry.operands.iter().map(|o| o.fixed(0)).collect(),
                ));
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
        assert_eq!(parsed[0], (op::CHAR_STRINGS, vec![100 << 16]));
        assert_eq!(
            parsed[1],
            (op::FONT_MATRIX, vec![-1000 << 16, 1131 << 16, -0x8000])
        );
        assert!(entries::<CFF_OPERANDS>(&[255], &[]).is_err());
        // A real without its end, and a two-byte integer cut short.
        assert!(entries::<CFF_OPERANDS>(&[30, 0x12], &[]).is_err());
        assert!(entries::<CFF_OPERANDS>(&[247], &[]).is_err());
    }

    #[test]
    fn cff2_blends_at_the_default_instance_in_16_16() {
        // 10 20 (deltas for one value over two regions) 1 blend 19 (Subrs).
        let bytes = [149, 159, 169, 140, 23, 19];
        let parsed = entries::<CFF2_OPERANDS>(&bytes, &[0, 0x10000]).unwrap();
        assert_eq!(parsed, [(op::SUBRS, vec![40 << 16])]);
        // 0.5 plus 0.25 times 1: 0.75.
        let bytes = [30, 0xa5, 0xff, 30, 0xa2, 0x5f, 140, 23, 19];
        let parsed = entries::<CFF2_OPERANDS>(&bytes, &[0x10000]).unwrap();
        assert_eq!(parsed, [(op::SUBRS, vec![0xc000])]);
        let mut stack = Stack::<i32, 4>::new();
        for v in [1, 2, 3, 4] {
            stack.push(v).unwrap();
        }
        assert!(stack.push(5).is_err());
        assert!(stack.blend(2, 1, |v, d| v + d[0]).is_ok());
        assert_eq!(stack.as_slice(), [4, 6]);
        assert!(stack.blend(3, 0, |v, _| v).is_err());
    }
}
