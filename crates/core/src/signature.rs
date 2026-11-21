//! Container signatures observed in a source prefix, and [`select`], the one walk
//! over them. A caller states in a [`Policy`] which opening boxes it accepts and how
//! a prefix that fits both transport spacings is resolved.

/// An EBML header element ID at the start of the source.
pub fn ebml(prefix: &[u8]) -> bool {
    prefix.starts_with(&[0x1a, 0x45, 0xdf, 0xa3])
}

/// The four bytes in the type field of the first ISO BMFF box, if available.
pub fn opening_box(prefix: &[u8]) -> Option<&[u8; 4]> {
    prefix.get(4..8)?.try_into().ok()
}

/// An Ogg capture pattern followed by its zero version byte.
pub fn ogg(prefix: &[u8]) -> bool {
    prefix.starts_with(b"OggS\0")
}

/// Three physical MPEG transport packet sync bytes at one spacing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportSync {
    /// 188-byte packets with sync at the first byte.
    Ts188,
    /// 192-byte packets with a four-byte arrival prefix.
    M2ts192,
}

impl TransportSync {
    const fn offsets(self) -> [usize; 3] {
        match self {
            Self::Ts188 => [0, 188, 376],
            Self::M2ts192 => [4, 196, 388],
        }
    }

    /// Prefix length through the third sync byte.
    pub const fn prefix_len(self) -> usize {
        self.offsets()[2] + 1
    }

    /// True for three matching bytes, false on the first mismatch, or
    /// `None` while the prefix agrees but another sync byte is needed.
    pub fn observed(self, prefix: &[u8]) -> Option<bool> {
        for at in self.offsets() {
            if *prefix.get(at)? != 0x47 {
                return Some(false);
            }
        }
        Some(true)
    }
}

/// A container selected from a source prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    /// An EBML header: Matroska or WebM.
    Matroska,
    /// A first box of a type the policy accepts: ISO BMFF, MP4 or QuickTime.
    Isobmff,
    /// An Ogg page.
    Ogg,
    /// MPEG transport packets at this spacing.
    Transport(TransportSync),
}

/// How [`select`] resolves a prefix whose sync bytes fit both transport spacings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportTie {
    /// The 188-byte spacing wins, and is selected as soon as its three sync bytes
    /// are seen.
    Ts188,
    /// Neither wins. Both spacings are tested on a prefix through the last sync
    /// byte of either (389 bytes), and one that fits both is
    /// [`Selection::Ambiguous`].
    Refuse,
}

/// What a caller of [`select`] accepts where the signatures leave a choice.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    /// Types of a first box that select [`Container::Isobmff`].
    pub opening_boxes: &'static [[u8; 4]],
    /// Whether an Ogg page selects [`Container::Ogg`].
    pub ogg: bool,
    /// How a prefix that fits both transport spacings is resolved.
    pub transport_tie: TransportTie,
}

/// What [`select`] makes of a prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    /// The prefix starts this container.
    Container(Container),
    /// Undecided: a prefix of this length says more.
    Need(usize),
    /// Both transport spacings fit, under [`TransportTie::Refuse`].
    Ambiguous,
    /// No container signature the policy accepts.
    Unrecognised,
}

/// Selects the container `prefix` starts under `policy`, testing in this order: an
/// EBML header, an accepted opening box, an Ogg page, transport sync bytes.
// Inline, so that a caller's constant policy compiles to its own tests: a probe
// calls this for every window it reads.
#[inline]
pub fn select(prefix: &[u8], policy: &Policy) -> Selection {
    if ebml(prefix) {
        return Selection::Container(Container::Matroska);
    }
    if opening_box(prefix).is_some_and(|kind| policy.opening_boxes.contains(kind)) {
        return Selection::Container(Container::Isobmff);
    }
    if policy.ogg && ogg(prefix) {
        return Selection::Container(Container::Ogg);
    }
    if prefix.len() < 4 {
        return Selection::Need(4);
    }
    if prefix.len() < 8
        && policy
            .opening_boxes
            .iter()
            .any(|kind| kind.starts_with(&prefix[4..]))
    {
        return Selection::Need(8);
    }
    let [ts188, m2ts192] = [TransportSync::Ts188, TransportSync::M2ts192];
    match policy.transport_tie {
        TransportTie::Ts188 => {
            for sync in [ts188, m2ts192] {
                match sync.observed(prefix) {
                    Some(true) => return Selection::Container(Container::Transport(sync)),
                    Some(false) => {}
                    None => return Selection::Need(sync.prefix_len()),
                }
            }
            Selection::Unrecognised
        }
        TransportTie::Refuse if prefix.len() < m2ts192.prefix_len() => {
            Selection::Need(m2ts192.prefix_len())
        }
        TransportTie::Refuse => match (ts188.observed(prefix), m2ts192.observed(prefix)) {
            (Some(true), Some(false)) => Selection::Container(Container::Transport(ts188)),
            (Some(false), Some(true)) => Selection::Container(Container::Transport(m2ts192)),
            (Some(true), Some(true)) => Selection::Ambiguous,
            _ => Selection::Unrecognised,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: Policy = Policy {
        opening_boxes: &[*b"ftyp", *b"moof"],
        ogg: true,
        transport_tie: TransportTie::Ts188,
    };
    const REFUSING: Policy = Policy {
        opening_boxes: &[*b"ftyp", *b"wide"],
        ogg: false,
        transport_tie: TransportTie::Refuse,
    };

    #[test]
    fn a_policy_decides_boxes_ogg_and_a_prefix_both_transport_spacings_fit() {
        let isobmff = Selection::Container(Container::Isobmff);
        assert_eq!(select(b"\0\0\0\x10moof", &FIRST), isobmff);
        assert_eq!(select(b"\0\0\0\x10wide", &REFUSING), isobmff);
        assert_eq!(select(b"\0\0\0\x10wide", &FIRST), Selection::Unrecognised);
        assert_eq!(select(b"\0\0\0\x10moof", &REFUSING), Selection::Need(389));
        assert_eq!(
            select(b"OggS\0", &FIRST),
            Selection::Container(Container::Ogg)
        );
        assert_eq!(select(b"OggS\0", &REFUSING), Selection::Need(389));
        for policy in [&FIRST, &REFUSING] {
            assert_eq!(
                select(&[0x1a, 0x45, 0xdf, 0xa3], policy),
                Selection::Container(Container::Matroska)
            );
            assert_eq!(select(b"\0\0", policy), Selection::Need(4));
            assert_eq!(select(b"\0\0\0\x10ft", policy), Selection::Need(8));
        }

        let mut both = [0; 389];
        for at in [0, 188, 376, 4, 196, 388] {
            both[at] = 0x47;
        }
        let ts188 = Selection::Container(Container::Transport(TransportSync::Ts188));
        let m2ts192 = Selection::Container(Container::Transport(TransportSync::M2ts192));
        assert_eq!(select(&both[..377], &FIRST), ts188);
        assert_eq!(select(&both[..377], &REFUSING), Selection::Need(389));
        assert_eq!(select(&both, &REFUSING), Selection::Ambiguous);
        both[188] = 0;
        assert_eq!(select(&both[..189], &FIRST), Selection::Need(389));
        assert_eq!(select(&both, &FIRST), m2ts192);
        assert_eq!(select(&both, &REFUSING), m2ts192);
        both[196] = 0;
        assert_eq!(select(&both, &FIRST), Selection::Unrecognised);
        assert_eq!(select(&both, &REFUSING), Selection::Unrecognised);
        both[188] = 0x47;
        assert_eq!(select(&both, &REFUSING), ts188);
    }
}
