//! The strict PES of one elementary stream of an active PMT, as the TS
//! subtitle bridges take them: checked against the bridge's binding and
//! gathered into one data field under a work budget.
pub mod dvb;
pub mod pgs;
pub mod teletext;

use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, SourceId, WorkBudget};
use triergon_mpegts::clock::SourceTimestamp;
use triergon_mpegts::pes::{Pes, stream_id};
use triergon_mpegts::psi::ElementaryStream;
use triergon_mpegts::stream::{FaultKind, TraceEvent};
use triergon_playback::{Delivery, Event, Output, TrackKey, TsEvent};

/// A PES taken from the player with its timestamps.
#[derive(Debug)]
pub(crate) struct TakenPes {
    pub pes: Pes,
    pub pts: Option<SourceTimestamp>,
    pub dts: Option<SourceTimestamp>,
}

/// What the player delivered for the bound stream.
#[expect(
    clippy::large_enum_variant,
    reason = "the PES moves from the player's output to the bridge by value, once per PES"
)]
#[derive(Debug)]
pub(crate) enum Taken {
    /// The next PES.
    Pes(TakenPes),
    /// The fault the player reported for the stream in place of a PES it discarded.
    Discarded(Error),
}

/// One elementary stream of one active PMT. A new playback generation, PMT
/// epoch or TS clock epoch needs a new binding.
#[derive(Debug)]
pub(crate) struct PesBinding {
    pool: Budget,
    source: SourceId,
    program: u16,
    pub(crate) pid: u16,
    /// The program's PCR PID, as the PMT names it (`pid::NULL` for none).
    pub(crate) pcr_pid: u16,
    generation: u64,
    pub(crate) configuration_epoch: u64,
    clock_epoch: Option<u64>,
    packet_limit: usize,
}

impl PesBinding {
    /// Bind to `pid` in the active PMT `output`, with its PMT entry.
    pub(crate) fn new(
        output: &Output,
        pid: u16,
        packet_limit: usize,
    ) -> Result<(Self, &ElementaryStream)> {
        let Event::Ts(TsEvent {
            source:
                TraceEvent::ProgramMap {
                    map,
                    active: true,
                    configuration_epoch,
                },
            ..
        }) = output.event()
        else {
            return Err(Error::Invalid("TS bridge requires an active program map"));
        };
        let stream = map
            .streams
            .as_slice()
            .iter()
            .find(|stream| stream.pid == pid)
            .ok_or(Error::Invalid("TS PID absent from program map"))?;
        let binding = Self {
            pool: output.pool().clone(),
            source: output.source_id(),
            program: map.header.extension,
            pid,
            pcr_pid: map.pcr_pid,
            generation: output.generation(),
            configuration_epoch: *configuration_epoch,
            clock_epoch: None,
            packet_limit,
        };
        Ok((binding, stream))
    }

    pub(crate) fn pool(&self) -> &Budget {
        &self.pool
    }

    pub(crate) fn source(&self) -> SourceId {
        self.source
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// The fault in `mailbox` the player reported for a PES of the stream it
    /// discarded, moved out of it; any other output is refused and stays there.
    #[cold]
    #[inline(never)]
    fn discarded(&self, mailbox: &mut Option<Output>) -> Result<Option<Taken>> {
        let Some(Event::Ts(TsEvent {
            source: TraceEvent::Diagnostic(fault),
            ..
        })) = mailbox.as_ref().map(Output::event)
        else {
            return Err(Error::Invalid("TS bridge requires strict TS PES"));
        };
        if fault.kind != FaultKind::Pes || fault.pid != Some(self.pid) {
            return Err(Error::Invalid("TS bridge requires strict TS PES"));
        }
        let error = fault.error;
        *mailbox = None;
        Ok(Some(Taken::Discarded(error)))
    }

    /// Move the next PES of the bound stream out of `mailbox`, or the fault the
    /// player reported for the stream in place of one. An empty mailbox gives
    /// `None`; a refused output stays in the mailbox.
    pub(crate) fn take(&mut self, mailbox: &mut Option<Output>) -> Result<Option<Taken>> {
        let Some(output) = mailbox.as_ref() else {
            return Ok(None);
        };
        if !output.belongs_to(&self.pool) {
            return Err(Error::ForeignPool);
        }
        if output.source_id() != self.source {
            return Err(Error::SourceMismatch);
        }
        if output.generation() != self.generation {
            return Err(Error::StaleGeneration {
                expected: self.generation,
                received: output.generation(),
            });
        }
        let Event::Ts(TsEvent {
            source: TraceEvent::Pes(pes),
            source_pts,
            source_dts,
            ..
        }) = output.event()
        else {
            return self.discarded(mailbox);
        };
        let track = TrackKey::Ts {
            program: self.program,
            pid: self.pid,
        };
        if output.delivery() == Delivery::Unselected
            || output.event().track() != Some(track)
            || pes.header.stream_id != stream_id::PRIVATE_STREAM_1
            || pes.epochs.configuration != self.configuration_epoch
        {
            return Err(Error::Invalid("TS bridge track or configuration changed"));
        }
        if self
            .clock_epoch
            .is_some_and(|clock| clock != pes.epochs.clock)
        {
            return Err(Error::Invalid("TS bridge clock epoch changed"));
        }
        if pes.payload_len == 0 || pes.spans().is_empty() {
            return Err(Error::Invalid("TS bridge empty PES"));
        }
        if pes.payload_len > self.packet_limit {
            return Err(Error::Limit {
                requested: pes.payload_len,
                maximum: self.packet_limit,
            });
        }
        // Taking the event and rebuilding refusals out of line raises subtitle counts by up to 0.83%.
        let (pts, dts) = (*source_pts, *source_dts);
        let Some(Event::Ts(TsEvent {
            source: TraceEvent::Pes(pes),
            ..
        })) = mailbox.take().map(Output::into_event)
        else {
            unreachable!("validated PES event")
        };
        self.clock_epoch = Some(pes.epochs.clock);
        Ok(Some(Taken::Pes(TakenPes { pes, pts, dts })))
    }
}

/// A PES data field gathered into one buffer: shared when it arrived in one
/// span, copied otherwise.
#[derive(Debug)]
pub(crate) struct PesData {
    pes: Pes,
    data: BudgetVec<u8>,
    span: usize,
    offset: usize,
    bytes: Option<Bytes>,
}

impl PesData {
    pub(crate) fn new(pes: Pes, pool: &Budget) -> Self {
        Self {
            pes,
            data: BudgetVec::new(pool),
            span: 0,
            offset: 0,
            bytes: None,
        }
    }

    /// The data field once gathered, `None` to yield. Charges one unit per
    /// copied byte.
    pub(crate) fn poll(&mut self, pool: &Budget, work: &mut WorkBudget) -> Result<Option<&Bytes>> {
        if self.bytes.is_none() {
            self.bytes = self.gather(pool, work)?;
        }
        Ok(self.bytes.as_ref())
    }

    fn gather(&mut self, pool: &Budget, work: &mut WorkBudget) -> Result<Option<Bytes>> {
        let spans = self.pes.spans();
        if let [span] = spans {
            return if work.try_charge(1) {
                span.payload_bytes().map(Some)
            } else {
                Ok(None)
            };
        }
        if self.data.capacity() == 0 {
            if !work.try_charge(1) {
                return Ok(None);
            }
            self.data.try_reserve_exact(self.pes.payload_len)?;
        }
        while let Some(span) = spans.get(self.span) {
            let remaining = &span.as_ref()[self.offset..];
            let take = work.take(remaining.len() as u64) as usize;
            if take == 0 && !remaining.is_empty() {
                return Ok(None);
            }
            self.data.try_extend_from_slice(&remaining[..take])?;
            if take == remaining.len() {
                self.span += 1;
                self.offset = 0;
            } else {
                self.offset += take;
            }
        }
        if self.data.len() != self.pes.payload_len {
            return Err(Error::Invalid("TS PES span length"));
        }
        if !work.try_charge(1) {
            return Ok(None);
        }
        match std::mem::replace(&mut self.data, BudgetVec::new(pool)).freeze() {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) => {
                self.data = error.value;
                Err(error.error)
            }
        }
    }
}
