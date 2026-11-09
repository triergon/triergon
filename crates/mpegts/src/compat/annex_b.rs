//! Unframed AVC and HEVC observations through logical source EOF.
//!
//! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.

mod continuity;

use crate::{
    CarrierObservation, EntityTransition, Packet, Reader, ReaderEvent, ReaderLimits, TraceEvent,
    Track,
};
use continuity::{EofStep, Proof, SourceJob};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::container::{ContainerReader, Event, ReaderStep, SeekRequest};
use triergon_core::{
    Budget, Error, InputRequest, InputToken, InputWindow, Result, SourceCapabilities,
    SourceFailure, SourceId, SourceLocation, Step, WorkBudget,
};

pub use continuity::{ContinuityEvidence, ContinuityResidue};

/// Reporting of an unframed AVC or HEVC run at logical source EOF.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VideoEofPolicy {
    /// Keep the video framer's leading-byte error.
    #[default]
    RejectUnframed,
    /// Report a nonempty run of validated PES payload with invalid leading bytes
    /// and no Annex B start code anywhere in the run.
    ReportUnframed,
}

/// Events from the original reader and the additional continuity observation.
#[derive(Debug)]
pub enum ContinuityEvent {
    /// An unchanged event from the source reader.
    Source(ReaderEvent),
    /// An original interruption or diagnostic with a pending continuity proof.
    /// The source event remains valid even if the proof later produces no residue.
    Pending(ReaderEvent),
    /// One unframed run proved at the original logical EOF.
    Residue(ContinuityResidue),
}

/// Observe opaque video through continuity changes in one entity with one
/// selected AVC or HEVC track. Owns the source reader and uses its original
/// cursor, transport carriers and clock transitions. Neutral packets and source
/// events remain those of that reader.
///
/// The source reader uses [VideoEofPolicy::ReportUnframed]. A continuity
/// difference must be observed within the first 16 selected carriers. A later
/// start code, an unsupported source change, or unavailable proof storage
/// retires the optional proof. Source backpressure still comes from the reader.
/// Every step uses the caller's work grant, including replay of the retained
/// carrier history and the scan through the final residue.
pub struct ContinuityReader {
    reader: Reader,
    limits: ReaderLimits,
    proof: Proof,
    pending: Option<SourceJob>,
    end: Option<SourceLocation>,
}

impl std::fmt::Debug for ContinuityReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContinuityReader")
            .field("reader", &self.reader)
            .field("proof_enabled", &self.proof.enabled())
            .field("end", &self.end)
            .finish_non_exhaustive()
    }
}

impl ContinuityReader {
    pub fn new(
        pool: &Budget,
        source: SourceId,
        caps: SourceCapabilities,
        mut limits: ReaderLimits,
        input_window: usize,
    ) -> Result<Self> {
        limits.video_eof_policy = VideoEofPolicy::ReportUnframed;
        Ok(Self {
            reader: Reader::new(pool, source, caps, limits, input_window)?,
            limits,
            proof: Proof::new(),
            pending: None,
            end: None,
        })
    }

    pub fn request(&self) -> Result<InputRequest> {
        self.reader.request()
    }

    /// Continue the ordinary reader across entities. The optional continuity
    /// proof is confined to a single entity and is retired on a valid transition.
    pub fn finish_entity(
        &mut self,
        source: SourceId,
        token: InputToken,
        next: SourceId,
        caps: SourceCapabilities,
        transition: EntityTransition,
    ) -> Result<()> {
        let result = self
            .reader
            .finish_entity(source, token, next, caps, transition);
        self.finish_result(result)?;
        self.proof.disable();
        Ok(())
    }

    /// Release the reader and its proof state. Returned observations keep their storage.
    pub fn close(&mut self) {
        self.reader.close();
        self.discard();
    }

    fn discard(&mut self) {
        self.proof.disable();
        self.pending = None;
        self.end = None;
    }

    fn finish_result(&mut self, result: Result<()>) -> Result<()> {
        // Terminal completion errors release the cursor. Protocol errors leave
        // the original request and pending proof available for a retry.
        if result.is_err() && self.reader.continuity_source().is_none() {
            self.discard();
        }
        result
    }

    /// Inspect carrier observations by reference and return the next packet,
    /// other source event or progress step. The continuity proof still sees
    /// its original carriers and applies the same bounds.
    pub fn step_with_carriers(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        mut observe: impl FnMut(CarrierObservation<'_>),
    ) -> Result<ReaderStep<Self>> {
        loop {
            let step = self.advance(work, cancel, |reader, proving, work, cancel| {
                if proving {
                    reader.step(work, cancel)
                } else {
                    reader.step_with_carriers(work, cancel, &mut observe)
                }
            })?;
            if let Step::Ready(Event::Other(ContinuityEvent::Source(event))) = &step
                && let Some(event) = event.carrier_observation()
            {
                observe(event);
            } else {
                return Ok(step);
            }
        }
    }

    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        mut read: impl FnMut(
            &mut Reader,
            bool,
            &mut WorkBudget,
            &AtomicBool,
        ) -> Result<ReaderStep<Reader>>,
    ) -> Result<ReaderStep<Self>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if let Some(job) = self.pending.as_mut() {
                let pending = match self
                    .proof
                    .observe(&self.reader, job, self.limits, work, cancel)
                {
                    Ok(Some(pending)) => pending,
                    Ok(None) => return Ok(Step::Yield),
                    Err(Error::Cancelled) => return Err(Error::Cancelled),
                    Err(_) => {
                        self.proof.disable();
                        false
                    }
                };
                let job = self.pending.take().expect("processed source");
                if job.published {
                    continue;
                }
                let event = job.event;
                return Ok(Step::Ready(Event::Other(if pending {
                    ContinuityEvent::Pending(event)
                } else {
                    ContinuityEvent::Source(event)
                })));
            }
            if let Some(end) = self.end {
                let result = self.proof.finish(&self.reader, end, work, cancel);
                match result {
                    Ok(EofStep::Yield) => return Ok(Step::Yield),
                    Ok(EofStep::Residue(residue)) => {
                        self.proof.disable();
                        return Ok(Step::Ready(Event::Other(ContinuityEvent::Residue(residue))));
                    }
                    Err(Error::Cancelled) => return Err(Error::Cancelled),
                    Ok(EofStep::Done) | Err(_) => {
                        self.proof.disable();
                        return Ok(Step::End(end));
                    }
                }
            }
            let step = match read(&mut self.reader, self.proof.enabled(), work, cancel) {
                Ok(step) => step,
                Err(error) => {
                    if error != Error::Allocation {
                        self.discard();
                    }
                    return Err(error);
                }
            };
            match step {
                Step::Ready(Event::Other(
                    event @ (ReaderEvent::Source(_)
                    | ReaderEvent::Transport(_)
                    | ReaderEvent::Continuity { .. }),
                )) if self.proof.enabled() => {
                    // Catalogue changes must accompany their source event. The
                    // proof observes that event before the next reader step.
                    if let ReaderEvent::Source(source) = &event
                        && matches!(
                            source.event(),
                            TraceEvent::ProgramMap { active: true, .. }
                                | TraceEvent::Discontinuity { .. }
                        )
                    {
                        self.pending = Some(SourceJob::new(event.clone(), true));
                        return Ok(Step::Ready(Event::Other(ContinuityEvent::Source(event))));
                    }
                    self.pending = Some(SourceJob::new(event, false));
                }
                Step::Ready(Event::Other(event)) => {
                    let pending = match &event {
                        ReaderEvent::Interrupted { cause, .. } => self.proof.matches(cause.event()),
                        ReaderEvent::Source(_)
                        | ReaderEvent::Transport(_)
                        | ReaderEvent::Continuity { .. }
                        | ReaderEvent::IncompleteVideo(_) => false,
                        _ => {
                            self.proof.disable();
                            false
                        }
                    };
                    return Ok(Step::Ready(Event::Other(if pending {
                        ContinuityEvent::Pending(event)
                    } else {
                        ContinuityEvent::Source(event)
                    })));
                }
                Step::Ready(Event::Packet(packet)) => {
                    self.proof.disable();
                    return Ok(Step::Ready(Event::Packet(packet)));
                }
                Step::Ready(Event::Seek(landing)) => {
                    self.proof.disable();
                    return Ok(Step::Ready(Event::Seek(landing)));
                }
                Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                Step::Yield => return Ok(Step::Yield),
                Step::Backpressure(_) if self.proof.retains_carriers() => {
                    // Release the optional proof before retrying the same source step.
                    self.proof.disable();
                    return Ok(Step::Yield);
                }
                Step::Backpressure(pressure) => return Ok(Step::Backpressure(pressure)),
                Step::End(end) => self.end = Some(end),
            }
        }
    }
}

impl ContainerReader for ContinuityReader {
    type Packet = Packet;
    type Track = Track;
    type Other = ContinuityEvent;

    fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.reader.provide(window)
    }
    fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        let result = self.reader.finish(source, token);
        self.finish_result(result)
    }
    fn fail(&mut self, source: SourceId, token: InputToken, failure: SourceFailure) -> Result<()> {
        self.reader.fail(source, token, failure)?;
        self.discard();
        Ok(())
    }
    fn reset(&mut self) -> Result<InputRequest> {
        let request = self.reader.reset()?;
        self.discard();
        self.proof = Proof::new();
        Ok(request)
    }
    fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ReaderStep<Self>> {
        self.advance(work, cancel, |reader, _, work, cancel| {
            reader.step(work, cancel)
        })
    }
    fn request_seek(&mut self, request: SeekRequest) -> Result<InputRequest> {
        self.reader.request_seek(request)
    }
    fn track_count(&self) -> usize {
        self.reader.track_count()
    }
    fn track(&self, index: usize) -> Option<&Track> {
        self.reader.track(index)
    }
    fn capabilities(&self) -> SourceCapabilities {
        self.reader.capabilities()
    }
}
