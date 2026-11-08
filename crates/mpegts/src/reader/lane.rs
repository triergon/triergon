use super::model::{ClockContext, PacketData, Shared};
use super::{
    AccessUnit, AudioEofPolicy, Packet, PacketClock, PayloadSource, ReaderLimits, SourceEvent,
    Track,
};
use crate::clock::{SourceTimePolicy, source_timestamp};
use crate::compat::pes::{PrivateAudioCarriage, PrivateAudioDescriptors};
use crate::psi::DescriptorNamespace;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputRequest, InputWindow, Result, SourceCapabilities,
    SourceId, Step, WorkBudget,
};
use triergon_framing::{
    AacAdtsStreamLimits, AacAdtsStreamReader, Ac3FrameLimits, Ac3FrameReader, Ac3StreamSelection,
    Codec, RawAnnexBReader,
};

// Keep concrete readers inline; their containing BudgetVec charges the whole state.
#[allow(clippy::large_enum_variant)]
enum Framer {
    Video(RawAnnexBReader),
    Adts(AacAdtsStreamReader),
    Ac3(Ac3FrameReader),
}

impl Framer {
    fn is_audio(&self) -> bool {
        matches!(self, Self::Adts(_) | Self::Ac3(_))
    }
    fn new(pool: &Budget, track: &Track, limits: ReaderLimits) -> Result<Self> {
        // Private concatenated elementary view, never an external source request.
        let source = SourceId(0);
        let caps = SourceCapabilities {
            length: None,
            random_access: false,
        };
        match track.stream().stream_type {
            kind @ (0x1b | 0x24) => Ok(Self::Video(RawAnnexBReader::new(
                pool,
                if kind == 0x1b {
                    Codec::Avc
                } else {
                    Codec::Hevc
                },
                source,
                caps,
                limits.video,
            )?)),
            0x0f => Ok(Self::Adts(AacAdtsStreamReader::new(
                pool,
                source,
                caps,
                AacAdtsStreamLimits {
                    maximum_frame_bytes: limits.maximum_audio_frame_bytes,
                    maximum_frames: limits.maximum_access_units,
                    maximum_input_window: limits.elementary_input_window,
                },
            )?)),
            0x06 if matches!(
                track.stream().descriptor_namespace(),
                DescriptorNamespace::Dvb(_)
            ) =>
            {
                let carriage = PrivateAudioCarriage::from_observed_pmt(
                    6,
                    PrivateAudioDescriptors {
                        program: track.program_map().program_descriptors.as_ref(),
                        elementary: track.stream().descriptor_bytes.as_ref(),
                    },
                    limits.transport,
                )?;
                if carriage != PrivateAudioCarriage::DvbAc3Tag {
                    return Err(Error::Unsupported(
                        "MPEG-TS E-AC-3 access-unit grouping unavailable",
                    ));
                }
                Ok(Self::Ac3(Ac3FrameReader::new(
                    pool,
                    source,
                    caps,
                    Ac3StreamSelection::Ac3Only,
                    Ac3FrameLimits {
                        maximum_frame_bytes: limits.maximum_audio_frame_bytes,
                        maximum_frames: limits.maximum_access_units,
                        maximum_input_window: limits.elementary_input_window,
                    },
                )?))
            }
            _ => Err(Error::Unsupported(
                "MPEG-TS carriage has no admitted access-unit framer",
            )),
        }
    }
    fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step<AccessUnit>> {
        fn map<T>(step: Step<T>, wrap: impl FnOnce(T) -> AccessUnit) -> Step<AccessUnit> {
            match step {
                Step::Ready(unit) => Step::Ready(wrap(unit)),
                Step::Yield => Step::Yield,
                Step::NeedInput(request) => Step::NeedInput(request),
                Step::End(location) => Step::End(location),
                Step::Backpressure(never) => match never {},
            }
        }
        Ok(match self {
            Self::Video(reader) => map(reader.step(work, cancel)?, AccessUnit::Video),
            Self::Adts(reader) => map(reader.step(work, cancel)?, AccessUnit::Adts),
            Self::Ac3(reader) => map(reader.step(work, cancel)?, AccessUnit::Ac3),
        })
    }
    fn provide(&mut self, window: InputWindow) -> Result<()> {
        match self {
            Self::Video(reader) => reader.provide_owned(window),
            Self::Adts(reader) => reader.provide_owned(window),
            Self::Ac3(reader) => reader.provide_owned(window),
        }
    }
    fn finish(&mut self, request: InputRequest) -> Result<()> {
        match self {
            Self::Video(reader) => reader.finish(request.source, request.token),
            Self::Adts(reader) => reader.finish(request.source, request.token),
            Self::Ac3(reader) => reader.finish(request.source, request.token),
        }
    }
}

struct Piece {
    range: std::ops::Range<u64>,
    source: SourceEvent,
    clock: ClockContext,
    serial: u64,
}

struct Input {
    source: SourceEvent,
    span: usize,
    offset: usize,
}

fn table_storage<T>(table: &BudgetVec<T>) -> Result<usize> {
    table
        .capacity()
        .checked_mul(size_of::<T>())
        .ok_or(Error::Overflow)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StorageCursor {
    Map,
    MapSource,
    MapProgram,
    MapStream(usize),
    Input,
    Lane(usize),
    Piece {
        lane: usize,
        piece: usize,
    },
    Span {
        lane: usize,
        piece: usize,
        span: usize,
    },
    End,
}

enum StorageItem<'a> {
    Fixed(usize),
    Bytes(&'a Bytes),
}

impl StorageCursor {
    // One structural edge per call, including empty tables. Both this walk and
    // the preceding-owner deduplication walk consume cooperative work.
    fn step<'a>(&mut self, reader: &'a super::Reader) -> Result<Option<StorageItem<'a>>> {
        let item = match *self {
            Self::Map => {
                *self = Self::MapSource;
                reader
                    .map
                    .as_ref()
                    .map(|source| {
                        let map = source.map();
                        Shared::<crate::TraceEvent>::allocation_bytes()?
                            .checked_add(table_storage(&map.streams)?)
                            .and_then(|n| n.checked_add(table_storage(&map.source_ranges).ok()?))
                            .map(StorageItem::Fixed)
                            .ok_or(Error::Overflow)
                    })
                    .transpose()?
            }
            Self::MapSource => {
                *self = Self::MapProgram;
                reader
                    .map
                    .as_ref()
                    .map(|source| StorageItem::Bytes(&source.map().source))
            }
            Self::MapProgram => {
                *self = Self::MapStream(0);
                reader
                    .map
                    .as_ref()
                    .map(|source| StorageItem::Bytes(&source.map().program_descriptors))
            }
            Self::MapStream(index) => {
                let stream = reader
                    .map
                    .as_ref()
                    .and_then(|source| source.map().streams.as_slice().get(index));
                if let Some(stream) = stream {
                    *self = Self::MapStream(index + 1);
                    Some(StorageItem::Bytes(&stream.descriptor_bytes))
                } else {
                    *self = Self::Input;
                    None
                }
            }
            Self::Input => {
                *self = Self::Lane(0);
                reader
                    .input
                    .as_ref()
                    .and_then(triergon_core::InputCursor::buffered_owner)
                    .map(StorageItem::Bytes)
            }
            Self::Lane(index) => {
                if let Some(lane) = reader.lanes.as_slice().get(index) {
                    if lane.framer.is_some()
                        || lane.input.is_some()
                        || lane.build.is_some()
                        || reader
                            .map
                            .as_ref()
                            .is_none_or(|map| !map.0.shares_storage_with(&lane.track.source.0))
                    {
                        return Err(Error::Invalid("MPEG-TS fragment audit before framer drain"));
                    }
                    *self = Self::Piece {
                        lane: index,
                        piece: 0,
                    };
                    Some(StorageItem::Fixed(table_storage(&lane.pieces)?))
                } else {
                    *self = Self::End;
                    None
                }
            }
            Self::Piece { lane, piece } => {
                if let Some(source) = reader.lanes.as_slice()[lane].pieces.as_slice().get(piece) {
                    let pes = source.source.pes();
                    // Source PES storage grows exactly at each admitted span.
                    let bytes = pes
                        .spans()
                        .len()
                        .checked_mul(size_of::<crate::PesSpan>())
                        .and_then(|n| n.checked_add(table_storage(&pes.header_source_ranges).ok()?))
                        .and_then(|n| {
                            n.checked_add(Shared::<crate::TraceEvent>::allocation_bytes().ok()?)
                        })
                        .ok_or(Error::Overflow)?;
                    *self = Self::Span {
                        lane,
                        piece,
                        span: 0,
                    };
                    Some(StorageItem::Fixed(bytes))
                } else {
                    *self = Self::Lane(lane + 1);
                    None
                }
            }
            Self::Span { lane, piece, span } => {
                let pes = reader.lanes.as_slice()[lane].pieces.as_slice()[piece]
                    .source
                    .pes();
                if let Some(source) = pes.spans().get(span) {
                    *self = Self::Span {
                        lane,
                        piece,
                        span: span + 1,
                    };
                    Some(StorageItem::Bytes(source.backing_bytes()))
                } else {
                    *self = Self::Piece {
                        lane,
                        piece: piece + 1,
                    };
                    None
                }
            }
            Self::End => None,
        };
        Ok(item)
    }
}

// No allocation is needed to decide whether allocation is possible. Comparing
// earlier owners may take several steps; cursors survive yields and cancellation.
// Caller-only reservations and output owners are never part of this graph.
pub(super) struct StorageScan {
    cursor: StorageCursor,
    prior: StorageCursor,
    candidate: Option<(StorageCursor, Bytes)>,
    total: usize,
}

impl StorageScan {
    pub(super) fn new(reader: &super::Reader) -> Result<Self> {
        if reader.stream.is_some()
            || reader.boundary.is_some()
            || !reader
                .pending
                .as_ref()
                .is_some_and(|source| matches!(source.event(), crate::TraceEvent::Finished))
        {
            return Err(Error::Invalid("MPEG-TS fragment audit before source EOF"));
        }
        let total = Budget::control_storage_bytes()
            .checked_add(table_storage(&reader.lanes)?)
            .and_then(|n| n.checked_add(table_storage(&reader.tracks).ok()?))
            .and_then(|n| n.checked_add(table_storage(&reader.staged_tracks).ok()?))
            .and_then(|n| n.checked_add(Shared::<crate::TraceEvent>::allocation_bytes().ok()?))
            .ok_or(Error::Overflow)?;
        Ok(Self {
            cursor: StorageCursor::Map,
            prior: StorageCursor::Map,
            candidate: None,
            total,
        })
    }
    pub(super) fn bytes(&self) -> usize {
        self.total
    }
    pub(super) fn step(
        &mut self,
        reader: &super::Reader,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.cursor == StorageCursor::End && self.candidate.is_none() {
                return Ok(true);
            }
            if !work.try_charge(1) {
                return Ok(false);
            }
            if let Some((at, bytes)) = &self.candidate {
                if self.prior == *at {
                    self.total = self
                        .total
                        .checked_add(bytes.retained_storage_bytes())
                        .ok_or(Error::Overflow)?;
                    self.candidate = None;
                } else if let Some(StorageItem::Bytes(prior)) = self.prior.step(reader)?
                    && prior.shares_storage_with(bytes)
                {
                    self.candidate = None;
                }
            } else {
                let at = self.cursor;
                match self.cursor.step(reader)? {
                    Some(StorageItem::Fixed(bytes)) => {
                        self.total = self.total.checked_add(bytes).ok_or(Error::Overflow)?;
                    }
                    Some(StorageItem::Bytes(bytes)) => {
                        self.candidate = Some((at, bytes.clone()));
                        self.prior = StorageCursor::Map;
                    }
                    None => (),
                }
            }
        }
    }
}

// Only validated PES spans enter this copy. The framer has already refused at
// logical EOF; keeping the copy separate makes every allocation/yield retryable.
struct Incomplete {
    range: std::ops::Range<u64>,
    buffer: Option<BudgetVec<u8>>,
    bytes: Option<Bytes>,
    prepared: bool,
    piece: usize,
    span: Option<usize>,
    span_start: usize,
    copied: usize,
    covered: usize,
    origins: usize,
    reader_storage: Option<usize>,
}

impl Incomplete {
    fn new(range: std::ops::Range<u64>, pool: &Budget) -> Self {
        Self {
            range,
            buffer: Some(BudgetVec::new(pool)),
            bytes: None,
            prepared: false,
            piece: 0,
            span: None,
            span_start: 0,
            copied: 0,
            covered: 0,
            origins: 0,
            reader_storage: None,
        }
    }

    fn scan(
        &mut self,
        pieces: &[Piece],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        while let Some(piece) = pieces.get(self.piece) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(false);
            }
            let pes = piece.source.pes();
            if let Some(index) = self.span {
                if let Some(span) = pes.spans().get(index) {
                    let length = span.as_ref().len();
                    let begin = piece
                        .range
                        .start
                        .checked_add(self.span_start as u64)
                        .ok_or(Error::Overflow)?;
                    let end = begin.checked_add(length as u64).ok_or(Error::Overflow)?;
                    let count = self
                        .range
                        .end
                        .min(end)
                        .saturating_sub(self.range.start.max(begin));
                    self.covered = self
                        .covered
                        .checked_add(usize::try_from(count).map_err(|_| Error::Overflow)?)
                        .ok_or(Error::Overflow)?;
                    self.span_start = self.span_start.checked_add(length).ok_or(Error::Overflow)?;
                    self.span = Some(index + 1);
                } else {
                    self.piece += 1;
                    self.span = None;
                    self.span_start = 0;
                }
            } else {
                if piece.range.start < self.range.end && self.range.start < piece.range.end {
                    self.origins += 1;
                }
                self.span = Some(0);
            }
        }
        Ok(true)
    }

    fn prepare(
        &mut self,
        pieces: &[Piece],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if self.prepared {
            return Ok(true);
        }
        if !self.scan(pieces, work, cancel)? || !work.try_charge(1) {
            return Ok(false);
        }
        let size =
            usize::try_from(self.range.end - self.range.start).map_err(|_| Error::Overflow)?;
        if self.covered != size {
            return Err(Error::Invalid("MPEG-TS incomplete audio source extent"));
        }
        // The EOF audit counted all remaining reader owners, without any
        // caller-only reservations. Add the complete publication peak here.
        let minimum = self
            .origins
            .checked_mul(size_of::<PayloadSource>())
            .and_then(|n| n.checked_add(self.reader_storage.expect("audited reader storage")))
            .and_then(|n| n.checked_add(size))
            .and_then(|n| n.checked_add(Bytes::owner_overhead_bytes()))
            .and_then(|n| n.checked_add(Shared::<PacketData>::allocation_bytes().ok()?))
            .ok_or(Error::Overflow)?;
        let buffer = self.buffer.as_mut().expect("incomplete audio buffer");
        if minimum > buffer.budget().limit() {
            return Err(Error::Limit {
                requested: minimum,
                maximum: buffer.budget().limit(),
            });
        }
        buffer.try_reserve_exact(size)?;
        self.prepared = true;
        self.piece = 0;
        self.span = Some(0);
        self.span_start = 0;
        Ok(true)
    }

    fn copy(
        &mut self,
        pieces: &[Piece],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if self.bytes.is_some() {
            return Ok(true);
        }
        if !self.prepare(pieces, work, cancel)? {
            return Ok(false);
        }
        while let Some(piece) = pieces.get(self.piece) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let index = self.span.expect("prepared span cursor");
            let Some(span) = piece.source.pes().spans().get(index) else {
                if !work.try_charge(1) {
                    return Ok(false);
                }
                self.piece += 1;
                self.span = Some(0);
                self.span_start = 0;
                continue;
            };
            let bytes = span.as_ref();
            let begin = piece
                .range
                .start
                .checked_add(self.span_start as u64)
                .ok_or(Error::Overflow)?;
            let end = begin
                .checked_add(bytes.len() as u64)
                .ok_or(Error::Overflow)?;
            let start = self.range.start.max(begin);
            let length = usize::try_from(self.range.end.min(end).saturating_sub(start))
                .map_err(|_| Error::Overflow)?;
            if self.copied == length {
                if !work.try_charge(1) {
                    return Ok(false);
                }
                self.span = Some(index + 1);
                self.span_start = self
                    .span_start
                    .checked_add(bytes.len())
                    .ok_or(Error::Overflow)?;
                self.copied = 0;
                continue;
            }
            let count = usize::try_from(work.take((length - self.copied).min(4096) as u64))
                .map_err(|_| Error::Overflow)?;
            if count == 0 {
                return Ok(false);
            }
            let at = usize::try_from(start - begin).map_err(|_| Error::Overflow)? + self.copied;
            self.buffer
                .as_mut()
                .expect("incomplete buffer")
                .try_extend_from_slice(&bytes[at..at + count])?;
            self.copied += count;
        }
        if !work.try_charge(1) {
            return Ok(false);
        }
        match self.buffer.take().expect("incomplete freeze").freeze() {
            Ok(bytes) => self.bytes = Some(bytes),
            Err(error) => {
                self.buffer = Some(error.buffer);
                return Err(error.error);
            }
        }
        Ok(true)
    }
}

struct Build {
    unit: Option<AccessUnit>,
    buffer: Option<BudgetVec<u8>>,
    bytes: Option<Bytes>,
    nal: usize,
    part: u8,
    copied: usize,
    origins: BudgetVec<PayloadSource>,
    origin: usize,
    clock: Option<PacketClock>,
    last_anchor: u64,
    retire: usize,
    planned_origins: Option<usize>,
}

impl Build {
    fn new(unit: AccessUnit, pool: &Budget, last_anchor: u64) -> Self {
        let bytes = match &unit {
            AccessUnit::Adts(frame) => Some(frame.source_bytes().clone()),
            AccessUnit::Ac3(frame) => Some(frame.bytes().clone()),
            AccessUnit::Video(_) => None,
            AccessUnit::IncompleteAudio { bytes, .. } => Some(bytes.clone()),
        };
        Self {
            unit: Some(unit),
            buffer: Some(BudgetVec::new(pool)),
            bytes,
            nal: 0,
            part: 0,
            copied: 0,
            origins: BudgetVec::new(pool),
            origin: 0,
            clock: None,
            last_anchor,
            retire: 0,
            planned_origins: None,
        }
    }
    fn unit(&self) -> &AccessUnit {
        self.unit.as_ref().expect("pending access unit")
    }
    fn copy(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        if self.bytes.is_some() {
            return Ok(true);
        }
        let AccessUnit::Video(unit) = self.unit.as_ref().expect("pending video") else {
            unreachable!()
        };
        let size =
            usize::try_from(unit.range.end - unit.range.start).map_err(|_| Error::Overflow)?;
        let buffer = self.buffer.as_mut().expect("pending coalescing storage");
        buffer.try_reserve_exact(size.saturating_sub(buffer.len()))?;
        const ZEROS: [u8; 256] = [0; 256];
        while let Some(nal) = unit.nals().get(self.nal) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let facts = nal.facts();
            let triergon_framing::NalFraming::AnnexB {
                leading_zero_bytes,
                start_code_bytes,
                ..
            } = facts.framing
            else {
                unreachable!()
            };
            let (bytes, total, zeros): (&[u8], usize, bool) = match self.part {
                0 => (&ZEROS, leading_zero_bytes, true),
                1 if start_code_bytes == 4 => (&[0, 0, 0, 1], 4, false),
                1 => (&[0, 0, 1], 3, false),
                2 => (nal.bytes().as_ref(), nal.bytes().len(), false),
                3 => (&ZEROS, facts.trailing_zero_bytes, true),
                _ => unreachable!(),
            };
            if self.copied == total {
                if !work.try_charge(1) {
                    return Ok(false);
                }
                self.copied = 0;
                self.part += 1;
                if self.part == 4 {
                    self.part = 0;
                    self.nal += 1;
                }
                continue;
            }
            let count = (total - self.copied).min(if zeros { ZEROS.len() } else { total });
            let count = usize::try_from(work.take(count as u64)).map_err(|_| Error::Overflow)?;
            if count == 0 {
                return Ok(false);
            }
            let start = if zeros { 0 } else { self.copied };
            buffer.try_extend_from_slice(&bytes[start..start + count])?;
            self.copied += count;
        }
        if buffer.len() != size {
            return Err(Error::Invalid("MPEG-TS AU source extent"));
        }
        if !work.try_charge(1) {
            return Ok(false);
        }
        match self.buffer.take().expect("coalescing storage").freeze() {
            Ok(bytes) => self.bytes = Some(bytes),
            Err(error) => {
                self.buffer = Some(error.buffer);
                return Err(error.error);
            }
        }
        Ok(true)
    }
}

pub(super) enum LaneStep {
    Packet(Packet),
    NeedPayload,
    Yield,
    End,
    DeferredAudio,
}

pub(super) struct Lane {
    pub track: Track,
    pub unsupported: Option<Error>,
    framer: Option<Framer>,
    pool: Budget,
    limits: ReaderLimits,
    input: Option<Input>,
    pieces: BudgetVec<Piece>,
    build: Option<Build>,
    incomplete: Option<Incomplete>,
    supplied: u64,
    serial: u64,
    last_anchor: u64,
    retire: usize,
    retire_credit: usize,
    eof: bool,
    completed: usize,
    published: u64,
    pub run: Option<(u64, u64, u64)>,
}

impl Lane {
    pub(super) fn new(pool: &Budget, track: Track, limits: ReaderLimits) -> Result<Self> {
        let (framer, unsupported) = match Framer::new(pool, &track, limits) {
            Ok(framer) => (Some(framer), None),
            Err(error @ Error::Unsupported(_)) => (None, Some(error)),
            Err(error) => return Err(error),
        };
        Ok(Self {
            track,
            unsupported,
            framer,
            pool: pool.clone(),
            limits,
            input: None,
            pieces: BudgetVec::new(pool),
            build: None,
            incomplete: None,
            supplied: 0,
            serial: 0,
            last_anchor: 0,
            retire: 0,
            retire_credit: 0,
            eof: false,
            completed: 0,
            published: 0,
            run: None,
        })
    }
    pub(super) fn unpublished(&self) -> Option<std::ops::Range<u64>> {
        (self.published < self.supplied).then_some(self.published..self.supplied)
    }
    pub(super) fn has_incomplete(&self) -> bool {
        self.incomplete.is_some()
    }
    pub(super) fn set_reader_storage(&mut self, bytes: usize) {
        self.incomplete
            .as_mut()
            .expect("pending fragment")
            .reader_storage = Some(bytes);
    }
    pub(super) fn push(
        &mut self,
        source: &SourceEvent,
        clock: ClockContext,
        work: &mut WorkBudget,
    ) -> Result<bool> {
        if self.framer.is_none() {
            return Ok(true);
        }
        if self.input.is_some() {
            return Err(Error::Invalid("MPEG-TS PES mailbox occupied"));
        }
        if self.pieces.len() == self.limits.maximum_pes_per_access_unit {
            return Err(Error::Limit {
                requested: self.pieces.len().saturating_add(1),
                maximum: self.limits.maximum_pes_per_access_unit,
            });
        }
        if !work.try_charge(1) {
            return Ok(false);
        }
        self.pieces.try_reserve_exact(1)?;
        let pes = source.pes();
        let end = self
            .supplied
            .checked_add(u64::try_from(pes.payload_len).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        let serial = self.serial.checked_add(1).ok_or(Error::Overflow)?;
        self.pieces
            .try_push(Piece {
                range: self.supplied..end,
                source: source.clone(),
                clock,
                serial,
            })
            .map_err(|e| e.error)?;
        self.serial = serial;
        self.run = Some((
            pes.epochs.configuration,
            pes.epochs.clock,
            pes.byte_run_epoch,
        ));
        self.input = Some(Input {
            source: source.clone(),
            span: 0,
            offset: 0,
        });
        Ok(true)
    }
    pub(super) fn step(
        &mut self,
        draining: bool,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<LaneStep> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if work.remaining() == 0 {
                return Ok(LaneStep::Yield);
            }
            if self.retire != 0 {
                let cost = self.pieces.len() - self.retire_credit;
                self.retire_credit +=
                    usize::try_from(work.take(cost as u64)).map_err(|_| Error::Overflow)?;
                if self.retire_credit != self.pieces.len() {
                    return Ok(LaneStep::Yield);
                }
                self.pieces.as_mut_slice().rotate_left(self.retire);
                self.pieces.truncate(self.pieces.len() - self.retire);
                self.retire = 0;
                self.retire_credit = 0;
                continue;
            }
            if self.build.is_some() {
                return self.publish(work, cancel);
            }
            if let Some(incomplete) = self.incomplete.as_mut() {
                if incomplete.reader_storage.is_none() {
                    return Ok(LaneStep::DeferredAudio);
                }
                if !incomplete.copy(self.pieces.as_slice(), work, cancel)? {
                    return Ok(LaneStep::Yield);
                }
                let incomplete = self.incomplete.take().expect("finished audio copy");
                let unit = AccessUnit::IncompleteAudio {
                    range: incomplete.range,
                    bytes: incomplete.bytes.expect("copied audio bytes"),
                    reason: Error::Truncated,
                };
                let mut build = Build::new(unit, &self.pool, self.last_anchor);
                build.planned_origins = Some(incomplete.origins);
                self.build = Some(build);
                continue;
            }
            let Some(framer) = &mut self.framer else {
                if draining && self.eof {
                    if !self.pieces.is_empty() {
                        self.retire = self.pieces.len();
                        continue;
                    }
                    self.pieces = BudgetVec::new(&self.pool);
                }
                return Ok(LaneStep::End);
            };
            let step = match framer.step(work, cancel) {
                Err(Error::Truncated)
                    if draining
                        && self.eof
                        && self.input.is_none()
                        && framer.is_audio()
                        && self.limits.audio_eof_policy == AudioEofPolicy::ReportIncomplete =>
                {
                    let range = self.unpublished().ok_or(Error::Truncated)?;
                    if self.completed == self.limits.maximum_access_units {
                        return Err(Error::Limit {
                            requested: self.completed.saturating_add(1),
                            maximum: self.limits.maximum_access_units,
                        });
                    }
                    let size =
                        usize::try_from(range.end - range.start).map_err(|_| Error::Overflow)?;
                    if size > self.limits.maximum_audio_frame_bytes {
                        return Err(Error::Limit {
                            requested: size,
                            maximum: self.limits.maximum_audio_frame_bytes,
                        });
                    }
                    self.incomplete = Some(Incomplete::new(range, &self.pool));
                    self.framer = None;
                    continue;
                }
                result => result?,
            };
            match step {
                Step::Ready(unit) => {
                    if self.completed == self.limits.maximum_access_units {
                        return Err(Error::Limit {
                            requested: self.completed.saturating_add(1),
                            maximum: self.limits.maximum_access_units,
                        });
                    }
                    self.build = Some(Build::new(unit, &self.pool, self.last_anchor));
                }
                Step::Yield => return Ok(LaneStep::Yield),
                Step::End(_) => {
                    if draining && self.limits.audio_eof_policy == AudioEofPolicy::ReportIncomplete
                    {
                        self.framer = None;
                        continue;
                    }
                    return Ok(LaneStep::End);
                }
                Step::NeedInput(request) => {
                    if request.offset != self.supplied {
                        return Err(Error::Invalid("MPEG-TS elementary input offset"));
                    }
                    if let Some(input) = &mut self.input {
                        let pes = input.source.pes();
                        let Some(span) = pes.spans().get(input.span) else {
                            self.input = None;
                            continue;
                        };
                        if !work.try_charge(1) {
                            return Ok(LaneStep::Yield);
                        }
                        let bytes = span.payload_bytes()?;
                        let count = (bytes.len() - input.offset).min(request.maximum);
                        if count == 0 {
                            return Err(Error::Invalid("empty MPEG-TS payload span"));
                        }
                        let end = self
                            .supplied
                            .checked_add(count as u64)
                            .ok_or(Error::Overflow)?;
                        framer.provide(InputWindow::new(
                            request.source,
                            request.token,
                            request.offset,
                            bytes.slice(input.offset..input.offset + count)?,
                        )?)?;
                        self.supplied = end;
                        input.offset += count;
                        if input.offset == bytes.len() {
                            input.span += 1;
                            input.offset = 0;
                        }
                    } else if draining {
                        if self.eof {
                            return Err(Error::Invalid("MPEG-TS framer requested input after EOF"));
                        }
                        if !work.try_charge(1) {
                            return Ok(LaneStep::Yield);
                        }
                        framer.finish(request)?;
                        self.eof = true;
                    } else {
                        return Ok(LaneStep::NeedPayload);
                    }
                }
            }
        }
    }
    fn publish(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<LaneStep> {
        let build = self.build.as_mut().expect("pending publication");
        if let Some(count) = build.planned_origins {
            if !work.try_charge(1) {
                return Ok(LaneStep::Yield);
            }
            build.origins.try_reserve_exact(count)?;
            build.planned_origins = None;
        }
        if !build.copy(work, cancel)? {
            return Ok(LaneStep::Yield);
        }
        let range = build.unit().range();
        let anchor_offset = build.unit().anchor_offset();
        while let Some(piece) = self.pieces.as_slice().get(build.origin) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(LaneStep::Yield);
            }
            let start = range.start.max(piece.range.start);
            let end = range.end.min(piece.range.end);
            if start < end {
                build.origins.try_reserve_exact(1)?;
                build
                    .origins
                    .try_push(PayloadSource {
                        elementary_range: start..end,
                        payload_range: usize::try_from(start - piece.range.start)
                            .map_err(|_| Error::Overflow)?
                            ..usize::try_from(end - piece.range.start)
                                .map_err(|_| Error::Overflow)?,
                        pes: piece.source.clone(),
                    })
                    .map_err(|e| e.error)?;
            }
            if piece.range.start <= anchor_offset && piece.serial > build.last_anchor {
                let pes = piece.source.pes();
                let policy = if piece.clock.quarantined {
                    SourceTimePolicy::Unanchored
                } else {
                    self.limits.time_policy
                };
                build.clock = Some(PacketClock {
                    key: piece.clock.key,
                    quarantined: piece.clock.quarantined,
                    pts: source_timestamp(pes.header.pts_raw33, piece.clock.key, policy)?,
                    dts: source_timestamp(pes.header.dts_raw33, piece.clock.key, policy)?,
                    pes: piece.source.clone(),
                });
                build.last_anchor = piece.serial;
            }
            build.origin += 1;
        }
        while let Some(piece) = self.pieces.as_slice().get(build.retire) {
            if piece.range.end > range.end || piece.serial > build.last_anchor {
                break;
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(LaneStep::Yield);
            }
            build.retire += 1;
        }
        if !work.try_charge(1) {
            return Ok(LaneStep::Yield);
        }
        let charge = Shared::<PacketData>::reserve(&self.pool)?;
        let duration = build.unit().duration()?;
        let duration90 = duration
            .map(|time| -> Result<Option<i64>> {
                let n = time
                    .numerator()
                    .checked_mul(90_000)
                    .ok_or(Error::Overflow)?;
                let d = i128::try_from(time.denominator()).map_err(|_| Error::Overflow)?;
                if n % d == 0 {
                    Ok(Some(i64::try_from(n / d).map_err(|_| Error::Overflow)?))
                } else {
                    Ok(None)
                }
            })
            .transpose()?
            .flatten();
        let pts = build
            .clock
            .as_ref()
            .and_then(|clock| clock.pts.ticks90)
            .map(i64::try_from)
            .transpose()
            .map_err(|_| Error::Overflow)?;
        let dts = build
            .clock
            .as_ref()
            .and_then(|clock| clock.dts.ticks90)
            .map(i64::try_from)
            .transpose()
            .map_err(|_| Error::Overflow)?;
        let epochs = build
            .origins
            .as_slice()
            .first()
            .ok_or(Error::Invalid("MPEG-TS AU has no source extent"))?
            .pes
            .pes()
            .epochs;
        let build = self.build.take().expect("prepared publication");
        self.retire = build.retire;
        self.last_anchor = build.last_anchor;
        self.completed += 1;
        self.published = range.end;
        Ok(LaneStep::Packet(Packet(Shared::new(
            PacketData {
                track: self.track.clone(),
                unit: build.unit.expect("prepared unit"),
                bytes: build.bytes.expect("prepared bytes"),
                sources: build.origins,
                clock: build.clock,
                epochs,
                duration,
                pts,
                dts,
                duration90,
            },
            charge,
        ))))
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod incomplete_audio_tests {
    use super::*;
    use crate::{Reader, ReaderEvent};
    use triergon_core::container::{Event, PacketInfo};

    const HEADER: [u8; 7] = [0xff, 0xf1, 0x4c, 0x80, 3, 0x7f, 0xfc];

    fn transport() -> Vec<u8> {
        // Keep the authored fixture's PAT, PMT and PES header. Only the finite
        // PES payload is replaced with an ADTS header declaring 27 bytes.
        let source = include_bytes!("../../tests/data/carriage-aac-adts-direct.ts");
        let mut raw = source[..376].to_vec();
        let mut packet = [0xff; 188];
        packet[..4].copy_from_slice(&source[376..380]);
        packet[4] = 162;
        packet[5] = 0;
        packet[167..181].copy_from_slice(&source[523..537]);
        packet[171..173].copy_from_slice(&15_u16.to_be_bytes());
        packet[181..].copy_from_slice(&HEADER);
        raw.extend_from_slice(&packet);
        raw
    }

    fn split_transport() -> Vec<u8> {
        let source = include_bytes!("../../tests/data/carriage-aac-adts-direct.ts");
        let mut raw = source[..376].to_vec();
        for (counter, byte) in HEADER.iter().enumerate() {
            let mut packet = [0xff; 188];
            packet[..4].copy_from_slice(&source[376..380]);
            packet[3] = 0x30 | u8::try_from(counter).unwrap();
            packet[4] = 168;
            packet[5] = 0;
            packet[173..187].copy_from_slice(&source[523..537]);
            packet[177..179].copy_from_slice(&9_u16.to_be_bytes());
            packet[187] = *byte;
            raw.extend_from_slice(&packet);
        }
        raw
    }

    fn reader(pool: &Budget) -> Reader {
        Reader::new(
            pool,
            SourceId(309),
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            ReaderLimits {
                audio_eof_policy: AudioEofPolicy::ReportIncomplete,
                ..ReaderLimits::default()
            },
            4096,
        )
        .unwrap()
    }

    fn checkpoint(reader: &Reader) -> Option<usize> {
        if reader.storage.is_some() {
            return Some(4);
        }
        reader.lanes.as_slice().iter().find_map(|lane| {
            if let Some(copy) = &lane.incomplete {
                if !copy.prepared && copy.piece == lane.pieces.len() {
                    return Some(0);
                }
                if copy.prepared && copy.piece == lane.pieces.len() && copy.bytes.is_none() {
                    return Some(1);
                }
            }
            let build = lane.build.as_ref()?;
            if !matches!(build.unit(), AccessUnit::IncompleteAudio { .. }) {
                return None;
            }
            if build.planned_origins.is_some() {
                return Some(2);
            }
            (build.origin == lane.pieces.len()).then_some(3)
        })
    }

    fn provide(reader: &mut Reader, pool: &Budget, raw: &[u8], request: InputRequest) {
        let at = usize::try_from(request.offset).unwrap();
        if at == raw.len() {
            reader.finish(request.source, request.token).unwrap();
        } else {
            let end = raw.len().min(at + request.maximum.min(7));
            let bytes = Bytes::copy_from_slice(pool, &raw[at..end]).unwrap();
            reader
                .provide(
                    &InputWindow::new(request.source, request.token, request.offset, bytes)
                        .unwrap(),
                )
                .unwrap();
        }
    }

    #[test]
    fn fragment_copy_freeze_origins_and_publication_retry_transactionally() {
        let raw = transport();
        let pool = Budget::new(32 << 20).unwrap();
        let mut reader = reader(&pool);
        let cancel = AtomicBool::new(false);
        let mut reached = 0;
        let mut fragment = None;
        let mut ended = false;
        for _ in 0..1_000_000 {
            if reached < 4 && checkpoint(&reader) == Some(reached) {
                let pressure = pool.reserve(pool.available()).unwrap();
                let mut refused = false;
                for _ in 0..10_000 {
                    match reader.step(&mut WorkBudget::new(1), &cancel).unwrap() {
                        Step::Yield => (),
                        Step::Backpressure(_) => {
                            refused = true;
                            break;
                        }
                        other => panic!("fragment advanced under pressure: {other:?}"),
                    }
                }
                assert!(refused, "checkpoint {reached}");
                drop(pressure);
                reached += 1;
            }
            match reader.step(&mut WorkBudget::new(1), &cancel).unwrap() {
                Step::NeedInput(request) => provide(&mut reader, &pool, &raw, request),
                Step::Ready(Event::Other(ReaderEvent::IncompleteAudio(packet))) => {
                    assert!(fragment.is_none());
                    assert_eq!(reached, 4);
                    assert_eq!(packet.payload(), HEADER);
                    assert_eq!(packet.elementary_range(), 0..7);
                    assert_eq!(packet.sources().len(), 1);
                    assert_eq!(packet.sources()[0].payload_range, 0..7);
                    assert_eq!(packet.pts(), Some(90_000));
                    fragment = Some(packet);
                }
                Step::Ready(Event::Packet(_)) => panic!("unfinished audio is not a packet"),
                Step::Ready(_) | Step::Yield => (),
                Step::End(_) => {
                    ended = true;
                    break;
                }
                other @ Step::Backpressure(_) => panic!("fragment retry failed: {other:?}"),
            }
        }
        assert!(ended);
        reader.close();
        assert_eq!(fragment.as_ref().unwrap().payload(), HEADER);
        drop(fragment);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }

    #[test]
    fn cancellation_at_each_fragment_allocation_precedes_zero_work_and_releases_storage() {
        let raw = transport();
        for target in 0..5 {
            let pool = Budget::new(32 << 20).unwrap();
            let mut reader = reader(&pool);
            let cancel = AtomicBool::new(false);
            let mut reached = false;
            for _ in 0..1_000_000 {
                if checkpoint(&reader) == Some(target) {
                    cancel.store(true, Ordering::Relaxed);
                    assert_eq!(
                        reader.step(&mut WorkBudget::new(0), &cancel).unwrap_err(),
                        Error::Cancelled
                    );
                    assert_eq!(pool.used(), Budget::control_storage_bytes());
                    reader.reset().unwrap();
                    reader.close();
                    assert_eq!(pool.used(), Budget::control_storage_bytes());
                    reached = true;
                    break;
                }
                match reader.step(&mut WorkBudget::new(1), &cancel).unwrap() {
                    Step::NeedInput(request) => provide(&mut reader, &pool, &raw, request),
                    Step::Ready(
                        Event::Packet(_) | Event::Other(ReaderEvent::IncompleteAudio(_)),
                    )
                    | Step::End(_) => panic!("fragment cancellation checkpoint not reached"),
                    Step::Ready(_) | Step::Yield => (),
                    other @ Step::Backpressure(_) => {
                        panic!("fragment cancellation failed: {other:?}")
                    }
                }
            }
            assert!(reached, "checkpoint {target}");
        }
    }

    #[test]
    fn fragment_storage_audit_counts_all_reader_owners_and_excludes_external_reservations() {
        let raw = split_transport();
        for chunk in [7, 4096] {
            let pool = Budget::new(32 << 20).unwrap();
            let mut reader = reader(&pool);
            let cancel = AtomicBool::new(false);
            let external = pool.reserve(19_937).unwrap();
            let mut reached = false;
            for _ in 0..1_000_000 {
                if let Some(lane) = reader.lanes.as_slice().iter().find(|lane| {
                    lane.incomplete
                        .as_ref()
                        .is_some_and(|copy| copy.reader_storage.is_some())
                }) {
                    let bytes = lane.incomplete.as_ref().unwrap().reader_storage.unwrap();
                    assert_eq!(bytes, pool.used() - external.bytes());
                    assert_eq!(lane.pieces.len(), 7);
                    if chunk == 7 {
                        let owners: Vec<_> = lane
                            .pieces
                            .as_slice()
                            .iter()
                            .map(|piece| piece.source.pes().spans()[0].backing_bytes())
                            .collect();
                        assert!(owners.iter().enumerate().all(|(i, owner)| {
                            owners[..i]
                                .iter()
                                .all(|prior| !prior.shares_storage_with(owner))
                        }));
                    }
                    reached = true;
                    break;
                }
                match reader.step(&mut WorkBudget::new(1), &cancel).unwrap() {
                    Step::NeedInput(request) => {
                        let at = usize::try_from(request.offset).unwrap();
                        if at == raw.len() {
                            reader.finish(request.source, request.token).unwrap();
                        } else {
                            let end = raw.len().min(at + request.maximum.min(chunk));
                            let bytes = Bytes::copy_from_slice(&pool, &raw[at..end]).unwrap();
                            reader
                                .provide(
                                    &InputWindow::new(
                                        request.source,
                                        request.token,
                                        request.offset,
                                        bytes,
                                    )
                                    .unwrap(),
                                )
                                .unwrap();
                        }
                    }
                    Step::Ready(Event::Other(ReaderEvent::IncompleteAudio(_))) | Step::End(_) => {
                        panic!("audit not observed")
                    }
                    Step::Ready(_) | Step::Yield => (),
                    other @ Step::Backpressure(_) => panic!("audit refused: {other:?}"),
                }
            }
            assert!(reached);
            reader.close();
            assert_eq!(
                pool.used(),
                Budget::control_storage_bytes() + external.bytes()
            );
            drop(external);
            assert_eq!(pool.used(), Budget::control_storage_bytes());
        }
    }

    fn tight_split_run(limit: usize, profile: bool, external_pressure: bool) -> (usize, usize) {
        let raw = split_transport();
        let pool = Budget::new(limit).unwrap();
        let mut reader = reader(&pool);
        let cancel = AtomicBool::new(false);
        let mut baseline = None;
        let mut minimum = 0;
        let mut sources = 0;
        let mut fragments = 0;
        let mut pressure = None;
        let mut injected = false;
        let mut retried = false;
        for _ in 0..1_000_000 {
            if baseline.is_none()
                && let Some(storage) = reader
                    .lanes
                    .as_slice()
                    .iter()
                    .find_map(|lane| lane.incomplete.as_ref()?.reader_storage)
            {
                baseline = Some(pool.peak());
                minimum = storage
                    + HEADER.len()
                    + Bytes::owner_overhead_bytes()
                    + 7 * size_of::<PayloadSource>()
                    + Shared::<PacketData>::allocation_bytes().unwrap();
                if profile {
                    assert_eq!(sources, 7);
                    return (baseline.unwrap(), minimum);
                }
            }
            if external_pressure && baseline.is_some() && !injected {
                pressure = Some(pool.reserve(pool.available()).unwrap());
                injected = true;
            }
            let mut work = WorkBudget::new(1);
            let step = reader.step(&mut work, &cancel);
            assert!(work.spent() <= 1);
            match step {
                Ok(Step::NeedInput(request)) => provide(&mut reader, &pool, &raw, request),
                Ok(Step::Ready(Event::Other(ReaderEvent::Source(source)))) => {
                    if matches!(source.event(), crate::TraceEvent::Pes(_)) {
                        sources += 1;
                    }
                    // Every original source event is discarded here.
                }
                Ok(Step::Ready(Event::Other(ReaderEvent::IncompleteAudio(packet)))) => {
                    assert_eq!(packet.payload(), HEADER);
                    assert_eq!(packet.sources().len(), 7);
                    fragments += 1;
                    // No returned packet or fragment remains held between steps.
                }
                Ok(Step::Backpressure(_)) if pressure.is_some() => {
                    drop(pressure.take());
                    retried = true;
                }
                Ok(Step::Backpressure(value)) => {
                    panic!("permanent EOF pressure with no held output at {limit}: {value:?}")
                }
                Ok(Step::Ready(Event::Packet(_))) => panic!("fragment is not an AU"),
                Ok(Step::Ready(_) | Step::Yield) => (),
                Ok(Step::End(_)) => {
                    assert_eq!(sources, 7);
                    assert_eq!(fragments, 1);
                    assert!(limit >= minimum);
                    assert_eq!(retried, external_pressure);
                    return (baseline.unwrap(), minimum);
                }
                Err(Error::Limit { requested, maximum }) => {
                    assert_eq!(sources, 7);
                    assert_eq!(fragments, 0);
                    assert_eq!(requested, minimum);
                    assert_eq!(maximum, limit);
                    assert!(minimum > limit);
                    return (baseline.unwrap(), minimum);
                }
                Err(error) => panic!("tight EOF failed at {limit}: {error:?}"),
            }
        }
        panic!("tight EOF did not terminate at {limit}");
    }

    #[test]
    fn seven_byte_adts_header_in_seven_validated_pes_terminates_across_tight_budgets() {
        let (prefix_peak, minimum) = tight_split_run(32 << 20, true, false);
        for limit in prefix_peak..=prefix_peak.max(minimum) + 32 {
            assert_eq!(tight_split_run(limit, false, false), (prefix_peak, minimum));
        }
        tight_split_run(prefix_peak.max(minimum) + 1, false, true);
    }
}
