//! The leading fields of H.264 parameter sets and slice headers that a container
//! reader needs without decoding: identifiers, the display raster, the VUI's
//! timing and its reorder limit (H.264 08/2024 §§7.3.2.1.1, 7.3.2.2, 7.3.3, E.1.1).
//!
//! Every reader takes the payload after the one-byte NAL header, reads `00 00 00`
//! to `00 00 02` inside it as data, and leaves the rest of the RBSP unread.

use crate::nal::syntax::{SyntaxError, skip_avc_scaling_matrix};
use crate::nal::{Codec, skip_vui_to_timing};
use triergon_bitstream::{BitError, BitSource};
use triergon_core::{Error, Result};

/// Reads RBSP prefixes without copying or decoding their uninspected suffixes.
struct Rbsp<'a>(triergon_bitstream::Rbsp<'a>);

fn rbsp_error(error: BitError) -> Error {
    match error {
        BitError::End => Error::Truncated,
        BitError::LongPrefix => Error::Invalid("AVC Exp-Golomb width"),
        BitError::MissingPrevention | BitError::InvalidPrevention => {
            Error::Invalid("AVC emulation-prevention byte")
        }
    }
}

impl<'a> Rbsp<'a> {
    fn new(rest: &'a [u8]) -> Self {
        Self(triergon_bitstream::Rbsp::lenient(rest))
    }

    fn bit(&mut self) -> Result<u32> {
        self.0.bit().map_err(rbsp_error)
    }

    fn fixed(&mut self, width: u32) -> Result<u32> {
        self.0.read(width).map_err(rbsp_error)
    }

    fn ue(&mut self) -> Result<u32> {
        self.0.ue().map_err(rbsp_error)
    }

    fn bounded(&mut self, maximum: u32) -> Result<u32> {
        let value = self.ue()?;
        if value > maximum {
            return Err(Error::Invalid("AVC RBSP field out of range"));
        }
        Ok(value)
    }

    fn se(&mut self) -> Result<i32> {
        self.0.se().map_err(rbsp_error)
    }
}

/// The display raster of an SPS: the coded frame after its frame cropping, in
/// luma samples. Neither side is zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcRaster {
    /// Width in luma samples.
    pub width: u32,
    /// Height in luma samples, of the frame whether or not fields are coded.
    pub height: u32,
}

/// The timing fields of an SPS's VUI (H.264 E.1.1) as coded. They alone do not
/// establish presentation timestamps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcVuiTiming {
    /// Clock units per tick, as coded in `num_units_in_tick`.
    pub num_units_in_tick: u32,
    /// Clock units per second, as coded in `time_scale`.
    pub time_scale: u32,
    /// Whether consecutive output pictures have a constrained temporal distance.
    pub fixed_frame_rate_flag: bool,
}

/// The `seq_parameter_set_id` and display raster of an SPS payload. Fails when the SPS
/// is cut short, a field is out of range, the profile is not one whose SPS syntax is
/// known, the cropping exceeds the coded frame or the raster is empty.
pub fn read_sps_raster(raw: &[u8]) -> Result<(u8, AvcRaster)> {
    let (id, raster, _) = sps_prefix(&mut Rbsp::new(raw))?;
    Ok((id, raster))
}

/// As [`read_sps_raster`], with the VUI's timing when the SPS has one. Also fails when
/// the VUI ends before its timing does.
pub fn read_sps_timing(raw: &[u8]) -> Result<(u8, AvcRaster, Option<AvcVuiTiming>)> {
    let mut r = Rbsp::new(raw);
    let (id, raster, _) = sps_prefix(&mut r)?;
    let timing = if r.bit()? != 0 {
        vui_timing(&mut r)?
    } else {
        None
    };
    // The final set bit is rbsp_stop_one_bit. It cannot supply a VUI field.
    if r.0.stop_bit_consumed() {
        return Err(Error::Truncated);
    }
    Ok((id, raster, timing))
}

/// The `seq_parameter_set_id` of a frame-only SPS payload and the number of frames its
/// pictures may be reordered by: `max_num_reorder_frames`, or 0 for a Constrained
/// Baseline SPS that does not state it. Fails as [`read_sps_raster`] does; also
/// `Unsupported` when the SPS codes fields, or when it does not state the limit and is
/// not Constrained Baseline.
pub fn read_sps_reorder(raw: &[u8]) -> Result<(u8, u8)> {
    let mut r = Rbsp::new(raw);
    let (id, _, frame_only) = sps_prefix(&mut r)?;
    if !frame_only {
        return Err(Error::Unsupported("AVC discovery field pictures"));
    }
    let constrained_baseline = raw[0] == 66 && raw[1] & 0x40 != 0;
    let reorder = if r.bit()? != 0 {
        vui_timing(&mut r)?;
        vui_reorder(&mut r)?
    } else {
        None
    };
    if r.0.stop_bit_consumed() {
        return Err(Error::Truncated);
    }
    let reorder = match (constrained_baseline, reorder) {
        (true, None | Some(0)) => 0,
        (false, Some(reorder)) => reorder,
        _ => return Err(Error::Unsupported("AVC discovery reorder declaration")),
    };
    Ok((id, reorder))
}

fn vui_reorder(r: &mut Rbsp<'_>) -> Result<Option<u8>> {
    let nal_hrd = read_hrd(r)?;
    let vcl_hrd = read_hrd(r)?;
    if nal_hrd || vcl_hrd {
        r.bit()?; // low_delay_hrd_flag
    }
    r.bit()?; // pic_struct_present_flag
    if r.bit()? == 0 {
        return Ok(None);
    }
    r.bit()?; // motion_vectors_over_pic_boundaries_flag
    for _ in 0..4 {
        r.ue()?;
    }
    let reorder = r.bounded(16)?;
    let buffering = r.bounded(16)?;
    if reorder > buffering {
        return Err(Error::Invalid("AVC reorder exceeds decoded picture buffer"));
    }
    Ok(Some(reorder as u8))
}

fn read_hrd(r: &mut Rbsp<'_>) -> Result<bool> {
    if r.bit()? == 0 {
        return Ok(false);
    }
    let entries = r.bounded(31)? + 1;
    r.fixed(8)?; // bit_rate_scale, cpb_size_scale
    for _ in 0..entries {
        r.ue()?; // bit_rate_value_minus1
        r.ue()?; // cpb_size_value_minus1
        r.bit()?; // cbr_flag
    }
    r.fixed(20)?; // Delay and time-offset length fields.
    Ok(true)
}

/// The `seq_parameter_set_id`, the display raster and whether only frames are
/// coded. [`read_sps`](crate::avc::read_sps) walks the same syntax for picture
/// output; this walk reads lenient RBSPs only, refuses profiles whose SPS syntax it
/// does not know, ignores `reserved_zero_2bits`, reads Exp-Golomb codes of up to 31
/// leading zero bits, fails on an overflowing frame size, checks the cropping, and
/// reports a field out of range as "AVC RBSP field out of range".
fn sps_prefix(r: &mut Rbsp<'_>) -> Result<(u8, AvcRaster, bool)> {
    let profile = r.fixed(8)?;
    r.fixed(16)?;
    let id = r.bounded(31)? as u8;
    let chroma_array = match profile {
        66 | 77 | 88 => 1,
        44 | 83 | 86 | 100 | 110 | 118 | 122 | 128 | 134 | 135 | 138 | 139 | 244 => {
            let chroma = r.bounded(3)?;
            let separate_plane = chroma == 3 && r.bit()? != 0;
            r.bounded(6)?;
            r.bounded(6)?;
            r.bit()?;
            skip_avc_scaling_matrix(&mut r.0, chroma).map_err(|error| match error {
                SyntaxError::Bits(error) => rbsp_error(error),
                SyntaxError::OutOfRange => Error::Invalid("AVC scaling list delta"),
            })?;
            if separate_plane { 0 } else { chroma }
        }
        _ => return Err(Error::Unsupported("AVC SPS profile for raster projection")),
    };
    r.bounded(12)?;
    match r.bounded(2)? {
        0 => {
            r.bounded(12)?;
        }
        1 => {
            r.bit()?;
            r.se()?;
            r.se()?;
            for _ in 0..r.bounded(255)? {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?;
    r.bit()?;
    let mb_width = r.ue()?.checked_add(1).ok_or(Error::Overflow)?;
    let map_height = r.ue()?.checked_add(1).ok_or(Error::Overflow)?;
    let field_factor = 2 - r.bit()?;
    if field_factor == 2 {
        r.bit()?;
    }
    r.bit()?;
    let coded_width = mb_width.checked_mul(16).ok_or(Error::Overflow)?;
    let coded_height = map_height
        .checked_mul(16 * field_factor)
        .ok_or(Error::Overflow)?;
    let (width, height) = if r.bit()? == 0 {
        (coded_width, coded_height)
    } else {
        let crop_x = if matches!(chroma_array, 1 | 2) { 2 } else { 1 };
        let crop_y = if chroma_array == 1 {
            2 * field_factor
        } else {
            field_factor
        };
        let left = r.ue()?;
        let right = r.ue()?;
        let top = r.ue()?;
        let bottom = r.ue()?;
        let crop = |size: u32, first: u32, second: u32, unit: u32| {
            first
                .checked_add(second)
                .and_then(|sum| sum.checked_mul(unit))
                .and_then(|sum| size.checked_sub(sum))
                .ok_or(Error::Invalid("AVC crop exceeds coded raster"))
        };
        (
            crop(coded_width, left, right, crop_x)?,
            crop(coded_height, top, bottom, crop_y)?,
        )
    };
    if width == 0 || height == 0 {
        return Err(Error::Invalid("empty AVC display raster"));
    }
    Ok((id, AvcRaster { width, height }, field_factor == 1))
}

fn vui_timing(r: &mut Rbsp<'_>) -> Result<Option<AvcVuiTiming>> {
    if !skip_vui_to_timing(&mut r.0, Codec::Avc).map_err(rbsp_error)? {
        return Ok(None);
    }
    Ok(Some(AvcVuiTiming {
        num_units_in_tick: r.fixed(32)?,
        time_scale: r.fixed(32)?,
        fixed_frame_rate_flag: r.bit()? != 0,
    }))
}

/// The `pic_parameter_set_id` and `seq_parameter_set_id` of a PPS payload. Fails when
/// the PPS is cut short or an identifier is out of range.
pub fn read_pps_ids(raw: &[u8]) -> Result<(u8, u8)> {
    let mut r = Rbsp::new(raw);
    Ok((r.bounded(255)? as u8, r.bounded(31)? as u8))
}

/// The `first_mb_in_slice`, `slice_type % 5` (0 P, 1 B, 2 I, 3 SP, 4 SI) and
/// `pic_parameter_set_id` of a slice payload. Fails when the header is cut short or a
/// field is out of range.
pub fn read_slice_prefix(raw: &[u8]) -> Result<(u32, u32, u8)> {
    let mut r = Rbsp::new(raw);
    let first_mb = r.ue()?;
    let slice_type = r.bounded(9)?;
    Ok((first_mb, slice_type % 5, r.bounded(255)? as u8))
}

#[cfg(test)]
mod tests {
    use super::Rbsp;
    use triergon_core::Result;

    #[test]
    fn fixed_fields_match_serial_bits_across_prevention_bytes_and_truncation() {
        let inputs: &[&[u8]] = &[
            &[],
            &[0xa5],
            &[0xa5, 0x81, 0x7e, 0x10, 0xff, 0],
            &[0, 0, 3, 0, 1, 0x80],
            &[0, 0, 3, 3, 0x55],
            &[0, 0, 3, 4],
            &[0, 0, 3],
        ];
        for input in inputs {
            for offset in 0..=input.len() * 8 {
                for width in 0..=32 {
                    let mut serial = Rbsp::new(input);
                    let mut grouped = Rbsp::new(input);
                    for _ in 0..offset {
                        assert_eq!(serial.bit(), grouped.bit());
                    }
                    let expected: Result<u32> =
                        (0..width).try_fold(0, |value, _| Ok((value << 1) | serial.bit()?));
                    assert_eq!(grouped.fixed(width), expected);
                    assert_eq!(grouped.0, serial.0);
                }
            }
        }
    }
}
