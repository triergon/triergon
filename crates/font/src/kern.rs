//! Pair kerning from the OpenType `kern` table (Microsoft version 0, format 0).
//!
//! Horizontal, non-minimum format 0 subtables are used; their values add up unless
//! a subtable sets the override bit, which replaces the sum so far. A pair count
//! that exceeds its subtable is cut to the pairs present. At most 32 subtables are
//! read. These rules, and searching unordered pairs linearly, follow FreeType.
//! Apple's version 1 table and the other formats are not read; GPOS kerning is
//! shaping.

use crate::GlyphId;
use crate::read::{Reader, slice, u16_at, u32_at};
use crate::sfnt::Table;
use crate::work::charge;
use triergon_core::{Budget, BudgetVec, Result, WorkBudget};

const MAX_SUBTABLES: u16 = 32;
const HORIZONTAL: u16 = 0x0001;
const MINIMUM: u16 = 0x0002;
const OVERRIDE: u16 = 0x0008;

#[derive(Clone, Copy, Debug)]
struct Subtable {
    /// The file offset of the first pair.
    pairs: usize,
    count: usize,
    ordered: bool,
    replace: bool,
}

#[derive(Debug)]
pub(crate) struct Kern {
    subtables: BudgetVec<Subtable>,
}

impl Kern {
    pub(crate) fn new(budget: &Budget) -> Self {
        Self {
            subtables: BudgetVec::new(budget),
        }
    }

    /// Reads the subtable headers and checks pair order. A table too short for its
    /// header has no kerning.
    pub(crate) fn parse(
        data: &[u8],
        table: Table,
        budget: &Budget,
        work: &mut WorkBudget,
    ) -> Result<Self> {
        let mut kern = Self::new(budget);
        let bytes = slice(data, table.offset, table.len)?;
        let Some(version) = u16_at(bytes, 0) else {
            return Ok(kern);
        };
        if version != 0 {
            return Ok(kern);
        }
        let count = u16_at(bytes, 2).unwrap_or(0).min(MAX_SUBTABLES);
        let mut at = 4;
        for _ in 0..count {
            let mut r = Reader::at(bytes, at)?;
            let (Ok(_), Ok(length), Ok(coverage)) = (r.u16(), r.u16(), r.u16()) else {
                break;
            };
            let length = usize::from(length);
            if length <= 14 {
                break;
            }
            let next = (at + length).min(bytes.len());
            if coverage >> 8 == 0
                && coverage & (HORIZONTAL | MINIMUM) == HORIZONTAL
                && at + 14 <= next
            {
                let declared = usize::from(u16_at(bytes, at + 6).unwrap_or(0));
                let first = at + 14;
                let count = declared.min((next - first) / 6);
                charge(work, count)?;
                let ordered = (1..count).all(|i| {
                    let key = |n: usize| u32_at(bytes, first + 6 * n);
                    key(i - 1) <= key(i)
                });
                let subtable = Subtable {
                    pairs: table.offset + first,
                    count,
                    ordered,
                    replace: coverage & OVERRIDE != 0,
                };
                kern.subtables.try_push(subtable).map_err(|e| e.error)?;
            }
            at = next;
        }
        Ok(kern)
    }

    /// Every (left, right) pair the used subtables list, in table order, with
    /// repeats. [`Self::pair`] gives the combined value of each.
    pub(crate) fn pairs<'a>(
        &'a self,
        data: &'a [u8],
    ) -> impl Iterator<Item = (GlyphId, GlyphId)> + 'a {
        self.subtables.as_slice().iter().flat_map(move |subtable| {
            (0..subtable.count).filter_map(move |i| {
                let at = subtable.pairs + 6 * i;
                Some((
                    GlyphId(u32::from(u16_at(data, at)?)),
                    GlyphId(u32::from(u16_at(data, at + 2)?)),
                ))
            })
        })
    }

    /// The kerning between `left` and `right` in font units.
    pub(crate) fn pair(&self, data: &[u8], left: GlyphId, right: GlyphId) -> i32 {
        let (Ok(left), Ok(right)) = (u16::try_from(left.0), u16::try_from(right.0)) else {
            return 0;
        };
        let key = u32::from(left) << 16 | u32::from(right);
        let mut total = 0;
        for subtable in self.subtables.as_slice() {
            let pair = |i: usize| -> Option<(u32, i16)> {
                let at = subtable.pairs + 6 * i;
                Some((u32_at(data, at)?, u16_at(data, at + 4)?.cast_signed()))
            };
            let found = if subtable.ordered {
                let (mut low, mut high) = (0, subtable.count);
                let mut found = None;
                while low < high {
                    let mid = low + (high - low) / 2;
                    let Some((candidate, value)) = pair(mid) else {
                        break;
                    };
                    if candidate == key {
                        found = Some(value);
                        break;
                    }
                    if candidate < key {
                        low = mid + 1;
                    } else {
                        high = mid;
                    }
                }
                found
            } else {
                (0..subtable.count).find_map(|i| pair(i).filter(|&(k, _)| k == key).map(|(_, v)| v))
            };
            if let Some(value) = found {
                total = if subtable.replace {
                    i32::from(value)
                } else {
                    total + i32::from(value)
                };
            }
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::Kern;
    use crate::GlyphId;
    use crate::sfnt::Table;
    use triergon_core::{Budget, WorkBudget};

    fn subtable(coverage: u16, pairs: &[(u16, u16, i16)]) -> Vec<u8> {
        let mut data = Vec::new();
        for v in [
            0,
            14 + 6 * pairs.len() as u16,
            coverage,
            pairs.len() as u16,
            0,
            0,
            0,
        ] {
            data.extend(v.to_be_bytes());
        }
        for &(l, r, v) in pairs {
            data.extend(l.to_be_bytes());
            data.extend(r.to_be_bytes());
            data.extend(v.to_be_bytes());
        }
        data
    }

    #[test]
    fn subtables_add_override_and_filter() {
        let mut data = vec![0, 0, 0, 4];
        data.extend(subtable(1, &[(1, 2, -50), (3, 4, 10)]));
        data.extend(subtable(1, &[(3, 4, 5), (1, 2, -7)])); // unordered
        data.extend(subtable(3, &[(1, 2, 1000)])); // minimum: ignored
        data.extend(subtable(9, &[(3, 4, 1)])); // override
        let budget = Budget::new(1 << 16).unwrap();
        let table = Table {
            offset: 0,
            len: data.len(),
        };
        let kern = Kern::parse(&data, table, &budget, &mut WorkBudget::new(100)).unwrap();
        assert_eq!(kern.pair(&data, GlyphId(1), GlyphId(2)), -57);
        assert_eq!(kern.pair(&data, GlyphId(3), GlyphId(4)), 1);
        assert_eq!(kern.pair(&data, GlyphId(2), GlyphId(1)), 0);
        assert_eq!(kern.pairs(&data).count(), 5);
        let apple = [0, 1, 0, 0, 0, 0, 0, 0];
        let table = Table { offset: 0, len: 8 };
        let kern = Kern::parse(&apple, table, &budget, &mut WorkBudget::new(100)).unwrap();
        assert_eq!(kern.pair(&apple, GlyphId(1), GlyphId(2)), 0);
    }
}
