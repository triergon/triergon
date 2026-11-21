//! The fragment start clock during FFmpeg's open.
//!
//! [`FragmentClockJob`] folds one fragment's sample durations onto FFmpeg's signed 64-bit
//! clock from its `tfdt` and reports where that clock overflows, with FFmpeg's return code.
//! [`check_signed_decode_runs`] refuses a file whose decode run ends past the signed clock.
//! The unsigned `tfdt` and the exact source end are kept beside the signed values.
use crate::{
    BuildStatus, Next, Sample,
    boxes::BoxView,
    compat::mov::clocks::ClockField,
    fragment::{FragmentCursor, FragmentIndex},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Error, InputCursor, InputToken, Result, SourceId, WorkBudget};

/// Selects FFmpeg's signed fragment clock; pass it to [`FragmentClockJob::new`] or
/// [`check_signed_decode_runs`]. It is not an ISO clock rule.
#[derive(Clone, Copy, Debug)]
pub struct FragmentClockProfile;
/// One fragment's start and end on FFmpeg's signed clock, beside the source values.
#[derive(Debug)]
pub struct FragmentClockResult {
    /// The fragment's `traf` box.
    pub raw_traf: BoxView,
    /// The fragment's `tfdt` box.
    pub raw_tfdt: BoxView,
    /// The input source the job was bound to.
    pub source: SourceId,
    /// The input token the job was bound to.
    pub token: InputToken,
    /// `baseMediaDecodeTime` of the `tfdt`, in ticks of the media timescale.
    pub source_start: u64,
    /// Exact decode end of the fragment's samples, in ticks of the media timescale.
    pub source_end: i128,
    /// The `tfdt` start as FFmpeg's signed field holds it: its 64 bits read as signed.
    pub demux_start: i64,
    /// `demux_start` plus every sample duration, or `None` when that sum overflows `i64`.
    pub demux_end: Option<i64>,
    /// Ordinal of the first sample whose decode end overflows the signed clock, if one does.
    pub rejected_source_ordinal: Option<u64>,
}
impl FragmentClockResult {
    /// FFmpeg's open return code: `Some(-1094995529)` (`AVERROR_INVALIDDATA`) when the
    /// signed clock overflows, `None` otherwise. `None` says nothing about the rest of the
    /// open.
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
/// Folds one fragment's sample durations onto FFmpeg's signed clock.
///
/// Each sample costs one work unit beyond the fragment cursor's own work. The job reads no
/// payload and never moves `input`; it checks that `input` stays the source, token and
/// position it started with.
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
    /// Starts the job for `index`, bound to `input`'s current source, token and position.
    /// Fails with `Error::ForeignPool` when the fragment is not in `input`'s pool,
    /// `Error::Unsupported` when it has no `tfdt` or no known decode end, and
    /// `Error::Limit` when it has more than `maximum` samples.
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
    /// Folds further sample durations. Fails when `input` is no longer the source, token
    /// and position the job started with.
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
                    Next::Yield => return Ok(BuildStatus::Yield),
                    Next::Item(sample) => self.pending = Some(sample),
                    Next::End => {
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
    /// Takes the result after [`poll`](Self::poll) returned `Ready`, without checking
    /// `input`; `Error::Invalid` before then or when called again.
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
    /// Takes the result as [`take_result`](Self::take_result) does, after checking `input`
    /// again.
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
