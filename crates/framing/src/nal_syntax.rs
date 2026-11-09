//! Parameter-set syntax that several readers pass over on the way to the fields
//! they keep. Each walk reads any [`BitSource`], so the caller chooses strict or
//! lenient emulation prevention and maps the errors to its own.

use crate::Codec;
use triergon_bitstream::{BitError, BitSource};

/// Why a syntax walk stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyntaxError {
    Bits(BitError),
    /// A field outside the range its syntax allows.
    OutOfRange,
}

impl From<BitError> for SyntaxError {
    fn from(error: BitError) -> Self {
        Self::Bits(error)
    }
}

/// Passes over an H.264 SPS's `seq_scaling_matrix_present_flag` and the scaling
/// lists after it (7.3.2.1.1.1). A `delta_scale` outside -128..=127 is out of
/// range, as in FFmpeg's decoder.
pub fn skip_avc_scaling_matrix(
    bits: &mut impl BitSource,
    chroma_format_idc: u32,
) -> Result<(), SyntaxError> {
    if !bits.flag()? {
        return Ok(());
    }
    for list in 0..if chroma_format_idc == 3 { 12 } else { 8 } {
        if !bits.flag()? {
            continue;
        }
        let (mut last, mut next) = (8, 8);
        for _ in 0..if list < 6 { 16 } else { 64 } {
            if next != 0 {
                let delta = bits.se()?;
                if !(-128..=127).contains(&delta) {
                    return Err(SyntaxError::OutOfRange);
                }
                next = (last + delta) & 0xff;
                if next != 0 {
                    last = next;
                }
            }
        }
    }
    Ok(())
}

/// Passes over the VUI fields before its timing (H.264 E.1.1, H.265 E.2.1) and
/// returns `timing_info_present_flag` (`vui_timing_info_present_flag`).
pub fn skip_vui_to_timing(bits: &mut impl BitSource, codec: Codec) -> Result<bool, BitError> {
    if bits.flag()? && bits.read(8)? == 255 {
        bits.read(32)?; // sar_width, sar_height
    }
    if bits.flag()? {
        bits.flag()?; // overscan_appropriate_flag
    }
    if bits.flag()? {
        bits.read(4)?; // video_format, video_full_range_flag
        if bits.flag()? {
            bits.read(24)?; // colour_primaries, transfer_characteristics, matrix_coeffs
        }
    }
    if bits.flag()? {
        bits.ue()?; // chroma_sample_loc_type_top_field
        bits.ue()?; // chroma_sample_loc_type_bottom_field
    }
    if codec == Codec::Hevc {
        bits.read(3)?; // neutral_chroma, field_seq, frame_field_info_present
        if bits.flag()? {
            for _ in 0..4 {
                bits.ue()?; // default display window offsets
            }
        }
    }
    bits.flag()
}

/// Passes over `profile_tier_level(1, max_sub_layers_minus1)` (H.265 7.3.3) and
/// returns its `reserved_zero_2bits`, which FFmpeg skips and a strict reader
/// requires to be zero.
pub fn skip_hevc_profile_tier_level(
    bits: &mut impl BitSource,
    max_sub_layers_minus1: u8,
) -> Result<u32, BitError> {
    for _ in 0..3 {
        bits.read(32)?; // general profile (88 bits) and general_level_idc
    }
    // Each sub-layer's profile_present and level_present flags.
    let mut present = [0; 7];
    let present = &mut present[..usize::from(max_sub_layers_minus1.min(7))];
    for flags in &mut *present {
        *flags = bits.read(2)?;
    }
    let reserved = match present.len() as u32 {
        0 => 0,
        sub_layers => bits.read(2 * (8 - sub_layers))?,
    };
    for &flags in &*present {
        if flags & 2 != 0 {
            bits.read(32)?;
            bits.read(32)?;
            bits.read(24)?; // sub-layer profile
        }
        if flags & 1 != 0 {
            bits.read(8)?; // sub_layer_level_idc
        }
    }
    Ok(reserved)
}

#[cfg(test)]
mod tests {
    use super::{SyntaxError, skip_avc_scaling_matrix, skip_hevc_profile_tier_level};
    use triergon_bitstream::{BitCursor, BitError, BitSource};

    #[test]
    fn avc_scaling_matrix_stops_at_a_zero_scale_or_an_out_of_range_delta() {
        // Present, list 0 present with delta -8 (se code 16 = 000010001): the
        // next scale is zero, so no further delta is read. Lists 1..7 absent.
        let mut bits = BitCursor::new(&[0b1100_0010, 0b0010_0000, 0]);
        assert_eq!(skip_avc_scaling_matrix(&mut bits, 1), Ok(()));
        assert_eq!(bits.read(6), Ok(0));
        // Delta 128: se code 255, eight zeros then 1_0000_0000.
        assert_eq!(
            skip_avc_scaling_matrix(&mut BitCursor::new(&[0b1100_0000, 0b0010_0000, 0]), 1),
            Err(SyntaxError::OutOfRange)
        );
    }

    #[test]
    fn hevc_profile_tier_level_returns_its_reserved_bits() {
        // Two sub-layers: flags 10 (profile) and 01 (level), then 12 reserved bits.
        let mut data = vec![0; 12];
        data.extend_from_slice(&[0b1001_0000, 0b0000_0011]);
        data.extend_from_slice(&[0; 12]);
        let mut bits = BitCursor::new(&data);
        assert_eq!(skip_hevc_profile_tier_level(&mut bits, 2), Ok(3));
        assert_eq!(bits.read(4), Err(BitError::End));
    }
}
