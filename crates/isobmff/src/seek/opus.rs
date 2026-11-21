//! Where a regular Opus track's decoder starts after a seek. [`OpusRecoveryLanding`]
//! names the sample at the landing and the earlier sample its `roll` group points back
//! to, which must give at least 80 ms of preroll unless it is the track's first sample.
//!
//! Specification: Opus in ISO BMFF §4.3.6.2.

use crate::{
    BuildStatus, Next, Sample,
    sample_groups::{RecoveryLimits, RecoveryLookup, RecoveryScope},
    table::{SampleLocator, SampleTable},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Error, Reservation, Result, Time, TimeBase, WorkBudget};

/// Where the decoder of a regular Opus track starts after a seek, from the track's
/// `roll` group, as the `with_opus_recovery` option of
/// [`GlobalSeekJob`](crate::compat::mov::seek::GlobalSeekJob) and
/// [`RangeSeekJob`](crate::compat::mov::seek::RangeSeekJob) reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpusRecoveryLanding {
    /// `track_ID` of the Opus track.
    pub track_id: u32,
    /// 1-based source ordinal of the audio sample at the video landing.
    pub sync_sample: u64,
    /// 1-based source ordinal at which the decoder must start; at most `sync_sample`,
    /// and 1 when recovery reaches back to the track's start.
    pub first_sample: u64,
    /// Decode time from `first_sample` to `sync_sample` on the track's media clock:
    /// at least 80 ms unless `first_sample` is 1.
    pub preroll: Time,
}

/// What an Opus recovery seek reads: the `roll` entry at the sync sample, then the sample
/// the decoder starts at.
#[derive(Debug)]
enum Stage<'a> {
    Roll(RecoveryLookup<'a>),
    Preroll(SampleLocator<'a>),
}

#[derive(Debug)]
pub(crate) struct OpusRecoverySeek<'a> {
    track_id: u32,
    table: &'a SampleTable,
    base: TimeBase,
    ordinal: u64,
    ticks: i128,
    stage: Stage<'a>,
    _charge: Reservation,
}

impl<'a> OpusRecoverySeek<'a> {
    pub(crate) fn new(
        pool: &Budget,
        track: crate::compat::mov::cursor::DemuxTrack<'a>,
        scope: RecoveryScope<'a>,
        sync: &Sample,
    ) -> Result<Box<Self>> {
        Ok(Box::new(Self {
            track_id: track.track_id,
            table: track.table,
            base: track.media_base,
            ordinal: sync.ordinal,
            ticks: sync
                .dts_ticks
                .ok_or(Error::Invalid("missing Opus seek clock"))?,
            stage: Stage::Roll(RecoveryLookup::new(
                scope,
                sync.ordinal,
                RecoveryLimits::default(),
            )?),
            _charge: pool.reserve(size_of::<Self>())?,
        }))
    }

    pub(crate) fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<OpusRecoveryLanding>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let minimum = Time::new(2, 25)?; // Opus in ISO BMFF, section 4.3.6.2.
        loop {
            match &mut self.stage {
                Stage::Roll(roll) => {
                    if roll.poll(work, cancel)? == BuildStatus::Yield || !work.try_charge(1) {
                        return Ok(None);
                    }
                    let selected = roll.take_selection()?;
                    let first = if selected.description.is_none() {
                        // An ungrouped sample near the beginning can decode from
                        // source start, which provides all available history.
                        if Time::from_ticks(self.ticks, self.base)? >= minimum {
                            return Err(Error::Unsupported("missing Opus roll at seek"));
                        }
                        1
                    } else {
                        let preceding = selected.preceding_samples()?;
                        if preceding == 0 {
                            return Err(Error::Invalid("zero Opus roll distance"));
                        }
                        self.ordinal.saturating_sub(u64::from(preceding)).max(1)
                    };
                    self.stage = Stage::Preroll(self.table.locate(first)?);
                }
                Stage::Preroll(locator) => {
                    let sample = match locator.poll(work)? {
                        Next::Yield => return Ok(None),
                        Next::Item(sample) => sample,
                        Next::End => return Err(Error::Invalid("missing Opus preroll sample")),
                    };
                    let start = sample
                        .dts_ticks
                        .ok_or(Error::Invalid("missing Opus preroll clock"))?;
                    let preroll = Time::from_ticks(
                        self.ticks.checked_sub(start).ok_or(Error::Overflow)?,
                        self.base,
                    )?;
                    if preroll < minimum && sample.ordinal != 1 {
                        return Err(Error::Unsupported(
                            "Opus roll provides less than 80 ms of preroll",
                        ));
                    }
                    return Ok(Some(OpusRecoveryLanding {
                        track_id: self.track_id,
                        sync_sample: self.ordinal,
                        first_sample: sample.ordinal,
                        preroll,
                    }));
                }
            }
        }
    }
}
