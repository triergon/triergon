//! MPEG-2 program streams (ISO/IEC 13818-1 §2.5.3) as DVD-Video stores them:
//! packs of PES packets, the subpictures in `private_stream_1` with a leading
//! substream number `0x20 + n`.
use std::ops::Range;

/// `private_stream_1`, which carries subpictures, AC-3, DTS and LPCM.
pub(crate) const PRIVATE_STREAM_1: u8 = 0xbd;
/// `padding_stream`.
pub(crate) const PADDING_STREAM: u8 = 0xbe;
/// `private_stream_2`, which carries DVD navigation packets.
pub(crate) const PRIVATE_STREAM_2: u8 = 0xbf;

/// One element of a program stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unit<'a> {
    /// A pack header or a system header.
    Header,
    Pes(Pes<'a>),
    /// `MPEG_program_end_code`.
    End,
}

/// One PES packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pes<'a> {
    pub stream_id: u8,
    /// The 33-bit presentation time stamp, in 90 kHz ticks.
    pub pts: Option<u64>,
    /// `PES_packet_data_byte`s.
    pub payload: &'a [u8],
    /// Where the packet lies in the window, from its start code.
    pub span: Range<usize>,
}

impl<'a> Pes<'a> {
    /// The subpicture stream number (0 to 31) and data of a `private_stream_1`
    /// packet whose substream is a subpicture.
    pub fn subpicture(&self) -> Option<(u8, &'a [u8])> {
        match self.payload {
            [substream @ 0x20..=0x3f, data @ ..] if self.stream_id == PRIVATE_STREAM_1 => {
                Some((substream - 0x20, data))
            }
            _ => None,
        }
    }
}

/// The units of a window of program-stream bytes, in order.
///
/// Bytes that do not start a unit (a window that begins inside a packet, or
/// damage) are skipped up to the next system start code. Iteration stops at the
/// first unit that does not end inside the window; [`Units::position`] then
/// says where it starts, so a caller can continue once more bytes arrive.
#[derive(Clone, Debug)]
pub struct Units<'a> {
    input: &'a [u8],
    at: usize,
}

impl<'a> Units<'a> {
    pub fn new(input: &'a [u8]) -> Self {
        Self { input, at: 0 }
    }

    /// The offset of the next unit not yet returned.
    pub fn position(&self) -> usize {
        self.at
    }

    /// The offset of the next start code at or after `from` whose code is a
    /// system start code (`0xb9` to `0xff`).
    fn resync(&self, from: usize) -> Option<usize> {
        self.input
            .get(from..)?
            .windows(4)
            .position(|w| w[..3] == [0, 0, 1] && w[3] >= 0xb9)
            .map(|offset| from + offset)
    }

    fn unit(&self, at: usize) -> Option<(Unit<'a>, usize)> {
        let rest = &self.input[at..];
        let code = *rest.get(3)?;
        match code {
            0xb9 => Some((Unit::End, at + 4)),
            0xba => {
                let length = if rest.get(4)? >> 6 == 1 {
                    14 + usize::from(rest.get(13)? & 7)
                } else {
                    12
                };
                (rest.len() >= length).then_some((Unit::Header, at + length))
            }
            _ => {
                let length = 6 + usize::from(u16::from_be_bytes([*rest.get(4)?, *rest.get(5)?]));
                let body = rest.get(6..length)?;
                let end = at + length;
                if code == 0xbb {
                    return Some((Unit::Header, end));
                }
                let (pts, payload) = pes_body(code, body);
                Some((
                    Unit::Pes(Pes {
                        stream_id: code,
                        pts,
                        payload,
                        span: at..end,
                    }),
                    end,
                ))
            }
        }
    }
}

impl<'a> Iterator for Units<'a> {
    type Item = Unit<'a>;

    fn next(&mut self) -> Option<Unit<'a>> {
        let Some(at) = self.resync(self.at) else {
            // A start code may still begin in the last three bytes.
            self.at = self.at.max(self.input.len().saturating_sub(3));
            return None;
        };
        self.at = at;
        let (unit, end) = self.unit(at)?;
        self.at = end;
        Some(unit)
    }
}

/// The PTS and data bytes of a PES packet body (after `PES_packet_length`).
/// Streams without the optional header (padding, `private_stream_2`, the
/// program stream map and directory and a few others) are all data.
fn pes_body(stream_id: u8, body: &[u8]) -> (Option<u64>, &[u8]) {
    if matches!(
        stream_id,
        0xbc | PADDING_STREAM | PRIVATE_STREAM_2 | 0xf0 | 0xf1 | 0xf2 | 0xf8 | 0xff
    ) {
        return (None, body);
    }
    if let [flags0, flags1, length, rest @ ..] = body
        && flags0 >> 6 == 2
    {
        let header = rest.get(..usize::from(*length)).unwrap_or(rest);
        let pts = (flags1 & 0x80 != 0).then(|| timestamp(header)).flatten();
        return (pts, rest.get(usize::from(*length)..).unwrap_or_default());
    }
    mpeg1_body(body)
}

/// An ISO/IEC 11172-1 packet header: stuffing, an optional buffer size and
/// optional time stamps.
fn mpeg1_body(body: &[u8]) -> (Option<u64>, &[u8]) {
    let mut rest = body;
    while let [0xff, tail @ ..] = rest {
        rest = tail;
    }
    if rest.first().is_some_and(|b| b >> 6 == 1) {
        rest = rest.get(2..).unwrap_or_default();
    }
    match rest.first().map(|b| b >> 4) {
        Some(0b0010) => (timestamp(rest), rest.get(5..).unwrap_or_default()),
        Some(0b0011) => (timestamp(rest), rest.get(10..).unwrap_or_default()),
        _ => (None, rest.get(1..).unwrap_or_default()),
    }
}

/// A 33-bit time stamp in its five marker-separated bytes.
fn timestamp(bytes: &[u8]) -> Option<u64> {
    let &[b0, b1, b2, b3, b4, ..] = bytes else {
        return None;
    };
    Some(
        u64::from(b0 >> 1 & 7) << 30
            | u64::from(b1) << 22
            | u64::from(b2 >> 1) << 15
            | u64::from(b3) << 7
            | u64::from(b4 >> 1),
    )
}
