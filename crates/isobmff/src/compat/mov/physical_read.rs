//! Physical reads for description recovery: the bytes of a packet that lie before the
//! physical end of the file, read as FFmpeg's MOV demuxer reads a packet the file ends
//! inside. The source declarations stay as stated.
use crate::table::recovery::{DescriptionEvent, DescriptionRead};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{
    BudgetVec, Bytes, Error, InputCursor, InputToken, Result, SourceId, Step, WorkBudget,
};

/// Opt-in token for physical reads, passed to [`PhysicalRead::from_event`],
/// [`PhysicalReader::new`] and the physical cursors of
/// [`DescriptionRecoveryTable`](crate::compat::mov::recovery::DescriptionRecoveryTable).
#[derive(Clone, Copy, Debug)]
pub struct PhysicalReadProfile;

/// A plan to read the bytes of one [`DescriptionEvent`]'s packet that lie before the
/// physical end of the file. Whether bytes exist is a fact about the file; treating the
/// packet as short there is how FFmpeg reads it, not something the read confirms.
#[derive(Clone, Debug)]
pub struct PhysicalRead {
    origin_pool: triergon_core::Budget,
    origin_source: SourceId,
    origin_token: InputToken,
    requested: Range<u64>,
    available: Range<u64>,
    physical_length: u64,
    declared_media: Range<u64>,
    source_range_error: Option<Error>,
    short_at_physical_end: bool,
}
impl PhysicalRead {
    /// Plans the read of `event`'s packet from `input`, under the media extent
    /// `declared_media`. Fails with `Error::Unsupported` when the input's length is unknown
    /// or it lacks random access, when the packet is empty, when `declared_media` leaves the
    /// source, or when the packet does not start before the physical end; with
    /// `Error::Invalid` for an event without a packet; and with `Error::Limit` when the
    /// packet's declared size exceeds `maximum_packet`.
    pub fn from_event(
        event: &DescriptionEvent,
        input: &InputCursor,
        declared_media: Range<u64>,
        maximum_packet: usize,
        _profile: PhysicalReadProfile,
    ) -> Result<Self> {
        let physical_length = input.capabilities().length.ok_or(Error::Unsupported(
            "physical read needs a known source length",
        ))?;
        if !input.capabilities().random_access {
            return Err(Error::Unsupported(
                "physical read needs a random-access source",
            ));
        }
        let packet = event
            .packet
            .as_ref()
            .ok_or(Error::Invalid("physical read of an event without a packet"))?;
        let requested = packet.source_range.clone();
        let size = requested
            .end
            .checked_sub(requested.start)
            .ok_or(Error::Overflow)?;
        if size == 0 {
            return Err(Error::Unsupported("physical read of an empty packet"));
        }
        let size = usize::try_from(size).map_err(|_| Error::Overflow)?;
        if size > maximum_packet {
            return Err(Error::Limit {
                requested: size,
                maximum: maximum_packet,
            });
        }
        if declared_media.start > declared_media.end || declared_media.end > physical_length {
            return Err(Error::Unsupported(
                "physical read media extent outside the source",
            ));
        }
        if event.read != DescriptionRead::Available || requested.start >= physical_length {
            return Err(Error::Unsupported(
                "physical read packet not available before the physical end",
            ));
        }
        let available = requested.start..requested.end.min(physical_length);
        Ok(Self {
            origin_pool: input.budget().clone(),
            origin_source: input.source(),
            origin_token: input.token(),
            short_at_physical_end: available.end < requested.end,
            requested,
            available,
            physical_length,
            declared_media,
            source_range_error: event.range_error,
        })
    }
    /// The packet's byte range as FFmpeg requests it, in absolute file offsets.
    pub fn requested(&self) -> Range<u64> {
        self.requested.clone()
    }
    /// The part of [`requested`](Self::requested) before the physical end; never empty.
    pub fn available(&self) -> Range<u64> {
        self.available.clone()
    }
    /// The event's `range_error`: why the packet's byte range leaves the media extent, if
    /// it does.
    pub fn source_range_error(&self) -> Option<Error> {
        self.source_range_error
    }
    /// The media extent the plan was made for, in absolute file offsets.
    pub fn declared_media(&self) -> Range<u64> {
        self.declared_media.clone()
    }
    /// Whether the file ends inside the packet, so that
    /// [`available`](Self::available) is shorter than [`requested`](Self::requested).
    pub fn is_short_at_physical_end(&self) -> bool {
        self.short_at_physical_end
    }
    /// Length of the source in bytes.
    pub fn physical_length(&self) -> u64 {
        self.physical_length
    }
    /// Whether FFmpeg's reading ends with end of file at `next`, the event after this one:
    /// true when this packet is short at the physical end and `next` is an
    /// [`UnavailablePhysicalStart`](DescriptionRead::UnavailablePhysicalStart) packet of 1
    /// byte to 16 MiB that starts exactly where this one ends. Any other `next`, such as a
    /// later chunk inside the file, gives false; it is neither dropped nor treated as a seek.
    pub fn modeled_following_eof(
        &self,
        next: &DescriptionEvent,
        _profile: PhysicalReadProfile,
    ) -> bool {
        if !self.short_at_physical_end || next.read != DescriptionRead::UnavailablePhysicalStart {
            return false;
        }
        let Some(packet) = &next.packet else {
            return false;
        };
        let Some(size) = packet
            .source_range
            .end
            .checked_sub(packet.source_range.start)
        else {
            return false;
        };
        size != 0
            && size <= 16 * 1024 * 1024
            && packet.source_range.start == self.requested.end
            && packet.source_range.start >= self.physical_length
    }
}

/// The bytes a [`PhysicalReader`] read, with the event and plan they belong to.
#[derive(Clone, Debug)]
pub struct PhysicalPayload {
    /// The description event the bytes belong to, unchanged.
    pub event: DescriptionEvent,
    /// The plan the bytes were read under.
    pub plan: PhysicalRead,
    /// The bytes of the plan's [`available`](PhysicalRead::available) range.
    pub payload: Bytes,
    /// The source the bytes came from.
    pub source: SourceId,
    /// The input token of the read.
    pub token: InputToken,
}

/// Reads the available bytes of a [`PhysicalRead`] into one buffer charged to the input's
/// budget, and returns them with the event rather than as a new sample. The pool, source,
/// input generation and request are checked before every read and before the result.
#[derive(Debug)]
pub struct PhysicalReader {
    event: DescriptionEvent,
    plan: PhysicalRead,
    token: InputToken,
    position: u64,
    length: usize,
    buffer: Option<BudgetVec<u8>>,
    freeze_paid: bool,
    failed: Option<Error>,
}
impl PhysicalReader {
    /// Reserves the buffer and requests the plan's first available byte. Fails with
    /// `Error::ForeignPool`, `Error::SourceMismatch`, `Error::StaleGeneration` or
    /// `Error::StaleRequest` when `input` is not the one the plan was made from, with
    /// `Error::Unsupported` when `event` or the source no longer matches the plan, and with
    /// `Error::Limit` when the requested size exceeds `maximum_packet`.
    pub fn new(
        input: &mut InputCursor,
        event: DescriptionEvent,
        plan: PhysicalRead,
        maximum_packet: usize,
        _profile: PhysicalReadProfile,
    ) -> Result<Self> {
        if !input.budget().same_pool(&plan.origin_pool) {
            return Err(Error::ForeignPool);
        }
        if input.source() != plan.origin_source {
            return Err(Error::SourceMismatch);
        }
        require_token(input.token(), &plan.origin_token)?;
        if input.capabilities().length != Some(plan.physical_length)
            || !input.capabilities().random_access
            || event.packet.as_ref().map(|p| &p.source_range) != Some(&plan.requested)
            || event.range_error != plan.source_range_error
            || event.read != DescriptionRead::Available
        {
            return Err(Error::Unsupported("physical read event/source binding"));
        }
        let size = usize::try_from(
            plan.requested
                .end
                .checked_sub(plan.requested.start)
                .ok_or(Error::Overflow)?,
        )
        .map_err(|_| Error::Overflow)?;
        if size > maximum_packet {
            return Err(Error::Limit {
                requested: size,
                maximum: maximum_packet,
            });
        }
        let length = usize::try_from(
            plan.available
                .end
                .checked_sub(plan.available.start)
                .ok_or(Error::Overflow)?,
        )
        .map_err(|_| Error::Overflow)?;
        let mut buffer = BudgetVec::new(input.budget());
        buffer.try_reserve_exact(length)?;
        input.request_range(plan.available.start)?;
        Ok(Self {
            event,
            token: input.token(),
            position: plan.available.start,
            plan,
            length,
            buffer: Some(buffer),
            freeze_paid: false,
            failed: None,
        })
    }
    /// Reads toward the end of the available range. Fails with `Error::Truncated` when the
    /// source ends first, and with `Error::Invalid` when the input moved outside the
    /// reader. A failure other than backpressure is sticky and releases the buffer.
    pub fn poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<PhysicalPayload>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(input, work, cancel);
        if let Err(error) = result
            && !matches!(error, Error::Backpressure { .. })
        {
            self.buffer = None;
            self.failed = Some(error);
        }
        result
    }
    fn poll_inner(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<PhysicalPayload>> {
        if !input.budget().same_pool(&self.plan.origin_pool) {
            return Err(Error::ForeignPool);
        }
        if input.source() != self.plan.origin_source {
            return Err(Error::SourceMismatch);
        }
        require_token(input.token(), &self.token)?;
        if input.position() != self.position {
            return Err(Error::Invalid("input changed outside physical reader"));
        }
        input.read_into(&mut [], work, cancel)?;
        if work.remaining() == 0 {
            return Ok(Step::Yield);
        }
        loop {
            let buffer = self
                .buffer
                .as_mut()
                .ok_or(Error::Invalid("physical reader completed"))?;
            if buffer.len() == self.length {
                if work.remaining() == 0 {
                    return Ok(Step::Yield);
                }
                if !self.freeze_paid {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    self.freeze_paid = true;
                }
                let buffer = self
                    .buffer
                    .take()
                    .ok_or(Error::Invalid("physical buffer absent"))?;
                let payload = match buffer.freeze() {
                    Ok(payload) => payload,
                    Err(failure) => {
                        self.buffer = Some(failure.value);
                        return Err(failure.error);
                    }
                };
                return Ok(Step::Ready(PhysicalPayload {
                    event: self.event.clone(),
                    plan: self.plan.clone(),
                    payload,
                    source: input.source(),
                    token: self.token.clone(),
                }));
            }
            let remaining = self.length - buffer.len();
            match input.read_append(buffer, remaining, work, cancel)? {
                Step::Ready(_) => {
                    self.position = input.position();
                }
                Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                Step::Yield => return Ok(Step::Yield),
                Step::End(_) => return Err(Error::Truncated),
            }
        }
    }
}
fn require_token(token: InputToken, expected: &InputToken) -> Result<()> {
    if token.generation() != expected.generation() {
        return Err(Error::StaleGeneration {
            expected: expected.generation(),
            received: token.generation(),
        });
    }
    if token.request() != expected.request() {
        return Err(Error::StaleRequest {
            expected: expected.request(),
            received: token.request(),
        });
    }
    if token != *expected {
        return Err(Error::ForeignPool);
    }
    Ok(())
}
