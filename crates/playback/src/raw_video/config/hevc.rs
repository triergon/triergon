//! HEVC configuration prefixes from H.265 (07/2024), §§7.3.2, 7.3.3,
//! 7.3.6.1 and Table 6-1, with an opt-in timing projection through Annex E.2.1.

use super::Sps;
use triergon_bitstream::{BitError, BitSource};
use triergon_core::{Error, Result};
use triergon_framing::{Codec, NalHeader};

pub(crate) fn vps(payload: &[u8]) -> Result<u8> {
    Ok(Rbsp::new(payload).fixed(4)? as u8)
}

pub(super) fn sps(payload: &[u8]) -> Result<(u8, Sps)> {
    let (id, info, _) = sps_prefix(&mut Rbsp::new(payload))?;
    Ok((id, info))
}

fn sps_prefix(bits: &mut Rbsp<'_>) -> Result<(u8, Sps, u8)> {
    let vps = bits.fixed(4)? as u8;
    let sublayers = bits.fixed(3)? as u8;
    if sublayers > 6 {
        return Err(Error::Invalid("HEVC SPS sublayer count"));
    }
    bits.fixed(1)?; // sps_temporal_id_nesting_flag
    skip_profile_tier_level(bits, sublayers)?;
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
    Ok((
        id,
        Sps {
            vps: Some(vps),
            width,
            height,
            avc_timing: None,
        },
        sublayers,
    ))
}

pub(crate) struct StartupChain<'a> {
    pub vps: &'a [u8],
    pub sps: &'a [u8],
    pub pps_id: u8,
    pub reorder: u8,
}

pub(crate) fn startup_chain<'a>(nals: impl Iterator<Item = &'a [u8]>) -> Result<StartupChain<'a>> {
    let mut vps_set = None;
    let mut sps_set = None;
    let mut pps_set = None;
    for (index, nal) in nals.enumerate() {
        if index == 16 || nal.len() > 4096 {
            return Err(Error::Unsupported("HEVC clock parameter-set extent"));
        }
        let NalHeader::Hevc {
            nal_unit_type,
            layer_id: 0,
            temporal_id_plus_one: 1,
        } = NalHeader::parse(Codec::Hevc, nal)?
        else {
            return Err(Error::Unsupported("HEVC discovery NAL layer"));
        };
        let payload = &nal[2..];
        match nal_unit_type {
            32 if vps_set.is_none() => vps_set = Some(payload),
            33 if sps_set.is_none() => sps_set = Some(payload),
            34 if pps_set.is_none() => pps_set = Some(pps(payload)?),
            39 | 40 => {}
            _ => return Err(Error::Unsupported("HEVC discovery parameter-set chain")),
        }
    }
    let vps_set = vps_set.ok_or(Error::Invalid("HEVC discovery VPS"))?;
    let sps_set = sps_set.ok_or(Error::Invalid("HEVC discovery SPS"))?;
    let (sps_id, sps_vps, reorder) = startup_sps(sps_set)?;
    let (pps_id, pps_sps) = pps_set.ok_or(Error::Invalid("HEVC discovery PPS"))?;
    if vps(vps_set)? != sps_vps || sps_id != pps_sps {
        return Err(Error::Invalid("HEVC discovery parameter-set reference"));
    }
    Ok(StartupChain {
        vps: vps_set,
        sps: sps_set,
        pps_id,
        reorder,
    })
}

/// Single-layer VPS timing fields through vps_time_scale (H.265 §7.3.2.1).
pub(crate) fn vps_timing(payload: &[u8]) -> Result<Option<(u32, u32)>> {
    let mut bits = Rbsp::new(payload);
    bits.skip(6)?; // VPS identifier and base-layer flags.
    if bits.fixed(6)? != 0 || bits.fixed(3)? != 0 {
        return Err(Error::Unsupported("HEVC clock VPS layers"));
    }
    bits.skip(1)?; // vps_temporal_id_nesting_flag
    if bits.fixed(16)? != 0xffff {
        return Err(Error::Invalid("HEVC VPS reserved bits"));
    }
    skip_profile_tier_level(&mut bits, 0)?;
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

/// Base-layer startup facts through sps_max_num_reorder_pics.
fn startup_sps(payload: &[u8]) -> Result<(u8, u8, u8)> {
    let mut bits = Rbsp::new(payload);
    let (id, info, sublayers) = sps_prefix(&mut bits)?;
    // This probe models one output order, with no temporal sublayer switching.
    if sublayers != 0 {
        return Err(Error::Unsupported("HEVC discovery temporal sublayers"));
    }
    bits.bounded(2)?; // bit_depth_luma_minus8
    bits.bounded(2)?; // bit_depth_chroma_minus8
    bits.bounded(12)?; // log2_max_pic_order_cnt_lsb_minus4
    bits.flag()?; // sps_sub_layer_ordering_info_present_flag
    let max_buffering = bits.bounded(15)?;
    let reorder = bits.bounded(max_buffering)? as u8;
    Ok((
        id,
        info.vps.ok_or(Error::Invalid("HEVC SPS VPS identifier"))?,
        reorder,
    ))
}

/// Literal HEVC SPS VUI clock and picture-order-count relationship fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HevcVuiTiming {
    pub num_units_in_tick: u32,
    pub time_scale: u32,
    pub poc_proportional_to_timing_flag: bool,
    pub num_ticks_poc_diff_one_minus1: Option<u32>,
}

pub(crate) fn sps_timing(payload: &[u8]) -> Result<Option<HevcVuiTiming>> {
    let mut bits = Rbsp::new(payload);
    let (_, _, sublayers) = sps_prefix(&mut bits)?;
    skip_sps_to_vui(&mut bits, sublayers)?;
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
            // H.265 §7.4.8: negative deltas nearest first, then positive
            // deltas nearest first. The next prediction's flags use this order.
            next[..kept].sort_unstable_by_key(|delta| (*delta > 0, delta.unsigned_abs()));
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

fn vui_timing(bits: &mut Rbsp<'_>) -> Result<Option<HevcVuiTiming>> {
    if bits.flag()? && bits.fixed(8)? == 255 {
        bits.skip(32)?; // Extended SAR.
    }
    if bits.flag()? {
        bits.bit()?; // Overscan.
    }
    if bits.flag()? {
        bits.skip(4)?; // Video format and full range.
        if bits.flag()? {
            bits.skip(24)?; // Colour description.
        }
    }
    if bits.flag()? {
        bits.ue()?;
        bits.ue()?;
    }
    bits.skip(3)?; // Neutral chroma, field sequence, frame-field info.
    if bits.flag()? {
        for _ in 0..4 {
            bits.ue()?; // Default display window.
        }
    }
    if !bits.flag()? {
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
    // The profile-dependent constraints have a fixed extent (88 bits for
    // the whole profile portion). Each level portion has another 8 bits.
    bits.skip(96)?;
    let mut presence = [0; 6];
    let active = &mut presence[..usize::from(sublayers)];
    for flags in &mut *active {
        *flags = bits.fixed(2)?;
    }
    if sublayers != 0 && bits.fixed(2 * (8 - sublayers))? != 0 {
        return Err(Error::Invalid("HEVC profile-tier-level reserved bits"));
    }
    for &flags in active.iter() {
        if flags & 2 != 0 {
            bits.skip(88)?;
        }
        if flags & 1 != 0 {
            bits.skip(8)?;
        }
    }
    Ok(())
}

pub(crate) fn pps(payload: &[u8]) -> Result<(u8, u8)> {
    let mut bits = Rbsp::new(payload);
    Ok((bits.id(63)?, bits.id(15)?))
}

pub(super) fn slice_pps(payload: &[u8], nal_type: u8) -> Result<u8> {
    Ok(slice_picture(payload, nal_type)?.1)
}

pub(crate) enum StartupNal {
    Picture {
        first: bool,
        random_access: bool,
        pps: u8,
    },
    Supplemental,
}

pub(crate) fn startup_nal(nal: &[u8], header: NalHeader) -> Result<StartupNal> {
    match header {
        NalHeader::Hevc {
            nal_unit_type: kind @ (0..=9 | 16..=21),
            layer_id: 0,
            temporal_id_plus_one: 1,
        } => {
            let (first, pps) = slice_picture(&nal[2..], kind)?;
            Ok(StartupNal::Picture {
                first,
                random_access: kind >= 16,
                pps,
            })
        }
        NalHeader::Hevc {
            nal_unit_type: 35 | 38..=40,
            layer_id: 0,
            ..
        } => Ok(StartupNal::Supplemental),
        _ => Err(Error::Unsupported("HEVC clock sample NAL")),
    }
}

fn slice_picture(payload: &[u8], nal_type: u8) -> Result<(bool, u8)> {
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
