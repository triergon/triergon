//! Container signatures observed in a source prefix. Callers choose which
//! opening boxes they accept and how to resolve simultaneous signatures.

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
