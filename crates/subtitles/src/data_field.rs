//! The PES data field of DVB data services: a data identifier, then the
//! service's data. EN 300 472 §4.3 and EN 301 775 carry teletext data units
//! in it, EN 300 743 §7.1 subtitling segments after a subtitle stream id.

/// A PES data field: its `data_identifier` and the bytes after it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DataField<'a> {
    pub(crate) identifier: u8,
    pub(crate) data: &'a [u8],
}

impl<'a> DataField<'a> {
    /// `None` for an empty field.
    pub(crate) fn new(field: &'a [u8]) -> Option<Self> {
        let (&identifier, data) = field.split_first()?;
        Some(Self { identifier, data })
    }

    /// Whether the data units are teletext ones: EBU data (EN 300 472) or
    /// EN 301 775 data that carries teletext.
    pub(crate) fn is_teletext(&self) -> bool {
        matches!(self.identifier, 0x10..=0x1F | 0x99..=0x9B)
    }

    /// Whether the data are DVB subtitling segments (identifier 0x20) of
    /// subtitle stream 0.
    pub(crate) fn is_subtitling(&self) -> bool {
        self.identifier == 0x20 && self.data.first() == Some(&0)
    }
}
