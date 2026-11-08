//! Output channel layouts: FFmpeg's FLAC defaults and the WAVEFORMATEXTENSIBLE channel
//! mask a VORBIS_COMMENT block may carry.
use triergon_codec::{ChannelLayout, Speaker};

/// The layout of `channels` coded channels without a channel mask.
///
/// The channel order is RFC 9639 section 9.1.3's. FFmpeg names its five- and six-channel
/// surround pair side left/right (`5.0(side)`, `5.1(side)`) where the RFC says
/// "back/surround", and so does this decoder. Every order is already ascending, so
/// coded channel `i` is plane `i`.
pub(crate) fn default_layout(channels: u8) -> ChannelLayout {
    use Speaker::{
        BackCenter, BackLeft, BackRight, FrontCenter, FrontLeft, FrontRight, LowFrequency,
        SideLeft, SideRight,
    };
    let speakers: &[Speaker] = match channels {
        1 => &[FrontCenter],
        2 => &[FrontLeft, FrontRight],
        3 => &[FrontLeft, FrontRight, FrontCenter],
        4 => &[FrontLeft, FrontRight, BackLeft, BackRight],
        5 => &[FrontLeft, FrontRight, FrontCenter, SideLeft, SideRight],
        6 => &[
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            SideLeft,
            SideRight,
        ],
        7 => &[
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackCenter,
            SideLeft,
            SideRight,
        ],
        _ => &[
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight,
            SideLeft,
            SideRight,
        ],
    };
    ChannelLayout::new(speakers.iter().copied()).unwrap_or_default()
}

/// The WAVEFORMATEXTENSIBLE speakers, in mask bit order (bit `i` is `SPEAKERS[i]`).
const SPEAKERS: [Speaker; 18] = {
    use Speaker::{
        BackCenter, BackLeft, BackRight, FrontCenter, FrontLeft, FrontLeftOfCenter, FrontRight,
        FrontRightOfCenter, LowFrequency, SideLeft, SideRight, TopBackCenter, TopBackLeft,
        TopBackRight, TopCenter, TopFrontCenter, TopFrontLeft, TopFrontRight,
    };
    [
        FrontLeft,
        FrontRight,
        FrontCenter,
        LowFrequency,
        BackLeft,
        BackRight,
        FrontLeftOfCenter,
        FrontRightOfCenter,
        BackCenter,
        SideLeft,
        SideRight,
        TopCenter,
        TopFrontLeft,
        TopFrontCenter,
        TopFrontRight,
        TopBackLeft,
        TopBackCenter,
        TopBackRight,
    ]
};

/// The layout a `WAVEFORMATEXTENSIBLE_CHANNEL_MASK` comment gives `channels` channels.
///
/// FFmpeg takes the first comment with that name (any case), reads its value as C's
/// `strtol` with base 0 does (`0x` hexadecimal, a leading `0` octal), and uses it when it
/// names exactly `channels` of the 18 WAVEFORMATEXTENSIBLE speakers and nothing else;
/// the channels then follow ascending mask bits. Every other value is ignored.
pub(crate) fn masked_layout(vorbis_comment: &[u8], channels: u8) -> Option<ChannelLayout> {
    let mask = parse_c_integer(channel_mask_comment(vorbis_comment)?)?;
    if mask == 0 || mask >> SPEAKERS.len() != 0 || mask.count_ones() != u32::from(channels) {
        return None;
    }
    ChannelLayout::new(
        SPEAKERS
            .iter()
            .enumerate()
            .filter(|(bit, _)| mask >> bit & 1 != 0)
            .map(|(_, &speaker)| speaker),
    )
}

/// The value of the first `WAVEFORMATEXTENSIBLE_CHANNEL_MASK` comment of a VORBIS_COMMENT
/// block body (RFC 9639 section 8.6: little-endian lengths, `NAME=value` entries).
fn channel_mask_comment(body: &[u8]) -> Option<&[u8]> {
    const NAME: &[u8] = b"WAVEFORMATEXTENSIBLE_CHANNEL_MASK=";
    let (_vendor, rest) = split_field(body)?;
    let (count, mut rest) = rest.split_first_chunk::<4>()?;
    for _ in 0..u32::from_le_bytes(*count) {
        let (comment, tail) = split_field(rest)?;
        rest = tail;
        if let Some((name, value)) = comment.split_at_checked(NAME.len())
            && name.eq_ignore_ascii_case(NAME)
        {
            return Some(value);
        }
    }
    None
}

/// A field with a 32-bit little-endian length, and the bytes after it.
fn split_field(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let (len, rest) = bytes.split_first_chunk::<4>()?;
    rest.split_at_checked(u32::from_le_bytes(*len) as usize)
}

/// A non-negative integer as `strtol(text, NULL, 0)` reads it: leading white space, an
/// optional sign, a base prefix, then as many digits as follow. `None` when negative;
/// values that do not fit saturate.
fn parse_c_integer(text: &[u8]) -> Option<u64> {
    let text = text.trim_ascii_start();
    let (negative, text) = match text.split_first() {
        Some((b'-', rest)) => (true, rest),
        Some((b'+', rest)) => (false, rest),
        _ => (false, text),
    };
    let (radix, digits) = match text {
        [b'0', b'x' | b'X', rest @ ..] if rest.first().is_some_and(u8::is_ascii_hexdigit) => {
            (16, rest)
        }
        [b'0', rest @ ..] => (8, rest),
        _ => (10, text),
    };
    let value = digits
        .iter()
        .map_while(|&d| char::from(d).to_digit(radix))
        .fold(0u64, |value, digit| {
            value
                .saturating_mul(u64::from(radix))
                .saturating_add(u64::from(digit))
        });
    (!negative || value == 0).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment_block(comments: &[&str]) -> Vec<u8> {
        let mut body = 1u32.to_le_bytes().to_vec();
        body.push(b'x');
        body.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for comment in comments {
            body.extend_from_slice(&(comment.len() as u32).to_le_bytes());
            body.extend_from_slice(comment.as_bytes());
        }
        body
    }

    fn names(layout: Option<ChannelLayout>) -> Option<Vec<&'static str>> {
        layout.map(|l| l.speakers().iter().map(|s| s.name()).collect())
    }

    #[test]
    fn channel_masks_follow_ffmpeg() {
        let quad_side = Some(vec!["FL", "FR", "SL", "SR"]);
        let mask = |value: &str| {
            let comment = format!("WAVEFORMATEXTENSIBLE_CHANNEL_MASK={value}");
            names(masked_layout(&comment_block(&[&comment]), 4))
        };
        for value in [
            "0x603",
            "0X603",
            " 0x603",
            "+0x603",
            "1539",
            "0x603junk",
            "0x00603",
        ] {
            assert_eq!(mask(value), quad_side, "{value}");
        }
        assert_eq!(mask("0603"), Some(vec!["FL", "FR", "FRC", "BC"]), "octal");
        assert_eq!(mask("0x30003"), Some(vec!["FL", "FR", "TBC", "TBR"]));
        for value in [
            "603",
            "0x3",
            "0x1f",
            "-0x603",
            "0x40007",
            "0x80000007",
            "",
            "0x",
        ] {
            assert_eq!(mask(value), None, "{value}");
        }
        let lower = comment_block(&["waveformatextensible_channel_mask=0x603"]);
        assert_eq!(names(masked_layout(&lower, 4)), quad_side);
        let first_wins = comment_block(&[
            "WAVEFORMATEXTENSIBLE_CHANNEL_MASK=0x3",
            "WAVEFORMATEXTENSIBLE_CHANNEL_MASK=0x603",
        ]);
        assert_eq!(masked_layout(&first_wins, 4), None);
        assert_eq!(masked_layout(&[1, 2, 3], 4), None, "truncated block");
    }

    #[test]
    fn defaults_are_ffmpeg_flac_layouts() {
        assert_eq!(
            names(Some(default_layout(6))),
            Some(vec!["FL", "FR", "FC", "LFE", "SL", "SR"])
        );
        for channels in 1..=8 {
            let layout = default_layout(channels);
            assert_eq!(layout.channels(), usize::from(channels));
            assert!(
                layout.speakers().is_sorted(),
                "{channels}: plane order is coded order"
            );
        }
    }
}
