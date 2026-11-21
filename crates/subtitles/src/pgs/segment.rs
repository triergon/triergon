use triergon_core::{Error, Result};

/// How the segments of a byte stream are framed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Envelope {
    /// Three-byte segment header; clocks belong to the enclosing packet.
    Packet,
    /// Thirteen-byte PG header with raw, wrapping 90 kHz source clocks.
    Sup,
}

/// The clocks of a SUP segment header, raw: 90 kHz ticks that wrap at 2³².
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SupClock {
    /// Presentation time stamp, in 90 kHz ticks.
    pub pts: u32,
    /// Decoding time stamp, in 90 kHz ticks.
    pub dts: u32,
}

/// One segment, borrowed from the input.
#[derive(Clone, Copy, Debug)]
pub struct Segment<'a> {
    /// Byte offset of the segment's first header byte in the input.
    pub offset: usize,
    /// Segment type: 0x14 PDS, 0x15 ODS, 0x16 PCS, 0x17 WDS, 0x80 END; any
    /// other value is kept as it is.
    pub kind: u8,
    /// The SUP header's clocks; `None` in packet framing.
    pub clock: Option<SupClock>,
    /// The whole segment, its headers included.
    pub raw: &'a [u8],
    /// The segment body: the bytes its 16-bit length counts.
    pub payload: &'a [u8],
}

/// The segments of a packet or SUP stream, in order, borrowed without
/// allocating. Segments of unknown types are returned like any other.
///
/// A header that does not read ends the iteration with one error:
/// `Error::Truncated` for a segment cut short, `Error::Invalid` for a SUP
/// header without its `PG` magic. No later header is searched for.
#[derive(Debug)]
pub struct SegmentIter<'a> {
    input: &'a [u8],
    at: usize,
    envelope: Envelope,
    failed: bool,
}

impl<'a> SegmentIter<'a> {
    /// The segments of `input`, framed as `envelope` says.
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

#[derive(Debug)]
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

fn body(segment: Segment<'_>, kind: u8) -> Result<Cursor<'_>> {
    if segment.kind != kind {
        return Err(Error::Invalid("PGS unexpected typed segment"));
    }
    Ok(Cursor {
        input: segment.payload,
        at: 0,
    })
}

/// One 5-byte entry of a PDS, as stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaletteEntry {
    /// The pixel value the entry colours.
    pub id: u8,
    /// Luma (Y').
    pub y: u8,
    /// Red-difference chroma.
    pub cr: u8,
    /// Blue-difference chroma.
    pub cb: u8,
    /// Opacity, as the PGS receiver uses it: 0 is transparent, 255 opaque.
    pub alpha: u8,
}

/// A palette definition segment (PDS).
#[derive(Clone, Copy, Debug)]
pub struct Palette<'a> {
    /// The palette's id, which a composition selects.
    pub id: u8,
    /// The palette version number, as stored.
    pub version: u8,
    entries: &'a [u8],
}

impl<'a> Palette<'a> {
    /// Reads a PDS.
    ///
    /// # Errors
    ///
    /// `Error::Invalid` when the segment is not a PDS (type 0x14) or its
    /// entries are not whole 5-byte records; `Error::Truncated` without the
    /// id and version.
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

    /// The entries in segment order, repeated pixel values included. A PDS
    /// may update only some entries; merging them into a palette is the
    /// receiver's work.
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

/// The fields that open an object's first fragment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectStart {
    /// The object's data bytes over all its fragments, from the width on: the
    /// four size bytes count, the 24-bit length field itself does not. At
    /// least 4.
    pub data_length: u32,
    /// Object width in pixels.
    pub width: u16,
    /// Object height in pixels.
    pub height: u16,
}

/// An object definition segment (ODS): one fragment of an object's
/// run-length coded bitmap.
#[derive(Clone, Copy, Debug)]
pub struct ObjectFragment<'a> {
    /// The object's id, which composition objects refer to.
    pub id: u16,
    /// The object version number, as stored.
    pub version: u8,
    /// Sequence flags: 0x80 first fragment, 0x40 last; other bits as stored.
    pub flags: u8,
    /// The object's size and data length in a first fragment; `None`
    /// otherwise.
    pub first: Option<ObjectStart>,
    /// Whether this is the object's last fragment (flag 0x40).
    pub last: bool,
    /// The fragment's RLE bytes.
    pub data: &'a [u8],
}

impl<'a> ObjectFragment<'a> {
    /// Reads an ODS.
    ///
    /// # Errors
    ///
    /// `Error::Invalid` when the segment is not an ODS (type 0x15) or a first
    /// fragment's data length is below 4; `Error::Truncated` when a field is
    /// cut short.
    pub fn parse(segment: Segment<'a>) -> Result<Self> {
        Self::parse_payload(body(segment, 0x15)?.input)
    }

    /// Reads an ODS body given without its segment header, failing as
    /// [`parse`](Self::parse) does apart from the type check.
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

/// The cropping rectangle of a composition object, as stored. The PGS
/// receiver presents the whole object whatever its crop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crop {
    /// Left column.
    pub x: u16,
    /// Top row.
    pub y: u16,
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
}

/// One composition object of a PCS: which object a display shows, in which
/// window and where.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompositionObject {
    /// The id of the object shown.
    pub id: u16,
    /// The id of the window it is shown in.
    pub window: u8,
    /// Flags: 0x80 cropped (a [`Crop`] follows), 0x40 forced; other bits as
    /// stored.
    pub flags: u8,
    /// Column of the object's top-left corner on the canvas.
    pub x: u16,
    /// Row of the object's top-left corner on the canvas.
    pub y: u16,
    /// The cropping rectangle, present when flag 0x80 is set.
    pub crop: Option<Crop>,
}

impl CompositionObject {
    /// Whether the forced flag (0x40) is set: the object is shown even when
    /// only forced subtitles are.
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

/// A presentation composition segment (PCS): the canvas, the composition
/// state, the palette and the composition objects of a display.
#[derive(Clone, Copy, Debug)]
pub struct Composition<'a> {
    /// Canvas width in pixels.
    pub width: u16,
    /// Canvas height in pixels.
    pub height: u16,
    /// The frame rate code, as stored.
    pub frame_rate: u8,
    /// The composition number, as stored.
    pub number: u16,
    /// The composition state. Either of its two high bits marks an epoch
    /// start or an acquisition point, at which the PGS receiver drops its
    /// cached objects and palettes.
    pub state: u8,
    /// The palette update flag byte, as stored; the PGS receiver does not
    /// read it.
    pub palette_update_flags: u8,
    /// The id of the palette the display's objects are coloured with.
    pub palette_id: u8,
    /// The number of composition objects that follow.
    pub object_count: u8,
    objects: &'a [u8],
}

impl<'a> Composition<'a> {
    /// Reads a PCS and checks its composition objects.
    ///
    /// # Errors
    ///
    /// `Error::Invalid` when the segment is not a PCS (type 0x16) or bytes
    /// follow its last composition object; `Error::Truncated` when a field
    /// or an object is cut short.
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

    /// The composition objects, in segment order. [`parse`](Self::parse)
    /// has checked that each reads.
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

/// The composition objects of a [`Composition`], in segment order.
#[derive(Debug)]
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

/// One window of a WDS: an area of the canvas that objects are shown in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// The window's id, which composition objects refer to.
    pub id: u8,
    /// Column of the window's top-left corner on the canvas.
    pub x: u16,
    /// Row of the window's top-left corner on the canvas.
    pub y: u16,
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
}

/// A window definition segment (WDS).
#[derive(Clone, Copy, Debug)]
pub struct Windows<'a> {
    entries: &'a [u8],
}

impl<'a> Windows<'a> {
    /// Reads a WDS.
    ///
    /// # Errors
    ///
    /// `Error::Invalid` when the segment is not a WDS (type 0x17) or bytes
    /// follow its last window; `Error::Truncated` when its count or the
    /// windows it declares are cut short.
    pub fn parse(segment: Segment<'a>) -> Result<Self> {
        let mut c = body(segment, 0x17)?;
        let count = usize::from(c.u8()?);
        let entries = c.take(count * 9)?;
        c.finish()?;
        Ok(Self { entries })
    }
    /// The windows, in segment order.
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
