//! Bidi classes, paired brackets and embedding levels.

use crate::Direction;
use crate::unicode::fribidi;

/// The Bidi_Class of a character (UAX #9, table 4), as FriBidi 1.0.17 assigns it
/// from Unicode 14.0.0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
    /// The class of `c`, as `fribidi_get_bidi_type` gives it.
    #[must_use]
    pub fn of(c: char) -> Self {
        fribidi::bidi_class(u32::from(c))
    }

    /// The strong class of a direction: L for even levels, R for odd ones.
    pub(super) const fn of_level(level: u8) -> Self {
        if level.is_multiple_of(2) {
            Self::L
        } else {
            Self::R
        }
    }

    /// The direction of the class as a level parity: 1 for R, AL, RLE, RLO and
    /// RLI, 0 otherwise.
    pub(super) const fn rtl_bit(self) -> u8 {
        matches!(self, Self::R | Self::Al | Self::Rle | Self::Rlo | Self::Rli) as u8
    }

    /// Strong in FriBidi's sense: the letters and the embedding and override
    /// initiators.
    pub(super) const fn is_strong(self) -> bool {
        matches!(
            self,
            Self::L | Self::R | Self::Al | Self::Lre | Self::Rle | Self::Lro | Self::Rlo
        )
    }

    /// L, R or AL.
    pub(super) const fn is_letter(self) -> bool {
        matches!(self, Self::L | Self::R | Self::Al)
    }

    /// The classes the neutral rules resolve: separators, whitespace, other
    /// neutrals and the isolate formatting characters.
    pub(super) const fn is_neutral(self) -> bool {
        matches!(
            self,
            Self::B | Self::S | Self::Ws | Self::On | Self::Lri | Self::Rli | Self::Fsi | Self::Pdi
        )
    }

    pub(super) const fn is_number(self) -> bool {
        matches!(self, Self::En | Self::An)
    }

    /// ES, ET or CS.
    pub(super) const fn is_number_punctuation(self) -> bool {
        matches!(self, Self::Es | Self::Et | Self::Cs)
    }

    /// LRE, RLE, LRO, RLO or PDF.
    pub(super) const fn is_embedding(self) -> bool {
        matches!(
            self,
            Self::Lre | Self::Rle | Self::Lro | Self::Rlo | Self::Pdf
        )
    }

    /// LRI, RLI, FSI or PDI.
    pub(super) const fn is_isolate(self) -> bool {
        matches!(self, Self::Lri | Self::Rli | Self::Fsi | Self::Pdi)
    }

    /// What rule X9 removes: the embedding and override characters and BN.
    pub(super) const fn is_removed(self) -> bool {
        self.is_embedding() || matches!(self, Self::Bn)
    }

    /// B or S.
    pub(super) const fn is_separator(self) -> bool {
        matches!(self, Self::B | Self::S)
    }

    /// EN and AN as R, for the neutral rules.
    pub(super) const fn numbers_as_r(self) -> Self {
        if self.is_number() { Self::R } else { self }
    }
}

/// A character's paired-bracket property (BidiBrackets.txt, with FriBidi's
/// compatibility folding): which brackets pair with it and whether it opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Bracket {
    /// The identity both brackets of a pair share (see [`fribidi::bracket`]).
    pair: u32,
    opens: bool,
}

impl Bracket {
    pub(crate) const fn new(pair: u32, opens: bool) -> Self {
        Self { pair, opens }
    }

    /// The bracket `c` is, if any, as `fribidi_get_bracket` gives it.
    #[must_use]
    pub fn of(c: char) -> Option<Self> {
        fribidi::bracket(u32::from(c))
    }

    /// Whether this is an opening bracket.
    #[must_use]
    pub const fn opens(self) -> bool {
        self.opens
    }

    /// Whether `self` and `other` are brackets of one pair (either may open).
    #[must_use]
    pub const fn pairs_with(self, other: Self) -> bool {
        self.pair == other.pair
    }

    /// The identity both brackets of a pair share: the opening bracket, after
    /// FriBidi's folding.
    pub(crate) const fn pair(self) -> u32 {
        self.pair
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
        self.0 % 2 == 1
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
    /// Left-to-right (`FRIBIDI_PAR_LTR`).
    LeftToRight,
    /// Right-to-left (`FRIBIDI_PAR_RTL`).
    RightToLeft,
    /// The direction of the first strong letter outside isolates, else
    /// left-to-right (`FRIBIDI_PAR_ON`, rules P2 and P3).
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
    fn classes_follow_fribidi() {
        assert_eq!(BidiClass::of('a'), BidiClass::L);
        assert_eq!(BidiClass::of('\u{05D0}'), BidiClass::R);
        assert_eq!(BidiClass::of('\u{0627}'), BidiClass::Al);
        assert_eq!(BidiClass::of('\u{2067}'), BidiClass::Rli);
        assert_eq!(BidiClass::of('\u{2029}'), BidiClass::B);
        assert_eq!(BidiClass::of('\u{FDD0}'), BidiClass::Bn);
    }

    #[test]
    fn brackets_pair_by_identity() {
        let open = Bracket::of('(').expect("bracket");
        let close = Bracket::of(')').expect("bracket");
        assert!(open.opens() && !close.opens());
        assert!(open.pairs_with(close));
        assert!(Bracket::of('\u{FF09}').expect("bracket").pairs_with(open));
        assert!(!Bracket::of(']').expect("bracket").pairs_with(open));
        assert_eq!(Bracket::of('a'), None);
    }

    #[test]
    fn levels() {
        assert!(Level(3).is_rtl());
        assert_eq!(Level(2).direction(), Direction::LeftToRight);
        assert_eq!(Level::of(Direction::RightToLeft), Level(1));
    }
}
