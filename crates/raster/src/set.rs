//! Small sets of the members of an enum, held as bits.

use std::fmt;
use std::marker::PhantomData;

/// An enum whose values a [`Set`] holds; `bit` is a distinct bit below 8.
pub(crate) trait Member: Copy {
    fn bit(self) -> u8;
}

/// A set of members of `T`.
pub(crate) struct Set<T> {
    bits: u8,
    member: PhantomData<T>,
}

impl<T> Clone for Set<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Set<T> {}

impl<T> PartialEq for Set<T> {
    fn eq(&self, other: &Self) -> bool {
        self.bits == other.bits
    }
}

impl<T> Eq for Set<T> {}

impl<T> Default for Set<T> {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl<T> fmt::Debug for Set<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Set({:#010b})", self.bits)
    }
}

impl<T> Set<T> {
    pub(crate) const EMPTY: Self = Self {
        bits: 0,
        member: PhantomData,
    };

    const fn from_bits(bits: u8) -> Self {
        Self {
            bits,
            member: PhantomData,
        }
    }

    #[inline(always)]
    pub(crate) const fn is_empty(self) -> bool {
        self.bits == 0
    }

    #[inline(always)]
    pub(crate) const fn union(self, other: Self) -> Self {
        Self::from_bits(self.bits | other.bits)
    }

    #[inline(always)]
    pub(crate) const fn intersection(self, other: Self) -> Self {
        Self::from_bits(self.bits & other.bits)
    }

    /// The members of `self` that are not in `other`.
    #[inline(always)]
    pub(crate) const fn minus(self, other: Self) -> Self {
        Self::from_bits(self.bits & !other.bits)
    }

    /// The members in exactly one of the two sets.
    #[inline(always)]
    pub(crate) const fn symmetric_difference(self, other: Self) -> Self {
        Self::from_bits(self.bits ^ other.bits)
    }
}

impl<T: Member> Set<T> {
    #[inline(always)]
    pub(crate) fn of<const N: usize>(members: [T; N]) -> Self {
        Self::from_bits(members.iter().fold(0, |bits, m| bits | m.bit()))
    }

    #[inline(always)]
    pub(crate) fn contains(self, member: T) -> bool {
        self.bits & member.bit() != 0
    }

    #[inline(always)]
    pub(crate) fn with(self, member: T) -> Self {
        Self::from_bits(self.bits | member.bit())
    }

    #[inline(always)]
    pub(crate) fn without(self, member: T) -> Self {
        Self::from_bits(self.bits & !member.bit())
    }

    #[inline(always)]
    pub(crate) fn toggled(self, member: T) -> Self {
        Self::from_bits(self.bits ^ member.bit())
    }

    /// Adds `member` when `on`, removes it otherwise.
    #[inline(always)]
    pub(crate) fn set(&mut self, member: T, on: bool) {
        *self = if on {
            self.with(member)
        } else {
            self.without(member)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::{Member, Set};

    #[derive(Clone, Copy, Debug)]
    enum Colour {
        Red,
        Green,
    }

    impl Member for Colour {
        fn bit(self) -> u8 {
            1 << self as u8
        }
    }

    #[test]
    fn sets_combine_by_member() {
        let both = Set::of([Colour::Red, Colour::Green]);
        assert!(both.contains(Colour::Green));
        assert_eq!(both.without(Colour::Red), Set::of([Colour::Green]));
        assert_eq!(both.toggled(Colour::Green).toggled(Colour::Green), both);
        assert!(both.minus(both).is_empty());
    }
}
