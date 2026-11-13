//! Closed captions carried in video (ATSC A/53, SCTE 20, DVD user data and
//! H.264/HEVC SEI).
//!
//! [`compat::cc`](crate::compat::cc) reproduces FFmpeg: its `cc_dec` decoder
//! of CEA-608, its order and timing of caption data taken from video, and its
//! `scc` demuxer. CEA-708 data is passed through untouched, as in FFmpeg.
//! [`video::nal`] names how a video stream carries its NAL units.

pub(crate) mod cc_data;
pub(crate) mod line21;
pub(crate) mod user_data;
pub mod video;

pub(crate) mod extraction;
pub(crate) mod receiver;
pub(crate) mod scc;
mod text;

use triergon_core::{Error, Result, WorkBudget};

/// Charges `units` of work, refusing input the budget cannot cover.
fn charge(work: &mut WorkBudget, units: usize) -> Result<()> {
    if work.try_charge(units as u64) {
        Ok(())
    } else {
        Err(Error::Limit {
            requested: units,
            maximum: usize::try_from(work.remaining()).unwrap_or(usize::MAX),
        })
    }
}
