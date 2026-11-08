//! Tags written around elementary audio: ID3v2 before the stream, ID3v1 and APEv2
//! after it. They are kept as bounded opaque bytes and never reach a packet.
use triergon_core::Bytes;

/// Which tag a [`Tag`] holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TagKind {
    /// ID3v2 (any version), including its footer.
    Id3v2,
    /// The 128-byte ID3v1 tag at the end of the source.
    Id3v1,
    /// An APEv2 tag; it runs to the end of the source together with any ID3v1 after it.
    Ape,
}

/// A tag found in the source.
#[derive(Clone, Debug)]
pub struct Tag {
    pub kind: TagKind,
    /// Source offset of the first byte.
    pub offset: u64,
    /// Bytes the tag occupies in the source.
    pub len: u64,
    /// The whole tag, or `None` when it was larger than the retention limit.
    pub bytes: Option<Bytes>,
}

/// Bytes a leading tag needs before its length is known.
pub(crate) const ID3V2_HEADER: usize = 10;

/// What starts a source: an ID3v2 tag or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Leading {
    No,
    /// [`ID3V2_HEADER`] bytes would decide.
    Short,
    /// A tag of this total length: header, body and footer.
    Tag(u64),
}

/// Whether an ID3v2 tag starts `view`.
pub(crate) fn id3v2(view: &[u8]) -> Leading {
    let marker = view
        .get(..3)
        .map_or(b"ID3".starts_with(view), |m| m == b"ID3");
    if !marker {
        return Leading::No;
    }
    let Some(header) = view.get(..ID3V2_HEADER) else {
        return Leading::Short;
    };
    let version_ok = header[3] != 0xff && header[4] != 0xff;
    let size = &header[6..10];
    if !version_ok || size.iter().any(|&b| b & 0x80 != 0) {
        return Leading::No;
    }
    let body = size.iter().fold(0u64, |v, &b| (v << 7) | u64::from(b));
    // ID3v2.4 section 3.1: the footer flag adds a ten-byte copy of the header.
    let footer = if header[3] >= 4 && header[5] & 0x10 != 0 {
        10
    } else {
        0
    };
    Leading::Tag(ID3V2_HEADER as u64 + body + footer)
}

/// Bytes that decide whether a trailing tag starts at a frame boundary.
pub(crate) const TRAILING_PROBE: usize = 8;
const ID3V1_LEN: u64 = 128;

/// What starts at a frame boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Trailing {
    /// Not a tag.
    No,
    /// More bytes, or the source length, would decide.
    Undecided,
    /// A tag starts here and the audio ends.
    Tag(TagKind),
}

/// Whether `view` starts a trailing tag. `remaining` is the number of source bytes from
/// the start of `view` when known, and `at_end` says `view` holds all of them.
pub(crate) fn trailing(view: &[u8], remaining: Option<u64>, at_end: bool) -> Trailing {
    let probe = &view[..view.len().min(TRAILING_PROBE)];
    if probe.len() < TRAILING_PROBE && !at_end && b"APETAGEX".starts_with(probe) {
        return Trailing::Undecided;
    }
    if probe == b"APETAGEX" {
        return Trailing::Tag(TagKind::Ape);
    }
    if !view.starts_with(b"TAG") {
        return if !at_end && b"TAG".starts_with(probe) {
            Trailing::Undecided
        } else {
            Trailing::No
        };
    }
    // ID3v1 is exactly the last 128 bytes of the source.
    let remaining = remaining.or(at_end.then_some(view.len() as u64));
    match remaining {
        Some(ID3V1_LEN) => Trailing::Tag(TagKind::Id3v1),
        Some(_) => Trailing::No,
        None if view.len() as u64 > ID3V1_LEN => Trailing::No,
        None => Trailing::Undecided,
    }
}

/// Bytes [`trailing`] may need to decide on a `TAG` marker without a known length.
pub(crate) const TRAILING_DECIDE: usize = ID3V1_LEN as usize + 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id3v2_lengths_count_the_footer() {
        let mut tag = *b"ID3\x04\x00\x00\x00\x00\x01\x7f";
        assert_eq!(id3v2(&tag), Leading::Tag(10 + 255));
        tag[5] = 0x10;
        assert_eq!(id3v2(&tag), Leading::Tag(10 + 255 + 10));
        tag[3] = 3;
        assert_eq!(
            id3v2(&tag),
            Leading::Tag(10 + 255),
            "only v2.4 has a footer"
        );
        assert_eq!(id3v2(b"ID"), Leading::Short);
        assert_eq!(id3v2(b"ID3\x04"), Leading::Short);
        assert_eq!(id3v2(b"ID3\x04\x00\x00\x80\x00\x00\x00"), Leading::No);
        assert_eq!(id3v2(b"\xff\xfb\x90\x64"), Leading::No);
    }

    #[test]
    fn trailing_tags_are_recognised_only_where_they_must_be() {
        assert_eq!(
            trailing(b"APETAGEX", None, false),
            Trailing::Tag(TagKind::Ape)
        );
        assert_eq!(trailing(b"APETA", None, false), Trailing::Undecided);
        assert_eq!(trailing(b"APETA", None, true), Trailing::No);
        let id3v1 = [b"TAG".as_slice(), &[0; 125]].concat();
        assert_eq!(
            trailing(&id3v1, Some(128), false),
            Trailing::Tag(TagKind::Id3v1)
        );
        assert_eq!(trailing(&id3v1, Some(300), false), Trailing::No);
        assert_eq!(trailing(&id3v1, None, true), Trailing::Tag(TagKind::Id3v1));
        assert_eq!(trailing(&id3v1, None, false), Trailing::Undecided);
        assert_eq!(trailing(b"\xff\xfb", None, false), Trailing::No);
    }
}
