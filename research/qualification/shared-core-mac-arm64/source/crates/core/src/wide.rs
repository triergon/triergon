//! Fixed-width scratch arithmetic for exact clock operations. No heap allocation.
//! The normal clock path stays in u128; these operations handle large products
//! whose reduced or rounded result may still fit the public clock representation.
use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Wide([u64; 4]);

impl Wide {
    pub(crate) const ZERO: Self = Self([0; 4]);

    pub(crate) fn from_u128(value: u128) -> Self {
        Self([value as u64, (value >> 64) as u64, 0, 0])
    }
    pub(crate) fn to_u128(self) -> Option<u128> {
        (self.0[2] == 0 && self.0[3] == 0)
            .then(|| u128::from(self.0[0]) | (u128::from(self.0[1]) << 64))
    }
    pub(crate) fn product(a: u128, b: u128) -> Self {
        let a = [a as u64, (a >> 64) as u64];
        let b = [b as u64, (b >> 64) as u64];
        let mut out = [0; 4];
        for (i, &a) in a.iter().enumerate() {
            let mut carry = 0;
            for (j, &b) in b.iter().enumerate() {
                // A limb product, an existing limb and a limb carry fit u128.
                let value = u128::from(a) * u128::from(b) + u128::from(out[i + j]) + carry;
                out[i + j] = value as u64;
                carry = value >> 64;
            }
            out[i + 2] = carry as u64;
        }
        Self(out)
    }
    pub(crate) fn checked_add(self, other: Self) -> Option<Self> {
        let mut out = [0; 4];
        let mut carry = false;
        for (i, slot) in out.iter_mut().enumerate() {
            let (a, c1) = self.0[i].overflowing_add(other.0[i]);
            let (b, c2) = a.overflowing_add(u64::from(carry));
            *slot = b;
            carry = c1 || c2;
        }
        (!carry).then_some(Self(out))
    }
    /// Caller establishes self >= other.
    pub(crate) fn sub(self, other: Self) -> Self {
        debug_assert!(self >= other);
        let mut out = [0; 4];
        let mut borrow = false;
        for (i, slot) in out.iter_mut().enumerate() {
            let (a, b1) = self.0[i].overflowing_sub(other.0[i]);
            let (b, b2) = a.overflowing_sub(u64::from(borrow));
            *slot = b;
            borrow = b1 || b2;
        }
        debug_assert!(!borrow);
        Self(out)
    }
    fn trailing_zeros(self) -> u32 {
        self.0
            .iter()
            .enumerate()
            .find_map(|(i, &v)| (v != 0).then(|| i as u32 * 64 + v.trailing_zeros()))
            .unwrap_or(256)
    }
    fn shr(self, bits: u32) -> Self {
        if bits >= 256 {
            return Self::ZERO;
        }
        let words = (bits / 64) as usize;
        let shift = bits % 64;
        let mut out = [0; 4];
        for (i, slot) in out.iter_mut().enumerate().take(4 - words) {
            *slot = self.0[i + words] >> shift;
            if shift != 0 && i + words + 1 < 4 {
                *slot |= self.0[i + words + 1] << (64 - shift);
            }
        }
        Self(out)
    }
    fn shl(self, bits: u32) -> Self {
        debug_assert!(bits < 256);
        let words = (bits / 64) as usize;
        let shift = bits % 64;
        let mut out = [0; 4];
        for (i, &value) in self.0.iter().enumerate().take(4 - words) {
            out[i + words] |= value << shift;
            if shift != 0 && i + words + 1 < 4 {
                out[i + words + 1] |= value >> (64 - shift);
            }
        }
        Self(out)
    }
    pub(crate) fn gcd(mut self, mut other: Self) -> Self {
        if self == Self::ZERO {
            return other;
        }
        if other == Self::ZERO {
            return self;
        }
        let common = self.trailing_zeros().min(other.trailing_zeros());
        self = self.shr(self.trailing_zeros());
        loop {
            other = other.shr(other.trailing_zeros());
            if self > other {
                std::mem::swap(&mut self, &mut other);
            }
            other = other.sub(self);
            if other == Self::ZERO {
                return self.shl(common);
            }
        }
    }
    /// Binary long division over at most 256 source bits. The partial remainder
    /// fits because it never exceeds the already consumed numerator prefix.
    pub(crate) fn div_rem(self, denominator: Self) -> (Self, Self) {
        assert_ne!(denominator, Self::ZERO);
        if let (Some(n), Some(d)) = (self.to_u128(), denominator.to_u128()) {
            return (Self::from_u128(n / d), Self::from_u128(n % d));
        }
        let mut quotient = Self::ZERO;
        let mut remainder = Self::ZERO;
        for bit in (0..256).rev() {
            debug_assert_eq!(remainder.0[3] >> 63, 0);
            remainder = remainder.shl(1);
            remainder.0[0] |= (self.0[bit / 64] >> (bit % 64)) & 1;
            if remainder >= denominator {
                remainder = remainder.sub(denominator);
                quotient.0[bit / 64] |= 1 << (bit % 64);
            }
        }
        (quotient, remainder)
    }
}

impl Ord for Wide {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.iter().rev().cmp(other.0.iter().rev())
    }
}
impl PartialOrd for Wide {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
