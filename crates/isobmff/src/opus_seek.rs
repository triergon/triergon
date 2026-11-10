//! Resolve a regular Opus sample's declared recovery on the source clock.

use crate::{
    BuildStatus, RecoveryLimits, RecoveryLookup, RecoveryScope, Sample, SampleLocator, SampleStep,
    SampleTable,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Error, Reservation, Result, Time, TimeBase, WorkBudget};

/// Decoder start for a regular Opus track selected during a movie seek.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpusRecoveryLanding {
    pub track_id: u32,
    /// One-based source ordinal of the audio sample at the video landing.
    pub sync_sample: u64,
    /// One-based source ordinal at which the decoder must start.
    pub first_sample: u64,
    /// Source-clock time available to warm up the decoder before the sync sample.
    pub preroll: Time,
}

#[derive(Debug)]
pub(crate) struct OpusRecoverySeek<'a> {
    track_id: u32,
    table: &'a SampleTable,
    base: TimeBase,
    ordinal: u64,
    ticks: i128,
    roll: Option<RecoveryLookup<'a>>,
    locate: Option<SampleLocator<'a>>,
    _charge: Reservation,
}

impl<'a> OpusRecoverySeek<'a> {
    pub(crate) fn new(
        pool: &Budget,
        track: crate::DemuxTrack<'a>,
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
            roll: Some(RecoveryLookup::new(
                scope,
                sync.ordinal,
                RecoveryLimits::default(),
            )?),
            locate: None,
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
        if let Some(roll) = &mut self.roll {
            if roll.poll(work, cancel)? == BuildStatus::Yield || !work.try_charge(1) {
                return Ok(None);
            }
            let selected = self.roll.take().expect("polled Opus roll group").finish()?;
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
            self.locate = Some(self.table.locate(first)?);
        }
        let locator = self
            .locate
            .as_mut()
            .ok_or(Error::Invalid("missing Opus preroll sample"))?;
        let sample = match locator.poll(work)? {
            SampleStep::Yield => return Ok(None),
            SampleStep::Sample(sample) => sample,
            SampleStep::End => return Err(Error::Invalid("missing Opus preroll sample")),
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
        Ok(Some(OpusRecoveryLanding {
            track_id: self.track_id,
            sync_sample: self.ordinal,
            first_sample: sample.ordinal,
            preroll,
        }))
    }
}
