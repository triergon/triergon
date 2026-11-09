//! Closed captions: CEA-608 and CEA-708 caption data carried in video (ATSC A/53,
//! SCTE 20) and its decoding to timed text.
//!
//! | Module | What it reads |
//! | --- | --- |
//! | [`cc_data`] | `cc_data()` constructs: field 1 and 2 byte pairs, DTVCC data |
//! | [`line21`] | CEA-608 byte pairs: control codes and character sets |
//! | [`dtvcc`] | CEA-708 caption channel packets and service blocks |
//! | [`user_data`] | ATSC A/53 `GA94` caption data and SCTE 20 user data |
//! | [`video`] | the caption data of MPEG-2 pictures and H.264/HEVC access units |
//!
//! [`compat::cc`](crate::compat::cc) reproduces FFmpeg: its `ccaption` decoder,
//! its order and timing of caption data taken from video, and its `scc` demuxer.

pub mod cc_data;
pub mod dtvcc;
pub mod line21;
pub mod user_data;
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
