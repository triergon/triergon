//! Numbers in DVD configuration and index text, read as C's conversion
//! functions read them in FFmpeg built for 32-bit ARMv7, where `long` has 32
//! bits: the decoder follows that build wherever the width of `long` decides a
//! result.

/// C's `isspace` in the C locale.
pub(super) fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t'..=b'\r')
}

/// `strtoul(text + start, &end, 16)` with a 32-bit `unsigned long`: the value
/// and `end`, which stays at `start` without digits. Out of range (above
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

/// One `%d` from `start`: a decimal number converted as `strtol` does with a
/// 32-bit `long`, as on 32-bit ARMv7 (out of range is `LONG_MAX` or
/// `LONG_MIN`, which an `int` holds unchanged); with the index after it.
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
