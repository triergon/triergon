//! Long term prediction (ISO/IEC 14496-3 4.6.6).
//!
//! The history holds the two previous fully reconstructed output frames followed by the
//! windowed (not yet overlapped) second half of the last inverse transform. The estimate
//! `coef * history[i + 2048 - lag]` is windowed and transformed with the forward MDCT, TNS
//! analysis filtered when TNS is active, and added to the bands flagged in ltp_long_used.

use crate::filterbank::Filterbank;
use crate::ics::{ChannelData, CoreCfg, EIGHT_SHORT, TNS_ANALYSIS};
use crate::tables::{LTP_COEF, MAX_LTP_LONG_SFB};
use triergon_codec::{Budget, BudgetVec, StorageError};

#[derive(Debug)]
pub(crate) struct LtpState {
    hist: BudgetVec<f32>,
    time: BudgetVec<f32>,
    spec: BudgetVec<f32>,
}

impl LtpState {
    pub(crate) fn new(budget: &Budget) -> Result<LtpState, StorageError> {
        Ok(LtpState {
            hist: BudgetVec::try_filled(budget, 0.0, 3072)?,
            time: BudgetVec::try_filled(budget, 0.0, 2048)?,
            spec: BudgetVec::try_filled(budget, 0.0, 1024)?,
        })
    }

    pub(crate) fn reset(&mut self) {
        self.hist.fill(0.0);
        self.time.fill(0.0);
        self.spec.fill(0.0);
    }
}

pub(crate) fn apply(
    l: &mut LtpState,
    fb: &mut Filterbank,
    cd: &mut ChannelData,
    prev_shape: u8,
    cfg: &CoreCfg,
) {
    let info = cd.info;
    let ltp = info.ltp[0];
    if info.window_sequence == EIGHT_SHORT || !ltp.present {
        return;
    }
    let coef = LTP_COEF[ltp.coef as usize];
    let lag = ltp.lag as usize;
    let n = if lag < 1024 { lag + 1024 } else { 2048 };
    for i in 0..2048 {
        l.time[i] = if i < n {
            l.hist[i + 2048 - lag] * coef
        } else {
            0.0
        };
    }
    fb.mdct_windowed(
        &l.time,
        info.window_sequence,
        info.window_shape,
        prev_shape,
        &mut l.spec,
    );
    if cd.tns.present {
        crate::ics::tns_filter(&cd.tns, &info, cfg, &mut l.spec, TNS_ANALYSIS);
    }
    for sfb in 0..info.max_sfb.min(MAX_LTP_LONG_SFB) {
        if ltp.long_used[sfb] {
            for k in info.swb[sfb] as usize..info.swb[sfb + 1] as usize {
                cd.coef[k] += l.spec[k];
            }
        }
    }
}

/// Shift the history after the filterbank of the current frame.
pub(crate) fn update(l: &mut LtpState, out: &[f32], overlap: &[f32]) {
    l.hist.copy_within(1024..2048, 0);
    l.hist[1024..2048].copy_from_slice(&out[..1024]);
    l.hist[2048..3072].copy_from_slice(&overlap[..1024]);
}
