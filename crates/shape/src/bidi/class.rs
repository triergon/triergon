//! Bidi classes, paired brackets, joining types and embedding levels.

use crate::Direction;
use crate::unicode::bidi as data;

/// The Bidi_Class of a character (UAX #9, table 4), from Unicode 14.0.0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum BidiClass {
    /// Left-to-right.
    L,
    /// Right-to-left.
    R,
    /// Arabic letter.
    Al,
    /// European number.
    En,
    /// European separator.
    Es,
    /// European terminator.
    Et,
    /// Arabic number.
    An,
    /// Common separator.
    Cs,
    /// Nonspacing mark.
    Nsm,
    /// Boundary neutral.
    Bn,
    /// Paragraph separator.
    B,
    /// Segment separator.
    S,
    /// Whitespace.
    Ws,
    /// Other neutral.
    On,
    /// Left-to-right embedding.
    Lre,
    /// Left-to-right override.
    Lro,
    /// Right-to-left embedding.
    Rle,
    /// Right-to-left override.
    Rlo,
    /// Pop directional format.
    Pdf,
    /// Left-to-right isolate.
    Lri,
    /// Right-to-left isolate.
    Rli,
    /// First strong isolate.
    Fsi,
    /// Pop directional isolate.
    Pdi,
}

impl BidiClass {
    /// Every class, in the order of their discriminants (the generated table's
    /// values).
    pub(crate) const ALL: [Self; 23] = [
        Self::L,
        Self::R,
        Self::Al,
        Self::En,
        Self::Es,
        Self::Et,
        Self::An,
        Self::Cs,
        Self::Nsm,
        Self::Bn,
        Self::B,
        Self::S,
        Self::Ws,
        Self::On,
        Self::Lre,
        Self::Lro,
        Self::Rle,
        Self::Rlo,
        Self::Pdf,
        Self::Lri,
        Self::Rli,
        Self::Fsi,
        Self::Pdi,
    ];

    /// The class of `c`. Unassigned code points have FriBidi's defaults: R in
    /// U+0590..U+08FF, U+FB1D..U+FB4F and U+10800..U+10FFF, AL in
    /// U+0600..U+07BF, U+FB50..U+FDCF, U+FDF0..U+FDFF and U+FE70..U+FEFF, BN for
    /// noncharacters and default ignorables, and L elsewhere. (Unicode gives
    /// the unassigned code points of more right-to-left blocks R or AL, and
    /// those of the Currency Symbols block ET.)
    #[must_use]
    pub fn of(c: char) -> Self {
        data::bidi_class(u32::from(c))
    }

    /// The strong class of the direction of `level`: L when it is even, R when
    /// it is odd.
    pub(super) const fn of_level(level: u8) -> Self {
        if level & 1 == 0 { Self::L } else { Self::R }
    }

    /// The characters rule X9 removes: BN and the embedding, override and PDF
    /// characters.
    pub(super) const fn is_removed_by_x9(self) -> bool {
        matches!(
            self,
            Self::Bn | Self::Lre | Self::Lro | Self::Rle | Self::Rlo | Self::Pdf
        )
    }

    /// LRI, RLI and FSI.
    pub(super) const fn is_isolate_initiator(self) -> bool {
        matches!(self, Self::Lri | Self::Rli | Self::Fsi)
    }

    /// The isolate initiators and PDI.
    pub(super) const fn is_isolate_control(self) -> bool {
        matches!(self, Self::Lri | Self::Rli | Self::Fsi | Self::Pdi)
    }

    /// The neutral and isolate formatting classes rules N1 and N2 resolve (NI).
    pub(super) const fn is_neutral_or_isolate(self) -> bool {
        matches!(
            self,
            Self::B | Self::S | Self::Ws | Self::On | Self::Lri | Self::Rli | Self::Fsi | Self::Pdi
        )
    }

    /// The direction a strong class or number takes in rules N0 to N2, where
    /// numbers count as R: `Some(false)` for L, `Some(true)` for R, EN and AN.
    /// RLE and RLO, which a sequence holds only after a paragraph separator
    /// (see `paragraph.rs`), count as R.
    pub(super) const fn strong_rtl(self) -> Option<bool> {
        match self {
            Self::L => Some(false),
            Self::R | Self::Al | Self::En | Self::An | Self::Rle | Self::Rlo => Some(true),
            _ => None,
        }
    }
}

/// Sets every class of `types` to `class`: one directly, more as a block.
#[inline]
pub(super) fn fill(types: &mut [BidiClass], class: BidiClass) {
    match types {
        [one] => *one = class,
        _ => types.fill(class),
    }
}

/// A character's paired-bracket property (BidiBrackets.txt): which brackets pair
/// with it and whether it opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Bracket {
    /// The identity both brackets of a pair share.
    pair: char,
    opens: bool,
}

impl Bracket {
    pub(crate) const fn new(pair: char, opens: bool) -> Self {
        Self { pair, opens }
    }

    /// The bracket `c` is, if any.
    #[must_use]
    pub fn of(c: char) -> Option<Self> {
        data::bracket(u32::from(c))
    }

    /// Whether this is an opening bracket.
    #[must_use]
    pub const fn opens(self) -> bool {
        self.opens
    }

    /// Whether `self` and `other` are brackets of one pair (either may open).
    #[must_use]
    pub const fn pairs_with(self, other: Self) -> bool {
        self.pair as u32 == other.pair as u32 && self.opens != other.opens
    }

    /// The identity both brackets of a pair share: the opening bracket, or the
    /// first character of the opening bracket's decomposition, canonical or
    /// compatibility. U+2329 and U+232A pair as U+3008, like U+3008 and U+3009,
    /// and U+FF08 and U+FF09 as U+0028, like U+0028 and U+0029.
    #[must_use]
    pub const fn pair(self) -> char {
        self.pair
    }
}

/// The joining type of a character (ArabicShaping.txt), from Unicode 14.0.0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum JoiningType {
    /// U: does not join.
    NonJoining,
    /// R: joins the preceding character (in logical order) only.
    RightJoining,
    /// D: joins both sides.
    DualJoining,
    /// C: causes joining on both sides without changing form.
    JoinCausing,
    /// T: marks and format characters, which joining passes over.
    Transparent,
    /// L: joins the following character only.
    LeftJoining,
    /// G: the control characters of class BN, which joining passes over.
    Ignored,
}

impl JoiningType {
    /// Every joining type, in the order of their discriminants (the generated
    /// table's values).
    pub(crate) const ALL: [Self; 7] = [
        Self::NonJoining,
        Self::RightJoining,
        Self::DualJoining,
        Self::JoinCausing,
        Self::Transparent,
        Self::LeftJoining,
        Self::Ignored,
    ];

    /// The joining type of `c`: ArabicShaping.txt's, else transparent for marks
    /// (Mn, Me) and format characters (Cf), ignored for the other characters of
    /// class BN (controls), and non-joining.
    #[must_use]
    pub fn of(c: char) -> Self {
        data::joining_type(u32::from(c))
    }

    /// Whether a character of this type joins the character after it.
    pub(super) const fn joins_following(self) -> bool {
        matches!(
            self,
            Self::DualJoining | Self::JoinCausing | Self::LeftJoining
        )
    }

    /// Whether a character of this type joins the character before it.
    pub(super) const fn joins_preceding(self) -> bool {
        matches!(
            self,
            Self::DualJoining | Self::JoinCausing | Self::RightJoining
        )
    }
}

/// A resolved embedding level: even levels are left-to-right, odd levels
/// right-to-left.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Level(pub(super) u8);

impl Level {
    /// The level of a direction's paragraph: 0 or 1.
    #[must_use]
    pub const fn of(direction: Direction) -> Self {
        match direction {
            Direction::LeftToRight => Self(0),
            Direction::RightToLeft => Self(1),
        }
    }

    /// The level number, at most 126.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Whether the level is odd.
    #[must_use]
    pub const fn is_rtl(self) -> bool {
        self.0 & 1 == 1
    }

    /// The direction of text at this level.
    #[must_use]
    pub const fn direction(self) -> Direction {
        if self.is_rtl() {
            Direction::RightToLeft
        } else {
            Direction::LeftToRight
        }
    }
}

/// The direction a paragraph is resolved with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BaseDirection {
    /// Left-to-right.
    LeftToRight,
    /// Right-to-left.
    RightToLeft,
    /// The direction of the first strong letter outside isolates, else
    /// left-to-right (rules P2 and P3).
    Auto,
}

impl From<Direction> for BaseDirection {
    fn from(direction: Direction) -> Self {
        match direction {
            Direction::LeftToRight => Self::LeftToRight,
            Direction::RightToLeft => Self::RightToLeft,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        assert_eq!(Level::of(Direction::RightToLeft).get(), 1);
        assert!(Level(3).is_rtl());
        assert_eq!(Level(4).direction(), Direction::LeftToRight);
        assert_eq!(BidiClass::of_level(5), BidiClass::R);
    }

    #[test]
    fn class_groups() {
        assert!(BidiClass::Pdf.is_removed_by_x9());
        assert!(!BidiClass::Pdi.is_removed_by_x9());
        assert!(BidiClass::Pdi.is_neutral_or_isolate());
        assert_eq!(BidiClass::An.strong_rtl(), Some(true));
        assert_eq!(BidiClass::On.strong_rtl(), None);
        assert!(JoiningType::JoinCausing.joins_following());
        assert!(!JoiningType::RightJoining.joins_following());
    }
}
