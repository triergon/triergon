//! CFF DICT data (Adobe Technical Note 5176, section 4; OpenType CFF2 DICTs).

use super::number::{Operand, number_length};
use super::path::Fixed;
use crate::fixed::mul_fix;
use crate::read::Reader;
use crate::work::charge;
use triergon_core::{Error, Result, WorkBudget};

/// The charstring operand stack ceilings: 48 in CFF, 513 in CFF2.
pub(crate) const CFF_OPERANDS: usize = 48;
pub(crate) const CFF2_OPERANDS: usize = 513;

/// The DICT parser's stack as FreeType sizes it (`CFF_MAX_STACK_DEPTH` and
/// `CFF2_DEFAULT_STACK`), plus the operator's slot: an operator after more
/// operands than that overflows it.
pub(crate) const CFF_DICT_STACK: usize = 97;
pub(crate) const CFF2_DICT_STACK: usize = 514;

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
    const STD_HW: u16 = 10;
    const STD_VW: u16 = 11;
    const FORCE_BOLD: u16 = 0x0c0e;
    const FORCE_BOLD_THRESHOLD: u16 = 0x0c0f;
    const LEN_IV: u16 = 0x0c10;
    const EXPANSION_FACTOR: u16 = 0x0c12;
    const INITIAL_RANDOM_SEED: u16 = 0x0c13;

    /// The Private DICT operators FreeType reads, each from at least one
    /// operand, that this crate has no use for (`cfftoken.h`).
    pub(crate) const fn unused_private(version: super::super::Version, op: u16) -> bool {
        match op {
            STD_HW | STD_VW | EXPANSION_FACTOR => true,
            DEFAULT_WIDTH_X | NOMINAL_WIDTH_X | FORCE_BOLD | FORCE_BOLD_THRESHOLD | LEN_IV
            | INITIAL_RANDOM_SEED => matches!(version, super::super::Version::Cff1),
            _ => false,
        }
    }
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

/// Calls `apply` for each entry, as `cff_parser_run` reads a DICT: a number
/// cut short at the end ends it, and operators without a meaning are
/// skipped with their operands. `variations` holds the region scalars of
/// each `vsindex` in a CFF2 Private DICT, the only DICT where `vsindex` and
/// `blend` apply; `blend` replaces its operands with their blends in place,
/// as `cff_blend_doBlend` does: each default in 16.16 plus its deltas times
/// the 16.16 region scalars. `vsindex` reaches `apply` too.
pub(crate) fn parse<'a, 's, const N: usize>(
    bytes: &'a [u8],
    work: &mut WorkBudget,
    variations: Option<&dyn Fn(usize) -> Result<&'s [Fixed]>>,
    mut apply: impl FnMut(Entry<'a, '_>) -> Result<()>,
) -> Result<()> {
    let mut r = Reader::new(bytes);
    let mut stack = Stack::<Operand<'a>, N>::new();
    let (mut vsindex, mut blended) = (0, false);
    while !r.at_end() {
        charge(work, 1)?;
        let at = r.offset();
        let byte = r.u8()?;
        let operator = match byte {
            12 => 0x0c00 | u16::from(r.u8()?),
            0..=26 | 31 | 255 => u16::from(byte),
            _ => {
                // FreeType finds a number cut short at the end harmless.
                let number = &bytes[at..];
                let Some(len) = number_length(number) else {
                    break;
                };
                r.skip(len - 1)?;
                stack.push(Operand::Encoded(number))?;
                continue;
            }
        };
        if stack.len() == N {
            return Err(Error::Invalid("CFF operand stack overflow"));
        }
        match (operator, variations) {
            (op::VSINDEX, Some(_)) => {
                if blended {
                    return Err(Error::Invalid("CFF2 vsindex after blend"));
                }
                vsindex = first(stack.as_slice())?.index()?;
                apply(Entry {
                    operator,
                    operands: stack.as_slice(),
                })?;
            }
            (op::BLEND, Some(scalars)) => {
                let count = stack.pop()?.index()?;
                let scalars = scalars(vsindex)?;
                stack.blend(count, scalars.len(), |default, deltas| {
                    let sum = deltas
                        .iter()
                        .zip(scalars)
                        .fold(default.fixed(0), |sum, (d, &s)| {
                            sum.wrapping_add(mul_fix(d.fixed(0), i64::from(s)))
                        });
                    Operand::Blended(sum as i32)
                })?;
                blended = true;
                continue;
            }
            (op::VSINDEX | op::BLEND, None) => {}
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
    use super::{CFF_DICT_STACK, CFF2_DICT_STACK, Stack, op, parse};
    use triergon_core::{Error, Result, WorkBudget};

    /// Each entry's operator and operands in 16.16, with `scalars` for the
    /// `blend`s of a CFF2 Private DICT, or none for another DICT.
    fn entries<const N: usize>(
        bytes: &[u8],
        scalars: Option<&[i32]>,
    ) -> Result<Vec<(u16, Vec<i64>)>> {
        let mut out = Vec::new();
        let variations = |_| Ok(scalars.unwrap_or_default());
        let variations = scalars.map(|_| &variations as &dyn Fn(usize) -> _);
        parse::<N>(bytes, &mut WorkBudget::new(1000), variations, |entry| {
            out.push((
                entry.operator,
                entry.operands.iter().map(|o| o.fixed(0)).collect(),
            ));
            Ok(())
        })?;
        Ok(out)
    }

    fn cff(bytes: &[u8]) -> Result<Vec<(u16, Vec<i64>)>> {
        entries::<CFF_DICT_STACK>(bytes, None)
    }

    #[test]
    fn numbers_and_operators() {
        // 100 17 (CharStrings); -1000 1131 -0.5 12 7 (FontMatrix, partial).
        let bytes = [239, 17, 254, 124, 250, 255, 30, 0xe0, 0xa5, 0xff, 12, 7];
        let parsed = cff(&bytes).unwrap();
        assert_eq!(parsed[0], (op::CHAR_STRINGS, vec![100 << 16]));
        assert_eq!(
            parsed[1],
            (op::FONT_MATRIX, vec![-1000 << 16, 1131 << 16, -0x8000])
        );
        // 27 is a one-byte number to FreeType, -112; 31 and 255 are
        // operators without a meaning, which take their operands along.
        assert_eq!(cff(&[27, 17]).unwrap(), [(17, vec![-112 << 16])]);
        assert_eq!(
            cff(&[139, 31, 140, 255, 141, 17]).unwrap()[2],
            (17, vec![2 << 16])
        );
        // An escape byte at the end has no operator.
        assert!(cff(&[12]).is_err());
    }

    #[test]
    fn a_number_cut_short_at_the_end_ends_the_dict() {
        // 100 17, then a real without its end, a two-byte and a five-byte
        // integer cut short: FreeType finds them harmless.
        for tail in [&[30, 0x12][..], &[247], &[29, 0, 0]] {
            let bytes = [&[239, 17][..], tail].concat();
            assert_eq!(cff(&bytes).unwrap(), [(17, vec![100 << 16])]);
        }
    }

    #[test]
    fn a_cff_dict_takes_96_operands() {
        let operands = |n| [vec![139; n], vec![17]].concat();
        assert_eq!(cff(&operands(96)).unwrap()[0].1.len(), 96);
        assert_eq!(
            cff(&operands(97)),
            Err(Error::Invalid("CFF operand stack overflow"))
        );
        // Trailing numbers without an operator are not read.
        assert_eq!(cff(&[139; 97]).unwrap(), []);
    }

    #[test]
    fn cff2_blends_at_the_default_instance_in_16_16() {
        // 10 20 (deltas for one value over two regions) 1 blend 19 (Subrs).
        let bytes = [149, 159, 169, 140, 23, 19];
        let private =
            |bytes: &[u8], scalars: &[i32]| entries::<CFF2_DICT_STACK>(bytes, Some(scalars));
        assert_eq!(
            private(&bytes, &[0, 0x10000]).unwrap(),
            [(op::SUBRS, vec![40 << 16])]
        );
        // 0.5 plus 0.25 times 1: 0.75.
        let bytes = [30, 0xa5, 0xff, 30, 0xa2, 0x5f, 140, 23, 19];
        assert_eq!(
            private(&bytes, &[0x10000]).unwrap(),
            [(op::SUBRS, vec![0xc000])]
        );
        // The blend is pushed as a 32-bit number: 30000 + 30000 wraps.
        let bytes = [28, 0x75, 0x30, 28, 0x75, 0x30, 140, 23, 19];
        let wrapped = i64::from((60000_i64 << 16) as i32);
        assert_eq!(
            private(&bytes, &[0x10000]).unwrap(),
            [(op::SUBRS, vec![wrapped])]
        );
        // vsindex reaches the DICT's reader, but not after a blend.
        assert_eq!(private(&[139, 22], &[]).unwrap(), [(op::VSINDEX, vec![0])]);
        assert_eq!(
            private(&[149, 159, 140, 23, 19, 139, 22], &[0x10000]),
            Err(Error::Invalid("CFF2 vsindex after blend"))
        );
        let mut stack = Stack::<i32, 4>::new();
        for v in [1, 2, 3, 4] {
            stack.push(v).unwrap();
        }
        assert!(stack.push(5).is_err());
        assert!(stack.blend(2, 1, |v, d| v + d[0]).is_ok());
        assert_eq!(stack.as_slice(), [4, 6]);
        assert!(stack.blend(3, 0, |v, _| v).is_err());
    }

    #[test]
    fn vsindex_and_blend_apply_only_in_a_cff2_private_dict() {
        // In a CFF2 Top DICT, as in CFF: skipped with their operands.
        let bytes = [139, 22, 149, 159, 140, 23, 239, 17];
        let parsed = entries::<CFF2_DICT_STACK>(&bytes, None).unwrap();
        assert_eq!(parsed, [(op::CHAR_STRINGS, vec![100 << 16])]);
        assert_eq!(cff(&bytes).unwrap(), parsed);
    }
}
