//! Pieces Layers I and II share: scalefactors, sample requantization and the joint
//! stereo bound (ISO/IEC 11172-3 2.4.3.2 and 2.4.3.3).

use triergon_framing::{MpegAudioHeader, MpegChannelMode};

/// Scalefactor `index` of Table B.1: `2^(1 - index / 3)`.
pub(crate) fn scalefactor(index: u32) -> f32 {
    const THIRDS: [f32; 3] = [1.0, 0.793_700_5, 0.629_960_5];
    2f32.powi(1 - (index / 3) as i32) * THIRDS[(index % 3) as usize]
}

/// The fraction a code `code` of a quantizer with `levels` steps stands for:
/// `(2 code - (levels - 1)) / levels`. This is `C (s''' + D)` of Table B.4 (and the
/// Layer I formula of 2.4.3.2.3) with the most significant bit inverted.
#[inline(always)]
pub(crate) fn dequantize(code: u32, levels: u32) -> f32 {
    (2.0 * code as f32 - (levels - 1) as f32) / levels as f32
}

/// The first subband coded once for both channels (`bound`), at most `limit`.
pub(crate) fn joint_bound(header: &MpegAudioHeader, limit: usize) -> usize {
    match header.channel_mode {
        MpegChannelMode::JointStereo => (4 * (usize::from(header.mode_extension) + 1)).min(limit),
        _ => limit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalefactors_follow_table_b1() {
        assert_eq!(scalefactor(0), 2.0);
        assert_eq!(scalefactor(3), 1.0);
        assert!((scalefactor(1) - 1.587_401).abs() < 1e-6);
        assert!((scalefactor(62) - 2f32.powf(1.0 - 62.0 / 3.0)).abs() < 1e-12);
    }

    #[test]
    fn requantized_levels_are_symmetric() {
        assert_eq!(
            [0, 1, 2].map(|c| dequantize(c, 3)),
            [-2.0 / 3.0, 0.0, 2.0 / 3.0]
        );
        assert_eq!(dequantize(3, 7), 0.0);
        assert_eq!(dequantize(0, 7), -dequantize(6, 7));
    }
}
