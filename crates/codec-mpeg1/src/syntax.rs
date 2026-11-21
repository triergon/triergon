//! The video bitstream syntax of ISO/IEC 11172-2 §2.4.2 that the encoder writes: the
//! sequence, group of pictures and picture headers, motion vector components, and the
//! DC differentials and run/level coefficients of the block layer. The variable-length
//! codes are those of Annex B ([`tables`](crate::tables)).

use crate::EncoderConfig;
use crate::bits::BitWriter;
use crate::block::bits;
use crate::tables::{
    COEFFICIENTS, DC_SIZE_CHROMA, DC_SIZE_LUMA, DCT_ESCAPE, END_OF_BLOCK, MOTION_CODE, SCAN_BIT,
    ZIGZAG,
};

/// The bit_rate value that identifies variable bit rate operation (§2.4.3.2): a rate the
/// header states is coded below it.
pub(crate) const VARIABLE_BIT_RATE: u32 = 0x3FFFF;

/// Picture rate 30 Hz: frame_rate_code 5 in Table 2-D.4.
const FRAME_RATE_CODE: u32 = 5;

/// The coding types of the pictures the encoder writes (picture_coding_type, §2.4.3.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PictureType {
    Intra,
    Predicted,
}

/// The sequence header of `cfg` (§2.4.2.3): square samples at 30 Hz, the default
/// quantiser matrices, and the configured rate and VBV buffer in their units.
pub(crate) fn write_sequence_header(w: &mut BitWriter, cfg: &EncoderConfig) {
    let g = cfg.geometry;
    w.start_code(0xB3);
    w.put(g.width() as u32, 12);
    w.put(g.height() as u32, 12);
    w.put(1, 4); // pel_aspect_ratio: 1.0 (square)
    w.put(FRAME_RATE_CODE, 4);
    w.put(cfg.max_rate.div_ceil(400), 18);
    w.put(1, 1); // marker
    w.put(cfg.vbv_bits.div_ceil(16384), 10);
    w.put(0, 1); // constrained_parameters_flag
    w.put(0, 1); // load_intra_quantizer_matrix
    w.put(0, 1); // load_non_intra_quantizer_matrix
}

/// The group of pictures header (§2.4.2.4) of a GOP starting at picture `index`:
/// its time code at 30 Hz.
pub(crate) fn write_gop_header(w: &mut BitWriter, index: u64) {
    let n = index;
    w.start_code(0xB8);
    w.put(0, 1); // drop_frame_flag
    w.put(((n / (30 * 3600)) % 24) as u32, 5);
    w.put(((n / 1800) % 60) as u32, 6);
    w.put(1, 1); // marker
    w.put(((n / 30) % 60) as u32, 6);
    w.put((n % 30) as u32, 6);
    // closed_gop: set on the session's first GOP only, as the reference
    // does; with no B pictures the flag has no decoding consequence.
    w.put((n == 0) as u32, 1);
    w.put(0, 1); // broken_link
}

/// The picture header (§2.4.2.5), with vbv_delay 0xFFFF (variable rate); `f_code` is
/// the forward_f_code of a P picture.
pub(crate) fn write_picture_header(
    w: &mut BitWriter,
    temporal_reference: u32,
    picture_type: PictureType,
    f_code: u32,
) {
    w.start_code(0x00);
    w.put(temporal_reference, 10);
    let picture_coding_type = match picture_type {
        PictureType::Intra => 0b001,
        PictureType::Predicted => 0b010,
    };
    w.put(picture_coding_type, 3);
    w.put(0xFFFF, 16); // vbv_delay: variable bit rate
    if picture_type == PictureType::Predicted {
        w.put(0, 1); // full_pel_forward_vector
        w.put(f_code, 3);
    }
    w.put(0, 1); // extra_bit_picture
}

/// The slice header (§2.4.2.6) of a slice starting in the first macroblock row.
pub(crate) fn write_slice_header(w: &mut BitWriter, quantizer_scale: u32) {
    w.start_code(1);
    w.put(quantizer_scale, 5);
    w.put(0, 1); // extra_bit_slice
}

/// motion_code, sign and motion_r for one differential component, wrapped
/// into the f_code range as permitted by the modular reconstruction rule.
pub(crate) fn write_mv_component(w: &mut BitWriter, delta: i32, f_code: u32) {
    let r_size = f_code - 1;
    let f = 1i32 << r_size;
    let (lo, hi) = (-16 * f, 16 * f - 1);
    let mut d = delta;
    if d < lo {
        d += 32 * f;
    } else if d > hi {
        d -= 32 * f;
    }
    if d == 0 {
        w.put(MOTION_CODE[0].0, MOTION_CODE[0].1);
        return;
    }
    let magnitude = d.unsigned_abs() - 1;
    let code = (magnitude >> r_size) + 1;
    let (c, l) = MOTION_CODE[code as usize];
    w.put(c << 1 | (d < 0) as u32, l + 1);
    if r_size > 0 {
        w.put(magnitude & (f as u32 - 1), r_size);
    }
}

/// dct_dc_size and differential bits for an intra DC difference.
#[inline]
pub(crate) fn write_dc(w: &mut BitWriter, diff: i32, luma: bool) {
    let size = 32 - diff.unsigned_abs().leading_zeros();
    let (code, len) = if luma {
        DC_SIZE_LUMA[size as usize]
    } else {
        DC_SIZE_CHROMA[size as usize]
    };
    w.put(code, len);
    if size > 0 {
        let bits = if diff > 0 {
            diff
        } else {
            diff + (1 << size) - 1
        };
        w.put(bits as u32, size);
    }
}

/// Write the run/level pairs of the levels selected by the raster `mask` in
/// zig-zag order, followed by end_of_block. Intra blocks start after DC;
/// non-intra blocks code a leading run-0 level-±1 with dct_coeff_first `1s`.
#[inline]
pub(crate) fn write_coefficients(w: &mut BitWriter, levels: &[i32; 64], mask: u64, intra: bool) {
    let mut scan = 0u64;
    for p in bits(mask) {
        scan |= SCAN_BIT[p];
    }
    let mut next = usize::from(intra);
    let mut first = !intra;
    for k in bits(scan) {
        let run = k - next;
        next = k + 1;
        let level = levels[ZIGZAG[k]];
        let sign = (level < 0) as u32;
        let magnitude = level.unsigned_abs();
        if first && run == 0 && magnitude == 1 {
            w.put(0b10 | sign, 2);
        } else if let Some((code, len)) = COEFFICIENTS.lookup(run, magnitude) {
            w.put(code | sign, len);
        } else {
            w.put(DCT_ESCAPE.0, DCT_ESCAPE.1);
            w.put(run as u32, 6);
            if magnitude < 128 {
                w.put((level & 0xFF) as u32, 8);
            } else if level > 0 {
                w.put(level as u32, 16);
            } else {
                w.put(0x8000 | (level + 256) as u32, 16);
            }
        }
        first = false;
    }
    w.put(END_OF_BLOCK.0, END_OF_BLOCK.1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::nonzero;

    fn written(write: impl FnOnce(&mut BitWriter)) -> String {
        let mut w = BitWriter::default();
        write(&mut w);
        let mut out = Vec::new();
        w.take_into(&mut out).unwrap();
        out.iter().map(|b| format!("{b:08b}")).collect()
    }

    #[test]
    fn dc_differentials() {
        let bits = written(|w| {
            write_dc(w, 0, true); // 100
            write_dc(w, -1, true); // 00 0
            write_dc(w, 3, false); // 10 11
            write_dc(w, -255, true); // 1111110 00000000
        });
        assert!(bits.starts_with("100000101111111100000000"));
    }

    #[test]
    fn non_intra_first_coefficient_short_form() {
        let mut levels = [0; 64];
        levels[0] = -1;
        levels[1] = 1;
        // 11 (first, -1) 110 (next run0 +1 = 11s) 10 (EOB)
        let bits = written(|w| write_coefficients(w, &levels, nonzero(&levels), false));
        assert_eq!(&bits[..8], "11110100");
    }

    #[test]
    fn escape_levels() {
        let mut levels = [0; 64];
        levels[ZIGZAG[1]] = -200;
        levels[ZIGZAG[2]] = 130;
        let bits = written(|w| write_coefficients(w, &levels, nonzero(&levels), true));
        let want = "000001".to_owned()
            + "000000"
            + &format!("{:016b}", 0x8000 | 0x38)
            + "000001"
            + "000000"
            + &format!("{:016b}", 130)
            + "10";
        assert!(bits.starts_with(&want));
    }
}
