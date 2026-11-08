//! A glyph set digest, HarfBuzz's set digest: a small over-approximation of a
//! set of glyphs. A lookup's digest holds the glyphs its subtables can start
//! at, so that glyphs it cannot apply to are passed over without reading the
//! font; the buffer's holds its glyphs, so that lookups none of them can start
//! are passed over entirely.
//!
//! The digest is exactly HarfBuzz's, false positives included, because one
//! answer depends on them: whether a lookup would substitute a glyph sequence
//! ([`super::would_apply`]) is decided for some contextual subtables by the
//! digest alone.

use super::data::Data;
use super::layout::Lookup;
use crate::work;
use triergon_core::{Result, WorkBudget};

/// Three 64-bit masks over the glyph index shifted by [`SHIFTS`]; a glyph may
/// be in the set when its bit is set in all three.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Digest([u64; 3]);

const SHIFTS: [u32; 3] = [4, 0, 6];

impl Digest {
    pub(crate) const EMPTY: Self = Self([0; 3]);

    const fn bit(glyph: u32, shift: u32) -> u64 {
        1 << ((glyph >> shift) & 63)
    }

    pub(crate) fn add(&mut self, glyph: u32) {
        for (mask, shift) in self.0.iter_mut().zip(SHIFTS) {
            *mask |= Self::bit(glyph, shift);
        }
    }

    fn is_full(&self) -> bool {
        self.0.iter().all(|&mask| mask == u64::MAX)
    }

    /// Adds `first..=last`, as HarfBuzz does even when the ends are reversed.
    fn add_range(&mut self, first: u32, last: u32) {
        for (mask, shift) in self.0.iter_mut().zip(SHIFTS) {
            if (last >> shift).wrapping_sub(first >> shift) >= 63 {
                *mask = u64::MAX;
            } else {
                // The bits from `first`'s to `last`'s, wrapping around.
                let (a, b) = (Self::bit(first, shift), Self::bit(last, shift));
                *mask |= b
                    .wrapping_add(b.wrapping_sub(a))
                    .wrapping_sub(u64::from(b < a));
            }
        }
    }

    fn union(&mut self, other: Self) {
        for (a, b) in self.0.iter_mut().zip(other.0) {
            *a |= b;
        }
    }

    pub(crate) fn intersects(&self, other: &Self) -> bool {
        self.0.iter().zip(other.0).all(|(a, b)| a & b != 0)
    }

    pub(crate) fn may_have(&self, glyph: u32) -> bool {
        self.0
            .iter()
            .zip(SHIFTS)
            .all(|(&mask, shift)| mask & Self::bit(glyph, shift) != 0)
    }

    /// Adds the glyphs of a coverage table, each record charged to `work`;
    /// nothing for a format this cannot read.
    fn add_coverage(&mut self, table: Data<'_>, work: &mut WorkBudget) -> Result<()> {
        match table.u16(0) {
            1 => {
                let count = table.count(2, 4, 2);
                work::charge(work, count)?;
                for i in 0..count {
                    self.add(u32::from(table.u16(4 + 2 * i)));
                }
            }
            2 => {
                for i in 0..table.count(2, 4, 6) {
                    // Once full, the digest cannot change.
                    if self.is_full() {
                        break;
                    }
                    work::charge(work, 1)?;
                    let at = 4 + 6 * i;
                    self.add_range(u32::from(table.u16(at)), u32::from(table.u16(at + 2)));
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The glyphs any subtable of `lookup` can start at, each subtable and
    /// coverage record read charged to `work`.
    pub(crate) fn of_lookup(lookup: &Lookup<'_>, work: &mut WorkBudget) -> Result<Self> {
        let mut digest = Self::EMPTY;
        for (kind, subtable) in lookup.subtables() {
            work::charge(work, 1)?;
            if let Some(coverage) = kind.first_coverage(subtable) {
                let mut own = Self::EMPTY;
                own.add_coverage(coverage, work)?;
                digest.union(own);
            }
        }
        Ok(digest)
    }
}

#[cfg(test)]
mod tests {
    use super::Data;
    use super::Digest;
    use triergon_core::WorkBudget;

    #[test]
    fn digests_hold_what_was_added() {
        let mut d = Digest::EMPTY;
        d.add(5);
        d.add_range(1000, 1100);
        d.add_range(60, 70);
        for g in [5, 1000, 1050, 1100, 60, 64, 70] {
            assert!(d.may_have(g), "{g}");
        }
        assert!(!Digest::EMPTY.may_have(0));
    }

    #[test]
    fn the_digest_holds_its_glyphs_and_some_others() {
        let mut work = WorkBudget::new(10);
        let mut digest = Digest::EMPTY;
        // Coverage format 2: glyphs 10 to 20.
        let coverage = Data(&[0, 2, 0, 1, 0, 10, 0, 20, 0, 0]);
        digest.add_coverage(coverage, &mut work).expect("work");
        assert!((10..=20).all(|g| digest.may_have(g)));
        assert!(!digest.may_have(9));
        // 10 + 64 matches every mask: a false positive HarfBuzz has too.
        assert!(digest.may_have(10 + 64 * 1024));
        let mut wide = Digest::EMPTY;
        wide.add_range(0, 5000);
        assert!(wide.is_full() && wide.may_have(123_456));
        // A range whose ends are reversed fills the masks it wraps around in.
        let mut reversed = Digest::EMPTY;
        reversed.add_range(20, 10);
        assert!((0..64).all(|g| reversed.may_have(g)));
        assert_eq!(work.spent(), 1);
    }
}
