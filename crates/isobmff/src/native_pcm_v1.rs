//! Explicit D32 QuickTime v0 PCM catalogue and chunk-packet observation view.
//! Original source descriptions, sample ranges and unit decode clocks survive.
//! This is neither a replacement source Sample nor an audio decoder.
use crate::{
    Access, BoxChildren, BoxStep, BoxView, Limits, Mov9CursorStatus, Mov9MediaClock,
    Mov9StreamView, QtPcmEncodingV0, QtSoundDescriptionV0, Sample, SampleCursor, SampleStep,
    SampleTable, TrackHeader, fields,
    tables::{Offsets, Words},
};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputToken, Result, SourceId, Step, WorkBudget,
};

/// Separate, explicit interpretation of D32 packet observations.
#[derive(Clone, Copy, Debug)]
pub struct PinnedMovD32PcmV1;

#[derive(Clone, Copy, Debug)]
pub struct Mov9PcmLimitsV1 {
    pub max_samples: u64,
    pub max_packets: u64,
    pub max_packet_bytes: usize,
    pub max_metadata_rows: u32,
}
impl Default for Mov9PcmLimitsV1 {
    fn default() -> Self {
        Self {
            max_samples: 1_000_000,
            max_packets: 1_000_000,
            max_packet_bytes: 16 * 1024 * 1024,
            max_metadata_rows: 1_000_000,
        }
    }
}
fn bounded(value: u64, maximum: u64) -> Result<()> {
    if value > maximum {
        return Err(Error::Limit {
            requested: usize::try_from(value).map_err(|_| Error::Overflow)?,
            maximum: usize::try_from(maximum).map_err(|_| Error::Overflow)?,
        });
    }
    Ok(())
}
fn header_binding(header: &TrackHeader) -> Result<()> {
    let raw = TrackHeader::parse(&header.raw_tkhd, &header.raw_mdhd, &header.raw_hdlr)?;
    if (
        raw.track_id,
        raw.timescale,
        raw.duration,
        raw.handler,
        raw.enabled,
    ) != (
        header.track_id,
        header.timescale,
        header.duration,
        header.handler,
        header.enabled,
    ) {
        return Err(Error::Invalid("PCM track header differs from raw source"));
    }
    Ok(())
}

/// Initial native catalogue and source entry are independent retained facts.
#[derive(Clone, Debug)]
pub struct Mov9PcmStreamV1 {
    native: Mov9StreamView,
    initial: QtSoundDescriptionV0,
}
impl Mov9PcmStreamV1 {
    pub fn new(
        header: &TrackHeader,
        table: &SampleTable,
        major_brand: Option<[u8; 4]>,
        _profile: PinnedMovD32PcmV1,
    ) -> Result<Self> {
        header_binding(header)?;
        if major_brand != Some(*b"qt  ") {
            return Err(Error::Unsupported(
                "PCM native view needs QuickTime major brand",
            ));
        }
        let entry = table
            .descriptions()
            .first()
            .ok_or(Error::Invalid("PCM initial description absent"))?;
        let initial = QtSoundDescriptionV0::parse(header, entry)?;
        let layout = initial.pcm_layout()?;
        if initial.revision() != 0 || initial.vendor() != [0; 4] || entry.data_reference_index != 1
        {
            return Err(Error::Unsupported(
                "PCM unobserved revision/vendor/data reference",
            ));
        }
        let clock = Mov9MediaClock::from_source(header.time_base()?)?;
        let b = header.raw_mdhd.payload();
        let language_at = match fields::full(b)?.0 {
            0 => 20,
            1 => 32,
            _ => return Err(Error::Unsupported("PCM mdhd version")),
        };
        let packed = fields::u16_at(b, language_at)?;
        let mut language = [0; 3];
        for (i, item) in language.iter_mut().enumerate() {
            let word = (packed >> (10 - 5 * i)) & 31;
            if !(1..=26).contains(&word) || packed & 0x8000 != 0 {
                return Err(Error::Unsupported("PCM unobserved media language"));
            }
            *item = u8::try_from(word).map_err(|_| Error::Overflow)? + 0x60;
        }
        let b = header.raw_hdlr.payload();
        if fields::u32_at(b, 4)? != 0 || fields::slice(b, 12, 12)? != [0; 12] {
            return Err(Error::Unsupported("PCM unobserved handler layout"));
        }
        let tail = b.get(24..).ok_or(Error::Truncated)?;
        if tail.len() > 256 {
            return Err(Error::Limit {
                requested: tail.len(),
                maximum: 256,
            });
        }
        let length = tail
            .iter()
            .position(|x| *x == 0)
            .ok_or(Error::Unsupported("PCM handler name terminator"))?;
        if !tail[..length].iter().all(u8::is_ascii) {
            return Err(Error::Unsupported("PCM unobserved handler encoding"));
        }
        let start = usize::from(header.raw_hdlr.header().header_size)
            .checked_add(24)
            .ok_or(Error::Overflow)?;
        let end = entry.raw().bytes().len();
        Ok(Self {
            native: Mov9StreamView {
                track_id: header.track_id,
                codec: match layout.encoding() {
                    QtPcmEncodingV0::Unsigned8 => "pcm_u8",
                    QtPcmEncodingV0::Signed16BigEndian => "pcm_s16be",
                },
                media_type: 1,
                time_base: clock.native_base,
                clock,
                extradata: entry.raw().bytes().slice(end..end)?,
                language,
                handler_name: header
                    .raw_hdlr
                    .bytes()
                    .slice(start..start.checked_add(length).ok_or(Error::Overflow)?)?,
                vendor_id: Some([0; 4]),
            },
            initial,
        })
    }
    pub fn native(&self) -> &Mov9StreamView {
        &self.native
    }
    pub fn initial(&self) -> &QtSoundDescriptionV0 {
        &self.initial
    }
}

/// One native request with its complete source interval and both descriptions.
/// A request may overlap another source chunk; its source interval never expands.
#[derive(Clone, Debug)]
pub struct Mov9PcmPacketV1 {
    initial: QtSoundDescriptionV0,
    description: QtSoundDescriptionV0,
    raw_table: BoxView,
    samples: Range<u64>,
    source_range: Range<u64>,
    native_range: Range<u64>,
    media: Range<u64>,
    ordinal: u64,
    dts: i64,
    duration: u32,
    pool: Budget,
    source: SourceId,
    generation: InputToken,
}
impl Mov9PcmPacketV1 {
    pub fn source_description(&self) -> &QtSoundDescriptionV0 {
        &self.description
    }
    pub fn initial_description(&self) -> &QtSoundDescriptionV0 {
        &self.initial
    }
    pub fn source_table(&self) -> &BoxView {
        &self.raw_table
    }
    pub fn source_samples(&self) -> Range<u64> {
        self.samples.clone()
    }
    pub fn source_range(&self) -> &Range<u64> {
        &self.source_range
    }
    pub fn native_range(&self) -> &Range<u64> {
        &self.native_range
    }
    pub fn native_ordinal(&self) -> u64 {
        self.ordinal
    }
    pub fn dts(&self) -> i64 {
        self.dts
    }
    pub fn pts(&self) -> i64 {
        self.dts
    }
    pub fn duration(&self) -> u32 {
        self.duration
    }
    pub fn flags(&self) -> u32 {
        1
    }
    fn bind(&self, input: &InputCursor) -> Result<()> {
        bind(input, &self.pool, self.source, &self.generation)
    }
}

fn bind(
    input: &InputCursor,
    pool: &Budget,
    source: SourceId,
    generation: &InputToken,
) -> Result<()> {
    if !input.budget().same_pool(pool) {
        return Err(Error::ForeignPool);
    }
    if input.source() != source {
        return Err(Error::SourceMismatch);
    }
    let current = input.token();
    if current.generation() != generation.generation() {
        return Err(Error::StaleGeneration {
            expected: generation.generation(),
            received: current.generation(),
        });
    }
    Ok(())
}
struct Group {
    count: u32,
    consumed: u32,
    first: u64,
    source_range: Range<u64>,
    native_range: Range<u64>,
    description: QtSoundDescriptionV0,
}

/// Source-table traversal and grouping are work bounded and allocate no index.
/// Each original source sample is validated; packet requests use the initial
/// native frame layout independently. Legitimate payload range requests do not
/// reset this metadata cursor; a source generation change always invalidates it.
pub struct Mov9PcmCursorV1<'a> {
    header: &'a TrackHeader,
    table: &'a SampleTable,
    source_cursor: SampleCursor<'a>,
    children: Option<BoxChildren>,
    stsc: Option<Words>,
    offsets: Option<Offsets>,
    initial: QtSoundDescriptionV0,
    media: Range<u64>,
    limits: Mov9PcmLimitsV1,
    pool: Budget,
    source: SourceId,
    generation: InputToken,
    chunk: u32,
    stsc_row: u32,
    chunk_left: u32,
    chunk_used: u32,
    source_done: u64,
    generated: u64,
    group: Option<Group>,
    pending: Option<Sample>,
    ready: Option<Mov9PcmPacketV1>,
    failed: Option<Error>,
    ended: bool,
}
impl<'a> Mov9PcmCursorV1<'a> {
    pub fn new(
        header: &'a TrackHeader,
        table: &'a SampleTable,
        input: &InputCursor,
        media: Range<u64>,
        limits: Mov9PcmLimitsV1,
        _profile: PinnedMovD32PcmV1,
    ) -> Result<Self> {
        if !table.raw().bytes().belongs_to(input.budget())
            || [&header.raw_tkhd, &header.raw_mdhd, &header.raw_hdlr]
                .iter()
                .any(|raw| !raw.bytes().belongs_to(input.budget()))
        {
            return Err(Error::ForeignPool);
        }
        bounded(table.sample_count(), limits.max_samples)?;
        header_binding(header)?;
        if media.start > media.end {
            return Err(Error::Invalid("PCM media extent"));
        }
        let entry = table
            .descriptions()
            .first()
            .ok_or(Error::Invalid("PCM initial description absent"))?;
        let initial = QtSoundDescriptionV0::parse(header, entry)?;
        initial.pcm_layout()?;
        Ok(Self {
            header,
            table,
            source_cursor: table.cursor(),
            children: Some(table.raw().children(0, Limits::default())?),
            stsc: None,
            offsets: None,
            initial,
            media,
            limits,
            pool: input.budget().clone(),
            source: input.source(),
            generation: input.token(),
            chunk: 0,
            stsc_row: 0,
            chunk_left: 0,
            chunk_used: 0,
            source_done: 0,
            generated: 0,
            group: None,
            pending: None,
            ready: None,
            failed: None,
            ended: false,
        })
    }
    fn check(
        &self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<()> {
        bind(input, &self.pool, self.source, &self.generation)?;
        input.read_into(&mut [], work, cancel)?;
        Ok(())
    }
    fn poison(&mut self, error: Error) -> Error {
        self.failed = Some(error);
        self.ready = None;
        self.group = None;
        self.pending = None;
        self.children = None;
        self.stsc = None;
        self.offsets = None;
        error
    }
    pub fn poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Mov9CursorStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.inner(input, work, cancel);
        result.map_err(|e| self.poison(e))
    }
    fn inner(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Mov9CursorStatus> {
        self.check(input, work, cancel)?;
        if self.ready.is_some() {
            return Ok(Mov9CursorStatus::Ready);
        }
        if self.ended {
            return Ok(Mov9CursorStatus::End);
        }
        loop {
            if work.remaining() == 0 {
                return Ok(Mov9CursorStatus::Yield);
            }
            if let Some(children) = &mut self.children {
                let limits = Limits {
                    max_records: self.limits.max_metadata_rows,
                    ..Limits::default()
                };
                match children.poll(work)? {
                    BoxStep::Yield => return Ok(Mov9CursorStatus::Yield),
                    BoxStep::Box(raw) => match &raw.header().kind {
                        b"stsc" => {
                            if self.stsc.is_some() {
                                return Err(Error::Invalid("PCM duplicate stsc"));
                            }
                            self.stsc = Some(Words::parse(raw, 12, limits)?);
                        }
                        b"stco" | b"co64" => {
                            if self.offsets.is_some() {
                                return Err(Error::Invalid("PCM duplicate chunk offsets"));
                            }
                            self.offsets = Some(Offsets::parse(raw, limits)?);
                        }
                        b"ctts" | b"stss" | b"stps" | b"sdtp" => {
                            return Err(Error::Unsupported(
                                "PCM unobserved composition/access table",
                            ));
                        }
                        _ => (),
                    },
                    BoxStep::End => {
                        if self.stsc.is_none() || self.offsets.is_none() {
                            return Err(Error::Invalid("PCM chunk tables absent"));
                        }
                        self.children = None;
                    }
                }
                continue;
            }
            if self.group.is_none() {
                if !work.try_charge(1) {
                    return Ok(Mov9CursorStatus::Yield);
                }
                if self.source_done == self.table.sample_count() {
                    if self.chunk_left != 0
                        || self.chunk
                            != self
                                .offsets
                                .as_ref()
                                .ok_or(Error::Invalid("PCM offsets absent"))?
                                .table
                                .count
                    {
                        return Err(Error::Invalid("PCM incomplete source chunk accounting"));
                    }
                    self.ended = true;
                    return Ok(Mov9CursorStatus::End);
                }
                self.start_group()?;
                continue;
            }
            if self.pending.is_none() {
                match self.source_cursor.poll(work)? {
                    SampleStep::Yield => return Ok(Mov9CursorStatus::Yield),
                    SampleStep::End => {
                        return Err(Error::Invalid("PCM source sample count ended early"));
                    }
                    SampleStep::Sample(sample) => self.pending = Some(sample),
                }
            }
            if !work.try_charge(1) {
                return Ok(Mov9CursorStatus::Yield);
            }
            let sample = self
                .pending
                .take()
                .ok_or(Error::Invalid("PCM pending source sample absent"))?;
            sample.require_within(self.media.clone())?;
            let group = self
                .group
                .as_mut()
                .ok_or(Error::Invalid("PCM group absent"))?;
            let expected = self.source_done.checked_add(1).ok_or(Error::Overflow)?;
            let frame = u64::try_from(group.description.pcm_layout()?.frame_bytes())
                .map_err(|_| Error::Overflow)?;
            if sample.ordinal != expected
                || sample.description_index != group.description.source().index
                || sample.duration_ticks != 1
                || sample.dts_ticks != Some(i128::from(self.source_done))
                || sample.cts_ticks != sample.dts_ticks
                || sample.composition_offset.is_some()
                || sample.access != Access::ContainerSync
                || sample.partial_sync
                || sample.dependency.is_some()
                || sample.fragment_flags.is_some()
                || sample.range.end.checked_sub(sample.range.start) != Some(frame)
                || (group.consumed != 0 && sample.range.start != group.source_range.end)
            {
                return Err(Error::Unsupported(
                    "PCM native grouping needs complete unit source frames",
                ));
            }
            if group.consumed == 0 {
                group.source_range.start = sample.range.start;
            }
            group.source_range.end = sample.range.end;
            group.consumed = group.consumed.checked_add(1).ok_or(Error::Overflow)?;
            self.source_done = expected;
            if group.consumed != group.count {
                continue;
            }
            let group = self
                .group
                .take()
                .ok_or(Error::Invalid("PCM complete group absent"))?;
            self.chunk_left = self
                .chunk_left
                .checked_sub(group.count)
                .ok_or(Error::Overflow)?;
            self.chunk_used = self
                .chunk_used
                .checked_add(group.count)
                .ok_or(Error::Overflow)?;
            let ordinal = self.generated;
            self.generated = self.generated.checked_add(1).ok_or(Error::Overflow)?;
            self.ready = Some(Mov9PcmPacketV1 {
                initial: self.initial.clone(),
                description: group.description,
                raw_table: self.table.raw().clone(),
                samples: group.first..expected.checked_add(1).ok_or(Error::Overflow)?,
                source_range: group.source_range,
                native_range: group.native_range,
                media: self.media.clone(),
                ordinal,
                dts: i64::try_from(group.first - 1).map_err(|_| Error::Overflow)?,
                duration: group.count,
                pool: self.pool.clone(),
                source: self.source,
                generation: self.generation.clone(),
            });
            return Ok(Mov9CursorStatus::Ready);
        }
    }
    fn start_group(&mut self) -> Result<()> {
        bounded(
            self.generated.checked_add(1).ok_or(Error::Overflow)?,
            self.limits.max_packets,
        )?;
        let stsc = self
            .stsc
            .as_ref()
            .ok_or(Error::Invalid("PCM stsc absent"))?;
        let offsets = self
            .offsets
            .as_ref()
            .ok_or(Error::Invalid("PCM offsets absent"))?;
        if self.chunk_left == 0 {
            if self.chunk >= offsets.table.count {
                return Err(Error::Invalid("PCM chunk count exhausted"));
            }
            self.chunk = self.chunk.checked_add(1).ok_or(Error::Overflow)?;
            let next = self.stsc_row.checked_add(1).ok_or(Error::Overflow)?;
            if next < stsc.count && stsc.word(next, 0)? == self.chunk {
                self.stsc_row = next;
            }
            self.chunk_left = stsc.word(self.stsc_row, 4)?;
            self.chunk_used = 0;
        }
        let count = self.chunk_left.min(1024);
        if count == 0 {
            return Err(Error::Invalid("PCM empty chunk group"));
        }
        let index = stsc.word(self.stsc_row, 8)?;
        let entry = self
            .table
            .descriptions()
            .get(
                usize::try_from(
                    index
                        .checked_sub(1)
                        .ok_or(Error::Invalid("PCM zero description"))?,
                )
                .map_err(|_| Error::Overflow)?,
            )
            .ok_or(Error::Invalid("PCM description absent"))?;
        let description = QtSoundDescriptionV0::parse(self.header, entry)?;
        description.pcm_layout()?;
        let initial_bytes =
            u64::try_from(self.initial.pcm_layout()?.frame_bytes()).map_err(|_| Error::Overflow)?;
        let start = offsets
            .get(self.chunk - 1)?
            .checked_add(
                u64::from(self.chunk_used)
                    .checked_mul(initial_bytes)
                    .ok_or(Error::Overflow)?,
            )
            .ok_or(Error::Overflow)?;
        let length = u64::from(count)
            .checked_mul(initial_bytes)
            .ok_or(Error::Overflow)?;
        bounded(
            length,
            u64::try_from(self.limits.max_packet_bytes).map_err(|_| Error::Overflow)?,
        )?;
        let end = start.checked_add(length).ok_or(Error::Overflow)?;
        // This bounded view does not guess a short native request outside media.
        if start < self.media.start || end > self.media.end {
            return Err(Error::Unsupported(
                "PCM native request outside established media",
            ));
        }
        self.group = Some(Group {
            count,
            consumed: 0,
            first: self.source_done.checked_add(1).ok_or(Error::Overflow)?,
            source_range: 0..0,
            native_range: start..end,
            description,
        });
        Ok(())
    }
    pub fn take_packet(
        &mut self,
        input: &mut InputCursor,
        cancel: &AtomicBool,
    ) -> Result<Mov9PcmPacketV1> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = (|| {
            self.check(input, &mut WorkBudget::new(0), cancel)?;
            self.ready
                .take()
                .ok_or(Error::Invalid("PCM packet not ready"))
        })();
        result.map_err(|e| self.poison(e))
    }
}

#[derive(Clone, Debug)]
pub struct Mov9PcmPayloadV1 {
    plan: Mov9PcmPacketV1,
    payload: Bytes,
    source: SourceId,
    token: InputToken,
}
impl Mov9PcmPayloadV1 {
    pub fn plan(&self) -> &Mov9PcmPacketV1 {
        &self.plan
    }
    pub fn payload(&self) -> &Bytes {
        &self.payload
    }
    pub fn source(&self) -> SourceId {
        self.source
    }
    pub fn token(&self) -> &InputToken {
        &self.token
    }
}

/// Actual charged physical read; never constructs a replacement source sample.
/// Refusals, cancellation, foreign pools and stale transactions stay core errors.
#[derive(Debug)]
pub struct Mov9PcmReaderV1 {
    plan: Mov9PcmPacketV1,
    token: InputToken,
    position: u64,
    length: usize,
    buffer: Option<BudgetVec<u8>>,
    freeze_paid: bool,
    failed: Option<Error>,
}
impl Mov9PcmReaderV1 {
    pub fn new(
        input: &mut InputCursor,
        plan: Mov9PcmPacketV1,
        maximum_packet: usize,
    ) -> Result<Self> {
        plan.bind(input)?;
        let range = &plan.native_range;
        let length = usize::try_from(range.end.checked_sub(range.start).ok_or(Error::Overflow)?)
            .map_err(|_| Error::Overflow)?;
        if length > maximum_packet {
            return Err(Error::Limit {
                requested: length,
                maximum: maximum_packet,
            });
        }
        if length == 0 || range.start < plan.media.start || range.end > plan.media.end {
            return Err(Error::Invalid("PCM physical request extent"));
        }
        let physical = input
            .capabilities()
            .length
            .ok_or(Error::Unsupported("PCM physical length unknown"))?;
        if range.end > physical || !input.capabilities().random_access {
            return Err(Error::Unsupported("PCM physical source availability"));
        }
        let mut buffer = BudgetVec::new(input.budget());
        buffer.try_reserve_exact(length)?;
        input.request_range(range.start)?;
        Ok(Self {
            token: input.token(),
            position: range.start,
            length,
            plan,
            buffer: Some(buffer),
            freeze_paid: false,
            failed: None,
        })
    }
    pub fn poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<Mov9PcmPayloadV1>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.inner(input, work, cancel);
        if let Err(error) = result
            && !matches!(error, Error::Backpressure { .. })
        {
            self.buffer = None;
            self.failed = Some(error);
        }
        result
    }
    fn inner(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<Mov9PcmPayloadV1>> {
        self.plan.bind(input)?;
        let current = input.token();
        if current != self.token {
            return Err(Error::StaleRequest {
                expected: self.token.request(),
                received: current.request(),
            });
        }
        if input.position() != self.position {
            return Err(Error::Invalid("PCM physical input moved outside reader"));
        }
        input.read_into(&mut [], work, cancel)?;
        if work.remaining() == 0 {
            return Ok(Step::Yield);
        }
        loop {
            let buffer = self
                .buffer
                .as_mut()
                .ok_or(Error::Invalid("PCM physical reader finished"))?;
            if buffer.len() == self.length {
                if !self.freeze_paid {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    self.freeze_paid = true;
                }
                let buffer = self
                    .buffer
                    .take()
                    .ok_or(Error::Invalid("PCM physical buffer absent"))?;
                let payload = match buffer.freeze() {
                    Ok(b) => b,
                    Err(failure) => {
                        self.buffer = Some(failure.buffer);
                        return Err(failure.error);
                    }
                };
                return Ok(Step::Ready(Mov9PcmPayloadV1 {
                    plan: self.plan.clone(),
                    payload,
                    source: input.source(),
                    token: self.token.clone(),
                }));
            }
            let remaining = self.length - buffer.len();
            match input.read_append(buffer, remaining, work, cancel)? {
                Step::Ready(_) => self.position = input.position(),
                Step::NeedInput(r) => return Ok(Step::NeedInput(r)),
                Step::Yield => return Ok(Step::Yield),
                Step::End(_) => return Err(Error::Truncated),
            }
        }
    }
}
