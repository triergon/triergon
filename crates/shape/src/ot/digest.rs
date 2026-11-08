//! A lookup's glyph digest: a small over-approximation of the glyphs its
//! subtables can start at, so that glyphs it cannot apply to are passed over
//! without reading the font.

use super::data::Data;
use super::layout::Lookup;

/// Three 64-bit masks over the glyph index at different bit positions; a glyph
/// is possibly covered when its bit is set in all three.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Digest([u64; 3]);

const SHIFTS: [u32; 3] = [0, 4, 9];

impl Digest {
    pub(crate) const EMPTY: Self = Self([0; 3]);

    pub(crate) fn add(&mut self, glyph: u32) {
        for (mask, shift) in self.0.iter_mut().zip(SHIFTS) {
            *mask |= 1 << ((glyph >> shift) & 63);
        }
    }

    fn add_range(&mut self, first: u32, last: u32) {
        for (mask, shift) in self.0.iter_mut().zip(SHIFTS) {
            let (a, b) = (first >> shift, last >> shift);
            if b - a >= 63 {
                *mask = u64::MAX;
            } else {
                let (a, b) = (a & 63, b & 63);
                *mask |= if a <= b {
                    (u64::MAX >> (63 - (b - a))) << a
                } else {
                    // The range wraps: everything but the gap between its ends.
                    !(((1_u64 << (a - b - 1)) - 1) << (b + 1))
                };
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
            .all(|(mask, shift)| mask & (1 << ((glyph >> shift) & 63)) != 0)
    }

    /// The glyphs of a coverage table; everything for one this cannot read.
    fn coverage(table: Data<'_>) -> Self {
        let mut digest = Self::EMPTY;
        match table.u16(0) {
            1 => {
                for i in 0..table.count(2, 4, 2) {
                    digest.add(u32::from(table.u16(4 + 2 * i)));
                }
            }
            2 => {
                for i in 0..table.count(2, 4, 6) {
                    let at = 4 + 6 * i;
                    let (first, last) = (u32::from(table.u16(at)), u32::from(table.u16(at + 2)));
                    if first <= last {
                        digest.add_range(first, last);
                    }
                }
            }
            _ => {}
        }
        digest
    }

    /// The glyphs any subtable of `lookup` can apply at.
    pub(crate) fn of_lookup(lookup: &Lookup<'_>, extension: u16) -> Self {
        let mut digest = Self::EMPTY;
        for subtable in lookup.subtables() {
            digest.union(Self::of_subtable(lookup.kind, subtable, extension));
        }
        digest
    }

    /// The first-glyph coverage of one subtable: at offset 2 for every type and
    /// format but the format 3 contextual ones, whose first input coverage comes
    /// after a count (or the backtrack).
    fn of_subtable(kind: u16, subtable: Data<'_>, extension: u16) -> Self {
        let format = subtable.u16(0);
        if kind == extension {
            let inner = subtable.u16(2);
            return if format == 1 && inner != extension {
                Self::of_subtable(inner, subtable.offset32(4), extension)
            } else {
                Self::EMPTY
            };
        }
        // GSUB 5/6 and GPOS 7/8 share their layout.
        match (extension.checked_sub(kind), format) {
            // Context format 3: glyph count, lookup count, then the coverages.
            (Some(2), 3) => Self::coverage(subtable.offset16(6)),
            // Chain context format 3: the input coverages follow the backtrack.
            (Some(1), 3) => {
                let backtrack = usize::from(subtable.u16(2));
                Self::coverage(subtable.offset16(4 + 2 * backtrack + 2))
            }
            _ => Self::coverage(subtable.offset16(2)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Digest;

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
}
