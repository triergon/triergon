//! Ogg pages (RFC 3533 section 6) and their capture from incremental input.
use std::sync::atomic::AtomicBool;
use triergon_bitstream::crc::Crc32Mpeg;
use triergon_core::{
    BudgetVec, Bytes, Error, InputCursor, InputRequest, Result, SourceLocation, Step, WorkBudget,
};

/// The page continues a packet from the previous page.
pub const CONTINUED: u8 = 1;
/// First page of a logical stream.
pub const BEGIN_OF_STREAM: u8 = 2;
/// Last page of a logical stream.
pub const END_OF_STREAM: u8 = 4;

/// The capture pattern every page starts with.
pub const CAPTURE: &[u8; 4] = b"OggS";
/// Bytes of a page header before its lacing values.
pub const FIXED_HEADER: usize = 27;
/// The largest page: a full header and 255 segments of 255 bytes.
pub const MAX_PAGE: usize = FIXED_HEADER + 255 + 255 * 255;

/// The fixed fields of a page header. The lacing values stay in the page bytes; the
/// methods that read them take the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageHeader {
    pub flags: u8,
    /// Position after the last packet that ends on this page; -1 when none ends here.
    pub granule: i64,
    pub serial: u32,
    pub sequence: u32,
    pub crc: u32,
    segments: u8,
    body_len: u16,
}

impl PageHeader {
    /// Parse the header at the start of `bytes`, lacing values included. `Err(Truncated)`
    /// asks for more bytes; another error means no page header starts here.
    pub fn parse(bytes: &[u8]) -> Result<PageHeader> {
        let fixed: &[u8; FIXED_HEADER] = bytes
            .get(..FIXED_HEADER)
            .and_then(|fixed| fixed.try_into().ok())
            .ok_or(Error::Truncated)?;
        if &fixed[..4] != CAPTURE {
            return Err(Error::Invalid("Ogg capture pattern"));
        }
        if fixed[4] != 0 {
            return Err(Error::Unsupported("Ogg stream structure version"));
        }
        let le32 = |at: usize| {
            u32::from_le_bytes([fixed[at], fixed[at + 1], fixed[at + 2], fixed[at + 3]])
        };
        let mut granule = [0; 8];
        granule.copy_from_slice(&fixed[6..14]);
        let segments = fixed[26];
        let lacing = bytes
            .get(FIXED_HEADER..FIXED_HEADER + usize::from(segments))
            .ok_or(Error::Truncated)?;
        // At most 255 * 255.
        let body_len = lacing.iter().map(|&v| u16::from(v)).sum();
        Ok(PageHeader {
            flags: fixed[5],
            granule: i64::from_le_bytes(granule),
            serial: le32(14),
            sequence: le32(18),
            crc: le32(22),
            segments,
            body_len,
        })
    }

    /// Header bytes, lacing values included.
    pub fn header_len(&self) -> usize {
        FIXED_HEADER + usize::from(self.segments)
    }

    /// Body bytes.
    pub fn body_len(&self) -> usize {
        usize::from(self.body_len)
    }

    /// Whole page bytes.
    pub fn page_len(&self) -> usize {
        self.header_len() + self.body_len()
    }

    /// The lacing values of `page`, the page this header starts.
    pub fn lacing<'a>(&self, page: &'a [u8]) -> &'a [u8] {
        page.get(FIXED_HEADER..self.header_len()).unwrap_or(&[])
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

    /// The granule position, or `None` when no packet ends on this page (-1).
    pub fn granule_position(&self) -> Option<i64> {
        (self.granule != -1).then_some(self.granule)
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

    /// The packet pieces of the body of `page`, in order, with whether each one ends its
    /// packet (a final run of 255-byte segments continues on the next page).
    pub fn pieces<'a>(
        &self,
        page: &'a [u8],
    ) -> impl Iterator<Item = (core::ops::Range<usize>, bool)> + 'a {
        let lacing = self.lacing(page);
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

    /// Number of packets that end on `page`.
    pub fn completed_packets(&self, page: &[u8]) -> usize {
        self.lacing(page)
            .iter()
            .filter(|&&value| value < 255)
            .count()
    }
}

/// A whole page with a checked CRC.
#[derive(Clone, Debug)]
pub struct Page {
    pub header: PageHeader,
    /// The page bytes, header included.
    pub bytes: Bytes,
    /// Source offset of the capture pattern.
    pub offset: u64,
    /// Bytes skipped to find this page since the previous one: garbage, pages with a bad
    /// CRC or an unknown version.
    pub skipped: u64,
}

/// What the bytes at the start of a buffer hold.
enum Examined {
    /// No page starts in the first `n` bytes.
    Garbage(usize),
    /// A page may start here; at least this many bytes are needed to tell.
    Need(usize),
    /// A whole page with a good CRC.
    Page(PageHeader),
}

fn examine(bytes: &[u8]) -> Examined {
    let Some(at) = bytes.windows(4).position(|window| window == CAPTURE) else {
        // Keep a tail that may begin a capture pattern split across windows.
        let tail = (1..=3.min(bytes.len()))
            .rev()
            .find(|&n| CAPTURE.starts_with(&bytes[bytes.len() - n..]))
            .unwrap_or(0);
        return match bytes.len() - tail {
            0 => Examined::Need(FIXED_HEADER),
            garbage => Examined::Garbage(garbage),
        };
    };
    if at > 0 {
        return Examined::Garbage(at);
    }
    match PageHeader::parse(bytes) {
        Err(Error::Truncated) => {
            let segments = bytes.get(26).map_or(0, |&n| usize::from(n));
            Examined::Need(FIXED_HEADER + segments)
        }
        Err(_) => Examined::Garbage(1),
        Ok(header) if bytes.len() < header.page_len() => Examined::Need(header.page_len()),
        // RFC 3533: a page whose CRC fails is not a page; resynchronise after its pattern.
        Ok(header) if !header.crc_ok(bytes) => Examined::Garbage(1),
        Ok(header) => Examined::Page(header),
    }
}

/// Finds and captures whole pages from an [`InputCursor`].
///
/// A page that lies inside one input window is shared with it without a copy; a page
/// split across windows is gathered into charged storage. Bytes before a capture pattern,
/// pages with a bad CRC and pages of another structure version are skipped, at most
/// `max_skip` bytes in a row.
#[derive(Debug)]
pub struct PageReader {
    /// A page gathered across windows, starting at `buffer_offset`.
    buffer: BudgetVec<u8>,
    buffer_offset: u64,
    skipped: u64,
    max_skip: u64,
}

/// What [`PageReader::next`] produced.
#[derive(Debug)]
pub enum PageStep {
    Page(Page),
    NeedInput(InputRequest),
    Yield,
    /// The input ended; a trailing partial page is counted in `skipped`.
    End {
        location: SourceLocation,
        skipped: u64,
    },
}

impl PageReader {
    pub fn new(input: &InputCursor, max_skip: u64) -> PageReader {
        PageReader {
            buffer: BudgetVec::new(input.budget()),
            buffer_offset: 0,
            skipped: 0,
            max_skip,
        }
    }

    /// Drop a partly gathered page, as after a jump in the input.
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.skipped = 0;
    }

    /// Offset of the next byte this reader examines.
    pub fn position(&self, input: &InputCursor) -> u64 {
        if self.buffer.is_empty() {
            input.position()
        } else {
            self.buffer_offset
        }
    }

    fn skip(&mut self, bytes: usize) -> Result<()> {
        self.skipped += bytes as u64;
        if self.skipped > self.max_skip {
            return Err(Error::Policy("Ogg capture pattern scan"));
        }
        Ok(())
    }

    /// The next whole page.
    pub fn next(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<PageStep> {
        loop {
            if !self.buffer.is_empty() {
                match examine(&self.buffer) {
                    Examined::Garbage(count) => {
                        self.skip(count)?;
                        let rest = self.buffer.len() - count;
                        self.buffer.copy_within(count.., 0);
                        self.buffer.truncate(rest);
                        self.buffer_offset += count as u64;
                    }
                    Examined::Page(header) => {
                        let offset = self.buffer_offset;
                        let length = header.page_len();
                        let bytes = if length < self.buffer.len() {
                            // Resynchronising inside the buffer left the next page's
                            // start behind this one.
                            let bytes =
                                Bytes::copy_from_slice(input.budget(), &self.buffer[..length])?;
                            let rest = self.buffer.len() - length;
                            self.buffer.copy_within(length.., 0);
                            self.buffer.truncate(rest);
                            self.buffer_offset += length as u64;
                            bytes
                        } else {
                            let buffer =
                                std::mem::replace(&mut self.buffer, BudgetVec::new(input.budget()));
                            match buffer.freeze() {
                                Ok(bytes) => bytes,
                                Err(rejected) => {
                                    self.buffer = rejected.buffer;
                                    return Err(rejected.error);
                                }
                            }
                        };
                        return Ok(PageStep::Page(self.page(header, bytes, offset)));
                    }
                    Examined::Need(needed) => {
                        let missing = needed - self.buffer.len();
                        match input.read_append(&mut self.buffer, missing, work, cancel)? {
                            Step::Ready(_) => {}
                            Step::NeedInput(request) => return Ok(PageStep::NeedInput(request)),
                            Step::Yield => return Ok(PageStep::Yield),
                            Step::Backpressure(never) => match never {},
                            Step::End(location) => {
                                let partial = self.buffer.len();
                                self.buffer.clear();
                                self.skipped += partial as u64;
                                return Ok(self.end(location));
                            }
                        }
                    }
                }
                continue;
            }
            let offset = input.position();
            if input.buffered().is_empty() {
                match input.read_discard(1, work, cancel)? {
                    Step::NeedInput(request) => return Ok(PageStep::NeedInput(request)),
                    Step::End(location) => return Ok(self.end(location)),
                    _ => return Err(Error::Invalid("empty Ogg input window")),
                }
            }
            let step = match examine(input.buffered()) {
                Examined::Garbage(count) => match input.read_discard(count, work, cancel)? {
                    Step::Ready(discarded) => {
                        self.skip(discarded)?;
                        continue;
                    }
                    step => step,
                },
                Examined::Page(header) if work.remaining() >= header.page_len() as u64 => {
                    return match input.read_span(header.page_len(), work, cancel)? {
                        Step::Ready(bytes) => Ok(PageStep::Page(self.page(header, bytes, offset))),
                        // The page is buffered and the work suffices.
                        _ => Err(Error::Invalid("buffered Ogg page")),
                    };
                }
                Examined::Page(header) => {
                    self.buffer_offset = offset;
                    input.read_append(&mut self.buffer, header.page_len(), work, cancel)?
                }
                Examined::Need(needed) => {
                    self.buffer_offset = offset;
                    input.read_append(&mut self.buffer, needed, work, cancel)?
                }
            };
            match step {
                Step::Ready(_) => {}
                Step::NeedInput(request) => return Ok(PageStep::NeedInput(request)),
                Step::Yield => return Ok(PageStep::Yield),
                Step::Backpressure(never) => match never {},
                Step::End(location) => return Ok(self.end(location)),
            }
        }
    }

    fn page(&mut self, header: PageHeader, bytes: Bytes, offset: u64) -> Page {
        Page {
            header,
            bytes,
            offset,
            skipped: std::mem::take(&mut self.skipped),
        }
    }

    fn end(&mut self, location: SourceLocation) -> PageStep {
        PageStep::End {
            location,
            skipped: std::mem::take(&mut self.skipped),
        }
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
    out.extend_from_slice(CAPTURE);
    out.push(0);
    out.push(flags);
    out.extend_from_slice(&granule.to_le_bytes());
    out.extend_from_slice(&serial.to_le_bytes());
    out.extend_from_slice(&sequence.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.push(u8::try_from(lacing.len()).unwrap_or(u8::MAX));
    out.extend_from_slice(lacing);
    out.extend_from_slice(body);
    let mut crc = Crc32Mpeg::from_register(0);
    crc.update(&out[start..]);
    out[start + 22..start + 26].copy_from_slice(&crc.remainder().to_le_bytes());
}

/// Lacing values for packets of the given sizes, all ending on one page.
pub fn lacing_for(sizes: &[usize], out: &mut Vec<u8>) {
    for &size in sizes {
        out.extend(std::iter::repeat_n(255, size / 255));
        out.push((size % 255) as u8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn examination_finds_split_capture_patterns_and_bad_pages() {
        let mut page = Vec::new();
        write_page(BEGIN_OF_STREAM, 0, 7, 0, &[3], b"abc", &mut page);
        assert!(matches!(examine(&page), Examined::Page(_)));
        assert!(matches!(examine(&page[..10]), Examined::Need(FIXED_HEADER)));
        assert!(matches!(examine(&page[..27]), Examined::Need(28)));
        assert!(matches!(examine(&page[..29]), Examined::Need(31)));
        assert!(matches!(examine(b"xxOg"), Examined::Garbage(2)));
        assert!(matches!(examine(b"Og"), Examined::Need(_)));
        assert!(matches!(examine(b"xyzw"), Examined::Garbage(4)));
        assert!(matches!(examine(b"abOggS"), Examined::Garbage(2)));
        let mut bad = page.clone();
        bad[30] ^= 1;
        assert!(matches!(examine(&bad), Examined::Garbage(1)));
        bad = page.clone();
        bad[4] = 1;
        assert!(matches!(examine(&bad), Examined::Garbage(1)));
    }

    #[test]
    fn pieces_follow_the_lacing() {
        let mut lacing = Vec::new();
        lacing_for(&[0, 255, 3], &mut lacing);
        assert_eq!(lacing, [0, 255, 0, 3]);
        let mut page = Vec::new();
        write_page(0, 5, 1, 0, &lacing, &[9; 258], &mut page);
        let header = PageHeader::parse(&page).unwrap();
        let pieces: Vec<_> = header.pieces(&page).collect();
        assert_eq!(pieces, [(0..0, true), (0..255, true), (255..258, true)]);
        assert_eq!(header.completed_packets(&page), 3);
        page.clear();
        write_page(0, -1, 1, 0, &[255, 255], &[9; 510], &mut page);
        let header = PageHeader::parse(&page).unwrap();
        assert_eq!(header.pieces(&page).collect::<Vec<_>>(), [(0..510, false)]);
        assert_eq!(header.granule_position(), None);
    }
}
