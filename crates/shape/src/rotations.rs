//! The rotation decisions of libass's glyph metrics cache for `@` fonts.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::Hash;
use triergon_core::{Budget, Error, Reservation, Result};
use triergon_font::GlyphId;

/// libass's first code point an `@` font draws rotated
/// (`VERTICAL_LOWER_BOUND`).
const VERTICAL_LOWER_BOUND: u32 = 0x2F1;

/// Whether an `@` font draws a glyph found through `code` turned by 90°
/// (`DECO_ROTATE` in `ass_render.c`), and whether that lookup rotates the
/// glyph's metrics when it is the glyph's first: code points from U+02F1.
pub const fn rotates(code: u32) -> bool {
    code >= VERTICAL_LOWER_BOUND
}

/// The rotation decisions of libass's glyph metrics cache for `@` fonts
/// (`get_cached_metrics` and `ass_glyph_metrics_construct` in
/// `ass_shaper.c`): whether the default-load metrics the cache keeps for a
/// glyph of face `K` are rotated, their horizontal advance replaced by the
/// vertical one.
///
/// The first lookup that loads a glyph decides: a character lookup through a
/// code point that [`rotates`] rotates it, and any other lookup (another
/// character, or an advance or extents asked of a glyph no character lookup
/// returned, such as one a substitution made) keeps it horizontal. libass
/// keys the cache by font, face, size and glyph and never cuts it, so its
/// owner keys the faces as libass does and keeps the decisions as long as it
/// shapes. Their storage is charged to a budget before it grows; a decision
/// whose storage cannot be charged is not kept, and the shaping call that
/// made it fails with the charge's error.
#[derive(Debug)]
pub struct GlyphRotations<K> {
    decisions: HashMap<(K, GlyphId), bool>,
    charge: Reservation,
    /// The failed charge of the current shaping call.
    refused: Option<Error>,
}

impl<K: Copy + Eq + Hash> GlyphRotations<K> {
    /// No decisions yet, their storage charged to `budget`.
    pub fn new(budget: &Budget) -> Self {
        Self {
            decisions: HashMap::new(),
            charge: Reservation::empty(budget),
            refused: None,
        }
    }

    /// The decision kept for `glyph` of face `key`, if a lookup made one.
    pub fn get(&self, key: K, glyph: GlyphId) -> Option<bool> {
        self.decisions.get(&(key, glyph)).copied()
    }

    /// How many decisions are kept.
    pub fn len(&self) -> usize {
        self.decisions.len()
    }

    /// Whether no decision is kept.
    pub fn is_empty(&self) -> bool {
        self.decisions.is_empty()
    }

    /// The decision kept for `glyph` of face `key`, else `rotate`, kept when
    /// its storage can be charged.
    fn decide(&mut self, key: K, glyph: GlyphId, rotate: bool) -> bool {
        if let Some(&rotated) = self.decisions.get(&(key, glyph)) {
            return rotated;
        }
        if self.refused.is_none()
            && let Err(e) = self.reserve()
        {
            self.refused = Some(e);
        }
        if self.refused.is_none() {
            self.decisions.insert((key, glyph), rotate);
        }
        rotate
    }

    /// Room for one more decision, charged before the map grows.
    fn reserve(&mut self) -> Result<()> {
        let capacity = self.decisions.capacity();
        if self.decisions.len() < capacity {
            return Ok(());
        }
        // The map at most doubles, and holds at least 3 when it first grows.
        let grown = capacity.saturating_mul(2).saturating_add(4);
        self.charge.resize_to(Self::bytes(grown))?;
        self.decisions
            .try_reserve(1)
            .map_err(|_| Error::Allocation)?;
        self.charge
            .resize_to(Self::bytes(self.decisions.capacity()))
    }

    /// The bytes of a map of `capacity`: a slot and a control byte per bucket,
    /// an eighth of the buckets spare, and a group of trailing control bytes.
    fn bytes(capacity: usize) -> usize {
        capacity
            .saturating_add(1)
            .saturating_mul(size_of::<((K, GlyphId), bool)>() + 1)
            .saturating_mul(8)
            / 7
            + 16
    }
}

/// The decisions of one face at one size, which [`SizedMetrics::rotating`]
/// reads and records.
///
/// [`SizedMetrics::rotating`]: crate::SizedMetrics::rotating
#[derive(Debug)]
pub struct FaceRotations<'a, K> {
    rotations: &'a RefCell<GlyphRotations<K>>,
    key: K,
}

impl<'a, K> FaceRotations<'a, K> {
    /// The decisions of face `key` among `rotations`.
    pub const fn new(rotations: &'a RefCell<GlyphRotations<K>>, key: K) -> Self {
        Self { rotations, key }
    }
}

/// What the font functions ask of a face's decisions, whatever its key.
pub(crate) trait Decide {
    /// Records that a lookup loaded `glyph` (through the character `c`, or
    /// `None` for another lookup) and returns whether its cached metrics are
    /// rotated: the decision of its first lookup.
    fn decide(&self, glyph: GlyphId, c: Option<char>) -> bool;
    /// The failed charge of the shaping call, taken.
    fn take_refusal(&self) -> Option<Error>;
}

impl<K: Copy + Eq + Hash> Decide for FaceRotations<'_, K> {
    fn decide(&self, glyph: GlyphId, c: Option<char>) -> bool {
        let rotate = c.is_some_and(|c| rotates(u32::from(c)));
        self.rotations.borrow_mut().decide(self.key, glyph, rotate)
    }

    fn take_refusal(&self) -> Option<Error> {
        self.rotations.borrow_mut().refused.take()
    }
}

#[cfg(test)]
mod tests {
    use super::{Decide, FaceRotations, GlyphRotations, rotates};
    use std::cell::RefCell;
    use triergon_core::{Budget, Error};
    use triergon_font::GlyphId;

    #[test]
    fn the_first_lookup_of_a_glyph_decides_for_its_face() {
        let rotations = RefCell::new(GlyphRotations::new(&Budget::new(1 << 20).unwrap()));
        let (regular, bold) = (
            FaceRotations::new(&rotations, 1_u8),
            FaceRotations::new(&rotations, 2),
        );
        // An advance first keeps a glyph horizontal, also when a rotating
        // character finds it later.
        assert!(!regular.decide(GlyphId(5), None));
        assert!(!regular.decide(GlyphId(5), Some('\u{fe11}')));
        // Another face decides on its own.
        assert!(bold.decide(GlyphId(5), Some('\u{fe11}')));
        assert!(bold.decide(GlyphId(5), None));
        assert!(!bold.decide(GlyphId(6), Some('A')));
        assert!(!bold.decide(GlyphId(6), Some('\u{3001}')));
        let kept = rotations.borrow();
        assert_eq!(
            [(1, 5), (2, 5), (2, 6), (1, 6)].map(|(k, g)| kept.get(k, GlyphId(g))),
            [Some(false), Some(true), Some(false), None]
        );
        assert_eq!(kept.len(), 3);
        assert!(!rotates(0x2f0) && rotates(0x2f1));
    }

    #[test]
    fn a_decision_whose_storage_cannot_be_charged_is_refused() {
        let budget = Budget::new(Budget::control_storage_bytes() + 64).unwrap();
        let rotations = RefCell::new(GlyphRotations::new(&budget));
        let face = FaceRotations::new(&rotations, 0_u32);
        // The first lookup's answer stands, but nothing is kept and the call
        // is refused once.
        assert!(face.decide(GlyphId(1), Some('\u{3042}')));
        assert!(!face.decide(GlyphId(1), None));
        assert!(rotations.borrow().is_empty());
        assert!(matches!(face.take_refusal(), Some(Error::Limit { .. })));
        assert_eq!(face.take_refusal(), None);
    }
}
