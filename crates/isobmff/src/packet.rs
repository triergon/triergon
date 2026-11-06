//! One indexed, self-contained source sample through the shared input contract.
//! No I/O, demux clock inference, configuration epoch or second allocation pool.
use crate::{BoxHeader, PinnedMovD32RegularRecoveryV1, Sample};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{
    BudgetVec, Bytes, Error, InputCursor, InputToken, Result, SourceId, SourceLocation, Step,
    WorkBudget,
};

#[derive(Clone, Debug)]
pub struct SourcePacket {
    pub sample: Sample,
    pub payload: Bytes,
    pub source: SourceId,
    pub token: InputToken,
}

/// A positive, physically observed prefix of an incomplete declared sample.
/// The declaration and the source-bound EOF remain separate facts.
#[derive(Clone, Debug)]
pub struct Mov9ShortSample {
    pub declared: Sample,
    pub payload: Bytes,
    pub end: SourceLocation,
    pub token: InputToken,
}

#[derive(Debug)]
pub enum Mov9SampleRead {
    Complete(SourcePacket),
    Short(Mov9ShortSample),
}

#[derive(Debug)]
pub enum Mov9SampleExtent {
    Finite(Range<u64>),
    Terminal(TerminalMdat),
}

/// A checked terminal, top-level size-zero MDAT declaration. This preserves an
/// unknown physical endpoint; it is not a read/EOF receipt or a finite range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalMdat {
    header: BoxHeader,
    payload_start: u64,
}
impl TerminalMdat {
    pub fn new(header: BoxHeader) -> Result<Self> {
        if header.kind != *b"mdat" || header.size.is_some() || header.header_size != 8 {
            return Err(Error::Invalid(
                "expected terminal size-zero MDAT declaration",
            ));
        }
        let payload_start = header.start.checked_add(8).ok_or(Error::Overflow)?;
        Ok(Self {
            header,
            payload_start,
        })
    }
    pub fn header(self) -> BoxHeader {
        self.header
    }
    pub fn payload_start(self) -> u64 {
        self.payload_start
    }
    /// Checks the source-declared lower bound. Actual sample bytes must still be
    /// read, and only a source-bound completion can establish the final endpoint.
    pub fn require_sample(self, sample: &Sample) -> Result<()> {
        if sample.range.start > sample.range.end || sample.range.start < self.payload_start {
            return Err(Error::Invalid("sample outside terminal MDAT declaration"));
        }
        Ok(())
    }
}

/// A bounded range transaction. A freeze backpressure retry retains all
/// completed bytes. The reserved constructors allocate before changing the
/// input transaction.
#[derive(Debug)]
pub struct SampleReader {
    sample: Sample,
    source: SourceId,
    token: InputToken,
    position: u64,
    length: usize,
    buffer: Option<BudgetVec<u8>>,
    failed: Option<Error>,
    mode: ReadMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadMode {
    Reserved,
    RecoverPrefix,
    SharedWindow,
}

impl SampleReader {
    pub fn new(
        input: &mut InputCursor,
        sample: Sample,
        media_extent: Range<u64>,
        maximum_packet: usize,
    ) -> Result<Self> {
        sample.require_within(media_extent)?;
        Self::new_checked(input, sample, maximum_packet, ReadMode::Reserved)
    }
    /// Retains an existing shared input window when it contains the complete
    /// sample and the work budget can consume it. Otherwise reserves a packet
    /// buffer before consuming any bytes. Retained packets keep the window's
    /// original allocation charged until the last view is dropped.
    pub fn new_with_shared_window(
        input: &mut InputCursor,
        sample: Sample,
        media_extent: Range<u64>,
        maximum_packet: usize,
    ) -> Result<Self> {
        sample.require_within(media_extent)?;
        Self::new_checked(input, sample, maximum_packet, ReadMode::SharedWindow)
    }
    /// Reads the complete declared sample under an open terminal MDAT. No final
    /// media/source length is fabricated; premature physical EOF stays Truncated.
    pub fn new_terminal_mdat(
        input: &mut InputCursor,
        sample: Sample,
        terminal: TerminalMdat,
        maximum_packet: usize,
    ) -> Result<Self> {
        terminal.require_sample(&sample)?;
        Self::new_checked(input, sample, maximum_packet, ReadMode::Reserved)
    }
    pub fn new_pinned_mov_recovery(
        input: &mut InputCursor,
        sample: Sample,
        media: Mov9SampleExtent,
        maximum_packet: usize,
        _profile: PinnedMovD32RegularRecoveryV1,
    ) -> Result<Self> {
        match media {
            Mov9SampleExtent::Finite(extent) => sample.require_within(extent)?,
            Mov9SampleExtent::Terminal(terminal) => terminal.require_sample(&sample)?,
        }
        Self::new_checked(input, sample, maximum_packet, ReadMode::RecoverPrefix)
    }
    fn new_checked(
        input: &mut InputCursor,
        sample: Sample,
        maximum_packet: usize,
        mode: ReadMode,
    ) -> Result<Self> {
        if input.capabilities().length.is_some_and(|end| {
            sample.range.end > end && (mode != ReadMode::RecoverPrefix || sample.range.start >= end)
        }) {
            return Err(Error::Invalid("sample exceeds source length"));
        }
        let length =
            usize::try_from(sample.range.end - sample.range.start).map_err(|_| Error::Overflow)?;
        if length > maximum_packet {
            return Err(Error::Limit {
                requested: length,
                maximum: maximum_packet,
            });
        }
        let mut buffer = BudgetVec::new(input.budget());
        if mode != ReadMode::SharedWindow {
            buffer.try_reserve_exact(length)?;
        }
        // Internal I/O transaction only; core preserves parser generation.
        if mode == ReadMode::SharedWindow && input.capabilities().random_access {
            input.request_range_reusing_window(sample.range.start)?;
        } else if input.position() != sample.range.start || input.capabilities().random_access {
            input.request_range(sample.range.start)?;
        }
        Ok(Self {
            position: sample.range.start,
            sample,
            source: input.source(),
            token: input.token(),
            length,
            buffer: Some(buffer),
            failed: None,
            mode,
        })
    }
    pub fn poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<SourcePacket>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self
            .poll_inner(input, work, cancel)
            .and_then(|step| match step {
                Step::End(_) => Err(Error::Truncated),
                other => Ok(other),
            });
        self.record(result)
    }
    /// The same bounded read transaction with an explicit native short result.
    /// Only the matching physical End with a positive prefix permits recovery.
    pub fn poll_pinned_mov_recovery(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        _profile: PinnedMovD32RegularRecoveryV1,
    ) -> Result<Step<Mov9SampleRead>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(input, work, cancel).and_then(|step| {
            Ok(match step {
                Step::Ready(packet) => Step::Ready(Mov9SampleRead::Complete(packet)),
                Step::NeedInput(r) => Step::NeedInput(r),
                Step::Yield => Step::Yield,
                Step::End(end) => {
                    if self.buffer.as_ref().is_none_or(BudgetVec::is_empty) {
                        return Err(Error::Truncated);
                    }
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    Step::Ready(Mov9SampleRead::Short(Mov9ShortSample {
                        declared: self.sample.clone(),
                        payload: self.freeze_buffer()?,
                        end,
                        token: self.token.clone(),
                    }))
                }
            })
        });
        self.record(result)
    }
    fn record<T>(&mut self, result: Result<T>) -> Result<T> {
        if let Err(error) = &result
            && !matches!(error, Error::Backpressure { .. })
        {
            self.failed = Some(*error);
        }
        result
    }
    fn poll_inner(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<SourcePacket>> {
        // Even a zero-byte sample or a completed buffer must observe cancellation
        // and transport failure before producing a packet.
        input.read_into(&mut [], work, cancel)?;
        if self.source != input.source() {
            return Err(Error::SourceMismatch);
        }
        let current = input.token();
        if current.generation() != self.token.generation() {
            return Err(Error::StaleGeneration {
                expected: self.token.generation(),
                received: current.generation(),
            });
        }
        if current.request() != self.token.request() {
            return Err(Error::StaleRequest {
                expected: self.token.request(),
                received: current.request(),
            });
        }
        if self.token != current {
            return Err(Error::ForeignPool);
        }
        if self.position != input.position() {
            return Err(Error::Invalid("input changed outside sample reader"));
        }
        loop {
            let buffer = self
                .buffer
                .as_mut()
                .ok_or(Error::Invalid("sample reader completed"))?;
            if buffer.len() == self.length {
                if !work.try_charge(1) {
                    return Ok(Step::Yield);
                }
                let payload = self.freeze_buffer()?;
                return Ok(Step::Ready(self.packet(payload)));
            }
            if self.mode == ReadMode::SharedWindow && buffer.is_empty() {
                let available = input.buffered().len();
                if available >= self.length
                    && input.buffered_owner().is_some()
                    && work.remaining() > self.length as u64
                {
                    let Step::Ready(payload) = input.read_span(self.length, work, cancel)? else {
                        return Err(Error::Invalid("buffered sample unavailable"));
                    };
                    work.take(1);
                    self.position = input.position();
                    self.buffer = None;
                    return Ok(Step::Ready(self.packet(payload)));
                }
                if available != 0 && work.remaining() != 0 {
                    buffer.try_reserve_exact(self.length)?;
                }
            }
            let maximum = self.length - buffer.len();
            match input.read_append(buffer, maximum, work, cancel)? {
                Step::Ready(_) => {
                    self.position = input.position();
                }
                Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                Step::Yield => return Ok(Step::Yield),
                Step::End(end) => return Ok(Step::End(end)),
            }
        }
    }
    fn packet(&self, payload: Bytes) -> SourcePacket {
        SourcePacket {
            sample: self.sample.clone(),
            payload,
            source: self.source,
            token: self.token.clone(),
        }
    }
    fn freeze_buffer(&mut self) -> Result<Bytes> {
        let buffer = self
            .buffer
            .take()
            .ok_or(Error::Invalid("missing sample buffer"))?;
        match buffer.freeze() {
            Ok(bytes) => Ok(bytes),
            Err(failure) => {
                self.buffer = Some(failure.buffer);
                Err(failure.error)
            }
        }
    }
}

#[cfg(test)]
mod terminal_mdat_tests {
    use super::*;
    use crate::Access;
    use triergon_core::{Budget, InputWindow, SourceCapabilities};
    fn sample(range: Range<u64>) -> Sample {
        Sample {
            ordinal: 1,
            description_index: 1,
            range,
            dts_ticks: None,
            cts_ticks: None,
            duration_ticks: 1,
            composition_offset: None,
            access: Access::ContainerSync,
            partial_sync: false,
            dependency: None,
            fragment_flags: None,
        }
    }
    fn header() -> BoxHeader {
        BoxHeader {
            kind: *b"mdat",
            start: 0,
            size: None,
            header_size: 8,
        }
    }
    fn input(b: &Budget) -> InputCursor {
        let mut input = InputCursor::new(
            b,
            SourceId(6409),
            SourceCapabilities {
                length: None,
                random_access: true,
            },
            64,
        )
        .unwrap();
        input.request_range(8).unwrap();
        input
    }
    #[test]
    fn terminal_mdat_declaration_and_packet_limits_refuse_before_input_mutation() {
        for h in [
            BoxHeader {
                kind: *b"moof",
                ..header()
            },
            BoxHeader {
                size: Some(8),
                ..header()
            },
            BoxHeader {
                header_size: 16,
                ..header()
            },
        ] {
            assert!(matches!(TerminalMdat::new(h), Err(Error::Invalid(_))));
        }
        assert_eq!(
            TerminalMdat::new(BoxHeader {
                start: u64::MAX - 4,
                ..header()
            }),
            Err(Error::Overflow)
        );
        let terminal = TerminalMdat::new(header()).unwrap();
        assert_eq!(terminal.header(), header());
        assert_eq!(terminal.payload_start(), 8);
        assert!(terminal.require_sample(&sample(8..8)).is_ok());
        let b = Budget::new(64 << 10).unwrap();
        let mut input = input(&b);
        let token = input.token();
        let used = b.used();
        for range in [Range { start: 7, end: 12 }, Range { start: 12, end: 8 }] {
            assert!(matches!(
                SampleReader::new_terminal_mdat(&mut input, sample(range), terminal, 64),
                Err(Error::Invalid(_))
            ));
        }
        assert!(matches!(
            SampleReader::new_terminal_mdat(&mut input, sample(8..20), terminal, 11),
            Err(Error::Limit {
                requested: 12,
                maximum: 11
            })
        ));
        assert_eq!(input.position(), 8);
        assert_eq!(input.token(), token);
        assert_eq!(b.used(), used);
    }
    #[test]
    fn terminal_mdat_packet_reservation_retry_workzero_and_retained_owner() {
        let b = Budget::new(64 << 10).unwrap();
        let mut input = input(&b);
        let token = input.token();
        let terminal = TerminalMdat::new(header()).unwrap();
        let hold = b.reserve(b.available()).unwrap();
        assert!(matches!(
            SampleReader::new_terminal_mdat(&mut input, sample(8..20), terminal, 12),
            Err(Error::Backpressure { .. })
        ));
        assert_eq!(input.token(), token);
        assert_eq!(input.position(), 8);
        drop(hold);
        let mut reader =
            SampleReader::new_terminal_mdat(&mut input, sample(8..20), terminal, 12).unwrap();
        let r = input.request();
        let bytes = Bytes::copy_from_slice(&b, b"terminaldata").unwrap();
        input
            .provide_owned(InputWindow::new(r.source, r.token.clone(), r.offset, bytes).unwrap())
            .unwrap();
        let used = b.used();
        assert!(matches!(
            reader
                .poll(&mut input, &mut WorkBudget::new(0), &AtomicBool::new(false))
                .unwrap(),
            Step::Yield
        ));
        assert_eq!(input.position(), 8);
        assert_eq!(b.used(), used);
        let mut completed = None;
        for _ in 0..64 {
            match reader
                .poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                Step::Ready(packet) => {
                    completed = Some(packet);
                    break;
                }
                Step::Yield => {}
                step => panic!("unexpected sample step {step:?}"),
            }
        }
        let packet = completed.expect("bounded source sample must complete");
        drop(reader);
        drop(input);
        assert_eq!(packet.payload.as_ref(), b"terminaldata");
        assert_eq!(packet.sample.dts_ticks, None);
        assert_eq!(packet.sample.cts_ticks, None);
        drop(packet);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
    #[test]
    fn terminal_mdat_reader_rejects_foreign_pool_even_with_equal_coordinates() {
        let b = Budget::new(64 << 10).unwrap();
        let mut original = input(&b);
        let terminal = TerminalMdat::new(header()).unwrap();
        let mut reader =
            SampleReader::new_terminal_mdat(&mut original, sample(8..20), terminal, 12).unwrap();
        let foreign = Budget::new(64 << 10).unwrap();
        let mut other = input(&foreign);
        // Match the request counter too: only the pool identity differs.
        other.request_range(8).unwrap();
        assert!(matches!(
            reader.poll(&mut other, &mut WorkBudget::new(0), &AtomicBool::new(false)),
            Err(Error::ForeignPool)
        ));
        assert_eq!(original.position(), 8);
        assert_eq!(other.position(), 8);
    }
}
