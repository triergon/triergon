//! Fragments of a forward-only movie whose metadata comes first, read as FFmpeg's
//! MOV demuxer reads them. `FrontTrack` carries one track's packet clock on from a
//! packet cursor that has reached its end, and `FrontFragment` yields each later
//! fragment's samples for it to project. The track state comes only from that
//! cursor, never from a clock the caller supplies.
use crate::{
    CursorStatus, Next, Sample,
    compat::mov::{
        clocks::{MediaClock, SchedulingClock},
        cursor::{ClockOrigin, FragmentEdit, PacketOccurrence, PredecessorClock, PreviousClock},
        view::PacketProjector,
    },
    fragment::{FragmentIndex, OwnedFragmentCursor},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, TimeBase, WorkBudget};

/// One fragment of a forward-only movie. A first pass over its samples finds their
/// least composition offset, read as signed 32-bit; a second yields each sample as
/// a [`FrontSample`]. Source times and offsets stay as declared.
#[derive(Debug)]
pub struct FrontFragment {
    cursor: OwnedFragmentCursor,
    scanning: bool,
    minimum: i64,
    ready: Result<Option<FrontSample>>,
}
/// A sample of a [`FrontFragment`] with the run and offset context
/// [`FrontTrack::project`] needs, which only the fragment can supply.
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
/// An empty sample, at which FFmpeg's packet reads on a forward-only input end.
/// The source can still declare more samples and hold more bytes after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrontReadBoundary {
    /// `track_ID` of the sample's track.
    pub track_id: u32,
    /// 1-based ordinal of the empty sample.
    pub source_ordinal: u64,
    /// Absolute file offset at which the empty sample is declared.
    pub source_offset: u64,
}
impl FrontSample {
    /// The boundary this sample sets when it is empty, given whether more samples
    /// follow it in the same `mdat`; `None` for a nonempty sample. The caller
    /// handles samples in later `mdat` boxes. `Error::Unsupported` for an empty
    /// sample followed by others in the `mdat` unless it starts the fragment's
    /// first run.
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
    /// The boundary this sample sets when it is empty, `None` otherwise. What the
    /// demuxer reports after the boundary is left to the caller.
    pub fn zero_read_boundary(&self) -> Option<FrontReadBoundary> {
        self.sample.range.is_empty().then_some(FrontReadBoundary {
            track_id: self.track,
            source_ordinal: self.sample.ordinal,
            source_offset: self.sample.range.start,
        })
    }
    /// Samples of this sample's `trun` that follow it.
    pub fn remaining_in_run(&self) -> u32 {
        self.run.remaining
    }
    /// The source sample, unchanged.
    pub fn source(&self) -> &Sample {
        &self.sample
    }
}
impl FrontFragment {
    /// Reads the samples of the fragment `index`.
    pub fn new(index: FragmentIndex) -> Self {
        Self {
            cursor: index.into_cursor(),
            scanning: true,
            minimum: 0,
            ready: Ok(None),
        }
    }
    /// The fragment's index.
    pub fn index(&self) -> &FragmentIndex {
        self.cursor.index()
    }
    /// Advances to the next sample. During the first pass it returns `Yield` after
    /// each sample and at the pass's end as well as when the budget is spent; poll
    /// again. Failures are sticky.
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
            Next::Yield => Ok(CursorStatus::Yield),
            Next::End if self.scanning => {
                self.cursor.restart();
                self.scanning = false;
                Ok(CursorStatus::Yield)
            }
            Next::End => Ok(CursorStatus::End),
            Next::Item(sample) if self.scanning => {
                if let Some(offset) = sample.composition_offset {
                    self.minimum = self.minimum.min(i64::from(i32::from_be_bytes(
                        offset.raw_bits().to_be_bytes(),
                    )));
                }
                Ok(CursorStatus::Yield)
            }
            Next::Item(sample) => {
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
    /// Takes the ready sample; `Error::Invalid` when none is ready.
    pub fn take_sample(&mut self) -> Result<FrontSample> {
        match &mut self.ready {
            Ok(ready) => ready
                .take()
                .ok_or(Error::Invalid("front native sample not ready")),
            Err(error) => Err(*error),
        }
    }
}

/// One track's packet clock on a forward-only input, taken from a cursor that has
/// reached its end with
/// [`PacketCursor::front_track`](crate::compat::mov::packets::PacketCursor::front_track)
/// and carried across later fragments. It is a plain value:
/// [`project`](Self::project) returns the next state, so a caller that cannot store
/// a packet keeps the old state and retries the same sample.
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
    /// The 1-based ordinal the track's next sample must have.
    pub fn next_source_ordinal(&self) -> Result<u64> {
        self.previous
            .map_or(Some(1), |p| p.sample.checked_add(1))
            .ok_or(Error::Overflow)
    }
    /// The packet for `head` and the track state after it. The caller checks that
    /// the sample lies in the current `mdat` and can store the packet before it
    /// keeps the new state; nothing is read. `Error::Invalid` for a sample of
    /// another track; `Error::Unsupported` for a track with regular samples or a
    /// sample out of ordinal order.
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
