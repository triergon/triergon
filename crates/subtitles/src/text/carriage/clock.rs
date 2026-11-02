//! Original byte grammar for the versioned native-carriage view. The existing
//! strict source clocks/oracle are separate and are not rewritten here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flavor {
    SubRip,
    WebVtt,
}

fn number(bytes: &[u8], at: &mut usize) -> Option<(i64, usize)> {
    let start = *at;
    let mut value = 0_i64;
    while let Some(byte) = bytes.get(*at).copied().filter(u8::is_ascii_digit) {
        if *at - start >= 18 {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(i64::from(byte - b'0'))?;
        *at += 1;
    }
    if *at == start {
        None
    } else {
        Some((value, *at - start))
    }
}
fn byte(bytes: &[u8], at: &mut usize, wanted: u8) -> Option<()> {
    if bytes.get(*at) != Some(&wanted) {
        return None;
    }
    *at += 1;
    Some(())
}
fn whitespace(bytes: &[u8], at: &mut usize) -> Option<()> {
    // Timestamp syntax is bounded independently of potentially large payload
    // lines/settings. Long prefixes remain a concrete unsupported grammar gap.
    let first = *at;
    while matches!(bytes.get(*at), Some(b' ' | b'\t')) {
        if *at - first >= 128 {
            return None;
        }
        *at += 1;
    }
    Some(())
}
fn timestamp(bytes: &[u8], at: &mut usize, flavor: Flavor) -> Option<i64> {
    let (first, _) = number(bytes, at)?;
    byte(bytes, at, b':')?;
    let (second, second_digits) = number(bytes, at)?;
    let (hours, minutes, seconds) = if bytes.get(*at) == Some(&b':') {
        *at += 1;
        let (third, third_digits) = number(bytes, at)?;
        if second_digits != 2 || third_digits != 2 {
            return None;
        }
        (first, second, third)
    } else {
        if flavor != Flavor::WebVtt || second_digits != 2 {
            return None;
        }
        (0, first, second)
    };
    if minutes >= 60 || seconds >= 60 {
        return None;
    }
    byte(
        bytes,
        at,
        if flavor == Flavor::SubRip { b',' } else { b'.' },
    )?;
    let (milliseconds, digits) = number(bytes, at)?;
    if flavor == Flavor::WebVtt && digits != 3 {
        return None;
    }
    hours
        .checked_mul(3600)?
        .checked_add(minutes.checked_mul(60)?)?
        .checked_add(seconds)?
        .checked_mul(1000)?
        .checked_add(milliseconds)
}

/// Returns literal millisecond start/duration and the first settings byte.
/// Negative/zero native intervals remain literal; strict source legality stays
/// the responsibility of the unchanged independent source API.
pub(crate) fn timing(bytes: &[u8], flavor: Flavor) -> Option<(i64, i64, usize)> {
    let mut at = 0;
    whitespace(bytes, &mut at)?;
    let start = timestamp(bytes, &mut at, flavor)?;
    whitespace(bytes, &mut at)?;
    for wanted in b"-->" {
        byte(bytes, &mut at, *wanted)?;
    }
    whitespace(bytes, &mut at)?;
    let end = timestamp(bytes, &mut at, flavor)?;
    whitespace(bytes, &mut at)?;
    Some((start, end.checked_sub(start)?, at))
}
