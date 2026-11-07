//! Ogg pages (RFC 3533 section 6).
use triergon_bitstream::crc::Crc32Mpeg;
use triergon_core::{Error, Result};

/// The page continues a packet from the previous page.
pub const CONTINUED: u8 = 1;
/// First page of a logical stream.
pub const BEGIN_OF_STREAM: u8 = 2;
/// Last page of a logical stream.
pub const END_OF_STREAM: u8 = 4;

/// Bytes of a page header before its lacing values.
pub const FIXED_HEADER: usize = 27;
/// The largest page: a full header and 255 segments of 255 bytes.
pub const MAX_PAGE: usize = FIXED_HEADER + 255 + 255 * 255;

/// A page header with its lacing values.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PageHeader {
    pub flags: u8,
    /// Position after the last packet that ends on this page; -1 when none ends here.
    pub granule: i64,
    pub serial: u32,
    pub sequence: u32,
    pub crc: u32,
    segments: u8,
    lacing: [u8; 255],
}

impl core::fmt::Debug for PageHeader {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PageHeader")
            .field("flags", &self.flags)
            .field("granule", &self.granule)
            .field("serial", &self.serial)
            .field("sequence", &self.sequence)
            .field("segments", &self.segments)
            .field("body_len", &self.body_len())
            .finish_non_exhaustive()
    }
}

impl PageHeader {
    /// Parse the header at the start of `bytes`. `Err(Truncated)` asks for more bytes;
    /// another error means no page header starts here.
    pub fn parse(bytes: &[u8]) -> Result<PageHeader> {
        let fixed = bytes.get(..FIXED_HEADER).ok_or(Error::Truncated)?;
        if &fixed[..4] != b"OggS" {
            return Err(Error::Invalid("Ogg capture pattern"));
        }
        if fixed[4] != 0 {
            return Err(Error::Unsupported("Ogg stream structure version"));
        }
        let le32 = |at: usize| u32::from_le_bytes([fixed[at], fixed[at + 1], fixed[at + 2], fixed[at + 3]]);
        let segments = fixed[26];
        let table = bytes
            .get(FIXED_HEADER..FIXED_HEADER + usize::from(segments))
            .ok_or(Error::Truncated)?;
        let mut lacing = [0u8; 255];
        lacing[..table.len()].copy_from_slice(table);
        Ok(PageHeader {
            flags: fixed[5],
            granule: i64::from_le_bytes(fixed[6..14].try_into().map_err(|_| Error::Truncated)?),
            serial: le32(14),
            sequence: le32(18),
            crc: le32(22),
            segments,
            lacing,
        })
    }

    /// Header bytes, lacing values included.
    pub fn header_len(&self) -> usize {
        FIXED_HEADER + usize::from(self.segments)
    }

    /// Body bytes.
    pub fn body_len(&self) -> usize {
        self.lacing().iter().map(|&v| usize::from(v)).sum()
    }

    /// Whole page bytes.
    pub fn page_len(&self) -> usize {
        self.header_len() + self.body_len()
    }

    /// The lacing values.
    pub fn lacing(&self) -> &[u8] {
        &self.lacing[..usize::from(self.segments)]
    }

    pub fn continued(&self) -> bool {
        self.flags & CONTINUED != 0
    }

    pub fn begin_of_stream(&self) -> bool {
        self.flags & BEGIN_OF_STREAM != 0
    }

    pub fn end_of_stream(&self) -> bool {
        self.flags & END_OF_STREAM != 0
    }

    /// Whether `page`, the whole page this header starts, carries the CRC it states.
    pub fn crc_ok(&self, page: &[u8]) -> bool {
        let Some(page) = page.get(..self.page_len()) else {
            return false;
        };
        let mut crc = Crc32Mpeg::from_register(0);
        crc.update(&page[..22]);
        crc.update(&[0; 4]);
        crc.update(&page[26..]);
        crc.remainder() == self.crc
    }

    /// The packet pieces of the body, in order, with whether each one ends its packet
    /// (a final run of 255-byte segments continues on the next page).
    pub fn pieces(&self) -> impl Iterator<Item = (core::ops::Range<usize>, bool)> + '_ {
        let lacing = self.lacing();
        let mut start = 0;
        let mut at = 0;
        let mut index = 0;
        core::iter::from_fn(move || {
            while let Some(&value) = lacing.get(index) {
                index += 1;
                at += usize::from(value);
                if value < 255 {
                    let piece = start..at;
                    start = at;
                    return Some((piece, true));
                }
            }
            // Only a trailing run of 255-byte segments leaves an open piece.
            if at > start {
                let piece = start..at;
                start = at;
                return Some((piece, false));
            }
            None
        })
    }
}

/// Write one page with lacing values `lacing` over `body`, computing its CRC; for tests
/// and fixture tools.
pub fn write_page(
    flags: u8,
    granule: i64,
    serial: u32,
    sequence: u32,
    lacing: &[u8],
    body: &[u8],
    out: &mut Vec<u8>,
) {
    let start = out.len();
    out.extend_from_slice(b"OggS");
    out.push(0);
    out.push(flags);
    out.extend_from_slice(&granule.to_le_bytes());
    out.extend_from_slice(&serial.to_le_bytes());
    out.extend_from_slice(&sequence.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.push(lacing.len() as u8);
    out.extend_from_slice(lacing);
    out.extend_from_slice(body);
    let mut crc = Crc32Mpeg::from_register(0);
    crc.update(&out[start..]);
    out[start + 22..start + 26].copy_from_slice(&crc.remainder().to_le_bytes());
}
