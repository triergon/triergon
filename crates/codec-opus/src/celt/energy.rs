//! Band energies (RFC 6716 section 4.3.2): coarse energy with its Laplace model, fine
//! energy and the final refinement from left-over bits.
//!
//! Energies are base-2 logarithms of the band amplitude with the band's mean
//! ([`E_MEANS`]) removed.

use super::alloc::Allocation;
use super::tables::{
    BANDS, BETA_COEF, BETA_INTRA, E_MEANS, E_PROB_MODEL, PRED_COEF, SMALL_ENERGY_ICDF,
};
use crate::range::RangeDecoder;
use core::ops::Range;

/// Log energies of up to two channels.
pub(super) type Energies = [[f32; BANDS]; 2];

/// Fine energy bits beyond which the refinement passes add nothing.
pub(super) const MAX_FINE_BITS: i32 = 8;

/// Laplace-distributed symbol with probability of zero `fs0` (Q15) and decay `decay`
/// (Q14), RFC 6716 section 4.3.2.1.
fn laplace(rc: &mut RangeDecoder<'_>, fs0: u32, decay: u32) -> i32 {
    const MIN_P: u32 = 1;
    const MIN_BINS: u32 = 16;
    let (fm, ext) = rc.decode_bin(15);
    let mut value = 0;
    let mut fl = 0;
    let mut fs = fs0;
    if fm >= fs {
        value += 1;
        fl = fs;
        fs = (((32768 - MIN_P * 2 * MIN_BINS - fs0) * (16384 - decay)) >> 15) + MIN_P;
        // The decaying part of the distribution.
        while fs > MIN_P && fm >= fl + 2 * fs {
            fs *= 2;
            fl += fs;
            fs = (((fs - 2 * MIN_P) * decay) >> 15) + MIN_P;
            value += 1;
        }
        // Everything beyond has the minimum probability.
        if fs <= MIN_P {
            let extra = (fm - fl) >> 1;
            value += extra as i32;
            fl += 2 * extra * MIN_P;
        }
        if fm < fl + fs {
            value = -value;
        } else {
            fl += fs;
        }
    }
    rc.update(fl, (fl + fs).min(32768), 32768, ext);
    value
}

/// Decode the coarse energies of `bands` (RFC 6716 section 4.3.2.1), predicting from the
/// previous frame's `energy` unless the frame is intra-coded.
pub(super) fn decode_coarse(
    rc: &mut RangeDecoder<'_>,
    energy: &mut Energies,
    bands: Range<usize>,
    intra: bool,
    channels: usize,
    lm: usize,
) {
    let model = &E_PROB_MODEL[lm][usize::from(intra)];
    let (alpha, beta) = if intra {
        (0.0, BETA_INTRA)
    } else {
        (PRED_COEF[lm], BETA_COEF[lm])
    };
    let budget = rc.len() as i32 * 8;
    let mut prev = [0f32; 2];
    for band in bands {
        for (channel, prev) in prev.iter_mut().enumerate().take(channels) {
            let left = budget - rc.tell();
            let q = if left >= 15 {
                let p = 2 * band.min(20);
                laplace(rc, u32::from(model[p]) << 7, u32::from(model[p + 1]) << 6)
            } else if left >= 2 {
                let symbol = rc.icdf(&SMALL_ENERGY_ICDF, 2) as i32;
                (symbol >> 1) ^ -(symbol & 1)
            } else if left >= 1 {
                -i32::from(rc.bit_logp(1))
            } else {
                -1
            } as f32;
            let old = &mut energy[channel][band];
            *old = alpha * old.max(-9.0) + *prev + q;
            *prev = *prev + q - beta * q;
        }
    }
}

/// Decode the fine energy bits of each band (RFC 6716 section 4.3.2.2).
pub(super) fn decode_fine(
    rc: &mut RangeDecoder<'_>,
    energy: &mut Energies,
    bands: Range<usize>,
    allocation: &Allocation,
    channels: usize,
) {
    for band in bands {
        let bits = allocation.fine_bits[band];
        if bits <= 0 {
            continue;
        }
        let scale = (1 << (14 - bits)) as f32 * (1.0 / 16384.0);
        for channel in energy.iter_mut().take(channels) {
            let q = rc.bits(bits as u32) as f32;
            channel[band] += (q + 0.5) * scale - 0.5;
        }
    }
}

/// Spend the bits left at the end of the frame on one more energy bit per band and
/// channel, first for the bands whose fine allocation was rounded down (RFC 6716
/// section 4.3.2.2).
pub(super) fn decode_final(
    rc: &mut RangeDecoder<'_>,
    energy: &mut Energies,
    bands: Range<usize>,
    allocation: &Allocation,
    mut bits_left: i32,
    channels: usize,
) {
    for priority in [false, true] {
        for band in bands.clone() {
            if bits_left < channels as i32 {
                break;
            }
            let bits = allocation.fine_bits[band];
            if bits >= MAX_FINE_BITS || allocation.fine_priority[band] != priority {
                continue;
            }
            let scale = (1 << (14 - bits - 1)) as f32 * (1.0 / 16384.0);
            for channel in energy.iter_mut().take(channels) {
                let q = rc.bits(1) as f32;
                channel[band] += (q - 0.5) * scale;
                bits_left -= 1;
            }
        }
    }
}

/// Band amplitudes from log energies: `2^(energy + mean)`, capped at `2^32` so that
/// extreme streams cannot overflow (RFC 8251 section 8). Bands outside `bands` are zero.
pub(super) fn amplitudes(energy: &[f32; BANDS], bands: Range<usize>, out: &mut [f32; BANDS]) {
    out.fill(0.0);
    for band in bands {
        out[band] = exp2((energy[band] + E_MEANS[band]).min(32.0));
    }
}

/// `2^x` as the reference computes it, through the natural exponential in double
/// precision.
#[inline]
pub(super) fn exp2(x: f32) -> f32 {
    (core::f64::consts::LN_2 * f64::from(x)).exp() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The coarse energies of a frame whose bits ran out: every band's delta is -1, and
    /// the deltas accumulate across bands with the decay `beta`, onto `alpha` times
    /// the previous frame's energy (floored at -9).
    fn starved_coarse(intra: bool, previous: f32) -> [f32; BANDS] {
        let mut energy = [[previous; BANDS]; 2];
        decode_coarse(
            &mut RangeDecoder::new(&[]),
            &mut energy,
            0..BANDS,
            intra,
            1,
            3,
        );
        energy[0]
    }

    #[test]
    fn coarse_energy_predicts_across_bands_and_from_the_previous_frame() {
        for (intra, alpha, beta) in [(true, 0.0, BETA_INTRA), (false, PRED_COEF[3], BETA_COEF[3])] {
            for previous in [0.0, 4.0, -20.0] {
                let mut prev = 0f32;
                for (band, &energy) in starved_coarse(intra, previous).iter().enumerate() {
                    let expected = alpha * previous.max(-9.0) + prev - 1.0;
                    assert_eq!(energy, expected, "intra {intra}, band {band}");
                    prev = prev - 1.0 + beta;
                }
            }
        }
    }

    #[test]
    fn amplitudes_add_the_band_mean_and_are_capped_at_2_to_the_32() {
        let mut out = [1.0; BANDS];
        amplitudes(&[0.0; BANDS], 2..19, &mut out);
        for (band, &amplitude) in out.iter().enumerate() {
            let expected = if (2..19).contains(&band) {
                exp2(E_MEANS[band])
            } else {
                0.0
            };
            assert_eq!(amplitude, expected, "band {band}");
        }
        amplitudes(&[100.0; BANDS], 0..BANDS, &mut out);
        assert_eq!(out, [4_294_967_296.0; BANDS]);
        assert_eq!(
            [exp2(-1.0), exp2(0.0), exp2(1.0), exp2(10.0)],
            [0.5, 1.0, 2.0, 1024.0]
        );
    }
}
