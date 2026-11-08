//! Subframe quantization gains (RFC 6716 section 4.2.7.4).

/// The coded gain of one subframe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GainIndex {
    /// A 6-bit log gain.
    Absolute(u8),
    /// A change from the previous subframe's log gain, 0 to 40.
    Delta(u8),
}

/// Log gain of the previous subframe after a reset; low enough that no clamp applies.
pub(super) const RESET_LOG_GAIN: i32 = 10;

/// Dequantize `indices` into Q16 gains. `prev` is the log gain of the previous subframe
/// in the same channel and is updated.
pub(super) fn dequantize(indices: &[GainIndex], prev: &mut i32, gains_q16: &mut [i32]) {
    for (&index, gain) in indices.iter().zip(gains_q16) {
        let log_gain = match index {
            GainIndex::Absolute(index) => i32::from(index).max(*prev - 16),
            GainIndex::Delta(delta) => {
                let delta = i32::from(delta);
                (2 * delta - 16).max(*prev + delta - 4).clamp(0, 63)
            }
        };
        *prev = log_gain;
        *gain = log2lin(((0x1D1C71 * log_gain) >> 16) + 2090);
    }
}

/// `silk_log2lin`: approximately 2^(log_q7 / 128). Gains only reach it with an integer
/// part of at least 16.
fn log2lin(log_q7: i32) -> i32 {
    let i = log_q7 >> 7;
    let f = log_q7 & 127;
    (1 << i) + (((-174 * f * (128 - f)) >> 16) + f) * ((1 << i) >> 7)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gains_span_the_documented_range() {
        let mut prev = RESET_LOG_GAIN;
        let mut gains = [0; 1];
        dequantize(&[GainIndex::Absolute(0)], &mut prev, &mut gains);
        assert_eq!(gains[0], 81920);
        dequantize(&[GainIndex::Absolute(63)], &mut prev, &mut gains);
        assert_eq!(gains[0], 1686110208);
    }

    #[test]
    fn independent_gains_fall_at_most_16_steps() {
        let mut prev = 40;
        let mut gains = [0; 1];
        dequantize(&[GainIndex::Absolute(3)], &mut prev, &mut gains);
        assert_eq!(prev, 24);
    }

    #[test]
    fn delta_gains_follow_the_two_slopes() {
        let mut prev = 20;
        let mut gains = [0; 3];
        // Small deltas step by one, large ones by two, and the result stays in 0..=63.
        dequantize(
            &[
                GainIndex::Delta(4),
                GainIndex::Delta(0),
                GainIndex::Delta(40),
            ],
            &mut prev,
            &mut gains,
        );
        assert_eq!(prev, 63);
        let mut prev = 20;
        dequantize(&[GainIndex::Delta(6)], &mut prev, &mut gains[..1]);
        assert_eq!(prev, 22);
        let mut prev = 0;
        dequantize(&[GainIndex::Delta(0)], &mut prev, &mut gains[..1]);
        assert_eq!(prev, 0);
        assert!(gains.windows(2).all(|pair| pair[0] > 0));
    }
}
