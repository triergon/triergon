//! Decimal numbers as the receivers write them into their text.

/// Hands `write` the decimal digits of `value`, most significant first,
/// without leading zeros (0 is `0`). The digits live in the caller's frame:
/// the loop unrolls into one call of `write` per length, each with a slice of
/// that fixed length.
pub(crate) fn decimal<R>(mut value: u32, write: impl FnOnce(&[u8]) -> R) -> R {
    let mut digits = [0; 10];
    let mut start = digits.len();
    loop {
        start -= 1;
        digits[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            return write(&digits[start..]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::decimal;

    #[test]
    fn digits_have_no_leading_zeros() {
        for (value, text) in [
            (0, "0"),
            (7, "7"),
            (10, "10"),
            (345, "345"),
            (u32::MAX, "4294967295"),
        ] {
            assert_eq!(decimal(value, <[u8]>::to_vec), text.as_bytes());
        }
    }
}
