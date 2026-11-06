//! A native-compatible application route with no strict Source decoder.
use super::display_model::PaletteSnapshot;
use super::display_work::DisplayWork;
use super::native_recovery::{
    Decoder, Phase, ReceiverObservations, RecoveredEvent, RecoveryStatus,
};
use super::recovery_role::RecoveryRole;
use super::{DisplayConfig, DisplayLimits, PacketClock};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, Result, WorkBudget};

/// Versioned packet/first-event compatibility with the pinned native PGS API.
/// This names a receiver contract, not complete PGS compatibility.
pub const NATIVE_COMPATIBILITY: &str = "native-pgs-first-event-compatibility-v1";

/// Owned native application output, distinct from the full Source+Receiver API.
/// Later segments still update the decoder's cache and final context canvas.
/// No Source validity, Source error or omitted Source output is represented here.
///
/// ```compile_fail
/// fn inspect(packet: &triergon_subtitles::pgs::NativeCompatibilityPacket) {
///     let _ = packet.source_sets();
/// }
/// ```
#[derive(Debug)]
pub struct NativeCompatibilityPacket {
    raw: Bytes,
    pub clock: PacketClock,
    event: Option<RecoveredEvent>,
    pub context_canvas: [u16; 2],
    pub receiver_observations: ReceiverObservations,
}

impl NativeCompatibilityPacket {
    pub fn raw_packet(&self) -> &Bytes {
        &self.raw
    }
    pub fn receiver_output(&self) -> Option<&RecoveredEvent> {
        self.event.as_ref()
    }
}

/// The native API has no delayed subtitle to publish on an empty drain call.
/// This result makes no statement about strict Source finalization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeCompatibilityEof {
    NoDelayedSubtitle,
}

/// Explicit native-compatible PGS entry point for a native event consumer.
///
/// The default full-recovery decoders remain separate. This decoder never builds
/// their strict Source graph, exports Source ARGB or invents Source fields.
/// Accepted inputs, caches and outputs stay charged to the supplied pool; work
/// is cumulative until reset and every operation receives caller cancellation.
///
/// ```compile_fail
/// fn export_source(
///     decoder: &mut triergon_subtitles::pgs::NativeCompatibilityDecoder,
///     writer: &mut triergon_subtitles::pgs::ArgbWriter,
///     target: &mut [u8],
///     work: &mut triergon_core::WorkBudget,
///     cancel: &std::sync::atomic::AtomicBool,
/// ) {
///     let _ = decoder.write_source_argb(writer, target, work, cancel);
/// }
/// ```
#[derive(Debug)]
pub struct NativeCompatibilityDecoder {
    inner: Decoder<NativeOnly>,
}

impl NativeCompatibilityDecoder {
    pub fn new(budget: &Budget, limits: DisplayLimits, config: DisplayConfig) -> Result<Self> {
        Ok(Self {
            inner: Decoder::new(budget, limits, config)?,
        })
    }

    /// Rejection leaves the caller's exact input owner in place.
    pub fn push(
        &mut self,
        raw: &mut Option<Bytes>,
        clock: PacketClock,
        cancel: &AtomicBool,
    ) -> Result<()> {
        self.inner.push(raw, clock, cancel)
    }

    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<RecoveryStatus> {
        self.inner.poll(work, cancel)
    }

    pub fn take_output(&mut self, cancel: &AtomicBool) -> Result<NativeCompatibilityPacket> {
        self.inner.take_output(cancel)
    }

    pub fn finish_input(&mut self, cancel: &AtomicBool) -> Result<()> {
        self.inner.finish_input(cancel)
    }

    pub fn eof_report(&self) -> Option<NativeCompatibilityEof> {
        self.inner.eof_report()
    }
    pub fn work_spent(&self) -> usize {
        self.inner.work_spent()
    }

    /// Discard receiver stream state while preserving the selected configuration.
    /// Previously returned outputs keep their own charged storage.
    pub fn reset(&mut self, cancel: &AtomicBool) -> Result<()> {
        self.inner.reset(cancel)
    }
}

// A compile-time role, not a flag that disables selected work in a full output.
// Its only retained product is the native API's first public event.
#[derive(Debug)]
pub(super) struct NativeOnly {
    event: Option<RecoveredEvent>,
}

impl RecoveryRole for NativeOnly {
    type Packet = NativeCompatibilityPacket;
    type Eof = NativeCompatibilityEof;

    fn new(_budget: &Budget, limits: DisplayLimits, _config: DisplayConfig) -> Result<Self> {
        limits.validate()?;
        Ok(Self { event: None })
    }
    fn begin_packet(&mut self, _budget: &Budget) {
        self.event = None;
    }
    fn first_phase(&self) -> Phase {
        Phase::Segment
    }
    fn poll_source(
        &mut self,
        _raw: &Bytes,
        _clock: PacketClock,
        _limits: &DisplayLimits,
        _caller: &mut WorkBudget,
        _spent: &mut usize,
        _cancel: &AtomicBool,
    ) -> Result<bool> {
        Err(Error::Invalid(
            "PGS native compatibility has no strict Source phase",
        ))
    }
    fn poison(&mut self, _error: Error) {
        self.event = None;
    }
    fn reset(&mut self, _cancel: &AtomicBool) -> Result<()> {
        self.event = None;
        Ok(())
    }
    fn index_slots(&self) -> Option<usize> {
        None
    }
    fn validated_indices(&self, _id: u16, _width: u16, _height: u16) -> Option<(Bytes, Bytes)> {
        None
    }
    fn palette_candidate(&self) -> Option<&PaletteSnapshot> {
        None
    }
    fn display_already_returned(&self) -> bool {
        self.event.is_some()
    }
    fn event_count(&self) -> Result<usize> {
        Ok(usize::from(self.event.is_some()))
    }
    fn reserve_event(&mut self) -> Result<()> {
        Ok(())
    }
    fn store_event(&mut self, event: RecoveredEvent) -> Result<()> {
        if self.event.is_some() {
            return Err(Error::Invalid("PGS native public event already stored"));
        }
        self.event = Some(event);
        Ok(())
    }
    fn finish_packet(
        &mut self,
        raw: Bytes,
        clock: PacketClock,
        context_canvas: [u16; 2],
        receiver_observations: ReceiverObservations,
    ) -> Result<Self::Packet> {
        Ok(NativeCompatibilityPacket {
            raw,
            clock,
            event: self.event.take(),
            context_canvas,
            receiver_observations,
        })
    }
    fn finish_input(&mut self, _cancel: &AtomicBool) -> Result<()> {
        Ok(())
    }
    fn poll_eof(
        &mut self,
        caller: &mut WorkBudget,
        spent: &mut usize,
        maximum: usize,
        cancel: &AtomicBool,
    ) -> Result<Option<Self::Eof>> {
        let mut credit = 0;
        if !(DisplayWork {
            caller,
            spent,
            maximum,
            cancel,
        })
        .pay(&mut credit, 1)?
        {
            return Ok(None);
        }
        Ok(Some(NativeCompatibilityEof::NoDelayedSubtitle))
    }
}
