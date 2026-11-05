use triergon_core::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Envelope {
    /// Three-byte segment header; clocks belong to the enclosing packet.
    Packet,
    /// Thirteen-byte PG header with raw, wrapping 90 kHz source clocks.
    Sup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SupClock {
    pub pts: u32,
    pub dts: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Segment<'a> {
    pub offset: usize,
    pub kind: u8,
    pub clock: Option<SupClock>,
    pub raw: &'a [u8],
    pub payload: &'a [u8],
}

/// Borrowing, allocation-free framing. Unknown segment kinds remain source data.
/// An error is emitted once; bytes are never scanned ahead to guess a new header.
#[derive(Debug)]
pub struct SegmentIter<'a> {
    input: &'a [u8],
    at: usize,
    envelope: Envelope,
    failed: bool,
}

impl<'a> SegmentIter<'a> {
    pub fn new(input: &'a [u8], envelope: Envelope) -> Self {
        Self {
            input,
            at: 0,
            envelope,
            failed: false,
        }
    }

    fn parse(&mut self) -> Result<Segment<'a>> {
        let start = self.at;
        let mut cursor = Cursor {
            input: self.input,
            at: start,
        };
        let clock = if self.envelope == Envelope::Sup {
            if cursor.take(2)? != b"PG" {
                return Err(Error::Invalid("PGS SUP magic"));
            }
            Some(SupClock {
                pts: cursor.u32()?,
                dts: cursor.u32()?,
            })
        } else {
            None
        };
        let kind = cursor.u8()?;
        let length = usize::from(cursor.u16()?);
        let payload = cursor.take(length)?;
        self.at = cursor.at;
        Ok(Segment {
            offset: start,
            kind,
            clock,
            raw: &self.input[start..self.at],
            payload,
        })
    }
}

impl<'a> Iterator for SegmentIter<'a> {
    type Item = Result<Segment<'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.at == self.input.len() {
            return None;
        }
        let result = self.parse();
        self.failed = result.is_err();
        Some(result)
    }
}

struct Cursor<'a> {
    input: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(count).ok_or(Error::Overflow)?;
        let bytes = self.input.get(self.at..end).ok_or(Error::Truncated)?;
        self.at = end;
        Ok(bytes)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u24(&mut self) -> Result<u32> {
        let b = self.take(3)?;
        Ok(u32::from_be_bytes([0, b[0], b[1], b[2]]))
    }
    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn finish(&self) -> Result<()> {
        if self.at == self.input.len() {
            Ok(())
        } else {
            Err(Error::Invalid("PGS trailing typed segment fields"))
        }
    }
}

fn body<'a>(segment: Segment<'a>, kind: u8) -> Result<Cursor<'a>> {
    if segment.kind != kind {
        return Err(Error::Invalid("PGS unexpected typed segment"));
    }
    Ok(Cursor {
        input: segment.payload,
        at: 0,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaletteEntry {
    pub id: u8,
    pub y: u8,
    pub cr: u8,
    pub cb: u8,
    pub alpha: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct Palette<'a> {
    pub id: u8,
    pub version: u8,
    entries: &'a [u8],
}

impl<'a> Palette<'a> {
    pub fn parse(segment: Segment<'a>) -> Result<Self> {
        let mut c = body(segment, 0x14)?;
        let id = c.u8()?;
        let version = c.u8()?;
        let entries = c.take(c.input.len() - c.at)?;
        if entries.len() % 5 != 0 {
            return Err(Error::Invalid("PGS partial palette entry"));
        }
        Ok(Self {
            id,
            version,
            entries,
        })
    }

    /// Duplicate indices and wire order remain observable. Applying a partial
    /// palette update is a display-state operation, not a parser-side repair.
    pub fn entries(&self) -> impl ExactSizeIterator<Item = PaletteEntry> + '_ {
        self.entries.chunks_exact(5).map(|b| PaletteEntry {
            id: b[0],
            y: b[1],
            cr: b[2],
            cb: b[3],
            alpha: b[4],
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectStart {
    /// Wire length includes the four dimension bytes, not the fragment prefix.
    pub data_length: u32,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct ObjectFragment<'a> {
    pub id: u16,
    pub version: u8,
    pub flags: u8,
    pub first: Option<ObjectStart>,
    pub last: bool,
    pub data: &'a [u8],
}

impl<'a> ObjectFragment<'a> {
    pub fn parse(segment: Segment<'a>) -> Result<Self> {
        Self::parse_payload(body(segment, 0x15)?.input)
    }

    /// Parses the ODS payload itself, without fabricating a segment envelope.
    pub fn parse_payload(payload: &'a [u8]) -> Result<Self> {
        let mut c = Cursor {
            input: payload,
            at: 0,
        };
        let id = c.u16()?;
        let version = c.u8()?;
        let flags = c.u8()?;
        let first = if flags & 0x80 != 0 {
            let data_length = c.u24()?;
            if data_length < 4 {
                return Err(Error::Invalid("PGS object length excludes dimensions"));
            }
            Some(ObjectStart {
                data_length,
                width: c.u16()?,
                height: c.u16()?,
            })
        } else {
            None
        };
        let data = c.take(c.input.len() - c.at)?;
        Ok(Self {
            id,
            version,
            flags,
            first,
            last: flags & 0x40 != 0,
            data,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crop {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompositionObject {
    pub id: u16,
    pub window: u8,
    pub flags: u8,
    pub x: u16,
    pub y: u16,
    pub crop: Option<Crop>,
}

impl CompositionObject {
    pub fn forced(&self) -> bool {
        self.flags & 0x40 != 0
    }
}

fn composition_object(c: &mut Cursor<'_>) -> Result<CompositionObject> {
    let id = c.u16()?;
    let window = c.u8()?;
    let flags = c.u8()?;
    let x = c.u16()?;
    let y = c.u16()?;
    let crop = if flags & 0x80 != 0 {
        Some(Crop {
            x: c.u16()?,
            y: c.u16()?,
            width: c.u16()?,
            height: c.u16()?,
        })
    } else {
        None
    };
    Ok(CompositionObject {
        id,
        window,
        flags,
        x,
        y,
        crop,
    })
}

#[derive(Clone, Copy, Debug)]
pub struct Composition<'a> {
    pub width: u16,
    pub height: u16,
    pub frame_rate: u8,
    pub number: u16,
    pub state: u8,
    pub palette_update_flags: u8,
    pub palette_id: u8,
    pub object_count: u8,
    objects: &'a [u8],
}

impl<'a> Composition<'a> {
    pub fn parse(segment: Segment<'a>) -> Result<Self> {
        let mut c = body(segment, 0x16)?;
        let width = c.u16()?;
        let height = c.u16()?;
        let frame_rate = c.u8()?;
        let number = c.u16()?;
        let state = c.u8()?;
        let palette_update_flags = c.u8()?;
        let palette_id = c.u8()?;
        let object_count = c.u8()?;
        let start = c.at;
        for _ in 0..object_count {
            composition_object(&mut c)?;
        }
        c.finish()?;
        Ok(Self {
            width,
            height,
            frame_rate,
            number,
            state,
            palette_update_flags,
            palette_id,
            object_count,
            objects: &c.input[start..],
        })
    }

    pub fn objects(&self) -> CompositionObjects<'a> {
        CompositionObjects {
            cursor: Cursor {
                input: self.objects,
                at: 0,
            },
            remaining: self.object_count,
        }
    }
}

pub struct CompositionObjects<'a> {
    cursor: Cursor<'a>,
    remaining: u8,
}

impl Iterator for CompositionObjects<'_> {
    type Item = Result<CompositionObject>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        Some(composition_object(&mut self.cursor))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = usize::from(self.remaining);
        (n, Some(n))
    }
}

impl ExactSizeIterator for CompositionObjects<'_> {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub id: u8,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct Windows<'a> {
    entries: &'a [u8],
}

impl<'a> Windows<'a> {
    pub fn parse(segment: Segment<'a>) -> Result<Self> {
        let mut c = body(segment, 0x17)?;
        let count = usize::from(c.u8()?);
        let entries = c.take(count * 9)?;
        c.finish()?;
        Ok(Self { entries })
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Window> + '_ {
        self.entries.chunks_exact(9).map(|b| Window {
            id: b[0],
            x: u16::from_be_bytes([b[1], b[2]]),
            y: u16::from_be_bytes([b[3], b[4]]),
            width: u16::from_be_bytes([b[5], b[6]]),
            height: u16::from_be_bytes([b[7], b[8]]),
        })
    }
}
