//! Pin-bound startup clock view over validated source fragments.
//! Unsigned tfdt and exact i128 source endpoints never become demux fields.
use crate::{BoxView, BuildStatus, ClockField, FragmentCursor, FragmentIndex, Sample, SampleStep};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Error, InputCursor, InputToken, Result, SourceId, WorkBudget};

/// Explicit admission to reference observations, not an ISO clock rule.
#[derive(Clone, Copy, Debug)]
pub struct FragmentClockProfile;
#[derive(Debug)]
pub struct FragmentClockResult {
    pub raw_traf: BoxView,
    pub raw_tfdt: BoxView,
    pub source: SourceId,
    pub token: InputToken,
    pub source_start: u64,
    pub source_end: i128,
    pub demux_start: i64,
    pub demux_end: Option<i64>,
    pub rejected_source_ordinal: Option<u64>,
}
impl FragmentClockResult {
    /// None admits only this clock domain, never the rest of demux open.
    pub fn demux_open_result(&self) -> Option<i32> {
        self.rejected_source_ordinal.map(|_| -1_094_995_529)
    }
}
/// Refuses a decode run that FFmpeg's signed clock cannot finish.
///
/// FFmpeg accumulates durations from an explicit `tfdt` at or below `i64::MAX`, so
/// the run must also end at or below it. Its open and index operations fail on such
/// a fragment. A higher start stays on [`ClockField`], and source indexes keep the
/// unsigned start and the exact end either way.
pub fn check_signed_decode_runs(
    fragments: &[FragmentIndex],
    _profile: FragmentClockProfile,
) -> Result<()> {
    let maximum = i128::from(i64::MAX);
    for fragment in fragments {
        if fragment
            .decode_time()
            .is_some_and(|start| i128::from(start.ticks()) <= maximum)
            && fragment.decode_end().is_some_and(|end| end > maximum)
        {
            return Err(Error::Invalid("native signed decode run overflow"));
        }
    }
    Ok(())
}
/// Source cursor syntax work plus one work unit for each demux duration fold.
/// One pending source sample allows Work1 progress without repeating parsing.
/// No payload read, source seek, copied workspace or demux confirmation is invented.
#[derive(Debug)]
pub struct FragmentClockJob<'a> {
    index: &'a FragmentIndex,
    cursor: Option<FragmentCursor<'a>>,
    pending: Option<Sample>,
    demux_start: i64,
    demux_end: i64,
    rejected: Option<u64>,
    pool: Budget,
    source: SourceId,
    token: InputToken,
    position: u64,
    ready: Option<FragmentClockResult>,
    failed: Option<Error>,
    finished: bool,
}
impl<'a> FragmentClockJob<'a> {
    pub fn new(
        index: &'a FragmentIndex,
        input: &InputCursor,
        maximum: u64,
        _profile: FragmentClockProfile,
    ) -> Result<Self> {
        if !index.raw().bytes().belongs_to(input.budget()) {
            return Err(Error::ForeignPool);
        }
        let start = index
            .decode_time()
            .ok_or(Error::Unsupported("v6 explicit tfdt required"))?
            .ticks();
        if index.sample_count() > maximum {
            return Err(Error::Limit {
                requested: usize::try_from(index.sample_count()).map_err(|_| Error::Overflow)?,
                maximum: usize::try_from(maximum).map_err(|_| Error::Overflow)?,
            });
        }
        if index.decode_end().is_none() {
            return Err(Error::Unsupported("v6 source decode endpoint unknown"));
        }
        let demux_start = ClockField::from_exact_ticks(i128::from(start))?.signed_ticks();
        Ok(Self {
            index,
            cursor: Some(index.cursor()),
            pending: None,
            demux_start,
            demux_end: demux_start,
            rejected: None,
            pool: input.budget().clone(),
            source: input.source(),
            token: input.token(),
            position: input.position(),
            ready: None,
            failed: None,
            finished: false,
        })
    }
    fn binding(&self, input: &InputCursor) -> Result<()> {
        if !input.budget().same_pool(&self.pool) {
            return Err(Error::ForeignPool);
        }
        if input.source() != self.source {
            return Err(Error::SourceMismatch);
        }
        let token = input.token();
        if token.generation() != self.token.generation() {
            return Err(Error::StaleGeneration {
                expected: self.token.generation(),
                received: token.generation(),
            });
        }
        if token != self.token {
            return Err(Error::StaleRequest {
                expected: self.token.request(),
                received: token.request(),
            });
        }
        if input.position() != self.position {
            return Err(Error::Invalid(
                "v6 input moved during startup clock inspection",
            ));
        }
        Ok(())
    }
    fn poison(&mut self, error: Error) -> Error {
        self.cursor = None;
        self.pending = None;
        self.ready = None;
        self.failed = Some(error);
        error
    }
    pub fn poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.inner(input, work, cancel);
        result.map_err(|error| self.poison(error))
    }
    fn inner(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStatus> {
        self.binding(input)?;
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        input.read_into(&mut [], work, cancel)?;
        if self.finished {
            return Err(Error::Invalid("v6 startup clock already finished"));
        }
        if self.ready.is_some() {
            return Ok(BuildStatus::Ready);
        }
        loop {
            if work.remaining() == 0 {
                return Ok(BuildStatus::Yield);
            }
            if self.pending.is_none() {
                match self
                    .cursor
                    .as_mut()
                    .ok_or(Error::Invalid("v6 clock cursor absent"))?
                    .poll(work)?
                {
                    SampleStep::Yield => return Ok(BuildStatus::Yield),
                    SampleStep::Sample(sample) => self.pending = Some(sample),
                    SampleStep::End => {
                        let tfdt = self
                            .index
                            .decode_time()
                            .ok_or(Error::Invalid("v6 tfdt disappeared"))?;
                        self.ready = Some(FragmentClockResult {
                            raw_traf: self.index.raw().clone(),
                            raw_tfdt: tfdt.raw().clone(),
                            source: self.source,
                            token: self.token.clone(),
                            source_start: tfdt.ticks(),
                            source_end: self
                                .index
                                .decode_end()
                                .ok_or(Error::Invalid("v6 endpoint disappeared"))?,
                            demux_start: self.demux_start,
                            demux_end: self.rejected.is_none().then_some(self.demux_end),
                            rejected_source_ordinal: self.rejected,
                        });
                        self.cursor = None;
                        return Ok(BuildStatus::Ready);
                    }
                }
            }
            if !work.try_charge(1) {
                return Ok(BuildStatus::Yield);
            }
            let sample = self
                .pending
                .take()
                .ok_or(Error::Invalid("v6 pending duration absent"))?;
            if self.rejected.is_none() {
                match self.demux_end.checked_add(i64::from(sample.duration_ticks)) {
                    Some(end) => self.demux_end = end,
                    None => self.rejected = Some(sample.ordinal),
                }
            }
        }
    }
    pub fn take_result(&mut self) -> Result<FragmentClockResult> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self
            .ready
            .take()
            .ok_or(Error::Invalid("v6 native startup clock not ready"))?;
        self.finished = true;
        Ok(result)
    }
    /// Recheck the live source transaction at the consumer publication boundary.
    pub fn try_finish(
        &mut self,
        input: &mut InputCursor,
        cancel: &AtomicBool,
    ) -> Result<FragmentClockResult> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let checked = (|| {
            self.binding(input)?;
            input.read_into(&mut [], &mut WorkBudget::new(0), cancel)?;
            self.take_result()
        })();
        checked.map_err(|error| self.poison(error))
    }
}
