//! Bounded C-style integer conversions in DVD configuration and index text.
//! Width-dependent results follow black-box observations of the reference.

/// Width of the reference's long integer conversions in DVD text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IntegerConversionWidth {
    /// 32-bit long, as on ARMv7. This is the default.
    #[default]
    Bits32,
    /// 64-bit long, as on Linux AArch64, narrowed to the destination field.
    Bits64,
}

/// C's `isspace` in the C locale.
pub(super) fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t'..=b'\r')
}

/// C's `strtoul(text + start, &end, 16)` (C17 7.22.1.4) with a 32-bit
/// `unsigned long`: the value and `end`, which stays at `start` without digits. Out of range (above
/// `0xffffffff`) is `ULONG_MAX` whatever the sign; in range, a minus negates
/// in unsigned arithmetic.
pub(super) fn strtoul16(text: &[u8], start: usize) -> (u32, usize) {
    let hex = |at: usize| match text.get(at) {
        Some(&b @ b'0'..=b'9') => Some(b - b'0'),
        Some(&b @ b'a'..=b'f') => Some(b - b'a' + 10),
        Some(&b @ b'A'..=b'F') => Some(b - b'A' + 10),
        _ => None,
    };
    let mut at = start;
    while text.get(at).copied().is_some_and(is_space) {
        at += 1;
    }
    let negative = text.get(at) == Some(&b'-');
    if matches!(text.get(at), Some(b'-' | b'+')) {
        at += 1;
    }
    if text.get(at) == Some(&b'0')
        && matches!(text.get(at + 1), Some(b'x' | b'X'))
        && hex(at + 2).is_some()
    {
        at += 2;
    }
    let digits = at;
    let (mut value, mut overflow) = (0_u32, false);
    while let Some(nibble) = hex(at) {
        overflow |= value >> 28 != 0;
        value = value << 4 | u32::from(nibble);
        at += 1;
    }
    if at == digits {
        (0, start)
    } else if overflow {
        (u32::MAX, at)
    } else if negative {
        (value.wrapping_neg(), at)
    } else {
        (value, at)
    }
}

/// A decimal number from `start`, with C's `strtol` semantics (C17 7.22.1.4)
/// for a 32-bit `long`: after white space and an optional sign, saturating at
/// 2³¹ − 1 and −2³¹; with the index after it, or `None` without digits.
pub(super) fn scan_int(text: &[u8], start: usize) -> Option<(i32, usize)> {
    let mut at = start;
    while text.get(at).copied().is_some_and(is_space) {
        at += 1;
    }
    let negative = text.get(at) == Some(&b'-');
    if matches!(text.get(at), Some(b'-' | b'+')) {
        at += 1;
    }
    let digits = at;
    let mut magnitude = 0_u32;
    while let Some(&byte @ b'0'..=b'9') = text.get(at) {
        magnitude = magnitude
            .saturating_mul(10)
            .saturating_add(u32::from(byte - b'0'));
        at += 1;
    }
    if at == digits {
        return None;
    }
    let value = if negative {
        0_i32.saturating_sub_unsigned(magnitude)
    } else {
        i32::try_from(magnitude).unwrap_or(i32::MAX)
    };
    Some((value, at))
}

/// Select the reference long width before narrowing to the signed field.
pub(super) fn scan_field(
    text: &[u8],
    start: usize,
    width: IntegerConversionWidth,
) -> Option<(i32, usize)> {
    if width == IntegerConversionWidth::Bits32 {
        return scan_int(text, start);
    }
    let mut at = start;
    while text.get(at).copied().is_some_and(is_space) {
        at += 1;
    }
    let negative = text.get(at) == Some(&b'-');
    if matches!(text.get(at), Some(b'-' | b'+')) {
        at += 1;
    }
    let digits = at;
    let mut magnitude = 0_u64;
    while let Some(&byte @ b'0'..=b'9') = text.get(at) {
        magnitude = magnitude
            .saturating_mul(10)
            .saturating_add(u64::from(byte - b'0'));
        at += 1;
    }
    if at == digits {
        return None;
    }
    let value = if negative {
        0_i64.saturating_sub_unsigned(magnitude)
    } else {
        i64::try_from(magnitude).unwrap_or(i64::MAX)
    };
    Some((value as i32, at))
}

pub(super) fn hex_field(text: &[u8], start: usize, width: IntegerConversionWidth) -> (u32, usize) {
    if width == IntegerConversionWidth::Bits32 {
        return strtoul16(text, start);
    }
    let mut at = start;
    while text.get(at).copied().is_some_and(is_space) {
        at += 1;
    }
    let negative = text.get(at) == Some(&b'-');
    if matches!(text.get(at), Some(b'-' | b'+')) {
        at += 1;
    }
    let hex = |b: &u8| char::from(*b).to_digit(16);
    if text.get(at) == Some(&b'0')
        && matches!(text.get(at + 1), Some(b'x' | b'X'))
        && text.get(at + 2).and_then(hex).is_some()
    {
        at += 2;
    }
    let digits = at;
    let mut value = 0_u64;
    let mut overflow = false;
    while let Some(digit) = text.get(at).and_then(hex) {
        overflow |= value > (u64::MAX - u64::from(digit)) / 16;
        value = value.wrapping_mul(16).wrapping_add(u64::from(digit));
        at += 1;
    }
    if at == digits {
        (0, start)
    } else if overflow {
        (u32::MAX, at)
    } else if negative {
        (value.wrapping_neg() as u32, at)
    } else {
        (value as u32, at)
    }
}
