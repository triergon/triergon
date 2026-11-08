//! The numeric conversions libass applies to script text, each with its
//! quirks: header integers reduced modulo 2^32, override integers saturated by
//! `strtoll` and clamped to 32 bits, `ass_strtod`'s decimal conversion and the
//! `sscanf` time fields.
//!
//! Every function reads from the start of a slice whose end stands for the C
//! terminator, and returns the value with the number of bytes consumed (0 when
//! nothing converted).

use crate::scan::{is_blank, is_space, starts_with_ignore_case};

/// libass's `ass_strtod`: an optionally signed decimal mantissa of at most 18
/// significant digits and an optional exponent, converted with the binary
/// powers of ten in the order libass multiplies them. Exponents saturate at
/// 511. An `e` or `E` is consumed even without exponent digits. Nothing
/// converted yields 0 (or -0 after a minus sign) with nothing consumed.
pub(crate) fn strtod(s: &[u8]) -> (f64, usize) {
    const POSITIVE: [f64; 9] = [10.0, 100.0, 1e4, 1e8, 1e16, 1e32, 1e64, 1e128, 1e256];
    const NEGATIVE: [f64; 9] = [0.1, 0.01, 1e-4, 1e-8, 1e-16, 1e-32, 1e-64, 1e-128, 1e-256];
    const MAX_EXPONENT: u64 = 511;
    // The product of the table entries selected by the bits of `exponent`.
    let power = |table: &[f64; 9], mut exponent: u64| {
        let mut product = 1.0;
        for factor in table {
            if exponent == 0 {
                break;
            }
            if exponent & 1 != 0 {
                product *= factor;
            }
            exponent >>= 1;
        }
        product
    };

    let byte = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut at = s.iter().position(|&c| !is_space(c)).unwrap_or(s.len());
    let negative = byte(at) == b'-';
    if matches!(byte(at), b'-' | b'+') {
        at += 1;
    }
    let apply_sign = |value: f64| if negative { -value } else { value };

    // The mantissa: digits with at most one point.
    let mut digits = 0_u64; // digit characters
    let mut before_point = None; // digits before the point
    let mut leading_zeros = None; // zero digits before the first nonzero digit
    let mut significant = [0_u8; 18];
    let mut kept = 0;
    loop {
        match byte(at) {
            b'.' if before_point.is_none() => before_point = Some(digits),
            c @ b'0'..=b'9' => {
                if c != b'0' && leading_zeros.is_none() {
                    leading_zeros = Some(digits);
                }
                if leading_zeros.is_some() && kept < significant.len() {
                    significant[kept] = c - b'0';
                    kept += 1;
                }
                digits += 1;
            }
            _ => break,
        }
        at += 1;
    }
    if digits == 0 {
        return (apply_sign(0.0), 0);
    }
    let leading_zeros = leading_zeros.unwrap_or(digits);
    let before_point = before_point.unwrap_or(digits);
    // Digits beyond the 18th significant one only move the exponent.
    let used = leading_zeros + kept as u64;
    let (mut fraction_negative, fraction_exponent) = if before_point < used {
        (true, used - before_point)
    } else {
        (false, before_point - used)
    };
    let (high, low) = significant[..kept].split_at(kept.saturating_sub(9));
    let fold = |part: &[u8]| part.iter().fold(0_i32, |n, &d| 10 * n + i32::from(d));
    let mut fraction = 1.0e9 * f64::from(fold(high)) + f64::from(fold(low));

    // The exponent, combined with the one the mantissa implies.
    let mut exponent = fraction_exponent;
    if matches!(byte(at), b'e' | b'E') {
        at += 1;
        let written_negative = byte(at) == b'-';
        if matches!(byte(at), b'-' | b'+') {
            at += 1;
        }
        let limit = if written_negative == fraction_negative {
            MAX_EXPONENT.saturating_sub(fraction_exponent) / 10
        } else {
            fraction_exponent / 10 + (fraction_exponent % 10 + MAX_EXPONENT) / 10
        };
        let mut written = 0_u64;
        let mut overflow = false;
        while byte(at).is_ascii_digit() {
            if written > limit {
                overflow = true;
            } else {
                written = written * 10 + u64::from(byte(at) - b'0');
            }
            at += 1;
        }
        if overflow {
            // libass keeps the written sign and saturates.
            exponent = MAX_EXPONENT;
            fraction_negative = written_negative;
        } else if written_negative == fraction_negative {
            exponent = fraction_exponent + written;
        } else if fraction_exponent <= written {
            exponent = written - fraction_exponent;
            fraction_negative = written_negative;
        } else {
            exponent = fraction_exponent - written;
        }
    }
    exponent = exponent.min(MAX_EXPONENT);

    let mut table = &POSITIVE;
    if exponent > 308 && fraction_negative {
        // Below the normal range: scale the mantissa under 1 first, then
        // multiply by negative powers so a subnormal result survives.
        let significant = used - leading_zeros;
        exponent -= significant;
        fraction /= power(&POSITIVE, significant);
        table = &NEGATIVE;
        fraction_negative = false;
    }
    let scale = power(table, exponent);
    if fraction_negative {
        fraction /= scale;
    } else {
        fraction *= scale;
    }
    (apply_sign(fraction), at)
}

/// C's `strtoll` in the C locale for base 10 or 16: blanks, a sign, for base
/// 16 a `0x` prefix when a hex digit follows, then digits, saturating at the
/// `i64` range.
pub(crate) fn strtoll(s: &[u8], base: u32) -> (i64, usize) {
    let byte = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut at = s.iter().position(|&c| !is_space(c)).unwrap_or(s.len());
    let negative = byte(at) == b'-';
    if matches!(byte(at), b'-' | b'+') {
        at += 1;
    }
    if base == 16
        && byte(at) == b'0'
        && matches!(byte(at + 1), b'x' | b'X')
        && byte(at + 2).is_ascii_hexdigit()
    {
        at += 2;
    }
    let start = at;
    let mut magnitude = 0_u64;
    let mut saturated = false;
    while let Some(digit) = char::from(byte(at)).to_digit(base) {
        match magnitude
            .checked_mul(u64::from(base))
            .and_then(|m| m.checked_add(u64::from(digit)))
        {
            Some(m) => magnitude = m,
            None => saturated = true,
        }
        at += 1;
    }
    if at == start {
        return (0, 0);
    }
    let value = match (negative, saturated) {
        (false, false) => i64::try_from(magnitude).unwrap_or(i64::MAX),
        (true, false) => 0_i64.checked_sub_unsigned(magnitude).unwrap_or(i64::MIN),
        (false, true) => i64::MAX,
        (true, true) => i64::MIN,
    };
    (value, at)
}

/// libass's `mystrtoi32`: `strtoll` clamped to the `i32` range.
pub(crate) fn strtoi32(s: &[u8], base: u32) -> (i32, usize) {
    let (value, used) = strtoll(s, base);
    (value.clamp(i32::MIN.into(), i32::MAX.into()) as i32, used)
}

/// libass's `parse_int_header`: an `&h` or `0x` prefix right at the start
/// selects hexadecimal; then blanks, a sign, for hexadecimal another `0x`, and
/// digits reduced modulo 2^32, as VSFilter's `scanf` does. No digits is 0.
pub(crate) fn header_int(s: &[u8]) -> i32 {
    let (base, s) = if starts_with_ignore_case(s, b"&h") || starts_with_ignore_case(s, b"0x") {
        (16, &s[2..])
    } else {
        (10, s)
    };
    let mut s = &s[s.iter().position(|&c| !is_blank(c)).unwrap_or(s.len())..];
    let negative = s.first() == Some(&b'-');
    if matches!(s.first(), Some(b'-' | b'+')) {
        s = &s[1..];
    }
    if base == 16 && starts_with_ignore_case(s, b"0x") {
        s = &s[2..];
    }
    let value = s
        .iter()
        .map_while(|&c| char::from(c).to_digit(base))
        .fold(0_u32, |v, d| v.wrapping_mul(base).wrapping_add(d));
    (if negative {
        value.wrapping_neg()
    } else {
        value
    }) as i32
}

/// libass's `parse_bool`: `yes` in any case, or a positive integer.
pub(crate) fn header_bool(s: &[u8]) -> bool {
    let s = &s[s.iter().position(|&c| !is_blank(c)).unwrap_or(s.len())..];
    starts_with_ignore_case(s, b"yes") || crate::c_long::strtol(s).0 > 0
}

/// A `%d` conversion of the C library's `sscanf`: blanks, then at most 512
/// characters of sign and digits converted by `strtol` and narrowed to `int`
/// ([`crate::c_long`]). `None` when no digit is read.
pub(crate) fn scanf_int(s: &[u8]) -> Option<(i32, usize)> {
    const WIDTH: usize = 512;
    let at = s.iter().position(|&c| !is_space(c)).unwrap_or(s.len());
    let rest = &s[at..];
    let sign = usize::from(matches!(rest.first(), Some(b'-' | b'+')));
    let digits = rest[sign..]
        .iter()
        .take(WIDTH - sign)
        .take_while(|c| c.is_ascii_digit())
        .count();
    if digits == 0 {
        return None;
    }
    let field = &rest[..sign + digits];
    Some((crate::c_long::atoi(field), at + field.len()))
}

/// The `[&H]` prefix of override colour and alpha arguments: any run of `&`
/// and uppercase `H`, then `strtoll` in base 16, clamped to `i32`.
pub(crate) fn hex_tag_value(s: &[u8]) -> i32 {
    let start = s
        .iter()
        .position(|&c| c != b'&' && c != b'H')
        .unwrap_or(s.len());
    strtoi32(&s[start..], 16).0
}

/// libass's `dtoi32`: truncation toward zero with x86's result, `i32::MIN`,
/// for NaN and values outside the range.
pub(crate) fn dtoi32(value: f64) -> i32 {
    if value.is_nan() || value <= f64::from(i32::MIN) || value >= f64::from(i32::MAX) + 1.0 {
        i32::MIN
    } else {
        value as i32
    }
}

/// C's `FFMAX` on doubles: `a > b ? a : b`, so a NaN `a` yields `b`.
pub(crate) fn ffmax(a: f64, b: f64) -> f64 {
    if a > b { a } else { b }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> (f64, usize) {
        strtod(s.as_bytes())
    }

    #[test]
    fn strtod_matches_libass_quirks() {
        assert_eq!(d("12.5x"), (12.5, 4));
        assert_eq!(d("  -0.25"), (-0.25, 7));
        assert_eq!(d("1e"), (1.0, 2)); // the exponent marker is consumed
        assert_eq!(d("2e+"), (2.0, 3));
        assert_eq!(d("1.5e2"), (150.0, 5));
        assert_eq!(d("."), (0.0, 0));
        let (minus, used) = d("-");
        assert!(minus == 0.0 && minus.is_sign_negative() && used == 0);
        assert_eq!(d("1e999").0, f64::INFINITY);
        assert!(d("0e400").0.is_nan()); // 0 times an infinite power
        assert_eq!(d("0.000001").0, 1e-6);
        assert_eq!(d("123456789012345678901").0, 1.2345678901234568e20);
        assert_eq!(d("4.9e-324").0, 5e-324);
    }

    #[test]
    fn integers_follow_their_c_functions() {
        assert_eq!(strtoll(b" -42z", 10), (-42, 4));
        assert_eq!(strtoll(b"0x1f", 16), (31, 4));
        assert_eq!(strtoll(b"0xg", 16), (0, 1));
        assert_eq!(strtoll(b"99999999999999999999", 10).0, i64::MAX);
        assert_eq!(strtoll(b"-99999999999999999999", 10).0, i64::MIN);
        assert_eq!(strtoi32(b"3000000000", 10).0, i32::MAX);
        assert_eq!(header_int(b"&H10"), 16);
        assert_eq!(header_int(b" &H10"), 0);
        assert_eq!(header_int(b"4294967297"), 1);
        assert_eq!(header_int(b"-1"), -1);
        assert_eq!(header_int(b"&h-0x10"), -16);
        assert!(header_bool(b" YES"));
        assert!(header_bool(b"2"));
        assert!(!header_bool(b"no"));
        assert_eq!(scanf_int(b" +12:"), Some((12, 4)));
        assert_eq!(scanf_int(b"-"), None);
        assert_eq!(hex_tag_value(b"&H&HFF&"), 255);
        assert_eq!(hex_tag_value(b"&hFF"), 0);
        assert_eq!(dtoi32(f64::NAN), i32::MIN);
        assert_eq!(dtoi32(-2.9), -2);
        assert_eq!(ffmax(f64::NAN, 0.0), 0.0);
    }
}
