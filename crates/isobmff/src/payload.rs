//! Sample payloads. [`SampleReader`] reads one sample's bytes through the caller's input
//! cursor into a [`SourcePacket`], and [`TerminalMdat`] bounds samples in a final `mdat`
//! whose length is not known.
use crate::{
    Sample,
    boxes::BoxHeader,
    compat::mov::field_recovery::{RecoverySampleExtent, RecoverySampleRead, ShortSample},
    table::recovery::RunRecoveryProfile,
};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{
    BudgetVec, Bytes, Error, InputCursor, InputToken, Result, SourceId, Step, WorkBudget,
};

/// One sample and its payload, from [`SampleReader`].
#[derive(Clone, Debug)]
pub struct SourcePacket {
    /// The sample the bytes belong to.
    pub sample: Sample,
    /// Exactly the sample's byte range, charged to the shared budget; it may share the
    /// storage of an input window.
    pub payload: Bytes,
    /// The source it was read from.
    pub source: SourceId,
    /// The input token of the read that produced it.
    pub token: InputToken,
}

/// A top-level size-zero `mdat`, which runs to the end of a source of unknown length.
/// It bounds samples from below only; reading a sample finds whether its bytes exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalMdat {
    header: BoxHeader,
    payload_start: u64,
}
impl TerminalMdat {
    /// The `mdat` of `header`; `Error::Invalid` unless it is an `mdat` of unknown size with
    /// an 8-byte header.
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
    /// The `mdat` header.
    pub fn header(self) -> BoxHeader {
        self.header
    }
    pub(crate) fn payload_start(self) -> u64 {
        self.payload_start
    }
    /// Checks that the sample starts at or after the payload start; `Error::Invalid`
    /// otherwise. Only reading the sample shows that the source holds its bytes.
    pub fn require_sample(self, sample: &Sample) -> Result<()> {
        self.require_range(&sample.range)
    }
    /// [`require_sample`](Self::require_sample) for a byte range; `Error::Invalid` also
    /// for a reversed range.
    pub(crate) fn require_range(self, range: &Range<u64>) -> Result<()> {
        if range.start > range.end || range.start < self.payload_start {
            return Err(Error::Invalid("sample outside terminal MDAT declaration"));
        }
        Ok(())
    }
}

/// Reads one sample's payload through the caller's input cursor, continuing from the
/// current window where it already holds the sample's start. The constructors check the
/// sample and, except [`new_with_shared_window`](Self::new_with_shared_window), reserve
/// its buffer before they move the cursor. Backpressure while finishing keeps the bytes
/// read for a retry; any other failure is kept for later polls.
#[derive(Debug)]
pub struct SampleReader {
    sample: Sample,
    source: SourceId,
    token: InputToken,
    position: u64,
    length: usize,
    buffer: SampleBuffer,
    failed: Option<Error>,
}

#[derive(Debug)]
enum SampleBuffer {
    WaitingForWindow,
    Copying(BudgetVec<u8>),
    Complete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadMode {
    Reserved,
    RecoverPrefix,
    SharedWindow,
}

impl SampleReader {
    /// A reader that copies `sample`, which must lie inside `media_extent`, the payload
    /// extent of its `mdat`. `Error::Limit` above `maximum_packet` bytes, `Error::Invalid`
    /// past the known source length.
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
    /// A reader for a sample of a [`TerminalMdat`]. The source ending inside the sample
    /// fails the read with `Error::Truncated`.
    pub fn new_terminal_mdat(
        input: &mut InputCursor,
        sample: Sample,
        terminal: TerminalMdat,
        maximum_packet: usize,
    ) -> Result<Self> {
        terminal.require_sample(&sample)?;
        Self::new_checked(input, sample, maximum_packet, ReadMode::Reserved)
    }
    /// A reader for the FFmpeg-compatible recovery view, whose
    /// [`poll_recovery`](Self::poll_recovery) returns what exists of a sample cut off by
    /// the end of the source. The sample may end past the known source length if it
    /// starts before it.
    pub fn new_recovery(
        input: &mut InputCursor,
        sample: Sample,
        media: RecoverySampleExtent,
        maximum_packet: usize,
        _profile: RunRecoveryProfile,
    ) -> Result<Self> {
        match media {
            RecoverySampleExtent::Finite(extent) => sample.require_within(extent)?,
            RecoverySampleExtent::Terminal(terminal) => terminal.require_sample(&sample)?,
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
        let buffer = if mode == ReadMode::SharedWindow {
            SampleBuffer::WaitingForWindow
        } else {
            let mut buffer = BudgetVec::new(input.budget());
            buffer.try_reserve_exact(length)?;
            SampleBuffer::Copying(buffer)
        };
        // Internal I/O transaction only; core preserves parser generation. Bytes
        // the current window already holds from the sample's start are read from
        // it rather than requested again.
        if input.capabilities().random_access {
            input.request_range_reusing_window(sample.range.start)?;
        } else if input.position() != sample.range.start {
            input.request_range(sample.range.start)?;
        }
        Ok(Self {
            position: sample.range.start,
            sample,
            source: input.source(),
            token: input.token(),
            length,
            buffer,
            failed: None,
        })
    }
    /// One bounded step towards the [`SourcePacket`]. `Error::Truncated` when the source
    /// ends inside the sample; a stale-token error or `Error::Invalid` when something else
    /// moved the cursor.
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
    /// [`poll`](Self::poll), except that a source ending inside the sample after at least
    /// one copied byte returns those bytes as [`RecoverySampleRead::Short`] instead of
    /// failing, as FFmpeg's MOV demuxer returns the part of a sample that exists.
    pub fn poll_recovery(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        _profile: RunRecoveryProfile,
    ) -> Result<Step<RecoverySampleRead>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(input, work, cancel).and_then(|step| {
            Ok(match step {
                Step::Ready(packet) => Step::Ready(RecoverySampleRead::Complete(packet)),
                Step::NeedInput(r) => Step::NeedInput(r),
                Step::Yield => Step::Yield,
                Step::End(end) => {
                    if !matches!(&self.buffer, SampleBuffer::Copying(buffer) if !buffer.is_empty())
                    {
                        return Err(Error::Truncated);
                    }
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    Step::Ready(RecoverySampleRead::Short(ShortSample {
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
        if self.token != current {
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
            return Err(Error::ForeignPool);
        }
        if self.position != input.position() {
            return Err(Error::Invalid("input changed outside sample reader"));
        }
        if matches!(self.buffer, SampleBuffer::WaitingForWindow) {
            if self.length == 0 {
                if !work.try_charge(1) {
                    return Ok(Step::Yield);
                }
                let payload = Bytes::copy_from_slice(input.budget(), &[])?;
                self.buffer = SampleBuffer::Complete;
                return Ok(Step::Ready(self.packet(payload)));
            }
            let available = input.buffered().len();
            if available == 0
                || (available >= self.length
                    && input.buffered_owner().is_some()
                    && work.remaining() > self.length as u64)
            {
                return match input.read_span(self.length, work, cancel)? {
                    Step::Ready(payload) => {
                        work.take(1);
                        self.position = input.position();
                        self.buffer = SampleBuffer::Complete;
                        Ok(Step::Ready(self.packet(payload)))
                    }
                    Step::NeedInput(request) => Ok(Step::NeedInput(request)),
                    Step::Yield => Ok(Step::Yield),
                    Step::End(end) => Ok(Step::End(end)),
                };
            }
            if work.remaining() == 0 {
                return Ok(Step::Yield);
            }
            let mut buffer = BudgetVec::new(input.budget());
            buffer.try_reserve_exact(self.length)?;
            self.buffer = SampleBuffer::Copying(buffer);
        }
        let SampleBuffer::Copying(buffer) = &mut self.buffer else {
            return Err(Error::Invalid("sample reader completed"));
        };
        loop {
            if buffer.len() == self.length {
                if !work.try_charge(1) {
                    return Ok(Step::Yield);
                }
                let payload = self.freeze_buffer()?;
                return Ok(Step::Ready(self.packet(payload)));
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
        let SampleBuffer::Copying(buffer) =
            std::mem::replace(&mut self.buffer, SampleBuffer::Complete)
        else {
            return Err(Error::Invalid("missing sample buffer"));
        };
        match buffer.freeze() {
            Ok(bytes) => Ok(bytes),
            Err(failure) => {
                self.buffer = SampleBuffer::Copying(failure.value);
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
    fn terminal_mdat_packet_reservation_retry_workzero_and_retained_backing() {
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
