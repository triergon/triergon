//! AAC Main profile backward-adaptive prediction (ISO/IEC 14496-3 4.6.7 / ISO/IEC 13818-7
//! 8.3): one second-order lattice LMS predictor per spectral line, state kept in 16-bit
//! (bfloat-like) precision with the rounding modes the standard prescribes.

use crate::ics::{ChannelData, CoreCfg, EIGHT_SHORT, NOISE_HCB};
use crate::tables::PRED_SFB_MAX;
use triergon_codec::{Budget, BudgetVec, StorageError};

const ALPHA: f32 = 0.90625;
const A: f32 = 0.953125;

#[derive(Debug, Clone, Copy)]
struct P {
    r0: f32,
    r1: f32,
    cor0: f32,
    cor1: f32,
    var0: f32,
    var1: f32,
}

const RESET: P = P {
    r0: 0.0,
    r1: 0.0,
    cor0: 0.0,
    cor1: 0.0,
    var0: 1.0,
    var1: 1.0,
};

#[derive(Debug)]
pub(crate) struct PredState {
    s: BudgetVec<P>,
}

impl PredState {
    pub(crate) fn new(budget: &Budget) -> Result<PredState, StorageError> {
        Ok(PredState {
            s: BudgetVec::try_filled(budget, RESET, 1024)?,
        })
    }

    pub(crate) fn reset(&mut self) {
        self.s.fill(RESET);
    }
}

/// Round to nearest (ties away) keeping the upper 16 bits of the IEEE single.
#[inline(always)]
fn round16(x: f32) -> f32 {
    f32::from_bits((x.to_bits().wrapping_add(0x8000)) & 0xffff_0000)
}

/// Round to nearest even keeping the upper 16 bits.
#[inline(always)]
fn even16(x: f32) -> f32 {
    let b = x.to_bits();
    f32::from_bits(b.wrapping_add(0x7fff + ((b >> 16) & 1)) & 0xffff_0000)
}

/// Truncate to the upper 16 bits.
#[inline(always)]
fn trunc16(x: f32) -> f32 {
    f32::from_bits(x.to_bits() & 0xffff_0000)
}

#[inline(always)]
fn predict(p: &mut P, coef: &mut f32, enable: bool) {
    let k1 = if p.var0 > 1.0 {
        p.cor0 * even16(A / p.var0)
    } else {
        0.0
    };
    let k2 = if p.var1 > 1.0 {
        p.cor1 * even16(A / p.var1)
    } else {
        0.0
    };
    let pv = round16(k1 * p.r0 + k2 * p.r1);
    if enable {
        *coef += pv;
    }
    let e0 = *coef;
    let e1 = e0 - k1 * p.r0;
    p.cor1 = trunc16(ALPHA * p.cor1 + p.r1 * e1);
    p.var1 = trunc16(ALPHA * p.var1 + 0.5 * (p.r1 * p.r1 + e1 * e1));
    p.cor0 = trunc16(ALPHA * p.cor0 + p.r0 * e0);
    p.var0 = trunc16(ALPHA * p.var0 + 0.5 * (p.r0 * p.r0 + e0 * e0));
    p.r1 = trunc16(A * (p.r0 - k1 * e0));
    p.r0 = trunc16(A * e0);
}

pub(crate) fn apply(ps: &mut PredState, cd: &mut ChannelData, cfg: &CoreCfg) {
    let info = cd.info;
    if info.window_sequence == EIGHT_SHORT {
        ps.s.fill(RESET);
        return;
    }
    let nsfb = (PRED_SFB_MAX[cfg.sf_index] as usize).min(info.num_swb);
    for sfb in 0..nsfb {
        let enable = info.predictor_data_present && info.prediction_used[sfb];
        let noise = sfb < info.max_sfb && cd.band_type[sfb] == NOISE_HCB;
        for k in info.swb[sfb] as usize..info.swb[sfb + 1] as usize {
            if noise {
                // PNS band: the predictor is reset instead of being run (4.6.13.3)
                ps.s[k] = RESET;
            } else {
                predict(&mut ps.s[k], &mut cd.coef[k], enable);
            }
        }
    }
    if info.predictor_reset_group > 0 {
        let mut k = info.predictor_reset_group as usize - 1;
        while k < 1024 {
            ps.s[k] = RESET;
            k += 30;
        }
    }
}
