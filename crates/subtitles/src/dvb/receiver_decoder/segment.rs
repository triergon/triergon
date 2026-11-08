//! Segment framing and the typed views of the segments the receiver reads
//! (EN 300 743 §7.2).
use crate::dvb::{Depth, Size};

const SYNC: u8 = 0x0f;

/// The segment types the receiver reads (Table 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    PageComposition,
    RegionComposition,
    ClutDefinition,
    ObjectData,
    DisplayDefinition,
    EndOfDisplaySet,
    /// Disparity, alternative CLUT, stuffing and reserved types.
    Other,
}

impl Kind {
    fn of(byte: u8) -> Self {
        match byte {
            0x10 => Self::PageComposition,
            0x11 => Self::RegionComposition,
            0x12 => Self::ClutDefinition,
            0x13 => Self::ObjectData,
            0x14 => Self::DisplayDefinition,
            0x80 => Self::EndOfDisplaySet,
            _ => Self::Other,
        }
    }
}

/// One segment of a codec packet.
#[derive(Clone, Copy, Debug)]
pub(super) struct Segment<'a> {
    pub(super) kind: Kind,
    pub(super) page: u16,
    pub(super) body: &'a [u8],
    /// The packet from the body on: the receiver reads a page composition's
    /// header bytes even past a short body.
    pub(super) tail: &'a [u8],
}

/// Why the segment loop stopped before the end of the packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stop {
    /// A segment claims more bytes than the packet holds.
    Overrun,
}

/// The segments of a packet in order, while a sync byte and a complete
/// header follow; anything after the last one is ignored.
#[derive(Debug)]
pub(super) struct Segments<'a> {
    rest: &'a [u8],
}

impl<'a> Segments<'a> {
    pub(super) fn new(packet: &'a [u8]) -> Self {
        Self { rest: packet }
    }
}

impl<'a> Iterator for Segments<'a> {
    type Item = Result<Segment<'a>, Stop>;

    fn next(&mut self) -> Option<Self::Item> {
        let (&[sync, kind, p0, p1, l0, l1], after) = self.rest.split_first_chunk::<6>()?;
        if sync != SYNC {
            return None;
        }
        let length = usize::from(u16::from_be_bytes([l0, l1]));
        let Some((body, rest)) = after.split_at_checked(length) else {
            self.rest = &[];
            return Some(Err(Stop::Overrun));
        };
        self.rest = rest;
        Some(Ok(Segment {
            kind: Kind::of(kind),
            page: u16::from_be_bytes([p0, p1]),
            body,
            tail: after,
        }))
    }
}

fn be16(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

/// A display definition: its version, the display size and the display
/// window's offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DisplayDefinition {
    pub(super) version: u8,
    pub(super) size: Size,
    pub(super) offset: (u16, u16),
}

impl DisplayDefinition {
    /// `None` when the display size, or a signalled window, is short.
    pub(super) fn parse(body: &[u8]) -> Option<Self> {
        let flags = *body.first()?;
        let offset = match (flags & 0x08 != 0, body.len()) {
            (_, ..5) | (true, ..13) => return None,
            (false, _) => (0, 0),
            (true, _) => (be16(body, 5), be16(body, 9)),
        };
        Some(Self {
            version: flags >> 4,
            size: Size {
                width: u32::from(be16(body, 1)) + 1,
                height: u32::from(be16(body, 3)) + 1,
            },
            offset,
        })
    }
}

/// A page composition: time-out, version, state and region placements.
#[derive(Clone, Copy, Debug)]
pub(super) struct PageComposition<'a> {
    pub(super) timeout: u8,
    pub(super) version: u8,
    pub(super) state: PageState,
    regions: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PageState {
    NormalCase,
    AcquisitionPoint,
    ModeChange,
    Reserved,
}

impl PageState {
    /// Whether the page starts a new epoch, discarding regions, CLUTs and objects.
    pub(super) fn starts_epoch(self) -> bool {
        matches!(self, Self::AcquisitionPoint | Self::ModeChange)
    }
}

/// A region's place on the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Placement {
    pub(super) region: u8,
    pub(super) x: u16,
    pub(super) y: u16,
}

impl<'a> PageComposition<'a> {
    /// The header bytes are read from the packet even past a short body; a
    /// byte past the packet reads as zero.
    pub(super) fn parse(segment: &Segment<'a>) -> Self {
        let byte = |at: usize| segment.tail.get(at).copied().unwrap_or(0);
        let flags = byte(1);
        Self {
            timeout: byte(0),
            version: flags >> 4,
            state: match (flags >> 2) & 3 {
                0 => PageState::NormalCase,
                1 => PageState::AcquisitionPoint,
                2 => PageState::ModeChange,
                _ => PageState::Reserved,
            },
            regions: segment.body.get(2..).unwrap_or_default(),
        }
    }

    pub(super) fn regions(&self) -> impl Iterator<Item = Placement> + 'a {
        self.regions.chunks_exact(6).map(|entry| Placement {
            region: entry[0],
            x: be16(entry, 2),
            y: be16(entry, 4),
        })
    }
}

/// A region composition.
#[derive(Clone, Copy, Debug)]
pub(super) struct RegionComposition<'a> {
    pub(super) id: u8,
    pub(super) fill: bool,
    pub(super) width: u16,
    pub(super) height: u16,
    pub(super) depth: Depth,
    pub(super) clut: u8,
    /// The background pixel code for `depth`.
    pub(super) background: u8,
    objects: &'a [u8],
}

/// An object's place in a region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ObjectPlacement {
    pub(super) object: u16,
    pub(super) x: u16,
    pub(super) y: u16,
}

impl<'a> RegionComposition<'a> {
    /// `None` when the fixed fields are short. An undefined depth reads as 4 bits.
    pub(super) fn parse(body: &'a [u8]) -> Option<Self> {
        let fixed = body.get(..10)?;
        let depth = match (fixed[6] >> 2) & 7 {
            1 => Depth::Two,
            3 => Depth::Eight,
            _ => Depth::Four,
        };
        Some(Self {
            id: fixed[0],
            fill: fixed[1] & 0x08 != 0,
            width: be16(fixed, 2),
            height: be16(fixed, 4),
            depth,
            clut: fixed[7],
            background: match depth {
                Depth::Two => (fixed[9] >> 2) & 3,
                Depth::Four => fixed[9] >> 4,
                Depth::Eight => fixed[8],
            },
            objects: &body[10..],
        })
    }

    /// Object placements; character objects carry two colour bytes more.
    pub(super) fn objects(&self) -> impl Iterator<Item = ObjectPlacement> + 'a {
        let mut rest = self.objects;
        std::iter::from_fn(move || {
            let (entry, after) = rest.split_first_chunk::<6>()?;
            let kind = entry[2] >> 6;
            rest = if matches!(kind, 1 | 2) {
                after.get(2..).unwrap_or_default()
            } else {
                after
            };
            Some(ObjectPlacement {
                object: be16(entry, 0),
                x: be16(entry, 2) & 0x0fff,
                y: be16(entry, 4) & 0x0fff,
            })
        })
    }
}

/// A CLUT definition.
#[derive(Clone, Copy, Debug)]
pub(super) struct ClutDefinition<'a> {
    pub(super) id: u8,
    pub(super) version: u8,
    /// Body bytes after the header.
    entries: usize,
    /// The packet from the first entry on.
    tail: &'a [u8],
}

/// One CLUT entry: its id, depth flags (bit 2: 2-bit, 1: 4-bit, 0: 8-bit)
/// and Y/Cr/Cb/T scaled to 8 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ClutEntry {
    pub(super) id: u8,
    pub(super) depths: u8,
    pub(super) y: u8,
    pub(super) cr: u8,
    pub(super) cb: u8,
    pub(super) t: u8,
}

impl<'a> ClutDefinition<'a> {
    pub(super) fn parse(segment: &Segment<'a>) -> Option<Self> {
        let (&[id, flags], _) = segment.body.split_first_chunk::<2>()?;
        Some(Self {
            id,
            version: flags >> 4,
            entries: segment.body.len() - 2,
            tail: &segment.tail[2..],
        })
    }

    /// Entries, full (8-bit) or reduced (6/4/4/2-bit) resolution. The
    /// receiver reads an entry only while more than four bytes of the body
    /// remain, so a final reduced entry is never read, and a final full one
    /// may take its last byte from past the body (zero past the packet).
    pub(super) fn entries(&self) -> impl Iterator<Item = ClutEntry> + 'a {
        let tail = self.tail;
        let end = self.entries;
        let mut at = 0;
        std::iter::from_fn(move || {
            if end.saturating_sub(at) <= 4 {
                return None;
            }
            let mut bytes = [0; 6];
            for (i, slot) in bytes.iter_mut().enumerate() {
                *slot = tail.get(at + i).copied().unwrap_or(0);
            }
            let [id, flags, a, b, c, d] = bytes;
            let depths = flags >> 5;
            let entry = if flags & 1 != 0 {
                at += 6;
                ClutEntry {
                    id,
                    depths,
                    y: a,
                    cr: b,
                    cb: c,
                    t: d,
                }
            } else {
                at += 4;
                ClutEntry {
                    id,
                    depths,
                    y: a & 0xfc,
                    cr: (a << 6 | (b >> 2 & 0x30)) & 0xf0,
                    cb: b << 2 & 0xf0,
                    t: b << 6,
                }
            };
            Some(entry)
        })
    }
}

/// An object data segment.
#[derive(Clone, Copy, Debug)]
pub(super) struct ObjectData<'a> {
    pub(super) id: u16,
    pub(super) non_modifying: bool,
    pub(super) coding: ObjectCoding<'a>,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum ObjectCoding<'a> {
    /// Interlaced pixel data: the top field and the bottom one, which repeats
    /// the top field when empty.
    Pixels { top: &'a [u8], bottom: &'a [u8] },
    /// Character codes or another coding method.
    Other,
}

/// Why an object data segment cannot be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ObjectFault {
    Short,
    FieldsOverrun,
}

impl<'a> ObjectData<'a> {
    pub(super) fn parse(body: &'a [u8]) -> Result<Self, ObjectFault> {
        let (&[i0, i1, flags], rest) = body.split_first_chunk::<3>().ok_or(ObjectFault::Short)?;
        let coding = if (flags >> 2) & 3 == 0 {
            let (&[t0, t1, b0, b1], fields) =
                rest.split_first_chunk::<4>().ok_or(ObjectFault::Short)?;
            let top = usize::from(u16::from_be_bytes([t0, t1]));
            let bottom = usize::from(u16::from_be_bytes([b0, b1]));
            if top + bottom > fields.len() {
                return Err(ObjectFault::FieldsOverrun);
            }
            let (top, rest) = fields.split_at(top);
            ObjectCoding::Pixels {
                top,
                bottom: &rest[..bottom],
            }
        } else {
            ObjectCoding::Other
        };
        Ok(Self {
            id: u16::from_be_bytes([i0, i1]),
            non_modifying: flags & 0x02 != 0,
            coding,
        })
    }
}
