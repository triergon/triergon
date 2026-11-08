//! Long-term prediction parameters (RFC 6716 section 4.2.7.6): pitch lags, the 5-tap LTP
//! filters and the LTP scaling factor.

use super::Bandwidth;
use super::frame::Coding;
use super::tables::*;
use crate::range::RangeDecoder;

/// The coded pitch parameters of a voiced SILK frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PitchIndices {
    /// Primary lag minus the minimum lag, unclamped (relative coding may leave the range).
    pub(super) lag_index: i32,
    contour: u8,
    periodicity: u8,
    filters: [u8; 4],
    scale: u8,
}

impl PitchIndices {
    pub(super) fn parse(
        rc: &mut RangeDecoder<'_>,
        bandwidth: Bandwidth,
        subframes: usize,
        coding: Coding,
        prev_lag: Option<i32>,
    ) -> PitchIndices {
        let relative = match (coding, prev_lag) {
            (Coding::Conditional, Some(prev)) => match rc.icdf(&PITCH_LAG_DELTA, 8) as i32 {
                0 => None,
                delta => Some(prev + delta - 9),
            },
            _ => None,
        };
        let lag_index = relative.unwrap_or_else(|| {
            let high = rc.icdf(&PITCH_LAG_HIGH, 8) as i32;
            let low = match bandwidth {
                Bandwidth::Narrow => rc.icdf(&UNIFORM4, 8),
                Bandwidth::Medium => rc.icdf(&UNIFORM6, 8),
                Bandwidth::Wide => rc.icdf(&UNIFORM8, 8),
            };
            high * (bandwidth.khz() / 2) as i32 + low as i32
        });
        let contour = rc.icdf(contour_table(bandwidth, subframes).0, 8) as u8;
        let periodicity = rc.icdf(&LTP_PERIODICITY, 8);
        let mut filters = [0; 4];
        for filter in &mut filters[..subframes] {
            *filter = rc.icdf(filter_table(periodicity).0, 8) as u8;
        }
        let scale = match coding {
            Coding::Independent { ltp_scaling: true } => rc.icdf(&LTP_SCALE, 8) as u8,
            _ => 0,
        };
        PitchIndices {
            lag_index,
            contour,
            periodicity: periodicity as u8,
            filters,
            scale,
        }
    }
}

/// The contour PDF and the subframe lag offsets of each contour.
fn contour_table(bandwidth: Bandwidth, subframes: usize) -> (&'static [u8], &'static [[i8; 4]]) {
    // 10 ms codebooks are stored with two subframes; pad them to the 4-subframe shape.
    static NB_10MS: [[i8; 4]; 3] = widen(CONTOUR_NB_10MS_LAGS);
    static MB_WB_10MS: [[i8; 4]; 12] = widen(CONTOUR_10MS_LAGS);
    match (bandwidth, subframes) {
        (Bandwidth::Narrow, 2) => (&CONTOUR_NB_10MS, &NB_10MS),
        (Bandwidth::Narrow, _) => (&CONTOUR_NB_20MS, &CONTOUR_NB_20MS_LAGS),
        (_, 2) => (&CONTOUR_10MS, &MB_WB_10MS),
        _ => (&CONTOUR_20MS, &CONTOUR_20MS_LAGS),
    }
}

const fn widen<const N: usize>(lags: [[i8; 2]; N]) -> [[i8; 4]; N] {
    let mut out = [[0; 4]; N];
    let mut i = 0;
    while i < N {
        out[i] = [lags[i][0], lags[i][1], 0, 0];
        i += 1;
    }
    out
}

/// The filter PDF and the Q7 filter taps of a periodicity index.
fn filter_table(periodicity: usize) -> (&'static [u8], &'static [[i8; 5]]) {
    match periodicity {
        0 => (&LTP_FILTER_0, &LTP_TAPS_0),
        1 => (&LTP_FILTER_1, &LTP_TAPS_1),
        _ => (&LTP_FILTER_2, &LTP_TAPS_2),
    }
}

/// The dequantized LTP parameters of a voiced frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Ltp {
    /// Pitch lag of each subframe, in samples.
    pub(super) lags: [usize; 4],
    /// Filter taps of each subframe, applied to lags +2 down to -2.
    pub(super) taps: [[f32; 5]; 4],
    /// LTP scaling of the rewhitened history, Q14.
    pub(super) scale_q14: i32,
}

impl Ltp {
    pub(super) fn new(indices: &PitchIndices, bandwidth: Bandwidth, subframes: usize) -> Ltp {
        let khz = bandwidth.khz() as i32;
        let (min_lag, max_lag) = (2 * khz, 18 * khz);
        let offsets = contour_table(bandwidth, subframes).1[usize::from(indices.contour)];
        let taps_q7 = filter_table(usize::from(indices.periodicity)).1;
        let mut ltp = Ltp {
            lags: [0; 4],
            taps: [[0.0; 5]; 4],
            scale_q14: LTP_SCALES_Q14[usize::from(indices.scale)],
        };
        for s in 0..subframes {
            let lag = min_lag + indices.lag_index + i32::from(offsets[s]);
            ltp.lags[s] = lag.clamp(min_lag, max_lag) as usize;
            let taps = taps_q7[usize::from(indices.filters[s])];
            ltp.taps[s] = taps.map(|tap| f32::from(tap) / 128.0);
        }
        ltp
    }
}
