//! The leading fields of H.265 parameter sets and slice segment headers that a
//! container reader needs without decoding: identifiers, the display raster, the
//! reorder limit and the timing of the VPS and of the SPS's VUI (H.265 07/2024
//! §§7.3.2, 7.3.3, 7.3.6.1, Table 6-1, Annex E.2.1).
//!
//! Every reader takes the payload after the two-byte NAL header and refuses
//! `00 00 00` to `00 00 02` inside it.

use crate::nal::{Codec, skip_hevc_profile_tier_level, skip_vui_to_timing};
use triergon_bitstream::{BitError, BitSource};
use triergon_core::{Error, Result};

/// What the start of an SPS says about its stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HevcSpsPrefix {
    /// `sps_seq_parameter_set_id`, 0 to 15.
    pub id: u8,
    /// Identifier of the referenced VPS, 0 to 15.
    pub vps_id: u8,
    max_sub_layers_minus1: u8,
    /// Width of the conformance window in luma samples, above 0.
    pub width: u32,
    /// Height of the conformance window in luma samples, above 0.
    pub height: u32,
}

/// The `vps_video_parameter_set_id` of a VPS payload. Fails when the VPS is cut short.
pub fn read_vps_id(payload: &[u8]) -> Result<u8> {
    Ok(Rbsp::new(payload).fixed(4)? as u8)
}

/// The identifiers, sub-layer count and display raster of an SPS payload. Fails when
/// the SPS is cut short, a field is out of range, the conformance window exceeds the
/// coded picture or the raster is empty.
pub fn read_sps_prefix(payload: &[u8]) -> Result<HevcSpsPrefix> {
    sps_prefix(&mut Rbsp::new(payload))
}

fn sps_prefix(bits: &mut Rbsp<'_>) -> Result<HevcSpsPrefix> {
    let vps_id = bits.fixed(4)? as u8;
    let max_sub_layers_minus1 = bits.fixed(3)? as u8;
    if max_sub_layers_minus1 > 6 {
        return Err(Error::Invalid("HEVC SPS sublayer count"));
    }
    bits.fixed(1)?; // sps_temporal_id_nesting_flag
    skip_profile_tier_level(bits, max_sub_layers_minus1)?;
    let id = bits.id(15)?;
    let chroma = bits.id(3)?;
    if chroma == 3 {
        bits.fixed(1)?; // separate_colour_plane_flag
    }
    let mut width = bits.ue()?;
    let mut height = bits.ue()?;
    if bits.fixed(1)? != 0 {
        let horizontal = if matches!(chroma, 1 | 2) { 2 } else { 1 };
        let vertical = if chroma == 1 { 2 } else { 1 };
        width = cropped(width, bits.ue()?, bits.ue()?, horizontal)?;
        height = cropped(height, bits.ue()?, bits.ue()?, vertical)?;
    }
    if width == 0 || height == 0 {
        return Err(Error::Invalid("empty HEVC display raster"));
    }
    Ok(HevcSpsPrefix {
        id,
        vps_id,
        max_sub_layers_minus1,
        width,
        height,
    })
}

/// The `vps_num_units_in_tick` and `vps_time_scale` of a single-layer VPS payload
/// (H.265 §7.3.2.1), when it has timing. Fails when the VPS is cut short or a field is
/// out of range, and `Unsupported` when it has more than one layer or layer set.
pub fn read_vps_timing(payload: &[u8]) -> Result<Option<(u32, u32)>> {
    vps_timing(&mut Rbsp::new(payload))
}

fn vps_timing(bits: &mut Rbsp<'_>) -> Result<Option<(u32, u32)>> {
    bits.skip(6)?; // VPS identifier and base-layer flags.
    if bits.fixed(6)? != 0 || bits.fixed(3)? != 0 {
        return Err(Error::Unsupported("HEVC clock VPS layers"));
    }
    bits.skip(1)?; // vps_temporal_id_nesting_flag
    if bits.fixed(16)? != 0xffff {
        return Err(Error::Invalid("HEVC VPS reserved bits"));
    }
    skip_profile_tier_level(bits, 0)?;
    bits.skip(1)?; // vps_sub_layer_ordering_info_present_flag
    let buffering = bits.bounded(15)?;
    bits.bounded(buffering)?; // vps_max_num_reorder_pics
    bits.ue()?; // vps_max_latency_increase_plus1
    if bits.fixed(6)? != 0 || bits.ue()? != 0 {
        return Err(Error::Unsupported("HEVC clock VPS layer sets"));
    }
    if !bits.flag()? {
        return Ok(None);
    }
    Ok(Some((bits.fixed(32)?, bits.fixed(32)?)))
}

/// Check the remaining single-layer VPS syntax through rbsp_trailing_bits.
fn check_vps(payload: &[u8]) -> Result<()> {
    let mut bits = Rbsp::new(payload);
    if let Some((units, scale)) = vps_timing(&mut bits)? {
        // Both clock fields are positive when timing is present (H.265 §7.4.3.1).
        if units == 0 || scale == 0 {
            return Err(Error::Invalid("HEVC VPS timing"));
        }
        if bits.flag()? {
            bits.bounded(u32::MAX - 1)?; // vps_num_ticks_poc_diff_one_minus1
        }
        if bits.bounded(1)? != 0 {
            // With one layer set, index zero is available only to an internal base layer.
            bits.bounded(0)?;
            if payload[0] & 8 == 0 {
                return Err(Error::Invalid("HEVC VPS HRD layer set"));
            }
            skip_vps_hrd(&mut bits)?;
        }
    }
    let extension = bits.flag()?;
    if bits.0.stop_bit_consumed() {
        return Err(Error::Truncated);
    }
    let mut stop = bits.flag()?;
    while extension && !bits.0.stop_bit_consumed() {
        stop = bits.flag()?;
    }
    if !stop || !bits.0.stop_bit_consumed() {
        return Err(Error::Invalid("HEVC VPS trailing bits"));
    }
    Ok(())
}

/// One HRD structure with common parameters and one temporal sublayer (Annex E.2.2).
fn skip_vps_hrd(bits: &mut Rbsp<'_>) -> Result<()> {
    let nal = bits.flag()?;
    let vcl = bits.flag()?;
    let sub_picture = (nal || vcl) && bits.flag()?;
    if nal || vcl {
        if sub_picture {
            bits.skip(19)?; // Sub-picture clock and delay parameters.
        }
        bits.skip(8)?; // Bit rate and CPB size scales.
        if sub_picture {
            bits.skip(4)?; // CPB size DU scale.
        }
        bits.skip(15)?; // Initial removal, access-unit removal and output delay lengths.
    }
    let fixed_rate = bits.flag()? || bits.flag()?;
    let low_delay = if fixed_rate {
        bits.ue()?; // elemental_duration_in_tc_minus1
        false
    } else {
        bits.flag()?
    };
    let entries = if low_delay { 1 } else { bits.bounded(31)? + 1 };
    for _ in 0..entries * (u32::from(nal) + u32::from(vcl)) {
        bits.ue()?; // bit_rate_value_minus1
        bits.ue()?; // cpb_size_value_minus1
        if sub_picture {
            bits.ue()?; // cpb_size_du_value_minus1
            bits.ue()?; // bit_rate_du_value_minus1
        }
        bits.flag()?; // cbr_flag
    }
    Ok(())
}

/// The `sps_seq_parameter_set_id`, `sps_video_parameter_set_id` and
/// `sps_max_num_reorder_pics` of an SPS payload with one temporal sub-layer and at most
/// 10 bits per sample. Fails as [`read_sps_prefix`] does; a bit depth above 10 is out
/// of range, and an SPS with more sub-layers, which may each reorder differently, is
/// `Unsupported`.
pub fn read_sps_reorder(payload: &[u8]) -> Result<(u8, u8, u8)> {
    let mut bits = Rbsp::new(payload);
    let sps = sps_prefix(&mut bits)?;
    if sps.max_sub_layers_minus1 != 0 {
        return Err(Error::Unsupported("HEVC discovery temporal sublayers"));
    }
    bits.bounded(2)?; // bit_depth_luma_minus8
    bits.bounded(2)?; // bit_depth_chroma_minus8
    bits.bounded(12)?; // log2_max_pic_order_cnt_lsb_minus4
    bits.flag()?; // sps_sub_layer_ordering_info_present_flag
    let max_buffering = bits.bounded(15)?;
    let reorder = bits.bounded(max_buffering)? as u8;
    Ok((sps.id, sps.vps_id, reorder))
}

/// The clock and picture-order-count relationship fields of an SPS's VUI (H.265
/// E.2.1) as coded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HevcVuiTiming {
    /// Clock units per tick, as coded in `vui_num_units_in_tick`.
    pub num_units_in_tick: u32,
    /// Clock units per second, as coded in `vui_time_scale`.
    pub time_scale: u32,
    /// Whether picture order count increments imply a fixed number of clock ticks.
    pub poc_proportional_to_timing_flag: bool,
    /// `vui_num_ticks_poc_diff_one_minus1`, present with the flag above.
    pub num_ticks_poc_diff_one_minus1: Option<u32>,
}

/// The VUI timing of an SPS payload, when it has one. Inferring it from the VPS and the
/// HRD is the caller's. Fails as [`read_sps_prefix`] does; also when a field between
/// the raster and the timing is cut short or out of range.
pub fn read_sps_timing(payload: &[u8]) -> Result<Option<HevcVuiTiming>> {
    let mut bits = Rbsp::new(payload);
    let sps = sps_prefix(&mut bits)?;
    skip_sps_to_vui(&mut bits, sps.max_sub_layers_minus1)?;
    let timing = if bits.flag()? {
        vui_timing(&mut bits)?
    } else {
        None
    };
    // At least the SPS extension flag remains. A VUI field cannot consume
    // the RBSP stop bit or its alignment padding in the final byte.
    if bits.0.stop_bit_consumed() {
        return Err(Error::Truncated);
    }
    Ok(timing)
}

fn skip_sps_to_vui(bits: &mut Rbsp<'_>, sublayers: u8) -> Result<()> {
    bits.ue()?; // Luma bit depth minus eight.
    bits.ue()?; // Chroma bit depth minus eight.
    let poc_width = bits.bounded(12)? as u8 + 4;
    let first_layer = if bits.flag()? { 0 } else { sublayers };
    let mut max_references = 0;
    for _ in first_layer..=sublayers {
        max_references = bits.bounded(15)?; // MaxDpbSize minus one, Annex A.4.
        bits.ue()?; // Reorder limit.
        bits.ue()?; // Latency limit.
    }
    for _ in 0..6 {
        bits.ue()?; // Coding/transform block sizes and hierarchy depths.
    }
    if bits.flag()? && bits.flag()? {
        skip_scaling_lists(bits)?;
    }
    bits.skip(2)?; // AMP and SAO.
    if bits.flag()? {
        bits.skip(8)?; // PCM bit depths.
        bits.ue()?; // PCM minimum coding block size.
        bits.ue()?; // PCM coding block size difference.
        bits.bit()?; // PCM loop filter.
    }
    skip_short_term_sets(bits, max_references)?;
    if bits.flag()? {
        for _ in 0..bits.bounded(32)? {
            bits.skip(poc_width + 1)?; // Long-term POC and used flag.
        }
    }
    bits.skip(2) // Temporal motion-vector prediction and intra smoothing.
}

fn skip_scaling_lists(bits: &mut Rbsp<'_>) -> Result<()> {
    for size_id in 0..4 {
        let matrices = if size_id == 3 { 2 } else { 6 };
        for matrix in 0..matrices {
            if bits.flag()? {
                if size_id > 1 {
                    bits.ue()?; // Codeword for signed DC coefficient.
                }
                for _ in 0..if size_id == 0 { 16 } else { 64 } {
                    bits.ue()?; // Codeword for signed coefficient delta.
                }
            } else {
                bits.bounded(matrix)?;
            }
        }
    }
    Ok(())
}

fn skip_short_term_sets(bits: &mut Rbsp<'_>, max_references: u32) -> Result<()> {
    let sets = bits.bounded(64)?;
    // An explicit set has at most 15 entries. Within an SPS each prediction
    // uses the preceding set and can add at most one entry.
    let mut previous = [0i32; 15 + 63];
    let mut len = 0;
    for index in 0..sets {
        if index != 0 && bits.flag()? {
            let sign = if bits.flag()? { -1 } else { 1 };
            let shift = sign * (bits.bounded(32767)? as i32 + 1);
            let mut next = [0; 15 + 63];
            let mut kept = 0;
            for delta in previous[..len].iter().copied().chain([0]) {
                let use_delta = bits.flag()? || bits.flag()?;
                let shifted = delta + shift;
                if use_delta && shifted != 0 {
                    next[kept] = shifted;
                    kept += 1;
                }
            }
            order_deltas(&mut next[..kept]);
            previous = next;
            len = kept;
        } else {
            let negative = bits.bounded(max_references)? as usize;
            let positive = bits.bounded(max_references - negative as u32)? as usize;
            len = negative + positive;
            let (negative, positive) = previous[..len].split_at_mut(negative);
            for (deltas, sign) in [(negative, -1), (positive, 1)] {
                let mut delta = 0;
                for entry in deltas {
                    delta += sign * (bits.bounded(32767)? as i32 + 1);
                    *entry = delta;
                    bits.bit()?; // Used by current picture.
                }
            }
        }
    }
    Ok(())
}

/// H.265 §7.4.8: negative deltas nearest first, then positive deltas nearest
/// first. The next prediction's flags use this order. A set has at most 78
/// distinct entries, so an insertion sort is enough.
fn order_deltas(deltas: &mut [i32]) {
    let key = |delta: i32| (delta > 0, delta.unsigned_abs());
    for end in 1..deltas.len() {
        let mut at = end;
        while at > 0 && key(deltas[at - 1]) > key(deltas[at]) {
            deltas.swap(at - 1, at);
            at -= 1;
        }
    }
}

fn vui_timing(bits: &mut Rbsp<'_>) -> Result<Option<HevcVuiTiming>> {
    if !skip_vui_to_timing(&mut bits.0, Codec::Hevc).map_err(rbsp_error)? {
        return Ok(None);
    }
    let num_units_in_tick = bits.fixed(32)?;
    let time_scale = bits.fixed(32)?;
    let poc_proportional_to_timing_flag = bits.flag()?;
    let num_ticks_poc_diff_one_minus1 = if poc_proportional_to_timing_flag {
        Some(bits.bounded(u32::MAX - 1)?)
    } else {
        None
    };
    Ok(Some(HevcVuiTiming {
        num_units_in_tick,
        time_scale,
        poc_proportional_to_timing_flag,
        num_ticks_poc_diff_one_minus1,
    }))
}

fn cropped(dimension: u32, first: u32, second: u32, unit: u32) -> Result<u32> {
    first
        .checked_add(second)
        .and_then(|sum| sum.checked_mul(unit))
        .and_then(|crop| dimension.checked_sub(crop))
        .ok_or(Error::Invalid(
            "HEVC conformance window exceeds coded raster",
        ))
}

fn skip_profile_tier_level(bits: &mut Rbsp<'_>, sublayers: u8) -> Result<()> {
    if skip_hevc_profile_tier_level(&mut bits.0, sublayers).map_err(rbsp_error)? != 0 {
        return Err(Error::Invalid("HEVC profile-tier-level reserved bits"));
    }
    Ok(())
}

/// The `pps_pic_parameter_set_id` and `pps_seq_parameter_set_id` of a PPS payload.
/// Fails when the PPS is cut short or an identifier is out of range.
pub fn read_pps_ids(payload: &[u8]) -> Result<(u8, u8)> {
    pps_ids(&mut Rbsp::new(payload))
}

fn pps_ids(bits: &mut Rbsp<'_>) -> Result<(u8, u8)> {
    Ok((bits.id(63)?, bits.id(15)?))
}

/// Checks the syntax of a single-layer parameter-set chain further than its
/// identifiers: the VPS through its trailing bits, the SPS through its VUI timing and
/// the PPS through its extension flags. Fails when a payload is cut short or a field is
/// out of range, and `Unsupported` for a VPS with more than one layer or layer set.
pub fn check_parameter_sets(vps: &[u8], sps: &[u8], pps: &[u8]) -> Result<()> {
    check_vps(vps)?;
    read_sps_timing(sps)?;
    check_pps_prefix(pps)
}

/// H.265 §7.3.2.3.1 through the base PPS syntax and extension flags.
fn check_pps_prefix(payload: &[u8]) -> Result<()> {
    let mut bits = Rbsp::new(payload);
    pps_ids(&mut bits)?;
    bits.skip(7)?; // Slice-header and CABAC flags.
    bits.bounded(14)?; // Default L0 references minus one.
    bits.bounded(14)?; // Default L1 references minus one.
    bits.ue()?; // Initial QP delta codeword.
    bits.skip(2)?; // Constrained intra prediction and transform skip.
    if bits.flag()? {
        bits.ue()?; // CU QP delta depth.
    }
    bits.ue()?; // Cb QP offset codeword.
    bits.ue()?; // Cr QP offset codeword.
    bits.skip(4)?; // Slice chroma offsets, weighted prediction and transquant bypass.
    let tiles = bits.flag()?;
    bits.skip(1)?; // Entropy coding sync.
    if tiles {
        let columns = bits.bounded(19)?;
        let rows = bits.bounded(21)?;
        if !bits.flag()? {
            for _ in 0..columns + rows {
                bits.ue()?;
            }
        }
        bits.skip(1)?; // Loop filter across tiles.
    }
    bits.skip(1)?; // Loop filter across slices.
    if bits.flag()? {
        bits.skip(1)?; // Deblocking override.
        if !bits.flag()? {
            bits.ue()?; // Beta offset codeword.
            bits.ue()?; // Tc offset codeword.
        }
    }
    if bits.flag()? {
        skip_scaling_lists(&mut bits)?;
    }
    bits.skip(1)?; // Reference-list modification.
    bits.ue()?; // Parallel merge level.
    bits.skip(1)?; // Slice-header extension enabled.
    if bits.flag()? {
        bits.skip(8)?; // Extension flags.
    }
    if bits.0.stop_bit_consumed() {
        return Err(Error::Truncated);
    }
    Ok(())
}

/// The `first_slice_segment_in_pic_flag` and `slice_pic_parameter_set_id` of a slice
/// segment payload of NAL unit type `nal_type`. Fails when the header is cut short or
/// the identifier is out of range.
pub fn read_slice_prefix(payload: &[u8], nal_type: u8) -> Result<(bool, u8)> {
    let mut bits = Rbsp::new(payload);
    let first = bits.flag()?;
    if (16..=23).contains(&nal_type) {
        bits.fixed(1)?; // no_output_of_prior_pics_flag
    }
    Ok((first, bits.id(63)?))
}

struct Rbsp<'a>(triergon_bitstream::Rbsp<'a>);

fn rbsp_error(error: BitError) -> Error {
    match error {
        BitError::End => Error::Truncated,
        BitError::LongPrefix => Error::Invalid("HEVC Exp-Golomb width"),
        BitError::MissingPrevention => Error::Invalid("HEVC missing emulation-prevention byte"),
        BitError::InvalidPrevention => Error::Invalid("HEVC emulation-prevention byte"),
    }
}

impl<'a> Rbsp<'a> {
    fn new(rest: &'a [u8]) -> Self {
        Self(triergon_bitstream::Rbsp::new(rest))
    }

    fn bit(&mut self) -> Result<u32> {
        self.0.bit().map_err(rbsp_error)
    }

    fn fixed(&mut self, width: u8) -> Result<u32> {
        self.0.read(u32::from(width)).map_err(rbsp_error)
    }

    fn flag(&mut self) -> Result<bool> {
        self.0.flag().map_err(rbsp_error)
    }

    fn skip(&mut self, count: u8) -> Result<()> {
        self.0.skip(u32::from(count)).map_err(rbsp_error)
    }

    fn ue(&mut self) -> Result<u32> {
        let value = self.0.exp_golomb(32).map_err(rbsp_error)?;
        u32::try_from(value).map_err(|_| Error::Invalid("HEVC Exp-Golomb value"))
    }

    fn bounded(&mut self, maximum: u32) -> Result<u32> {
        let value = self.ue()?;
        if value > maximum {
            return Err(Error::Invalid("HEVC configuration field out of range"));
        }
        Ok(value)
    }

    fn id(&mut self, maximum: u32) -> Result<u8> {
        Ok(self.bounded(maximum)? as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_vps_checks_the_timed_suffix_and_hrd() {
        // A timing-present VPS with one NAL HRD entry and no extension data.
        let vps = [
            0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03,
            0x00, 0x00, 0x03, 0x00, 0x1e, 0xba, 0x03, 0x00, 0x00, 0x03, 0x03, 0xe9, 0x00, 0x00,
            0x5d, 0xc0, 0xd6, 0x00, 0x00, 0x03, 0x00, 0xf9,
        ];
        assert_eq!(read_vps_timing(&vps), Ok(Some((1001, 24_000))));
        assert_eq!(check_vps(&vps), Ok(()));
        assert_eq!(check_vps(&vps[..vps.len() - 1]), Err(Error::Truncated));

        // Low-delay VCL HRD with sub-picture fields and multi-byte extension data.
        let extended = [
            0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03,
            0x00, 0x00, 0x03, 0x00, 0x1e, 0xba, 0x03, 0x00, 0x00, 0x03, 0x03, 0xe9, 0x00, 0x00,
            0x5d, 0xc0, 0x2b, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x00, 0xfa, 0x81, 0x01,
        ];
        assert_eq!(read_vps_timing(&extended), Ok(Some((1001, 24_000))));
        assert_eq!(check_vps(&extended), Ok(()));
    }

    #[test]
    fn repeated_vps_refuses_a_zero_timing_field() {
        // Both control VPSs have the original HRD entry and trailing bits.
        let prefix = [
            0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03,
            0x00, 0x00, 0x03, 0x00, 0x1e, 0xba, 0x03,
        ];
        let zero_tick = [0, 0, 3, 0, 0, 3, 0, 0, 0x5d, 0xc0];
        let zero_scale = [0, 0, 3, 3, 0xe9, 0, 0, 3, 0, 0];
        let suffix = [0xd6, 0, 0, 3, 0, 0xf9];
        for clock in [zero_tick, zero_scale] {
            let vps = [prefix.as_slice(), clock.as_slice(), suffix.as_slice()].concat();
            assert_eq!(check_vps(&vps), Err(Error::Invalid("HEVC VPS timing")));
        }
    }

    #[test]
    fn reads_and_skips_unaligned_fields_across_emulation_prevention() {
        let mut bits = Rbsp::new(&[0xb5, 0, 0, 3, 1, 0xcd, 0xf2]);
        assert!(bits.flag().unwrap());
        assert_eq!(bits.fixed(7), Ok(0x35));
        bits.skip(19).unwrap();
        assert_eq!(bits.fixed(13), Ok(0x01cd));
        assert_eq!(bits.fixed(8), Ok(0xf2));
        assert_eq!(bits.bit(), Err(Error::Truncated));

        let mut bits = Rbsp::new(&[0x80, 0, 0, 3, 1, 0x12]);
        bits.skip(1).unwrap();
        assert_eq!(bits.fixed(32), Ok(2));
        assert_eq!(bits.fixed(7), Ok(0x12));
    }

    #[test]
    fn skips_validate_each_crossed_byte_without_reading_ahead() {
        for (raw, error) in [
            (
                &[0, 0, 1][..],
                Error::Invalid("HEVC missing emulation-prevention byte"),
            ),
            (
                &[0, 0, 3, 4][..],
                Error::Invalid("HEVC emulation-prevention byte"),
            ),
            (&[0, 0, 3][..], Error::Truncated),
        ] {
            let mut bits = Rbsp::new(raw);
            bits.skip(16).unwrap();
            assert_eq!(bits.skip(1), Err(error));
        }
    }
}
