//! The two sample widths a subframe decodes to.
//!
//! Every channel fits `i32` except the side channel of a 32-bit stereo frame, which has 33
//! bits (RFC 9639 section 4.2) and decodes to `i64`. Arithmetic wraps: for a valid stream
//! every sample fits its width, and malformed input must not panic.
use core::ops::{BitAnd, BitOr, Shr};
use triergon_bitstream::CachedBitReader;

pub(crate) trait Sample:
    Copy + Default + From<i32> + BitAnd<Output = Self> + BitOr<Output = Self> + Shr<u32, Output = Self>
{
    /// The low 32 bits, as the output keeps them.
    fn to_i32(self) -> i32;
    /// A two's complement field of `bits` bits (1 to the type's width, at most 33).
    fn read(reader: &mut CachedBitReader<'_>, bits: u32) -> Self;
    fn wrapping_add(self, other: Self) -> Self;
    fn wrapping_sub(self, other: Self) -> Self;
    fn wrapping_shl(self, bits: u32) -> Self;
}

impl Sample for i32 {
    #[inline(always)]
    fn to_i32(self) -> i32 {
        self
    }
    #[inline(always)]
    fn read(reader: &mut CachedBitReader<'_>, bits: u32) -> Self {
        reader.read_signed(bits)
    }
    #[inline(always)]
    fn wrapping_add(self, other: Self) -> Self {
        i32::wrapping_add(self, other)
    }
    #[inline(always)]
    fn wrapping_sub(self, other: Self) -> Self {
        i32::wrapping_sub(self, other)
    }
    #[inline(always)]
    fn wrapping_shl(self, bits: u32) -> Self {
        i32::wrapping_shl(self, bits)
    }
}

impl Sample for i64 {
    #[inline(always)]
    fn to_i32(self) -> i32 {
        self as i32
    }
    #[inline(always)]
    fn read(reader: &mut CachedBitReader<'_>, bits: u32) -> Self {
        if bits <= 32 {
            i64::from(reader.read_signed(bits))
        } else {
            let high = i64::from(reader.read_signed(bits - 32));
            (high << 32) | i64::from(reader.read(32))
        }
    }
    #[inline(always)]
    fn wrapping_add(self, other: Self) -> Self {
        i64::wrapping_add(self, other)
    }
    #[inline(always)]
    fn wrapping_sub(self, other: Self) -> Self {
        i64::wrapping_sub(self, other)
    }
    #[inline(always)]
    fn wrapping_shl(self, bits: u32) -> Self {
        i64::wrapping_shl(self, bits)
    }
}
