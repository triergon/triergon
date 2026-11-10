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
use triergon_framing::{
    AnnexBUnits, Codec as Coding, avc, hevc, skip_avc_scaling_matrix, skip_hevc_profile_tier_level,
    skip_vui_to_timing, split_length_prefixed,
};

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

/// `slice_type` of an H.264 slice (Table 7-6), whether or not every slice of the
/// picture has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SliceType {
    P,
    B,
    I,
    Sp,
    Si,
}

impl SliceType {
    #[must_use]
    pub(crate) const fn is_intra(self) -> bool {
        matches!(self, Self::I | Self::Si)
    }
}

/// The `slice_type` of an H.264 coded slice NAL unit.
#[must_use]
pub(crate) fn h264_slice_type(nal: &[u8]) -> Option<SliceType> {
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
pub(crate) struct Timing {
    pub num_units_in_tick: u32,
    pub time_scale: u32,
}

/// The VUI timing of a sequence parameter set NAL unit (H.264 7.3.2.1.1 and
/// E.1.1, HEVC 7.3.2.2 and E.2.1), `None` without one.
#[must_use]
pub(crate) fn sps_timing(codec: Codec, nal: &[u8]) -> Option<Timing> {
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
        skip_avc_scaling_matrix(bits, chroma_format_idc).map_err(|_| Invalid)?;
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
    Ok(vui_timing(bits, Coding::Avc)?)
}

/// The VUI's timing, `None` without it or with a zero field.
fn vui_timing(bits: &mut Bits<'_>, codec: Coding) -> Result<Option<Timing>, BitError> {
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

fn hevc_sps_timing(bits: &mut Bits<'_>) -> Result<Option<Timing>, Invalid> {
    bits.read(4)?; // sps_video_parameter_set_id
    let sub_layers = bits.read(3)? as usize; // sps_max_sub_layers_minus1
    bits.flag()?;
    skip_hevc_profile_tier_level(bits, sub_layers as u8)?;
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
    Ok(vui_timing(bits, Coding::Hevc)?)
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

/// `user_data_registered_itu_t_t35`.
pub(crate) const SEI_USER_DATA_REGISTERED: u32 = 4;
/// `recovery_point` (H.264).
pub(crate) const SEI_RECOVERY_POINT: u32 = 6;

/// One SEI message: its type and its payload with emulation prevention
/// removed, copied into `payload` up to its capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SeiMessage<'a> {
    pub payload_type: u32,
    /// The payload size the message states.
    pub payload_size: usize,
    /// The payload, possibly cut to the reader's buffer.
    pub payload: &'a [u8],
}

/// Reads the SEI messages of an SEI NAL unit payload (after the NAL header),
/// calling `each` with every message. A message cut short ends the reading;
/// a payload longer than `buffer` is cut to it.
pub(crate) fn sei_messages(
    payload: &[u8],
    buffer: &mut [u8],
    mut each: impl FnMut(SeiMessage<'_>),
) {
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
    use triergon_harness_protocol::unhex;

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
        assert_eq!(Codec::H264.nal_type(&[0x65, 0]), Some(avc::IDR));
        // first_mb_in_slice 0, slice_type 7 (I).
        assert_eq!(h264_slice_type(&[0x65, 0b1000_1000]), Some(SliceType::I));
    }

    #[test]
    fn sequence_parameter_sets_state_the_frame_rate() {
        // From libx264 (High 4:4:4) and libx265 encodes at 30000/1001 Hz.
        let h264 = unhex("67f4000a919a623d808800001f4800075300789129c0");
        let hevc = unhex(
            "4201010408000003009e0800000300001e90041062cb2225c995e02dc080800100000303e90000753008",
        );
        assert_eq!(
            sps_timing(Codec::H264, &h264),
            Some(Timing {
                num_units_in_tick: 1001,
                time_scale: 60000
            })
        );
        assert_eq!(
            sps_timing(Codec::Hevc, &hevc),
            Some(Timing {
                num_units_in_tick: 1001,
                time_scale: 30000
            })
        );
    }

    #[test]
    fn hevc_poc_lsb_lengths_are_limited_to_16_bits_as_ffmpeg_sums_them() {
        let timing = Some(Timing {
            num_units_in_tick: 1001,
            time_scale: 60_000,
        });
        assert_eq!(sps_timing(Codec::Hevc, &hevc_sps(12)), timing);
        assert_eq!(sps_timing(Codec::Hevc, &hevc_sps(13)), None);
        assert_eq!(sps_timing(Codec::Hevc, &hevc_sps(0xffff_fffb)), None);
        // FFmpeg sums in 32 bits: 0xFFFFFFFE codes two-bit LSBs.
        assert_eq!(sps_timing(Codec::Hevc, &hevc_sps(0xffff_fffe)), timing);
    }

    #[test]
    fn an_h264_scaling_delta_out_of_range_rejects_the_parameter_set() {
        // Profile 100 with a first scaling list delta of i32::MAX.
        let access_unit = [
            0, 0, 1, 0x67, 0x64, 0, 0, 0x5d, 0x80, 0, 0, 0, 0xff, 0xff, 0xff, 0xfe,
        ];
        let nal = nal_units(&access_unit, Framing::StartCodes)
            .next()
            .expect("NAL unit");
        assert_eq!(sps_timing(Codec::H264, nal), None);
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

    /// Writes RBSP bits and frames them as a NAL unit.
    #[derive(Default)]
    struct BitWriter {
        bits: Vec<bool>,
    }

    impl BitWriter {
        fn put(&mut self, value: u64, width: u32) -> &mut Self {
            self.bits
                .extend((0..width).rev().map(|bit| value >> bit & 1 == 1));
            self
        }

        fn flag(&mut self, set: bool) -> &mut Self {
            self.put(u64::from(set), 1)
        }

        fn ue(&mut self, value: u32) -> &mut Self {
            let code = u64::from(value) + 1;
            let width = 64 - code.leading_zeros();
            self.put(0, width - 1).put(code, width)
        }

        /// The NAL unit: `header`, the bits with their stop bit, and emulation
        /// prevention bytes.
        fn nal(&mut self, header: &[u8]) -> Vec<u8> {
            self.flag(true);
            while !self.bits.len().is_multiple_of(8) {
                self.flag(false);
            }
            let mut nal = header.to_vec();
            let mut zeros = 0;
            for byte in self.bits.chunks(8) {
                let byte = byte.iter().fold(0u8, |b, &bit| b << 1 | u8::from(bit));
                if zeros == 2 && byte <= 3 {
                    nal.push(3);
                    zeros = 0;
                }
                zeros = if byte == 0 { zeros + 1 } else { 0 };
                nal.push(byte);
            }
            nal
        }
    }

    /// An HEVC sequence parameter set with VUI timing 1001/60000 whose
    /// `log2_max_pic_order_cnt_lsb_minus4` is coded as `poc_lsb_minus4`, with one
    /// long-term reference picture whose POC LSBs take that many bits plus four.
    fn hevc_sps(poc_lsb_minus4: u32) -> Vec<u8> {
        let mut bits = BitWriter::default();
        bits.put(0, 4).put(0, 3).flag(true); // VPS, one sub-layer, nesting
        bits.put(0x0160_0000, 32).put(0, 32).put(0, 24).put(93, 8); // profile_tier_level
        bits.ue(0).ue(1).ue(1920).ue(1080).flag(false); // id, 4:2:0, size, no window
        bits.ue(0).ue(0).ue(poc_lsb_minus4);
        bits.flag(true).ue(4).ue(0).ue(0); // sub-layer ordering
        for _ in 0..6 {
            bits.ue(0); // block sizes and transform depths
        }
        bits.flag(false).put(0, 2).flag(false); // no scaling lists, AMP, SAO, PCM
        bits.ue(0); // no short-term reference picture sets
        let poc_lsb_bits = poc_lsb_minus4.wrapping_add(4).min(16);
        bits.flag(true).ue(1).put(1, poc_lsb_bits).flag(true); // one long-term picture
        bits.put(0, 2).flag(true); // TMVP, strong intra smoothing; VUI
        bits.put(0, 4).put(0, 3).flag(false); // VUI flags up to the display window
        bits.flag(true).put(1001, 32).put(60_000, 32).flag(false);
        bits.flag(false).flag(false); // no bitstream restriction; no extensions
        bits.nal(&[0x42, 0x01])
    }
}
