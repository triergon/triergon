//! H.264 and HEVC NAL units and SEI messages, as far as caption data needs them.
//!
//! An access unit's NAL units are delimited by start codes (Annex B, as in
//! MPEG-TS) or prefixed by their length (as in Matroska and MP4). SEI NAL units
//! (H.264 type 6, HEVC types 39 and 40) hold SEI messages: a type and a size,
//! each coded as a run of `0xFF` bytes and a final byte, then the payload. NAL
//! unit payloads carry emulation prevention bytes, which the readers here
//! remove. Besides SEI messages this reads an H.264 slice's type and the VUI
//! timing of a sequence parameter set.

use triergon_bitstream::{BitError, BitSource, Rbsp};

/// How an access unit delimits its NAL units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// Start codes (`00 00 01`, Annex B).
    StartCodes,
    /// A big-endian length of 1 to 4 bytes before each NAL unit.
    Lengths(u8),
}

/// The video coding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

/// H.264 NAL unit types (Table 7-1).
pub mod h264 {
    pub const SLICE: u8 = 1;
    pub const IDR: u8 = 5;
    pub const SEI: u8 = 6;
    pub const SPS: u8 = 7;
}

/// HEVC NAL unit types (Table 7-1).
pub mod hevc {
    pub const RASL_N: u8 = 8;
    pub const RASL_R: u8 = 9;
    pub const BLA_W_LP: u8 = 16;
    pub const BLA_N_LP: u8 = 18;
    pub const IDR_W_RADL: u8 = 19;
    pub const IDR_N_LP: u8 = 20;
    pub const CRA: u8 = 21;
    pub const IRAP_LAST: u8 = 23;
    pub const SPS: u8 = 33;
    pub const EOS: u8 = 36;
    pub const PREFIX_SEI: u8 = 39;
    pub const SUFFIX_SEI: u8 = 40;
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
            Self::Hevc => nal_type < 32,
        }
    }

    /// True for an SEI NAL unit (HEVC: prefix or suffix).
    #[must_use]
    pub fn is_sei(self, nal_type: u8) -> bool {
        match self {
            Self::H264 => nal_type == h264::SEI,
            Self::Hevc => matches!(nal_type, hevc::PREFIX_SEI | hevc::SUFFIX_SEI),
        }
    }
}

/// `slice_type` of an H.264 slice (Table 7-6), whether or not every slice of the
/// picture has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliceType {
    P,
    B,
    I,
    Sp,
    Si,
}

impl SliceType {
    #[must_use]
    pub const fn is_intra(self) -> bool {
        matches!(self, Self::I | Self::Si)
    }
}

/// The `slice_type` of an H.264 coded slice NAL unit.
#[must_use]
pub fn h264_slice_type(nal: &[u8]) -> Option<SliceType> {
    let mut bits = Rbsp::lenient(Codec::H264.payload(nal));
    bits.ue().ok()?; // first_mb_in_slice
    Some(match bits.ue().ok()? % 5 {
        0 => SliceType::P,
        1 => SliceType::B,
        2 => SliceType::I,
        3 => SliceType::Sp,
        _ => SliceType::Si,
    })
}

/// VUI timing: a clock tick is `num_units_in_tick / time_scale` seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    pub num_units_in_tick: u32,
    pub time_scale: u32,
}

/// The VUI timing of a sequence parameter set NAL unit (H.264 7.3.2.1.1 and
/// E.1.1, HEVC 7.3.2.2 and E.2.1), `None` without one.
#[must_use]
pub fn sps_timing(codec: Codec, nal: &[u8]) -> Option<Timing> {
    let mut bits = Rbsp::lenient(codec.payload(nal));
    match codec {
        Codec::H264 => h264_sps_timing(&mut bits).ok().flatten(),
        Codec::Hevc => hevc_sps_timing(&mut bits).ok().flatten(),
    }
}

type Bits<'a> = Rbsp<'a>;

/// A parameter set FFmpeg's decoder does not take: cut short or out of range.
struct Invalid;

impl From<BitError> for Invalid {
    fn from(_: BitError) -> Self {
        Self
    }
}

/// Passes over a scaling list. A delta outside -128..=127 makes the parameter
/// set invalid, as FFmpeg's decoder rejects it.
fn skip_scaling_list(bits: &mut Bits<'_>, size: usize) -> Result<(), Invalid> {
    let (mut last, mut next) = (8i32, 8i32);
    for _ in 0..size {
        if next != 0 {
            let delta = bits.se()?;
            if !(-128..=127).contains(&delta) {
                return Err(Invalid);
            }
            next = (last + delta) & 0xff;
        }
        last = if next == 0 { last } else { next };
    }
    Ok(())
}

fn h264_sps_timing(bits: &mut Bits<'_>) -> Result<Option<Timing>, Invalid> {
    let profile = bits.read(8)?;
    bits.read(16)?; // constraint flags, level_idc
    bits.ue()?; // seq_parameter_set_id
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma_format_idc = bits.ue()?;
        if chroma_format_idc == 3 {
            bits.flag()?;
        }
        bits.ue()?;
        bits.ue()?;
        bits.flag()?;
        if bits.flag()? {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if bits.flag()? {
                    skip_scaling_list(bits, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    bits.ue()?; // log2_max_frame_num_minus4
    match bits.ue()? {
        0 => {
            bits.ue()?;
        }
        1 => {
            bits.flag()?;
            bits.se()?;
            bits.se()?;
            for _ in 0..bits.ue()?.min(255) {
                bits.se()?;
            }
        }
        _ => {}
    }
    bits.ue()?; // max_num_ref_frames
    bits.flag()?;
    bits.ue()?;
    bits.ue()?;
    if !bits.flag()? {
        bits.flag()?; // mb_adaptive_frame_field_flag
    }
    bits.flag()?;
    if bits.flag()? {
        for _ in 0..4 {
            bits.ue()?;
        }
    }
    if !bits.flag()? {
        return Ok(None);
    }
    Ok(skip_vui_until_timing(bits, Codec::H264)?)
}

/// A VUI's fields before its timing information; then the timing.
fn skip_vui_until_timing(bits: &mut Bits<'_>, codec: Codec) -> Result<Option<Timing>, BitError> {
    if bits.flag()? && bits.read(8)? == 255 {
        bits.read(32)?; // sar_width, sar_height
    }
    if bits.flag()? {
        bits.flag()?; // overscan_appropriate_flag
    }
    if bits.flag()? {
        bits.read(4)?; // video_format, video_full_range_flag
        if bits.flag()? {
            bits.read(24)?;
        }
    }
    if bits.flag()? {
        bits.ue()?;
        bits.ue()?;
    }
    if codec == Codec::Hevc {
        bits.read(3)?; // neutral_chroma, field_seq, frame_field_info_present
        if bits.flag()? {
            for _ in 0..4 {
                bits.ue()?;
            }
        }
    }
    if !bits.flag()? {
        return Ok(None);
    }
    let num_units_in_tick = bits.read(32)?;
    let time_scale = bits.read(32)?;
    Ok((num_units_in_tick > 0 && time_scale > 0).then_some(Timing {
        num_units_in_tick,
        time_scale,
    }))
}

fn hevc_sps_timing(bits: &mut Bits<'_>) -> Result<Option<Timing>, Invalid> {
    bits.read(4)?; // sps_video_parameter_set_id
    let sub_layers = bits.read(3)? as usize; // sps_max_sub_layers_minus1
    bits.flag()?;
    // profile_tier_level(1, sub_layers)
    bits.read(32)?;
    bits.read(32)?;
    bits.read(24)?;
    bits.read(8)?; // general_level_idc
    let mut present = [(false, false); 8];
    for flags in present.iter_mut().take(sub_layers) {
        *flags = (bits.flag()?, bits.flag()?);
    }
    if sub_layers > 0 {
        for _ in sub_layers..8 {
            bits.read(2)?;
        }
    }
    for &(profile, level) in present.iter().take(sub_layers) {
        if profile {
            bits.read(32)?;
            bits.read(32)?;
            bits.read(24)?;
        }
        if level {
            bits.read(8)?;
        }
    }
    bits.ue()?; // sps_seq_parameter_set_id
    if bits.ue()? == 3 {
        bits.flag()?;
    }
    bits.ue()?;
    bits.ue()?;
    if bits.flag()? {
        for _ in 0..4 {
            bits.ue()?;
        }
    }
    bits.ue()?;
    bits.ue()?;
    // log2_max_pic_order_cnt_lsb, summed in 32 bits and limited to 16 as
    // FFmpeg's decoder does.
    let poc_lsb_bits = bits.ue()?.wrapping_add(4);
    if poc_lsb_bits > 16 {
        return Err(Invalid);
    }
    let all_layers = bits.flag()?;
    for _ in (if all_layers { 0 } else { sub_layers })..=sub_layers {
        for _ in 0..3 {
            bits.ue()?;
        }
    }
    for _ in 0..6 {
        bits.ue()?; // block sizes and transform hierarchy depths
    }
    if bits.flag()? && bits.flag()? {
        // scaling_list_data()
        for size in 0..4 {
            for _ in (0..6).step_by(if size == 3 { 3 } else { 1 }) {
                if bits.flag()? {
                    let coefficients = 64.min(1 << (4 + (size << 1)));
                    if size > 1 {
                        bits.se()?;
                    }
                    for _ in 0..coefficients {
                        bits.se()?;
                    }
                } else {
                    bits.ue()?;
                }
            }
        }
    }
    bits.read(2)?; // amp_enabled_flag, sample_adaptive_offset_enabled_flag
    if bits.flag()? {
        bits.read(8)?;
        bits.ue()?;
        bits.ue()?;
        bits.flag()?;
    }
    let sets = bits.ue()?.min(64) as usize;
    let mut delta_pocs = [0u32; 64];
    for i in 0..sets {
        let predicted = i != 0 && bits.flag()?;
        delta_pocs[i] = if predicted {
            bits.flag()?;
            bits.ue()?;
            let mut count = 0;
            for _ in 0..=delta_pocs[i - 1] {
                let used = bits.flag()?;
                if used || bits.flag()? {
                    count += 1;
                }
            }
            count
        } else {
            let negative = bits.ue()?.min(16);
            let positive = bits.ue()?.min(16);
            for _ in 0..negative + positive {
                bits.ue()?;
                bits.flag()?;
            }
            negative + positive
        };
    }
    if bits.flag()? {
        for _ in 0..bits.ue()?.min(32) {
            bits.read(poc_lsb_bits)?;
            bits.flag()?;
        }
    }
    bits.read(2)?; // sps_temporal_mvp_enabled_flag, strong_intra_smoothing_enabled_flag
    if !bits.flag()? {
        return Ok(None);
    }
    Ok(skip_vui_until_timing(bits, Codec::Hevc)?)
}

/// The NAL units of an access unit, each from its header byte on. A length
/// running past the access unit ends it; lengths of other than 1 to 4 bytes
/// frame nothing.
pub fn nal_units(access_unit: &[u8], framing: Framing) -> impl Iterator<Item = &[u8]> {
    let mut rest = access_unit;
    std::iter::from_fn(move || match framing {
        Framing::Lengths(size @ 1..=4) => {
            let (prefix, tail) = rest.split_at_checked(usize::from(size))?;
            let length = prefix
                .iter()
                .fold(0usize, |n, &b| (n << 8) | usize::from(b));
            let (unit, tail) = tail.split_at_checked(length)?;
            rest = tail;
            Some(unit)
        }
        Framing::Lengths(_) => None,
        Framing::StartCodes => loop {
            let start = find_start_code(rest, 0)?;
            let body = &rest[start + 3..];
            let end = find_start_code(body, 0).unwrap_or(body.len());
            // A zero byte before the next start code belongs to it.
            let mut unit = &body[..end];
            while let [head @ .., 0] = unit {
                unit = head;
            }
            rest = &body[end..];
            if !unit.is_empty() {
                return Some(unit);
            }
        },
    })
}

fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    data.get(from..)?
        .windows(3)
        .position(|w| w == [0, 0, 1])
        .map(|n| from + n)
}

/// `user_data_registered_itu_t_t35`.
pub const SEI_USER_DATA_REGISTERED: u32 = 4;
/// `recovery_point` (H.264).
pub const SEI_RECOVERY_POINT: u32 = 6;

/// One SEI message: its type and its payload with emulation prevention
/// removed, copied into `payload` up to its capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeiMessage<'a> {
    pub payload_type: u32,
    /// The payload size the message states.
    pub payload_size: usize,
    /// The payload, possibly cut to the reader's buffer.
    pub payload: &'a [u8],
}

/// Reads the SEI messages of an SEI NAL unit payload (after the NAL header),
/// calling `each` with every message. A message cut short ends the reading;
/// a payload longer than `buffer` is cut to it.
pub fn sei_messages(payload: &[u8], buffer: &mut [u8], mut each: impl FnMut(SeiMessage<'_>)) {
    let mut bits = Rbsp::lenient(payload);
    while let Some(first) = byte(&mut bits) {
        // The rbsp_trailing_bits end the message list.
        if first == 0x80 && bits.stop_bit_consumed() {
            return;
        }
        let Some(payload_type) = coded_value(first, &mut bits) else {
            return;
        };
        let Some(payload_size) = byte(&mut bits).and_then(|b| coded_value(b, &mut bits)) else {
            return;
        };
        let mut kept = 0;
        for _ in 0..payload_size {
            let Some(b) = byte(&mut bits) else { return };
            if let Some(slot) = buffer.get_mut(kept) {
                *slot = b;
                kept += 1;
            }
        }
        each(SeiMessage {
            payload_type,
            payload_size: payload_size as usize,
            payload: &buffer[..kept],
        });
    }
}

fn byte(bits: &mut Bits<'_>) -> Option<u8> {
    bits.read(8).ok().map(|b| b as u8)
}

/// An SEI type or size from its first byte on: `0xFF` bytes each adding 255,
/// then the last byte.
fn coded_value(first: u8, bits: &mut Bits<'_>) -> Option<u32> {
    let mut value = u32::from(first);
    let mut last = first;
    while last == 0xff {
        last = byte(bits)?;
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
        sei_messages(&sei, &mut buffer, |m| {
            seen.push((m.payload_type, m.payload.to_vec()));
        });
        assert_eq!(seen, [(4, vec![0, 0, 1, 0xff]), (5, vec![0x42])]);
        assert_eq!(
            Codec::Hevc.nal_type(&[0x4e, 0x01, 4]),
            Some(hevc::PREFIX_SEI)
        );
        assert_eq!(Codec::Hevc.payload(&[0x4e, 0x01, 4]), [4]);
        assert_eq!(Codec::H264.nal_type(&[0x65, 0]), Some(h264::IDR));
        // first_mb_in_slice 0, slice_type 7 (I).
        assert_eq!(h264_slice_type(&[0x65, 0b1000_1000]), Some(SliceType::I));
    }
}
