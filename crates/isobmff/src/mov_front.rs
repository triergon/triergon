//! Explicit front-source demux continuation, separate from ISO source clocks.
//! Fragment heads are emitted only by an owned, checked metadata cursor. The
//! track state originates in an exhausted from-start demux index, never a clock
//! guessed from a file name, EOF position, caller token or reference packet.
use crate::{
    ClockOrigin, CursorStatus, FragmentIndex, MediaClock, OwnedFragmentCursor, PacketOccurrence,
    PacketProjector, PredecessorClock, Sample, SampleStep, SchedulingClock,
    mov_cursor::{FragmentEdit, PreviousClock},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, TimeBase, WorkBudget};

/// One independently owned fragment; the preliminary signed-CTO scan is resumable
/// and visits only its declared samples. Source times and raw CTO stay literal.
/// The immutable metadata backing remains charged by its original storage.
#[derive(Debug)]
pub struct FrontFragment {
    cursor: OwnedFragmentCursor,
    scanning: bool,
    minimum: i64,
    ready: Result<Option<FrontSample>>,
}
/// A source occurrence issued by the checked front fragment job. Private demux
/// context fields prevent callers from substituting a run or clock authority.
#[derive(Clone, Debug)]
pub struct FrontSample {
    sample: Sample,
    track: u32,
    run: FragmentRunHead,
    minimum: i64,
    run_start: bool,
}
#[derive(Clone, Debug)]
struct FragmentRunHead {
    fragment_first_ordinal: u64,
    index: u32,
    remaining: u32,
}
/// A demux boundary reported after a zero-sized sample. The Source reader can
/// still have more declarations and bytes to consume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrontReadBoundary {
    pub track_id: u32,
    pub source_ordinal: u64,
    pub source_offset: u64,
}
impl FrontSample {
    pub fn read_boundary(&self) -> Result<Option<FrontReadBoundary>> {
        self.read_boundary_in_media(true)
    }
    /// A final zero sample in the admitted media extent can end the demux
    /// packet sequence. The caller still owns all samples in later extents.
    pub fn read_boundary_in_media(
        &self,
        has_more_samples_in_media: bool,
    ) -> Result<Option<FrontReadBoundary>> {
        let Some(boundary) = self.zero_read_boundary() else {
            return Ok(None);
        };
        if has_more_samples_in_media && !(self.run_start && self.run.index == 0) {
            return Err(Error::Unsupported(
                "native zero read outside first fragment run remains unmodeled",
            ));
        }
        Ok(Some(boundary))
    }
    /// A zero-size declaration and the source coordinate that caused it.
    /// Demux output after this boundary needs a separate receiver policy.
    pub fn zero_read_boundary(&self) -> Option<FrontReadBoundary> {
        self.sample.range.is_empty().then_some(FrontReadBoundary {
            track_id: self.track,
            source_ordinal: self.sample.ordinal,
            source_offset: self.sample.range.start,
        })
    }
    pub fn remaining_in_run(&self) -> u32 {
        self.run.remaining
    }
    pub fn source(&self) -> &Sample {
        &self.sample
    }
}
impl FrontFragment {
    pub fn new(index: FragmentIndex) -> Self {
        Self {
            cursor: index.into_cursor(),
            scanning: true,
            minimum: 0,
            ready: Ok(None),
        }
    }
    pub fn index(&self) -> &FragmentIndex {
        self.cursor.index()
    }
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        if let Err(error) = &self.ready {
            return Err(*error);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(error) = &result {
            self.ready = Err(*error);
        }
        result
    }
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if work.remaining() == 0 {
            return Ok(CursorStatus::Yield);
        }
        if matches!(self.ready, Ok(Some(_))) {
            return Ok(CursorStatus::Ready);
        }
        match self.cursor.poll(work)? {
            SampleStep::Yield => Ok(CursorStatus::Yield),
            SampleStep::End if self.scanning => {
                self.cursor.restart();
                self.scanning = false;
                Ok(CursorStatus::Yield)
            }
            SampleStep::End => Ok(CursorStatus::End),
            SampleStep::Sample(sample) if self.scanning => {
                if let Some(offset) = sample.composition_offset {
                    self.minimum = self.minimum.min(i64::from(i32::from_be_bytes(
                        offset.raw_bits().to_be_bytes(),
                    )));
                }
                Ok(CursorStatus::Yield)
            }
            SampleStep::Sample(sample) => {
                self.ready = Ok(Some(FrontSample {
                    sample,
                    track: self.cursor.index().track_id(),
                    run: FragmentRunHead {
                        fragment_first_ordinal: self.cursor.index().first_ordinal(),
                        index: u32::try_from(self.cursor.run_index())
                            .map_err(|_| Error::Overflow)?,
                        remaining: self.cursor.remaining_in_delivered_run(),
                    },
                    minimum: self.minimum,
                    run_start: self.cursor.delivered_run_start(),
                }));
                Ok(CursorStatus::Ready)
            }
        }
    }
    pub fn take_sample(&mut self) -> Result<FrontSample> {
        match &mut self.ready {
            Ok(ready) => ready
                .take()
                .ok_or(Error::Invalid("front native sample not ready")),
            Err(error) => Err(*error),
        }
    }
}

/// A bounded scalar snapshot of one validated initial track. Returned states are
/// values: allocation refusal can keep the original state and retry the same
/// source head without advancing a demux clock twice.
#[derive(Clone, Copy, Debug)]
pub struct FrontTrack {
    track: u32,
    base: TimeBase,
    clock: MediaClock,
    previous: Option<PreviousClock>,
    minimum: i64,
    regular_samples: u64,
    edit: Option<FragmentEdit>,
    run: Option<(u64, u32)>,
    discard_boundary: i64,
}
impl FrontTrack {
    pub(crate) fn from_index(
        track: u32,
        base: TimeBase,
        clock: MediaClock,
        previous: Option<PreviousClock>,
        minimum: i64,
        regular_samples: u64,
        edit: Option<FragmentEdit>,
    ) -> Self {
        Self {
            track,
            base,
            clock,
            previous,
            minimum,
            regular_samples,
            edit,
            run: None,
            discard_boundary: i64::MIN,
        }
    }
    pub fn next_source_ordinal(&self) -> Result<u64> {
        self.previous
            .map_or(Some(1), |p| p.sample.checked_add(1))
            .ok_or(Error::Overflow)
    }
    /// Pure, bounded projection of a metadata-issued head. The caller validates
    /// the actual current MDAT declaration and reserves output capacity before
    /// committing the returned state. This method does not read or infer EOF.
    pub fn project(&self, head: &FrontSample) -> Result<(Self, PacketOccurrence)> {
        if head.track != self.track {
            return Err(Error::Invalid("native continuation track mismatch"));
        }
        if self.regular_samples != 0 {
            return Err(Error::Unsupported(
                "native edited/mixed regular fragment continuation",
            ));
        }
        let edit = self.edit;
        let offset = i128::from(edit.map_or(0, |edit| edit.packet_offset));
        let sample = &head.sample;
        if sample.dts_ticks.is_some() != sample.cts_ticks.is_some() {
            return Err(Error::Invalid("inconsistent source clock knownness"));
        }
        if sample.ordinal != self.next_source_ordinal()? {
            return Err(Error::Unsupported(
                "native continuation source ordinal gap or reorder",
            ));
        }
        let (dts, origin) = if let Some(ticks) = sample.dts_ticks {
            (ticks, ClockOrigin::SourceClock)
        } else if let Some(previous) = self.previous {
            let ticks = previous
                .dts
                .checked_add(i128::from(previous.duration))
                .ok_or(Error::Overflow)?;
            (
                ticks,
                ClockOrigin::PredecessorEnd(PredecessorClock {
                    predecessor_sample: previous.sample,
                    predecessor_demux_dts_ticks: previous
                        .dts
                        .checked_sub(offset)
                        .ok_or(Error::Overflow)?,
                    predecessor_duration_ticks: previous.duration,
                    predecessor_source_dts_known: previous.source_known,
                    derived_dts_ticks: ticks.checked_sub(offset).ok_or(Error::Overflow)?,
                }),
            )
        } else {
            (0, ClockOrigin::FrontSourceMediaStart)
        };
        let mut next = *self;
        next.minimum = next.minimum.min(head.minimum);
        let presentation_dts = dts.checked_sub(offset).ok_or(Error::Overflow)?;
        let mut demux = PacketProjector::new(self.base, None).project_fragment(
            sample,
            presentation_dts,
            next.minimum,
        )?;
        let run_key = (head.run.fragment_first_ordinal, head.run.index);
        if next.run != Some(run_key) {
            next.discard_boundary = next.previous.map_or(i64::MIN, |p| p.signed_dts);
            next.run = Some(run_key);
        }
        if demux.dts_field.signed_ticks() <= next.discard_boundary {
            demux.flags |= 4;
        }
        next.previous = Some(PreviousClock {
            sample: sample.ordinal,
            dts,
            signed_dts: demux.dts_field.signed_ticks(),
            duration: sample.duration_ticks,
            source_known: sample.dts_ticks.is_some(),
        });
        Ok((
            next,
            PacketOccurrence {
                track_id: self.track,
                source: sample.clone(),
                demux,
                edit: edit.map(|edit| edit.identity(sample.ordinal)),
                clock_origin: origin,
                media_clock: self.clock,
                scheduling_clock: SchedulingClock::NonseekableSourcePosition(sample.range.start),
            },
        ))
    }
}
