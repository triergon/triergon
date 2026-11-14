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

/// The DICT parser's stack as FreeType sizes it: Top and Font DICTs take
/// `CFF_MAX_STACK_DEPTH` or `CFF2_DEFAULT_STACK` entries, operator included,
/// and a Private DICT one more (`cffload.c`); a CFF2 Private DICT's is the
/// Top DICT's `maxstack` plus one, which FreeType clamps to 513.
pub(crate) const CFF_TOP_STACK: usize = 96;
pub(crate) const CFF_PRIVATE_STACK: usize = 97;
pub(crate) const CFF2_TOP_STACK: usize = 513;
pub(crate) const CFF2_PRIVATE_STACK: usize = CFF2_MAXSTACK + 1;
/// `CFF2_MAX_STACK` and `CFF2_DEFAULT_STACK`, which bound `maxstack` both ways.
pub(crate) const CFF2_MAXSTACK: usize = 513;

/// Which DICT a parse reads, for the operators FreeType knows in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Top,
    /// A Font DICT of the FDArray; in CFF, FreeType reads it as a Top DICT.
    Font,
    Private,
}

/// A DICT operator: one byte, or 12 followed by a second byte as `0x0c00 | byte`.
pub(crate) mod op {
    use super::Kind;
    use crate::cff::Version;

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
    pub(crate) const VSINDEX: u16 = 22;
    pub(crate) const BLEND: u16 = 23;
    pub(crate) const VSTORE: u16 = 24;
    pub(crate) const MAXSTACK: u16 = 25;
    pub(crate) const ENCODING: u16 = 16;
    pub(crate) const CHARSET: u16 = 15;
    pub(crate) const CHAR_STRINGS: u16 = 17;
    pub(crate) const CHARSTRING_TYPE: u16 = 0x0c06;
    pub(crate) const FONT_MATRIX: u16 = 0x0c07;
    pub(crate) const MULTIPLE_MASTER: u16 = 0x0c18;
    pub(crate) const SYNTHETIC_BASE: u16 = 0x0c14;
    pub(crate) const ROS: u16 = 0x0c1e;
    pub(crate) const FD_ARRAY: u16 = 0x0c24;
    pub(crate) const FD_SELECT: u16 = 0x0c25;

    /// The operands FreeType needs for `op` in a DICT of `kind`, by
    /// `cfftoken.h` and the field readers of `cffparse.c`: `None` for an
    /// operator it does not know there, which it skips.
    pub(crate) const fn min_operands(version: Version, kind: Kind, op: u16) -> Option<usize> {
        match (version, kind) {
            (Version::Cff1, Kind::Top | Kind::Font) => match op {
                // Version, Notice, Copyright, FullName, FamilyName, Weight,
                // isFixedPitch to CharstringType, UniqueID, StrokeWidth,
                // charset, Encoding, CharStrings, SyntheticBase,
                // PostScript, and the CID entries from CIDFontVersion on.
                0..=4
                | 13
                | 15..=17
                | 0x0c00..=0x0c06
                | 0x0c08
                | 0x0c14
                | 0x0c15
                | 0x0c1f..=0x0c26 => Some(1),
                PRIVATE => Some(2),
                ROS => Some(3),
                5 => Some(4),
                MULTIPLE_MASTER => Some(5),
                FONT_MATRIX => Some(6),
                _ => None,
            },
            (Version::Cff2, Kind::Top) => match op {
                CHAR_STRINGS | FD_ARRAY | FD_SELECT | VSTORE | MAXSTACK => Some(1),
                FONT_MATRIX => Some(6),
                _ => None,
            },
            (Version::Cff2, Kind::Font) => match op {
                PRIVATE => Some(2),
                FONT_MATRIX => Some(6),
                _ => None,
            },
            (_, Kind::Private) => match op {
                // The blue zones and stem snaps may be empty.
                6..=9 | 0x0c0c | 0x0c0d => Some(0),
                // BlueScale, BlueShift, BlueFuzz, StdHW, StdVW,
                // LanguageGroup, ExpansionFactor and Subrs.
                0x0c09..=0x0c0b | 10 | 11 | 0x0c11 | 0x0c12 | SUBRS => Some(1),
                // ForceBold, ForceBoldThreshold, lenIV, initialRandomSeed,
                // defaultWidthX and nominalWidthX.
                0x0c0e | 0x0c0f | 0x0c10 | 0x0c13 | 20 | 21 if matches!(version, Version::Cff1) => {
                    Some(1)
                }
                VSINDEX if matches!(version, Version::Cff2) => Some(1),
                _ => None,
            },
        }
    }
}

/// The region scalars a CFF2 `blend` weighs its deltas with.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Weights<'s> {
    /// Every one of so many regions weighs zero, as FreeType's blend vector
    /// without a design vector.
    Zero(usize),
    Regions(&'s [Fixed]),
}

impl Weights<'_> {
    const fn len(&self) -> usize {
        match self {
            Self::Zero(len) => *len,
            Self::Regions(scalars) => scalars.len(),
        }
    }

    fn get(&self, i: usize) -> Fixed {
        match self {
            Self::Zero(_) => 0,
            Self::Regions(scalars) => scalars[i],
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
    variations: Option<&dyn Fn(usize) -> Result<Weights<'s>>>,
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
                // FreeType casts it to `FT_UInt` and checks it at `blend`.
                vsindex = first(stack.as_slice())?.unsigned();
                apply(Entry {
                    operator,
                    operands: stack.as_slice(),
                })?;
            }
            (op::BLEND, Some(scalars)) => {
                let weights = scalars(vsindex)?;
                let count = stack.pop()?.unsigned();
                stack.blend(count, weights.len(), |default, deltas| {
                    let sum = deltas
                        .iter()
                        .enumerate()
                        .fold(default.fixed(0), |sum, (i, d)| {
                            sum.wrapping_add(mul_fix(d.fixed(0), i64::from(weights.get(i))))
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

/// The first operand of an entry that needs one.
pub(crate) fn first<'a>(operands: &[Operand<'a>]) -> Result<Operand<'a>> {
    operands
        .first()
        .copied()
        .ok_or(Error::Invalid("CFF DICT operand count"))
}

#[cfg(test)]
mod tests {
    use super::{
        CFF_PRIVATE_STACK, CFF_TOP_STACK, CFF2_PRIVATE_STACK, CFF2_TOP_STACK, Stack, Weights, op,
        parse,
    };
    use triergon_core::{Error, Result, WorkBudget};

    /// Each entry's operator and operands in 16.16, with `scalars` for the
    /// `blend`s of a CFF2 Private DICT, or none for another DICT.
    fn entries<const N: usize>(
        bytes: &[u8],
        scalars: Option<&[i32]>,
    ) -> Result<Vec<(u16, Vec<i64>)>> {
        let mut out = Vec::new();
        let variations = |_| Ok(Weights::Regions(scalars.unwrap_or_default()));
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
        entries::<CFF_TOP_STACK>(bytes, None)
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
    fn dict_stacks_are_sized_as_freetype_sizes_them() {
        let operands = |n| [vec![139; n], vec![17]].concat();
        let overflow = Err(Error::Invalid("CFF operand stack overflow"));
        // A CFF Top or Font DICT takes 95 operands, a Private DICT 96.
        assert_eq!(cff(&operands(95)).unwrap()[0].1.len(), 95);
        assert_eq!(cff(&operands(96)), overflow);
        let private = |n| entries::<CFF_PRIVATE_STACK>(&operands(n), None);
        assert_eq!(private(96).unwrap()[0].1.len(), 96);
        assert_eq!(private(97), overflow);
        // In CFF2, 512 and 513.
        let top = |n| entries::<CFF2_TOP_STACK>(&operands(n), None);
        assert_eq!(top(512).unwrap()[0].1.len(), 512);
        assert_eq!(top(513), overflow);
        let private = |n| entries::<CFF2_PRIVATE_STACK>(&operands(n), Some(&[]));
        assert_eq!(private(513).unwrap()[0].1.len(), 513);
        assert_eq!(private(514), overflow);
        // Trailing numbers without an operator are not read.
        assert_eq!(cff(&[139; 96]).unwrap(), []);
    }

    #[test]
    fn zero_weights_leave_each_blend_its_default() {
        // 10 20 30 (a default and two deltas) 1 blend 19 (Subrs).
        let bytes = [149, 159, 169, 140, 23, 19];
        let mut out = Vec::new();
        let zero = |_| Ok(Weights::Zero(2));
        parse::<CFF2_PRIVATE_STACK>(&bytes, &mut WorkBudget::new(100), Some(&zero), |entry| {
            out.push(entry.operands[0].fixed(0));
            Ok(())
        })
        .unwrap();
        assert_eq!(out, [10 << 16]);
    }

    #[test]
    fn cff2_blends_at_the_default_instance_in_16_16() {
        // 10 20 (deltas for one value over two regions) 1 blend 19 (Subrs).
        let bytes = [149, 159, 169, 140, 23, 19];
        let private =
            |bytes: &[u8], scalars: &[i32]| entries::<CFF2_PRIVATE_STACK>(bytes, Some(scalars));
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
        let parsed = entries::<CFF2_TOP_STACK>(&bytes, None).unwrap();
        assert_eq!(parsed, [(op::CHAR_STRINGS, vec![100 << 16])]);
        assert_eq!(cff(&bytes).unwrap(), parsed);
    }
}
