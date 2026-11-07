//! Explicit reference first-packet failure, distinct from a source-parser error.
//!
//! The admitted passive observations establish one operation transition: when
//! EVERY supported open window requires an absolute packet seek beyond the
//! physical source, the wrapper rejects that seek and the MOV read reports
//! invalid-data. This models that transition, not an observed demux I/O state.
//! A physical length or read quantum by itself establishes no such transition.
use crate::{
    BoxView, BuildStatus, CursorStatus, DescriptionEmission, DescriptionEvent,
    DescriptionPacketCursor, DescriptionRead, DescriptionRecoveryTable, ModeledOpenState,
    PacketLimits, RecoveryTrackHeader, RootWalkTransportModel, RootWalkTransportProfile,
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, Error, InputCursor, InputToken, Result, SourceId, WorkBudget};

/// Opt-in to the operation/error contract; no default source view is changed.
#[derive(Clone, Copy, Debug)]
pub struct RejectedPacketSeekProfile;

/// A demux-operation projection with the exact source failure kept separately.
/// No source sample, clock, configuration or range is repaired by this value.
#[derive(Clone, Debug)]
pub struct RejectedPacketSeek {
    source_table: BoxView,
    source_mdhd: BoxView,
    event: DescriptionEvent,
    source_error: Error,
    open: ModeledOpenState,
    physical_length: u64,
}
impl RejectedPacketSeek {
    pub fn source_table(&self) -> &BoxView {
        &self.source_table
    }
    pub fn source_mdhd(&self) -> &BoxView {
        &self.source_mdhd
    }
    pub fn source_event(&self) -> &DescriptionEvent {
        &self.event
    }
    pub fn source_error(&self) -> Error {
        self.source_error
    }
    pub fn modeled_open_state(&self) -> ModeledOpenState {
        self.open
    }
    pub fn physical_length(&self) -> u64 {
        self.physical_length
    }
    /// Literal MOV read result established by the reference observations. This is
    /// not the core Error value or the range callback's return code.
    pub fn demux_read_result(&self) -> i32 {
        -1_094_995_529
    }
    pub fn demux_packets_delivered(&self) -> u64 {
        0
    }
    pub fn demux_catalogue_published(&self) -> bool {
        false
    }
    pub fn demux_confirmed_eof(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct InitialOperation {
    rejected: Option<RejectedPacketSeek>,
}
impl InitialOperation {
    /// None means this job made no demux failure claim. The caller must run
    /// another established operation or retain its explicit refusal.
    pub fn rejected_seek(&self) -> Option<&RejectedPacketSeek> {
        self.rejected.as_ref()
    }
    pub fn into_rejected_seek(self) -> Option<RejectedPacketSeek> {
        self.rejected
    }
}

/// A bounded first-event probe followed by a real fallible core range request.
/// The cursor is built internally from immutable table/header/model storage; no
/// caller-supplied event or expected/demux output can choose the disposition.
pub struct InitialOperationJob<'a> {
    cursor: Option<DescriptionPacketCursor<'a>>,
    table: &'a DescriptionRecoveryTable,
    model: &'a RootWalkTransportModel,
    mdhd: Option<BoxView>,
    pool: Budget,
    source: SourceId,
    token: InputToken,
    position: u64,
    maximum_packet: usize,
    pending: Option<DescriptionEvent>,
    ready: Option<InitialOperation>,
    finished: bool,
    failed: Option<Error>,
}
impl std::fmt::Debug for InitialOperationJob<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InitialOperationJob")
            .field("source", &self.source)
            .field("token", &self.token)
            .field("position", &self.position)
            .field("ready", &self.ready)
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}
impl<'a> InitialOperationJob<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        table: &'a DescriptionRecoveryTable,
        header: &RecoveryTrackHeader,
        media: &'a [Range<u64>],
        model: &'a RootWalkTransportModel,
        input: &InputCursor,
        limits: PacketLimits,
        maximum_packet: usize,
        _profile: RejectedPacketSeekProfile,
    ) -> Result<Self> {
        if !table.raw().bytes().belongs_to(input.budget())
            || !header.source().raw_mdhd.bytes().belongs_to(input.budget())
            || !header.source().raw_tkhd.bytes().belongs_to(input.budget())
            || !header.source().raw_hdlr.bytes().belongs_to(input.budget())
        {
            return Err(Error::ForeignPool);
        }
        if input.capabilities().length != Some(model.physical_length())
            || !input.capabilities().random_access
        {
            return Err(Error::Unsupported("v5 rejected seek source capabilities"));
        }
        let cursor = table.root_walk_packet_cursor(
            input.budget(),
            header,
            media,
            limits,
            model,
            RootWalkTransportProfile,
        )?;
        Ok(Self {
            cursor: Some(cursor),
            table,
            model,
            mdhd: Some(header.source().raw_mdhd.clone()),
            pool: input.budget().clone(),
            source: input.source(),
            token: input.token(),
            position: input.position(),
            maximum_packet,
            pending: None,
            ready: None,
            finished: false,
            failed: None,
        })
    }
    fn poison(&mut self, error: Error) -> Error {
        self.cursor = None;
        self.pending = None;
        self.ready = None;
        self.mdhd = None;
        self.failed = Some(error);
        error
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
            return Err(Error::Invalid("v5 input moved during initial operation"));
        }
        if input.capabilities().length != Some(self.model.physical_length())
            || !input.capabilities().random_access
        {
            return Err(Error::Unsupported("v5 source capability changed"));
        }
        Ok(())
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
        match result {
            Err(error) => Err(self.poison(error)),
            status => status,
        }
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
            return Err(Error::Invalid("v5 initial operation already finished"));
        }
        if self.ready.is_some() {
            return Ok(BuildStatus::Ready);
        }
        if work.remaining() == 0 {
            return Ok(BuildStatus::Yield);
        }
        if self.pending.is_none() {
            let cursor = self
                .cursor
                .as_mut()
                .ok_or(Error::Invalid("v5 probe cursor absent"))?;
            match cursor.poll(work, cancel)? {
                CursorStatus::Yield => return Ok(BuildStatus::Yield),
                CursorStatus::End => {
                    self.cursor = None;
                    self.ready = Some(InitialOperation { rejected: None });
                    return Ok(BuildStatus::Ready);
                }
                CursorStatus::Ready => self.pending = Some(cursor.take_event()?),
            }
        }
        if !work.try_charge(1) {
            return Ok(BuildStatus::Yield);
        }
        let event = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("v5 first event absent"))?;
        let open = self.model.modeled_open_state();
        let packet = event.packet.as_ref();
        let reject = if let Some(packet) = packet {
            let size = packet
                .source_range
                .end
                .checked_sub(packet.source_range.start)
                .ok_or(Error::Overflow)?;
            if size > u64::try_from(self.maximum_packet).map_err(|_| Error::Overflow)? {
                return Err(Error::Limit {
                    requested: usize::try_from(size).map_err(|_| Error::Overflow)?,
                    maximum: self.maximum_packet,
                });
            }
            event.declaration.ordinal == 1
                && packet.source_ordinal == 1
                && event.emission == DescriptionEmission::Packet
                && event.read == DescriptionRead::UnavailablePhysicalStart
                && size != 0
                && open.possible_windows != 0
                && open.source_position_min <= open.source_position_max
                && open.source_position_max <= self.model.physical_length()
                && packet.source_range.start > self.model.physical_length()
                && packet
                    .source_range
                    .start
                    .checked_sub(open.source_position_max)
                    .is_some_and(|gap| gap > 32768)
        } else {
            false
        };
        if !reject {
            self.pending = None;
            self.cursor = None;
            self.ready = Some(InitialOperation { rejected: None });
            return Ok(BuildStatus::Ready);
        }
        let offset = packet
            .ok_or(Error::Invalid("v5 rejected packet absent"))?
            .source_range
            .start;
        // Health/token/cancellation were checked above. The actual core operation
        // must reject this range without moving or resetting its source cursor.
        let error = match input.request_range(offset) {
            Err(error @ Error::Invalid(_)) => error,
            Err(error) => return Err(error),
            Ok(_) => {
                return Err(Error::Invalid(
                    "v5 out-of-source request unexpectedly accepted",
                ));
            }
        };
        self.binding(input)?;
        let event = self
            .pending
            .take()
            .ok_or(Error::Invalid("v5 first event absent"))?;
        self.cursor = None;
        self.ready = Some(InitialOperation {
            rejected: Some(RejectedPacketSeek {
                source_table: self.table.raw().clone(),
                source_mdhd: self
                    .mdhd
                    .take()
                    .ok_or(Error::Invalid("v5 source clock absent"))?,
                event,
                source_error: error,
                open,
                physical_length: self.model.physical_length(),
            }),
        });
        Ok(BuildStatus::Ready)
    }
    pub fn try_finish(
        &mut self,
        input: &mut InputCursor,
        cancel: &AtomicBool,
    ) -> Result<InitialOperation> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let check = (|| -> Result<()> {
            self.binding(input)?;
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            input.read_into(&mut [], &mut WorkBudget::new(0), cancel)?;
            Ok(())
        })();
        if let Err(error) = check {
            return Err(self.poison(error));
        }
        match self.ready.take() {
            Some(ready) if !self.finished => {
                self.finished = true;
                self.mdhd = None;
                Ok(ready)
            }
            _ => Err(self.poison(Error::Invalid("v5 initial operation not ready"))),
        }
    }
}
