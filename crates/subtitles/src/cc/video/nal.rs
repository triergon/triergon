//! H.264 and HEVC NAL units and SEI messages, as far as caption data needs them.
//!
//! An access unit's NAL units are delimited by start codes (Annex B, as in
//! MPEG-TS) or prefixed by their length (as in Matroska and MP4). SEI NAL units
//! (H.264 type 6, HEVC types 39 and 40) hold SEI messages: a type and a size,
//! each coded as a run of `0xFF` bytes and a final byte, then the payload. NAL
//! unit payloads carry emulation prevention bytes, which the readers here
//! remove. The sibling `avc` and `hevc` modules read the parameter sets and slice
//! headers.

use triergon_bitstream::{BitError, BitSource};
use triergon_core::{Budget, BudgetVec};
use triergon_framing::{
    avc, hevc,
    nal::{AnnexBUnits, Codec as Coding, skip_vui_to_timing, split_length_prefixed},
};

/// How an access unit delimits its NAL units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// Start codes (`00 00 01`, Annex B).
    StartCodes,
    /// A big-endian length of 1 to 4 bytes before each NAL unit.
    Lengths(u8),
}

impl Framing {
    /// The framing of the samples an `avcC` or `hvcC` decoder configuration
    /// record (ISO/IEC 14496-15) describes: lengths of
    /// `lengthSizeMinusOne + 1` bytes, or start codes when the record is too
    /// short to say.
    #[must_use]
    pub fn of_configuration(codec: Codec, record: &[u8]) -> Self {
        let at = match codec {
            Codec::H264 => 4,
            Codec::Hevc => 21,
        };
        record
            .get(at)
            .map_or(Self::StartCodes, |b| Self::Lengths((b & 3) + 1))
    }
}

/// The video coding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

impl Codec {
    /// A NAL unit's `nal_unit_type`.
    #[must_use]
    pub fn nal_type(self, nal: &[u8]) -> Option<u8> {
        let &first = nal.first()?;
        Some(match self {
            Self::H264 => first & 0x1f,
            Self::Hevc => (first >> 1) & 0x3f,
        })
    }

    /// The NAL unit after its header.
    #[must_use]
    pub fn payload(self, nal: &[u8]) -> &[u8] {
        let header = match self {
            Self::H264 => 1,
            Self::Hevc => 2,
        };
        nal.get(header..).unwrap_or_default()
    }

    /// True for a coded slice.
    #[must_use]
    pub fn is_slice(self, nal_type: u8) -> bool {
        match self {
            Self::H264 => matches!(nal_type, 1..=5),
            Self::Hevc => hevc::is_slice(nal_type),
        }
    }

    /// True for an SEI NAL unit (HEVC: prefix or suffix).
    #[must_use]
    pub fn is_sei(self, nal_type: u8) -> bool {
        match self {
            Self::H264 => nal_type == avc::SEI,
            Self::Hevc => matches!(nal_type, hevc::PREFIX_SEI | hevc::SUFFIX_SEI),
        }
    }
}

/// VUI timing: a clock tick is `num_units_in_tick / time_scale` seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Timing {
    pub num_units_in_tick: u32,
    pub time_scale: u32,
}

/// The VUI's timing, `None` without it or with a zero field.
pub(crate) fn vui_timing(
    bits: &mut impl BitSource,
    codec: Coding,
) -> Result<Option<Timing>, BitError> {
    if !skip_vui_to_timing(bits, codec)? {
        return Ok(None);
    }
    let num_units_in_tick = bits.read(32)?;
    let time_scale = bits.read(32)?;
    Ok((num_units_in_tick > 0 && time_scale > 0).then_some(Timing {
        num_units_in_tick,
        time_scale,
    }))
}

/// The payload of the last parameter set of a kind read, so that a repeated
/// one is not read again.
#[derive(Debug)]
pub(crate) struct LastPayload {
    bytes: BudgetVec<u8>,
    valid: bool,
}

impl LastPayload {
    pub(crate) fn new(pool: &Budget) -> Self {
        Self {
            bytes: BudgetVec::new(pool),
            valid: false,
        }
    }

    /// True when `payload` repeats the last one remembered.
    pub(crate) fn repeats(&self, payload: &[u8]) -> bool {
        self.valid && self.bytes.as_slice() == payload
    }

    pub(crate) fn remember(&mut self, payload: &[u8]) -> triergon_core::Result<()> {
        self.valid = false;
        self.bytes.clear();
        self.bytes.try_extend_from_slice(payload)?;
        self.valid = true;
        Ok(())
    }

    pub(crate) fn forget(&mut self) {
        self.valid = false;
    }
}

/// The NAL units of an access unit, each from its header byte on. A length
/// running past the access unit ends it; lengths of other than 1 to 4 bytes
/// frame nothing.
pub(crate) fn nal_units(access_unit: &[u8], framing: Framing) -> impl Iterator<Item = &[u8]> {
    let start_codes = (framing == Framing::StartCodes).then(|| AnnexBUnits::new(access_unit));
    let width = match framing {
        Framing::Lengths(size @ 1..=4) => usize::from(size),
        _ => 0,
    };
    let mut rest = access_unit;
    let lengths = std::iter::from_fn(move || {
        let (unit, tail) = split_length_prefixed(rest, width).filter(|_| width > 0)?;
        rest = tail;
        Some(unit)
    });
    start_codes.into_iter().flatten().chain(lengths)
}

/// The NAL units an `avcC` or `hvcC` decoder configuration record lists, in
/// record order: an `avcC` record's SPSs then PPSs, each of an `hvcC`
/// record's arrays in turn. A list cut short ends them.
pub(crate) fn configuration_units(codec: Codec, record: &[u8]) -> impl Iterator<Item = &[u8]> {
    // Where the next unit's 16-bit length is, the units left in the current
    // list and the lists after it.
    let (mut at, mut left, mut lists) = match codec {
        Codec::H264 => (6, record.get(5).map_or(0, |b| usize::from(b & 0x1f)), 1),
        Codec::Hevc => (23, 0, record.get(22).map_or(0, |&b| usize::from(b))),
    };
    std::iter::from_fn(move || {
        while left == 0 {
            if lists == 0 {
                return None;
            }
            lists -= 1;
            // An `avcC` record's PPS count is one byte; each `hvcC` array
            // starts with its NAL type byte and a 16-bit count.
            let count = match codec {
                Codec::H264 => record.get(at).map(|&b| (1, usize::from(b))),
                Codec::Hevc => record
                    .get(at + 1..at + 3)
                    .map(|n| (3, usize::from(u16::from_be_bytes([n[0], n[1]])))),
            };
            let Some((header, count)) = count else {
                lists = 0;
                return None;
            };
            at += header;
            left = count;
        }
        let unit = record.get(at..at + 2).and_then(|len| {
            let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
            record.get(at + 2..at + 2 + len)
        });
        if let Some(unit) = unit {
            at += 2 + unit.len();
            left -= 1;
        } else {
            (left, lists) = (0, 0);
        }
        unit
    })
}

/// `user_data_registered_itu_t_t35`.
pub(crate) const SEI_USER_DATA_REGISTERED: u32 = 4;
/// `recovery_point` (H.264).
pub(crate) const SEI_RECOVERY_POINT: u32 = 6;

/// One SEI message: its type and its payload with emulation prevention
/// removed, copied into `payload` up to its capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SeiMessage<'a> {
    pub payload_type: u32,
    /// The payload, possibly cut to the reader's buffer.
    pub payload: &'a [u8],
}

/// Reads the SEI messages of an SEI NAL unit payload (after the NAL header),
/// calling `each` with every message whose type `wanted` accepts; the
/// payloads of the others are passed over. A message cut short ends the
/// reading; a payload longer than `buffer` is cut to it.
pub(crate) fn sei_messages(
    payload: &[u8],
    buffer: &mut [u8],
    wanted: impl Fn(u32) -> bool,
    mut each: impl FnMut(SeiMessage<'_>),
) {
    let mut bytes = Unescaped::new(payload);
    while let Some(first) = bytes.next() {
        // The rbsp_trailing_bits end the message list.
        if first == 0x80 && bytes.is_empty() {
            return;
        }
        let Some(payload_type) = coded_value(first, &mut bytes) else {
            return;
        };
        let Some(payload_size) = bytes.next().and_then(|b| coded_value(b, &mut bytes)) else {
            return;
        };
        if !wanted(payload_type) {
            for _ in 0..payload_size {
                if bytes.next().is_none() {
                    return;
                }
            }
            continue;
        }
        let mut kept = 0;
        for _ in 0..payload_size {
            let Some(b) = bytes.next() else { return };
            if let Some(slot) = buffer.get_mut(kept) {
                *slot = b;
                kept += 1;
            }
        }
        each(SeiMessage {
            payload_type,
            payload: &buffer[..kept],
        });
    }
}

/// The bytes of a NAL unit payload without its emulation prevention bytes,
/// read leniently: `00 00 03` drops the `03`, and an `03` after two zero bytes
/// followed by a byte above `03` ends the payload.
struct Unescaped<'a> {
    rest: &'a [u8],
    zeroes: u8,
}

impl<'a> Unescaped<'a> {
    fn new(payload: &'a [u8]) -> Self {
        Self {
            rest: payload,
            zeroes: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }
}

impl Iterator for Unescaped<'_> {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        let (&first, rest) = self.rest.split_first()?;
        let (byte, rest) = if first == 3 && self.zeroes == 2 {
            let (&next, rest) = rest.split_first()?;
            if next > 3 {
                self.rest = &[];
                return None;
            }
            (next, rest)
        } else {
            (first, rest)
        };
        self.rest = rest;
        self.zeroes = if byte == 0 {
            (self.zeroes + 1).min(2)
        } else {
            0
        };
        Some(byte)
    }
}

/// An SEI type or size from its first byte on: `0xFF` bytes each adding 255,
/// then the last byte.
fn coded_value(first: u8, bytes: &mut Unescaped<'_>) -> Option<u32> {
    let mut value = u32::from(first);
    let mut last = first;
    while last == 0xff {
        last = bytes.next()?;
        value = value.saturating_add(u32::from(last));
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_access_units() {
        let annex_b = [
            0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x06, 0x05, 0, 0, 1, 0x65, 0x88,
        ];
        let units: Vec<_> = nal_units(&annex_b, Framing::StartCodes).collect();
        assert_eq!(units, [&[0x09, 0xf0][..], &[0x06, 0x05], &[0x65, 0x88]]);
        let lengths = [0, 2, 0x09, 0xf0, 0, 1, 0x65];
        let units: Vec<_> = nal_units(&lengths, Framing::Lengths(2)).collect();
        assert_eq!(units, [&[0x09, 0xf0][..], &[0x65]]);
    }

    #[test]
    fn reads_sei_messages_through_emulation_prevention() {
        // Type 4, size 5: 00 00 03 01 ff -> 00 00 01 ff; then type 5, size 1.
        let sei = [4, 4, 0, 0, 3, 1, 0xff, 5, 1, 0x42, 0x80];
        let mut buffer = [0; 16];
        let mut seen = Vec::new();
        sei_messages(
            &sei,
            &mut buffer,
            |_| true,
            |m| {
                seen.push((m.payload_type, m.payload.to_vec()));
            },
        );
        assert_eq!(seen, [(4, vec![0, 0, 1, 0xff]), (5, vec![0x42])]);
        assert_eq!(
            Codec::Hevc.nal_type(&[0x4e, 0x01, 4]),
            Some(hevc::PREFIX_SEI)
        );
        assert_eq!(Codec::Hevc.payload(&[0x4e, 0x01, 4]), [4]);
        assert_eq!(Codec::H264.nal_type(&[0x65, 0]), Some(avc::IDR));
    }

    #[test]
    fn nal_lengths_past_the_access_unit_or_of_no_size_frame_nothing() {
        let access_unit = [0xff, 0xff, 0xff, 0xff, 0x00];
        assert_eq!(nal_units(&access_unit, Framing::Lengths(4)).count(), 0);
        for size in [0, 5, 255] {
            assert_eq!(nal_units(&access_unit, Framing::Lengths(size)).count(), 0);
        }
        // A complete unit before an overlong length is still read.
        let units: Vec<_> = nal_units(&[0, 1, 0x06, 0xff, 0xff, 9], Framing::Lengths(2)).collect();
        assert_eq!(units, [&[0x06][..]]);
    }
}
