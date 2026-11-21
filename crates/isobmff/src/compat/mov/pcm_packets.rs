//! QuickTime version 0 PCM as FFmpeg's MOV demuxer delivers it: the stream entry
//! (`PcmStream`), the packets of up to 1024 samples of one chunk (`PcmCursor`,
//! `PcmPacket`) and their bytes (`PcmReader`). Each packet keeps the source
//! descriptions, sample ordinals, byte ranges and one-tick decode clocks it covers.
//! Nothing here decodes audio.
use crate::{
    Access, CursorStatus, Limits, Next, Sample,
    boxes::{BoxChildren, BoxView},
    compat::mov::{clocks::MediaClock, view::StreamView},
    fields,
    movie::TrackHeader,
    quicktime::{PcmEncoding, SoundDescriptionV0},
    table::{
        SampleCursor, SampleTable,
        rows::{Offsets, Words},
    },
};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputToken, Result, SourceId, Step, WorkBudget,
};

/// Token that selects FFmpeg's QuickTime PCM packets for [`PcmStream`] and
/// [`PcmCursor`].
#[derive(Clone, Copy, Debug)]
pub struct PcmProfile;

/// Limits of a [`PcmCursor`]; passing one is `Error::Limit`.
#[derive(Clone, Copy, Debug)]
pub struct PcmLimits {
    /// Samples in the track's sample table.
    pub max_samples: u64,
    /// Packets the cursor yields.
    pub max_packets: u64,
    /// Bytes the demuxer reads for one packet.
    pub max_packet_bytes: usize,
    /// Rows in `stsc` and in the chunk offset table.
    pub max_metadata_rows: u32,
}
impl Default for PcmLimits {
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

/// A QuickTime PCM track as FFmpeg's MOV demuxer lists it, beside the track's first
/// sound description.
#[derive(Clone, Debug)]
pub struct PcmStream {
    demux: StreamView,
    initial: SoundDescriptionV0,
}
impl PcmStream {
    /// The stream of a PCM track. `Error::Unsupported` outside a `qt  ` major-brand
    /// file, for a first description with a nonzero revision or vendor or another
    /// data reference, for a QuickTime language code and for a handler outside the
    /// ISO `hdlr` layout; `Error::Invalid` when `header` differs from its raw boxes
    /// or the table has no description.
    pub fn new(
        header: &TrackHeader,
        table: &SampleTable,
        major_brand: Option<[u8; 4]>,
        _profile: PcmProfile,
    ) -> Result<Self> {
        header_binding(header)?;
        if major_brand != Some(*b"qt  ") {
            return Err(Error::Unsupported(
                "PCM view needs the QuickTime major brand",
            ));
        }
        let entry = table
            .descriptions()
            .first()
            .ok_or(Error::Invalid("PCM initial description absent"))?;
        let initial = SoundDescriptionV0::parse(header, entry)?;
        let layout = initial.pcm_layout()?;
        if initial.revision() != 0 || initial.vendor() != [0; 4] || entry.data_reference_index != 1
        {
            return Err(Error::Unsupported(
                "PCM description revision, vendor or data reference other than 0, 0 and 1",
            ));
        }
        let clock = MediaClock::from_source(header.time_base()?)?;
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
                return Err(Error::Unsupported(
                    "PCM media language other than a packed ISO code",
                ));
            }
            *item = u8::try_from(word).map_err(|_| Error::Overflow)? + 0x60;
        }
        let b = header.raw_hdlr.payload();
        if fields::u32_at(b, 4)? != 0 || fields::slice(b, 12, 12)? != [0; 12] {
            return Err(Error::Unsupported(
                "PCM handler with nonzero predefined or reserved fields",
            ));
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
            return Err(Error::Unsupported("PCM handler name not ASCII"));
        }
        let start = usize::from(header.raw_hdlr.header().header_size)
            .checked_add(24)
            .ok_or(Error::Overflow)?;
        let end = entry.raw().bytes().len();
        Ok(Self {
            demux: StreamView {
                track_id: header.track_id,
                codec: match layout.encoding() {
                    PcmEncoding::Unsigned8 => "pcm_u8",
                    PcmEncoding::Signed16BigEndian => "pcm_s16be",
                },
                media_type: 1,
                time_base: clock.demux_base,
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
    /// The stream: `pcm_u8` or `pcm_s16be`, audio, in the media time base, with no
    /// codec private data and a zero vendor code.
    pub fn demux(&self) -> &StreamView {
        &self.demux
    }
    /// The track's first sound description.
    pub fn initial(&self) -> &SoundDescriptionV0 {
        &self.initial
    }
}

/// One packet FFmpeg's MOV demuxer emits for QuickTime PCM: up to 1024 consecutive
/// samples of one chunk. The demuxer reads it with the first description's frame
/// size, so where a later description's frames differ, the bytes it reads can
/// reach into another chunk; the source samples it covers are kept beside them.
#[derive(Clone, Debug)]
pub struct PcmPacket {
    initial: SoundDescriptionV0,
    description: SoundDescriptionV0,
    raw_table: BoxView,
    samples: Range<u64>,
    source_range: Range<u64>,
    demux_range: Range<u64>,
    media: Range<u64>,
    ordinal: u64,
    dts: i64,
    duration: u32,
    pool: Budget,
    source: SourceId,
    generation: InputToken,
}
impl PcmPacket {
    /// The sound description that `stsc` names for the packet's chunk.
    pub fn source_description(&self) -> &SoundDescriptionV0 {
        &self.description
    }
    /// The track's first sound description, whose frame size sets the bytes read.
    pub fn initial_description(&self) -> &SoundDescriptionV0 {
        &self.initial
    }
    /// The track's `stbl` box.
    pub fn source_table(&self) -> &BoxView {
        &self.raw_table
    }
    /// 1-based ordinals of the source samples the packet covers, end exclusive.
    pub fn source_samples(&self) -> Range<u64> {
        self.samples.clone()
    }
    /// Absolute file offsets of those samples' bytes.
    pub fn source_range(&self) -> &Range<u64> {
        &self.source_range
    }
    /// Absolute file offsets of the bytes the demuxer reads for the packet: the
    /// sample count times the first description's frame size, from the samples'
    /// place in their chunk.
    pub fn demux_range(&self) -> &Range<u64> {
        &self.demux_range
    }
    /// 0-based position of the packet in the track's packets.
    pub fn demux_ordinal(&self) -> u64 {
        self.ordinal
    }
    /// Decode time in media ticks: the 0-based index of the first sample, each
    /// sample lasting one tick.
    pub fn dts(&self) -> i64 {
        self.dts
    }
    /// Presentation time, equal to [`dts`](Self::dts).
    pub fn pts(&self) -> i64 {
        self.dts
    }
    /// Duration in media ticks: the number of samples.
    pub fn duration(&self) -> u32 {
        self.duration
    }
    /// Packet flags as FFmpeg reports them: always 1, key.
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
#[derive(Debug)]
struct Group {
    count: u32,
    consumed: u32,
    first: u64,
    source_range: Range<u64>,
    demux_range: Range<u64>,
    description: SoundDescriptionV0,
}

/// The chunk tables of a PCM track and where the packets have reached in them.
#[derive(Debug)]
struct Grouping {
    stsc: Words,
    offsets: Offsets,
    chunk: u32,
    stsc_row: u32,
    chunk_left: u32,
    chunk_used: u32,
    /// The packet being grouped, a source sample read for it, and a packet grouped
    /// but not yet taken.
    group: Option<Group>,
    pending: Option<Sample>,
    ready: Option<PcmPacket>,
}

/// Where a [`PcmCursor`] stands.
// Boxing the grouping would put the per-sample state behind a pointer.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum PcmState {
    /// Reading `stsc` and the chunk offsets among the `stbl` children.
    Tables {
        children: BoxChildren,
        stsc: Option<Words>,
        offsets: Option<Offsets>,
    },
    Packets(Grouping),
    Ended,
    Failed(Error),
}

/// Groups a QuickTime PCM track's samples into the [`PcmPacket`]s FFmpeg's MOV
/// demuxer emits, from the sample table, with no per-sample storage. Every source
/// sample is checked. The cursor is bound to the input's pool, source and
/// generation: payload reads in the same generation leave it usable, and a new
/// generation fails it with `Error::StaleGeneration`.
#[derive(Debug)]
pub struct PcmCursor<'a> {
    header: &'a TrackHeader,
    table: &'a SampleTable,
    source_cursor: SampleCursor<'a>,
    initial: SoundDescriptionV0,
    media: Range<u64>,
    limits: PcmLimits,
    pool: Budget,
    source: SourceId,
    generation: InputToken,
    source_done: u64,
    generated: u64,
    state: PcmState,
}
impl<'a> PcmCursor<'a> {
    /// A cursor over `table`, whose samples must lie in the `media` extent
    /// (absolute file offsets). `Error::ForeignPool` when the table or header boxes
    /// are charged to another pool than `input`'s; `Error::Invalid` for an inverted
    /// extent, a header that differs from its raw boxes or a table without a
    /// description.
    pub fn new(
        header: &'a TrackHeader,
        table: &'a SampleTable,
        input: &InputCursor,
        media: Range<u64>,
        limits: PcmLimits,
        _profile: PcmProfile,
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
        let initial = SoundDescriptionV0::parse(header, entry)?;
        initial.pcm_layout()?;
        Ok(Self {
            header,
            table,
            source_cursor: table.cursor(),
            initial,
            media,
            limits,
            pool: input.budget().clone(),
            source: input.source(),
            generation: input.token(),
            source_done: 0,
            generated: 0,
            state: PcmState::Tables {
                children: table.raw().children(0, Limits::default())?,
                stsc: None,
                offsets: None,
            },
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
        self.state = PcmState::Failed(error);
        error
    }
    /// Advances to the next packet. Failures are sticky. `Error::Unsupported` for
    /// a composition or access table, for samples that are not consecutive one-tick
    /// sync frames of their description's size, and for a packet read outside
    /// `media`; `Error::Invalid` where the chunk tables and samples disagree.
    pub fn poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<CursorStatus> {
        if let PcmState::Failed(error) = self.state {
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
    ) -> Result<CursorStatus> {
        self.check(input, work, cancel)?;
        match &self.state {
            PcmState::Packets(grouping) if grouping.ready.is_some() => {
                return Ok(CursorStatus::Ready);
            }
            PcmState::Ended => return Ok(CursorStatus::End),
            _ => {}
        }
        loop {
            if work.remaining() == 0 {
                return Ok(CursorStatus::Yield);
            }
            let grouping = match &mut self.state {
                PcmState::Tables {
                    children,
                    stsc,
                    offsets,
                } => {
                    let limits = Limits {
                        max_records: self.limits.max_metadata_rows,
                        ..Limits::default()
                    };
                    match children.poll(work)? {
                        Next::Yield => return Ok(CursorStatus::Yield),
                        Next::Item(raw) => match &raw.header().kind {
                            b"stsc" => {
                                if stsc.is_some() {
                                    return Err(Error::Invalid("PCM duplicate stsc"));
                                }
                                *stsc = Some(Words::parse(raw, 12, limits)?);
                            }
                            b"stco" | b"co64" => {
                                if offsets.is_some() {
                                    return Err(Error::Invalid("PCM duplicate chunk offsets"));
                                }
                                *offsets = Some(Offsets::parse(raw, limits)?);
                            }
                            b"ctts" | b"stss" | b"stps" | b"sdtp" => {
                                return Err(Error::Unsupported(
                                    "PCM track with a composition or access table",
                                ));
                            }
                            _ => (),
                        },
                        Next::End => {
                            let (Some(stsc), Some(offsets)) = (stsc.take(), offsets.take()) else {
                                return Err(Error::Invalid("PCM chunk tables absent"));
                            };
                            self.state = PcmState::Packets(Grouping {
                                stsc,
                                offsets,
                                chunk: 0,
                                stsc_row: 0,
                                chunk_left: 0,
                                chunk_used: 0,
                                group: None,
                                pending: None,
                                ready: None,
                            });
                        }
                    }
                    continue;
                }
                PcmState::Packets(grouping) => grouping,
                PcmState::Ended => return Ok(CursorStatus::End),
                PcmState::Failed(error) => return Err(*error),
            };
            let Some(group) = &mut grouping.group else {
                if !work.try_charge(1) {
                    return Ok(CursorStatus::Yield);
                }
                if self.source_done == self.table.sample_count() {
                    if grouping.chunk_left != 0 || grouping.chunk != grouping.offsets.table.count {
                        return Err(Error::Invalid("PCM incomplete source chunk accounting"));
                    }
                    self.state = PcmState::Ended;
                    return Ok(CursorStatus::End);
                }
                bounded(
                    self.generated.checked_add(1).ok_or(Error::Overflow)?,
                    self.limits.max_packets,
                )?;
                grouping.group = Some(grouping.start_group(
                    self.header,
                    self.table,
                    &self.initial,
                    &self.media,
                    self.limits,
                    self.source_done,
                )?);
                continue;
            };
            let sample = if let Some(sample) = grouping.pending.take() {
                sample
            } else {
                match self.source_cursor.poll(work)? {
                    Next::Yield => return Ok(CursorStatus::Yield),
                    Next::End => {
                        return Err(Error::Invalid("PCM source sample count ended early"));
                    }
                    Next::Item(sample) => sample,
                }
            };
            if !work.try_charge(1) {
                grouping.pending = Some(sample);
                return Ok(CursorStatus::Yield);
            }
            sample.require_within(self.media.clone())?;
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
                    "PCM source samples are not consecutive one-tick sync frames",
                ));
            }
            if group.consumed == 0 {
                group.source_range.start = sample.range.start;
            }
            group.source_range.end = sample.range.end;
            group.consumed = group.consumed.checked_add(1).ok_or(Error::Overflow)?;
            self.source_done = expected;
            let Some(group) = grouping
                .group
                .take_if(|group| group.consumed == group.count)
            else {
                continue;
            };
            grouping.chunk_left = grouping
                .chunk_left
                .checked_sub(group.count)
                .ok_or(Error::Overflow)?;
            grouping.chunk_used = grouping
                .chunk_used
                .checked_add(group.count)
                .ok_or(Error::Overflow)?;
            let ordinal = self.generated;
            self.generated = self.generated.checked_add(1).ok_or(Error::Overflow)?;
            grouping.ready = Some(PcmPacket {
                initial: self.initial.clone(),
                description: group.description,
                raw_table: self.table.raw().clone(),
                samples: group.first..expected.checked_add(1).ok_or(Error::Overflow)?,
                source_range: group.source_range,
                demux_range: group.demux_range,
                media: self.media.clone(),
                ordinal,
                dts: i64::try_from(group.first - 1).map_err(|_| Error::Overflow)?,
                duration: group.count,
                pool: self.pool.clone(),
                source: self.source,
                generation: self.generation.clone(),
            });
            return Ok(CursorStatus::Ready);
        }
    }
    /// Takes the ready packet; `Error::Invalid` when none is ready.
    pub fn take_packet(
        &mut self,
        input: &mut InputCursor,
        cancel: &AtomicBool,
    ) -> Result<PcmPacket> {
        if let PcmState::Failed(error) = self.state {
            return Err(error);
        }
        let result = (|| {
            self.check(input, &mut WorkBudget::new(0), cancel)?;
            match &mut self.state {
                PcmState::Packets(grouping) => grouping.ready.take(),
                _ => None,
            }
            .ok_or(Error::Invalid("PCM packet not ready"))
        })();
        result.map_err(|e| self.poison(e))
    }
}

impl Grouping {
    /// The next packet's group: up to 1024 samples of the current chunk, or of the next
    /// chunk once it is used up.
    fn start_group(
        &mut self,
        header: &TrackHeader,
        table: &SampleTable,
        initial: &SoundDescriptionV0,
        media: &Range<u64>,
        limits: PcmLimits,
        source_done: u64,
    ) -> Result<Group> {
        if self.chunk_left == 0 {
            if self.chunk >= self.offsets.table.count {
                return Err(Error::Invalid("PCM chunk count exhausted"));
            }
            self.chunk = self.chunk.checked_add(1).ok_or(Error::Overflow)?;
            let next = self.stsc_row.checked_add(1).ok_or(Error::Overflow)?;
            if next < self.stsc.count && self.stsc.word(next, 0)? == self.chunk {
                self.stsc_row = next;
            }
            self.chunk_left = self.stsc.word(self.stsc_row, 4)?;
            self.chunk_used = 0;
        }
        let count = self.chunk_left.min(1024);
        if count == 0 {
            return Err(Error::Invalid("PCM empty chunk group"));
        }
        let index = self.stsc.word(self.stsc_row, 8)?;
        let entry = table
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
        let description = SoundDescriptionV0::parse(header, entry)?;
        description.pcm_layout()?;
        let initial_bytes =
            u64::try_from(initial.pcm_layout()?.frame_bytes()).map_err(|_| Error::Overflow)?;
        let start = self
            .offsets
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
            u64::try_from(limits.max_packet_bytes).map_err(|_| Error::Overflow)?,
        )?;
        let end = start.checked_add(length).ok_or(Error::Overflow)?;
        // This bounded view does not guess a short demux request outside media.
        if start < media.start || end > media.end {
            return Err(Error::Unsupported(
                "PCM packet read outside the media extent",
            ));
        }
        Ok(Group {
            count,
            consumed: 0,
            first: source_done.checked_add(1).ok_or(Error::Overflow)?,
            source_range: 0..0,
            demux_range: start..end,
            description,
        })
    }
}

/// The bytes of one [`PcmPacket`], read by a [`PcmReader`].
#[derive(Clone, Debug)]
pub struct PcmPayload {
    plan: PcmPacket,
    payload: Bytes,
    source: SourceId,
    token: InputToken,
}
impl PcmPayload {
    /// The packet that was read.
    pub fn plan(&self) -> &PcmPacket {
        &self.plan
    }
    /// The bytes of the packet's [`demux_range`](PcmPacket::demux_range).
    pub fn payload(&self) -> &Bytes {
        &self.payload
    }
    /// The input source they were read from.
    pub fn source(&self) -> SourceId {
        self.source
    }
    /// The input request they were read under.
    pub fn token(&self) -> &InputToken {
        &self.token
    }
}

/// Reads one [`PcmPacket`]'s bytes from a seekable input into a buffer charged to
/// the input's pool. Cancellation, foreign pools and stale requests fail with the
/// core errors.
#[derive(Debug)]
pub struct PcmReader {
    plan: PcmPacket,
    token: InputToken,
    position: u64,
    length: usize,
    buffer: Option<BudgetVec<u8>>,
    freeze_paid: bool,
    failed: Option<Error>,
}
impl PcmReader {
    /// Requests `plan`'s bytes from `input`. `Error::Limit` past `maximum_packet`
    /// bytes; `Error::Unsupported` unless the input is seekable with a known length
    /// that covers them; `Error::Invalid` for an empty read or one outside the
    /// packet's media extent.
    pub fn new(input: &mut InputCursor, plan: PcmPacket, maximum_packet: usize) -> Result<Self> {
        plan.bind(input)?;
        let range = &plan.demux_range;
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
    /// Advances the read; `Ready` with the payload. `Error::Truncated` when the
    /// input ends first. Failures other than `Error::Backpressure` are sticky.
    pub fn poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<PcmPayload>> {
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
    ) -> Result<Step<PcmPayload>> {
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
        let length = self.length;
        loop {
            let full = self
                .buffer
                .as_ref()
                .is_some_and(|buffer| buffer.len() == length);
            if full && !self.freeze_paid {
                if !work.try_charge(1) {
                    return Ok(Step::Yield);
                }
                self.freeze_paid = true;
            }
            if let Some(buffer) = self.buffer.take_if(|buffer| buffer.len() == length) {
                let payload = match buffer.freeze() {
                    Ok(b) => b,
                    Err(failure) => {
                        self.buffer = Some(failure.value);
                        return Err(failure.error);
                    }
                };
                return Ok(Step::Ready(PcmPayload {
                    plan: self.plan.clone(),
                    payload,
                    source: input.source(),
                    token: self.token.clone(),
                }));
            }
            let buffer = self
                .buffer
                .as_mut()
                .ok_or(Error::Invalid("PCM physical reader finished"))?;
            let remaining = length - buffer.len();
            match input.read_append(buffer, remaining, work, cancel)? {
                Step::Ready(_) => self.position = input.position(),
                Step::NeedInput(r) => return Ok(Step::NeedInput(r)),
                Step::Yield => return Ok(Step::Yield),
                Step::End(_) => return Err(Error::Truncated),
            }
        }
    }
}
