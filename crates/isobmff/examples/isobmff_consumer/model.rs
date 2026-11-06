//! Blocking research consumer over core InputCursor and existing format jobs.
//! Parsing helper provenance: this repo's independently authored source-trace
//! adapter. No FFmpeg/third-party implementation code or new core framework.
use super::protocol::{self, Packet, Retention, Stats};
use std::{
    fmt,
    fmt::Write as _,
    io::{self, Read, Seek, SeekFrom},
    ops::Range,
    sync::atomic::AtomicBool,
};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputWindow, SourceCapabilities,
    SourceFailure, SourceId, Step, Time, WorkBudget,
};
use triergon_isobmff::{
    BoxEvent, BoxReader, BoxStep, BoxView, BuildStatus, EditList, FragmentBuilder, FragmentContext,
    FragmentIndex, ImplicitBase, Limits, MediaDataPolicy, Mov9CursorStatus,
    Mov9NativeGlobalSeekJob, Mov9NativeIndexCursor, Mov9NativeIndexLimits, Mov9NativeSeekLimits,
    Mov9PacketCursor, Mov9PacketLimits, Mov9PartialSampleReader, Mov9PayloadBounds, Mov9Profile,
    Mov9StreamView, Mov9Track, MovieHeader, MovieSeekClocks, MovieSeekLimits,
    MovieSourceSeekContext, MovieSourceSeekPlanner, PinnedMovD32RegularRecoveryV1,
    RecoveryDescriptions, RecoveryKind, RecoveryMappings, RecoveryScope, RecoverySyntax, Sample,
    SampleReader, SampleStep, SampleTable, SampleTableBuilder, SampleTablePolicy, SourcePacket,
    SourceTrack, TrackHeader, TrackSampleCursor, Trex,
};

const MAX_SAMPLE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub enum Failure {
    Core(Error),
    Io(io::Error),
}
impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Self::Core(e)
    }
}
impl From<io::Error> for Failure {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Core(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for Failure {}
pub type Result<T> = std::result::Result<T, Failure>;
#[derive(Clone, Copy)]
pub struct NativeSeekRequest {
    pub track: i32,
    pub ticks: i64,
    pub count: u32,
    pub after_eof: bool,
}
#[derive(Clone, Copy)]
pub enum NativeOperation {
    Open,
    Index,
    Seek(NativeSeekRequest),
}
trait NativeRows {
    fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<Mov9CursorStatus>;
    fn take(&mut self) -> triergon_core::Result<triergon_isobmff::Mov9PacketOccurrence>;
}
impl NativeRows for Mov9PacketCursor<'_> {
    fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<Mov9CursorStatus> {
        Mov9PacketCursor::poll(self, work, cancel)
    }
    fn take(&mut self) -> triergon_core::Result<triergon_isobmff::Mov9PacketOccurrence> {
        self.take_occurrence()
    }
}
fn append<T>(target: &mut BudgetVec<T>, value: T) -> Result<()> {
    target.try_push(value).map_err(|e| Failure::Core(e.error))
}
fn word(bytes: &[u8], offset: usize) -> Result<u32> {
    let end = offset.checked_add(4).ok_or(Error::Overflow)?;
    let b = bytes.get(offset..end).ok_or(Error::Truncated)?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}
fn children(raw: &BoxView, skip: usize, budget: &Budget) -> Result<BudgetVec<BoxView>> {
    let mut cursor = raw.children(skip, Limits::default())?;
    let mut out = BudgetVec::new(budget);
    loop {
        match cursor.poll(&mut WorkBudget::new(4096))? {
            BoxStep::Box(raw) => append(&mut out, raw)?,
            BoxStep::Yield => {}
            BoxStep::End => return Ok(out),
        }
    }
}
fn optional(raw: &BoxView, kind: &[u8; 4], budget: &Budget) -> Result<Option<BoxView>> {
    let mut found = None;
    for child in children(raw, 0, budget)?.as_slice() {
        if child.header().kind == *kind {
            if found.is_some() {
                return Err(Error::Invalid("duplicate required child").into());
            }
            found = Some(child.clone());
        }
    }
    Ok(found)
}
fn child(raw: &BoxView, kind: &[u8; 4], budget: &Budget) -> Result<BoxView> {
    optional(raw, kind, budget)?.ok_or(Failure::Core(Error::Invalid("missing required child")))
}
fn track_header(track: &BoxView, budget: &Budget) -> Result<(TrackHeader, BoxView)> {
    let mdia = child(track, b"mdia", budget)?;
    let header = TrackHeader::parse(
        &child(track, b"tkhd", budget)?,
        &child(&mdia, b"mdhd", budget)?,
        &child(&mdia, b"hdlr", budget)?,
    )?;
    Ok((header, mdia))
}
fn self_contained_table(
    mdia: &BoxView,
    budget: &Budget,
    quantum: u64,
    table_policy: SampleTablePolicy,
) -> Result<SampleTable> {
    let minf = child(mdia, b"minf", budget)?;
    let dref = child(&child(&minf, b"dinf", budget)?, b"dref", budget)?;
    if word(dref.payload(), 0)? != 0 {
        return Err(Error::Unsupported("data reference version/flags").into());
    }
    let refs = children(&dref, 8, budget)?;
    if refs.len() != usize::try_from(word(dref.payload(), 4)?).map_err(|_| Error::Overflow)? {
        return Err(Error::Invalid("data reference count").into());
    }
    let mut builder = SampleTableBuilder::with_policy(
        child(&minf, b"stbl", budget)?,
        budget.clone(),
        Limits::default(),
        table_policy,
    )?;
    while builder.poll(&mut WorkBudget::new(quantum))? != BuildStatus::Ready {}
    let table = builder.finish()?;
    for description in table.descriptions() {
        let reference = usize::from(description.data_reference_index)
            .checked_sub(1)
            .ok_or(Error::Invalid("zero data ref"))?;
        let target = refs
            .as_slice()
            .get(reference)
            .ok_or(Error::Invalid("dangling data reference"))?;
        if target.header().kind != *b"url " || target.payload() != [0, 0, 0, 1] {
            return Err(Error::Unsupported(
                "external/unknown data reference; no file or URL opened",
            )
            .into());
        }
    }
    Ok(table)
}
struct FragmentTrack {
    id: u32,
    descriptions: u32,
    next_ordinal: u64,
    defaults: Trex,
}

fn index_fragments(
    movie: &BoxView,
    moofs: &[BoxView],
    budget: &Budget,
    quantum: u64,
) -> Result<BudgetVec<FragmentIndex>> {
    let mut indexes = BudgetVec::new(budget);
    if moofs.is_empty() {
        return Ok(indexes);
    }
    let extends = child(movie, b"mvex", budget)?;
    let mut defaults = BudgetVec::new(budget);
    for raw in children(&extends, 0, budget)?.as_slice() {
        if raw.header().kind != *b"trex" {
            continue;
        }
        let trex = Trex::parse(raw)?;
        if defaults
            .as_slice()
            .iter()
            .any(|previous: &Trex| previous.track_id == trex.track_id)
        {
            return Err(Error::Invalid("duplicate trex for track").into());
        }
        append(&mut defaults, trex)?;
    }
    let mut tracks = BudgetVec::new(budget);
    for track in children(movie, 0, budget)?
        .as_slice()
        .iter()
        .filter(|b| b.header().kind == *b"trak")
    {
        let media = child(track, b"mdia", budget)?;
        let header = TrackHeader::parse(
            &child(track, b"tkhd", budget)?,
            &child(&media, b"mdhd", budget)?,
            &child(&media, b"hdlr", budget)?,
        )?;
        if tracks.len() >= 64 {
            return Err(Error::Limit {
                requested: tracks.len() + 1,
                maximum: 64,
            }
            .into());
        }
        if tracks
            .as_slice()
            .iter()
            .any(|previous: &FragmentTrack| previous.id == header.track_id)
        {
            return Err(Error::Invalid("duplicate fragment track ID").into());
        }
        let stbl = child(&child(&media, b"minf", budget)?, b"stbl", budget)?;
        let descriptions = word(child(&stbl, b"stsd", budget)?.payload(), 4)?;
        if descriptions > Limits::default().max_descriptions {
            return Err(Error::Limit {
                requested: usize::try_from(descriptions).map_err(|_| Error::Overflow)?,
                maximum: usize::try_from(Limits::default().max_descriptions)
                    .map_err(|_| Error::Overflow)?,
            }
            .into());
        }
        // Both selected size layouts carry sample_count at payload offset8.
        // The real SampleTableBuilder below validates their full wire grammar.
        let sizes = match (
            optional(&stbl, b"stsz", budget)?,
            optional(&stbl, b"stz2", budget)?,
        ) {
            (Some(_), Some(_)) => return Err(Error::Invalid("both stsz and stz2").into()),
            (Some(raw), None) | (None, Some(raw)) => raw,
            (None, None) => return Err(Error::Invalid("missing sample size table").into()),
        };
        let regular_count = word(sizes.payload(), 8)?;
        let defaults = defaults
            .as_slice()
            .iter()
            .find(|d| d.track_id == header.track_id)
            .copied()
            .ok_or(Error::Invalid("missing trex for track"))?;
        append(
            &mut tracks,
            FragmentTrack {
                id: header.track_id,
                descriptions,
                next_ordinal: u64::from(regular_count) + 1,
                defaults,
            },
        )?;
    }
    for moof in moofs {
        let mfhd = child(moof, b"mfhd", budget)?;
        if mfhd.payload().len() != 8 || word(mfhd.payload(), 0)? != 0 {
            return Err(Error::Unsupported("mfhd version/flags/layout").into());
        }
        let mut previous: Option<triergon_core::Result<u64>> = None;
        for traf in children(moof, 0, budget)?
            .as_slice()
            .iter()
            .filter(|b| b.header().kind == *b"traf")
        {
            let tfhd = child(traf, b"tfhd", budget)?;
            let id = word(tfhd.payload(), 4)?;
            let state = tracks
                .as_mut_slice()
                .iter_mut()
                .find(|t| t.id == id)
                .ok_or(Error::Invalid("unknown fragment track"))?;
            let implicit_base = if word(tfhd.payload(), 0)? & 0x020001 != 0 {
                // Either explicit base or moof-relative addressing makes the
                // preceding traf's implicit endpoint irrelevant.
                ImplicitBase::FirstTraf
            } else {
                match previous {
                    None => ImplicitBase::FirstTraf,
                    Some(Ok(end)) => ImplicitBase::PreviousTraf(end),
                    Some(Err(error)) => return Err(error.into()),
                }
            };
            let context = FragmentContext {
                moof_start: moof.header().start,
                implicit_base,
                decode_origin: None,
                first_ordinal: state.next_ordinal,
                description_count: state.descriptions,
            };
            let mut builder = FragmentBuilder::new(
                traf.clone(),
                state.defaults,
                context,
                budget.clone(),
                Limits::default(),
            )?;
            while builder.poll(&mut WorkBudget::new(quantum))? != BuildStatus::Ready {}
            let index = builder.finish()?;
            state.next_ordinal = state
                .next_ordinal
                .checked_add(index.sample_count())
                .ok_or(Error::Overflow)?;
            previous = Some(index.implicit_data_end());
            append(&mut indexes, index)?;
        }
    }
    Ok(indexes)
}

pub trait Transport: Read + Seek {}
impl<T: Read + Seek> Transport for T {}
pub struct Driver<'a> {
    transport: &'a mut dyn Transport,
    pub input: InputCursor,
    pub budget: Budget,
    length: u64,
    window: usize,
    quantum: u64,
    position: Option<u64>,
    cancel: AtomicBool,
}
struct Scanned {
    movie: BoxView,
    media: BudgetVec<Range<u64>>,
    truncated_media: Option<Range<u64>>,
    moofs: BudgetVec<BoxView>,
    indexes: BudgetVec<BoxView>,
    brand: Option<[u8; 4]>,
}
pub struct Record {
    pub header: TrackHeader,
    pub table: SampleTable,
    pub edits: Option<EditList>,
}
pub struct Model {
    pub records: BudgetVec<Record>,
    pub fragments: BudgetVec<FragmentIndex>,
    pub media: BudgetVec<Range<u64>>,
    truncated_media: Option<Range<u64>>,
    pub movie: MovieHeader,
    raw_movie: BoxView,
    indexes: BudgetVec<BoxView>,
    brand: Option<[u8; 4]>,
}

#[derive(Clone, Copy, Default)]
pub struct OpenPolicy {
    pub tables: SampleTablePolicy,
    pub media_data: MediaDataPolicy,
}
impl<'a> Driver<'a> {
    pub fn new(
        transport: &'a mut dyn Transport,
        length: u64,
        window: usize,
        quantum: u64,
    ) -> Result<Self> {
        if window == 0 || window > 65536 || quantum == 0 {
            return Err(Error::Invalid("consumer window/work policy").into());
        }
        transport.seek(SeekFrom::Start(0))?;
        let budget = Budget::new(64 * 1024 * 1024)?;
        let input = InputCursor::new(
            &budget,
            SourceId(1),
            SourceCapabilities {
                length: Some(length),
                random_access: true,
            },
            window,
        )?;
        Ok(Self {
            transport,
            input,
            budget,
            length,
            window,
            quantum,
            position: Some(0),
            cancel: AtomicBool::new(false),
        })
    }
    fn supply(&mut self, request: InputRequest) -> Result<()> {
        let maximum = request.maximum.min(self.window).min(65536);
        if maximum == 1 {
            self.supply_buffer::<1>(request, maximum)
        } else if maximum <= InputCursor::INLINE_CAPACITY {
            self.supply_buffer::<{ InputCursor::INLINE_CAPACITY }>(request, maximum)
        } else if maximum <= 4096 {
            self.supply_buffer::<4096>(request, maximum)
        } else {
            self.supply_buffer::<65536>(request, maximum)
        }
    }
    // Keep the 64 KiB initialization/frame outside the small-request path.
    // Each monomorph remains initialized safe stack storage; the I/O and core
    // handoff below are shared, with no per-refill heap/cache or work change.
    #[inline(never)]
    fn supply_buffer<const CAPACITY: usize>(
        &mut self,
        request: InputRequest,
        maximum: usize,
    ) -> Result<()> {
        let mut scratch = [0u8; CAPACITY];
        let result = (|| -> io::Result<usize> {
            if self.position != Some(request.offset) {
                self.transport.seek(SeekFrom::Start(request.offset))?;
            }
            self.transport.read(&mut scratch[..maximum])
        })();
        let count = match result {
            Ok(n) => n,
            Err(error) => {
                self.input
                    .fail(request.source, request.token, SourceFailure::Transport)?;
                return Err(error.into());
            }
        };
        self.position = Some(
            request
                .offset
                .checked_add(u64::try_from(count).map_err(|_| Error::Overflow)?)
                .ok_or(Error::Overflow)?,
        );
        if count == 0 {
            self.input.finish(request.source, request.token)?;
        } else if count <= InputCursor::INLINE_CAPACITY {
            self.input.provide_inline(
                request.source,
                request.token,
                request.offset,
                &scratch[..count],
            )?;
        } else {
            let bytes = Bytes::copy_from_slice(&self.budget, &scratch[..count])?;
            self.input.provide_owned(InputWindow::new(
                request.source,
                request.token,
                request.offset,
                bytes,
            )?)?;
        }
        Ok(())
    }
    fn scan(&mut self, media_data: MediaDataPolicy) -> Result<Scanned> {
        let mut reader =
            BoxReader::with_media_data_policy(&self.input, 16 * 1024 * 1024, media_data)?;
        let (mut movie, mut brand) = (None, None);
        let mut media = BudgetVec::new(&self.budget);
        let mut truncated_media = None;
        let mut moofs = BudgetVec::new(&self.budget);
        let mut indexes = BudgetVec::new(&self.budget);
        loop {
            match reader.poll(
                &mut self.input,
                &mut WorkBudget::new(self.quantum),
                &self.cancel,
            )? {
                Step::NeedInput(request) => self.supply(request)?,
                Step::Yield => {}
                Step::End(_) => break,
                Step::Ready(BoxEvent::Header(header)) => {
                    if header.kind == *b"mdat" && header.end()?.is_some_and(|end| end > self.length)
                    {
                        let declared = header.payload_range()?;
                        append(&mut media, declared.clone())?;
                        truncated_media = Some(declared);
                        break;
                    }
                    if matches!(
                        &header.kind,
                        b"ftyp" | b"moov" | b"moof" | b"sidx" | b"mfra"
                    ) {
                        reader.read_metadata(&self.input)?;
                    } else {
                        reader.skip(&mut self.input)?;
                    }
                }
                Step::Ready(BoxEvent::Skipped(header)) => {
                    if header.kind == *b"mdat" {
                        append(&mut media, header.payload_range()?)?;
                    }
                }
                Step::Ready(BoxEvent::Metadata(raw)) => match &raw.header().kind {
                    b"ftyp" => {
                        if brand.is_some() {
                            return Err(Error::Invalid("duplicate file type box").into());
                        }
                        let b = raw.payload();
                        if b.len() < 8 || b.len() % 4 != 0 {
                            return Err(Error::Invalid("file type box extent").into());
                        }
                        brand = Some(b[..4].try_into().map_err(|_| Error::Truncated)?);
                    }
                    b"moov" => {
                        if movie.is_some() {
                            return Err(Error::Invalid("duplicate movie").into());
                        }
                        movie = Some(raw);
                    }
                    b"moof" => {
                        if moofs.len() >= 65536 {
                            return Err(Error::Limit {
                                requested: moofs.len() + 1,
                                maximum: 65536,
                            }
                            .into());
                        }
                        append(&mut moofs, raw)?;
                    }
                    b"sidx" | b"mfra" => append(&mut indexes, raw)?,
                    _ => {}
                },
            }
        }
        Ok(Scanned {
            movie: movie.ok_or(Error::Invalid("missing movie"))?,
            media,
            truncated_media,
            moofs,
            indexes,
            brand,
        })
    }
    pub fn open(&mut self) -> Result<Model> {
        self.open_with_policy(OpenPolicy::default())
    }

    pub fn open_with_policy(&mut self, policy: OpenPolicy) -> Result<Model> {
        let scanned = self.scan(policy.media_data)?;
        let movie = MovieHeader::parse(&child(&scanned.movie, b"mvhd", &self.budget)?)?;
        let fragments = index_fragments(
            &scanned.movie,
            scanned.moofs.as_slice(),
            &self.budget,
            self.quantum,
        )?;
        let mut records: BudgetVec<Record> = BudgetVec::new(&self.budget);
        for track in children(&scanned.movie, 0, &self.budget)?
            .as_slice()
            .iter()
            .filter(|b| b.header().kind == *b"trak")
        {
            let (header, mdia) = track_header(track, &self.budget)?;
            if records.len() >= 64 {
                return Err(Error::Limit {
                    requested: records.len() + 1,
                    maximum: 64,
                }
                .into());
            }
            if records
                .as_slice()
                .iter()
                .any(|r| r.header.track_id == header.track_id)
            {
                return Err(Error::Invalid("duplicate track ID").into());
            }
            let table = self_contained_table(&mdia, &self.budget, self.quantum, policy.tables)?;
            let edits = optional(track, b"edts", &self.budget)?
                .map(|raw| {
                    child(&raw, b"elst", &self.budget).and_then(|raw| {
                        EditList::parse(raw, Limits::default()).map_err(Failure::Core)
                    })
                })
                .transpose()?;
            append(
                &mut records,
                Record {
                    header,
                    table,
                    edits,
                },
            )?;
        }
        Ok(Model {
            records,
            fragments,
            media: scanned.media,
            truncated_media: scanned.truncated_media,
            movie,
            raw_movie: scanned.movie,
            indexes: scanned.indexes,
            brand: scanned.brand,
        })
    }
    pub fn packet(&mut self, sample: Sample, media: &[Range<u64>]) -> Result<SourcePacket> {
        let extent = media
            .iter()
            .find(|r| sample.require_within((*r).clone()).is_ok())
            .cloned()
            .ok_or(Error::Invalid("consumer sample outside media extents"))?;
        let mut reader = SampleReader::new(&mut self.input, sample, extent, MAX_SAMPLE_BYTES)?;
        loop {
            match reader.poll(
                &mut self.input,
                &mut WorkBudget::new(self.quantum),
                &self.cancel,
            )? {
                Step::Ready(packet) => return Ok(packet),
                Step::NeedInput(request) => self.supply(request)?,
                Step::Yield => {}
                Step::End(_) => return Err(Error::Truncated.into()),
            }
        }
    }
    pub fn native_open(
        &mut self,
        model: &Model,
        stats: &mut Stats,
        retention: &mut Retention,
        trace: Option<&mut String>,
    ) -> Result<()> {
        self.native_operation(model, stats, retention, NativeOperation::Open, trace)
    }

    #[cold]
    fn terminal_payload(&mut self, sample: Sample, declared: &Range<u64>) -> Result<Bytes> {
        let profile = PinnedMovD32RegularRecoveryV1;
        let mut reader = Mov9PartialSampleReader::new(
            &mut self.input,
            sample,
            declared.clone(),
            MAX_SAMPLE_BYTES,
            profile,
        )?;
        loop {
            match reader.poll(
                &mut self.input,
                &mut WorkBudget::new(self.quantum),
                &self.cancel,
            )? {
                Step::Ready(packet) => {
                    debug_assert!(packet.bounds.incomplete_at_known_eof);
                    return Ok(packet.payload);
                }
                Step::NeedInput(request) => self.supply(request)?,
                Step::Yield => {}
                Step::End(_) => return Err(Error::Truncated.into()),
            }
        }
    }

    pub fn native_operation(
        &mut self,
        model: &Model,
        stats: &mut Stats,
        retention: &mut Retention,
        operation: NativeOperation,
        mut trace: Option<&mut String>,
    ) -> Result<()> {
        if matches!(operation, NativeOperation::Seek(_)) && model.truncated_media.is_some() {
            return Err(Error::Truncated.into());
        }
        let mut tracks = BudgetVec::new(&self.budget);
        let mut streams = BudgetVec::new(&self.budget);
        let movie_base = model.movie.time_base()?;
        for record in model.records.as_slice() {
            let has_fragments = model
                .fragments
                .as_slice()
                .iter()
                .any(|f| f.track_id() == record.header.track_id);
            append(
                &mut streams,
                Mov9StreamView::for_source(
                    &record.header,
                    &record.table,
                    has_fragments,
                    model.brand,
                )?,
            )?;
            append(
                &mut tracks,
                Mov9Track {
                    track_id: record.header.track_id,
                    table: &record.table,
                    fragments: model.fragments.as_slice(),
                    media_base: record.header.time_base()?,
                    edits: record.edits.as_ref().map(|e| (e, movie_base)),
                },
            )?;
        }
        let mut drain = Stats::default();
        let seek = match operation {
            NativeOperation::Seek(r) => Some(r),
            _ => None,
        };
        if let Some(r) = seek {
            if r.count > 64 {
                return Err(Error::Unsupported("native seek delivery count").into());
            }
            if !tracks
                .as_slice()
                .iter()
                .any(|t| i32::try_from(t.track_id).ok() == Some(r.track))
            {
                return Err(Error::Invalid("native seek track ID absent").into());
            }
            if r.after_eof {
                let mut cursor = Mov9PacketCursor::new_complete_source(
                    &self.budget,
                    tracks.as_slice(),
                    model.media.as_slice(),
                    Mov9PacketLimits::default(),
                    Mov9Profile::PinnedNoParseNoFillin,
                )?;
                loop {
                    match cursor.poll(&mut WorkBudget::new(self.quantum), &self.cancel)? {
                        Mov9CursorStatus::Yield => {}
                        Mov9CursorStatus::End => break,
                        Mov9CursorStatus::Ready => {
                            let row = cursor.take_occurrence()?;
                            let payload = self.packet(row.source, model.media.as_slice())?.payload;
                            let stream = tracks
                                .as_slice()
                                .iter()
                                .position(|t| t.track_id == row.track_id)
                                .ok_or(Error::Invalid("native drain track"))?;
                            drain.packet(&Packet::from_view(
                                stream,
                                row.track_id,
                                &row.native,
                                payload,
                            )?)?;
                        }
                    }
                }
                for value in [0x44, drain.packets, drain.bytes, drain.digest] {
                    protocol::fold(&mut stats.digest, value);
                }
            }
        }
        let plan = if let Some(r) = seek {
            let mut job = Mov9NativeGlobalSeekJob::new_complete_source(
                &self.budget,
                tracks.as_slice(),
                model.media.as_slice(),
                u32::try_from(r.track).map_err(|_| Error::Unsupported("native signed track ID"))?,
                r.ticks,
                Mov9NativeSeekLimits::default(),
                Mov9Profile::PinnedNoParseNoFillin,
            )?;
            while job.poll(&mut WorkBudget::new(self.quantum), &self.cancel)? != BuildStatus::Ready
            {
            }
            let plan = job.finish()?;
            self.input
                .reset(plan.receipt().selected.occurrence.source.range.start)?;
            protocol::fold(&mut stats.digest, 0x51);
            protocol::fold(&mut stats.digest, 0);
            Some(plan)
        } else {
            None
        };
        if let Some(out) = trace.as_deref_mut() {
            let name = match operation {
                NativeOperation::Open => "mov-native-open-consumer-v33",
                NativeOperation::Index => "mov-native-index-consumer-v33",
                NativeOperation::Seek(_) => "mov-native-seek-consumer-v33",
            };
            write!(out, "{{\"schema\":33,\"operation\":\"{name}\"").expect("String write");
            if let Some(r) = seek {
                write!(out,",\"track_id\":{},\"native_ticks\":{},\"after_eof\":{},\"seek_result\":0,\"native_drain\":{{\"packets\":{},\"bytes\":{},\"digest\":{}}}",r.track,r.ticks,r.after_eof,drain.packets,drain.bytes,drain.digest).expect("String write");
            }
            out.push_str(",\"packets\":[");
        }
        let (reads, eof) = if let Some(plan) = &plan {
            let mut cursor = plan.cursor()?;
            self.deliver_rows(
                model,
                &mut cursor,
                seek.map(|r| r.count),
                stats,
                retention,
                trace.as_deref_mut(),
            )?
        } else {
            let mut cursor = Mov9PacketCursor::new_complete_source(
                &self.budget,
                tracks.as_slice(),
                model.media.as_slice(),
                Mov9PacketLimits::default(),
                Mov9Profile::PinnedNoParseNoFillin,
            )?;
            self.deliver_rows(
                model,
                &mut cursor,
                None,
                stats,
                retention,
                trace.as_deref_mut(),
            )?
        };
        if let Some(out) = trace.as_deref_mut() {
            out.push_str("],\"streams\":[");
        }
        protocol::fold(&mut stats.digest, 0x43);
        protocol::fold(
            &mut stats.digest,
            u64::try_from(streams.len()).map_err(|_| Error::Overflow)?,
        );
        for (i, stream) in streams.as_slice().iter().enumerate() {
            protocol::consume_stream(&mut stats.digest, i, stream)?;
            if let Some(out) = trace.as_deref_mut() {
                if i != 0 {
                    out.push(',');
                }
                protocol::trace_stream(out, i, stream);
            }
        }
        protocol::fold(&mut stats.digest, 0x47);
        protocol::fold(&mut stats.digest, 0); // current MOV profile: no programs
        if let Some(out) = trace.as_deref_mut() {
            out.push_str("],\"programs\":[]");
        }
        if matches!(operation, NativeOperation::Index) {
            self.native_indexes(model, tracks.as_slice(), stats, trace.as_deref_mut())?;
        }
        let read_result = if eof { protocol::EOF } else { 0 };
        protocol::fold(&mut stats.digest, 0x45);
        protocol::fold(&mut stats.digest, protocol::signed(read_result));
        protocol::fold(&mut stats.digest, u64::from(eof));
        if let Some(out) = trace {
            write!(
                out,
                ",\"read_result\":{read_result},\"confirmed_eof\":{eof}"
            )
            .expect("String write");
            if let Some(r) = seek {
                write!(
                    out,
                    ",\"read_calls_after_seek\":{reads},\"delivery_limit_reached\":{}",
                    !eof && stats.packets >= u64::from(r.count)
                )
                .expect("String write");
            }
        }
        Ok(())
    }
    fn deliver_rows(
        &mut self,
        model: &Model,
        cursor: &mut impl NativeRows,
        limit: Option<u32>,
        stats: &mut Stats,
        retention: &mut Retention,
        mut trace: Option<&mut String>,
    ) -> Result<(u64, bool)> {
        let (mut reads, mut first) = (0u64, true);
        let mut short_end = None;
        while limit.is_none_or(|maximum| stats.packets < u64::from(maximum)) {
            if limit.is_some() {
                reads = reads.checked_add(1).ok_or(Error::Overflow)?;
            }
            loop {
                match cursor.poll(&mut WorkBudget::new(self.quantum), &self.cancel)? {
                    Mov9CursorStatus::Yield => {}
                    Mov9CursorStatus::End => return Ok((reads, true)),
                    Mov9CursorStatus::Ready => break,
                }
            }
            let item = cursor.take()?;
            let index = model
                .records
                .as_slice()
                .iter()
                .position(|r| r.header.track_id == item.track_id)
                .ok_or(Error::Invalid("native occurrence track"))?;
            let terminal = model.truncated_media.as_ref().filter(|extent| {
                item.source.range.end > self.length
                    && item.source.require_within((*extent).clone()).is_ok()
            });
            let payload = if let Some(declared) = terminal {
                // A positive short read can be followed by the adjacent
                // declared sample at EOF. Validate its full declared size.
                if short_end == Some(item.source.range.start) && !item.source.range.is_empty() {
                    Mov9PayloadBounds::from_source(
                        &item.source,
                        declared.clone(),
                        self.length,
                        MAX_SAMPLE_BYTES,
                        PinnedMovD32RegularRecoveryV1,
                    )?;
                    return Ok((reads, true));
                }
                let end = item.source.range.end;
                let payload = self.terminal_payload(item.source, declared)?;
                short_end = Some(end);
                payload
            } else {
                short_end = None;
                self.packet(item.source, model.media.as_slice())?.payload
            };
            let mut packet = Packet::from_view(index, item.track_id, &item.native, payload)?;
            if terminal.is_some() {
                packet.flags |= protocol::CORRUPT_PACKET;
            }
            stats.packet(&packet)?;
            if let Some(out) = trace.as_deref_mut() {
                if !first {
                    out.push(',');
                }
                packet.trace(out);
            }
            first = false;
            retention.retain(packet);
        }
        Ok((reads, false))
    }
    fn native_indexes(
        &self,
        model: &Model,
        tracks: &[Mov9Track<'_>],
        stats: &mut Stats,
        mut trace: Option<&mut String>,
    ) -> Result<()> {
        protocol::fold(&mut stats.digest, 0x49);
        protocol::fold(
            &mut stats.digest,
            u64::try_from(tracks.len()).map_err(|_| Error::Overflow)?,
        );
        if let Some(out) = trace.as_deref_mut() {
            out.push_str(",\"native_indexes\":[");
        }
        for (i, track) in tracks.iter().enumerate() {
            let mut index = Mov9NativeIndexCursor::new_complete_source(
                &self.budget,
                *track,
                model.media.as_slice(),
                Mov9NativeIndexLimits::default(),
                Mov9Profile::PinnedNoParseNoFillin,
            )?;
            let (mut count, mut digest) = (0u64, 0u64);
            if let Some(out) = trace.as_deref_mut() {
                if i != 0 {
                    out.push(',');
                }
                write!(
                    out,
                    "{{\"stream\":{i},\"pid\":{},\"entries\":[",
                    track.track_id
                )
                .expect("String write");
            }
            loop {
                match index.poll(&mut WorkBudget::new(self.quantum), &self.cancel)? {
                    Mov9CursorStatus::Yield => {}
                    Mov9CursorStatus::End => break,
                    Mov9CursorStatus::Ready => {
                        let entry = index.take_entry()?;
                        for value in [
                            protocol::signed(entry.position),
                            protocol::signed(entry.timestamp),
                            protocol::signed(i64::from(entry.flags)),
                            u64::from(entry.size),
                            protocol::signed(i64::from(entry.min_distance)),
                        ] {
                            protocol::fold(&mut digest, value);
                        }
                        if let Some(out) = trace.as_deref_mut() {
                            if count != 0 {
                                out.push(',');
                            }
                            write!(out,"{{\"ordinal\":{},\"position\":{},\"timestamp\":{},\"flags\":{},\"size\":{},\"min_distance\":{}}}",entry.ordinal,entry.position,entry.timestamp,entry.flags,entry.size,entry.min_distance).expect("String write");
                        }
                        count = count.checked_add(1).ok_or(Error::Overflow)?;
                    }
                }
            }
            protocol::fold(
                &mut stats.digest,
                u64::try_from(i).map_err(|_| Error::Overflow)?,
            );
            protocol::fold(&mut stats.digest, count);
            protocol::append_words(
                &mut stats.digest,
                digest,
                u32::try_from(count.checked_mul(5).ok_or(Error::Overflow)?)
                    .map_err(|_| Error::Overflow)?,
            );
            if let Some(out) = trace.as_deref_mut() {
                out.push_str("]}");
            }
        }
        if let Some(out) = trace {
            out.push(']');
        }
        Ok(())
    }
    pub fn index_trace(&self, model: &Model, out: &mut String) -> Result<()> {
        use triergon_isobmff::{
            FragmentRandomAccessStep, IndexStep, MovieFragmentRandomAccessTail, SegmentIndex,
            TrackFragmentRandomAccess,
        };
        out.push_str("{\"schema\":33,\"operation\":\"source-index-audit-v33\",\"verified_targets\":false,\"indexes\":[");
        let mut first = true;
        for raw in model.indexes.as_slice() {
            if !first {
                out.push(',');
            }
            first = false;
            write!(out, "{{\"offset\":{},\"raw_hex\":\"", raw.header().start)
                .expect("String write");
            triergon_harness_protocol::push_hex(out, raw.bytes().as_ref());
            out.push_str("\",\"entries\":[");
            let mut entry_first = true;
            if raw.header().kind == *b"sidx" {
                let index = SegmentIndex::parse(raw.clone(), Limits::default())?;
                let mut cursor = index.cursor();
                loop {
                    match cursor.poll(&mut WorkBudget::new(self.quantum))? {
                        IndexStep::Yield => {}
                        IndexStep::End => break,
                        IndexStep::Reference(r) => {
                            if !entry_first {
                                out.push(',');
                            }
                            entry_first = false;
                            write!(out,"{{\"ordinal\":{},\"range\":[{},{}],\"references_index\":{},\"sap_bits\":{},\"size_bits\":{},\"earliest\":[{},{}],\"duration\":[{},{}]}}",
                            r.ordinal,r.range.start,r.range.end,r.references_index,r.raw_sap_word,r.raw_size_word,r.earliest_presentation.numerator(),r.earliest_presentation.denominator(),r.presentation_duration.numerator(),r.presentation_duration.denominator()).expect("String write");
                        }
                    }
                }
            } else {
                for child in children(raw, 0, &self.budget)?.as_slice() {
                    if child.header().kind == *b"tfra" {
                        let index =
                            TrackFragmentRandomAccess::parse(child.clone(), Limits::default())?;
                        let mut cursor = index.cursor();
                        loop {
                            match cursor.poll(&mut WorkBudget::new(self.quantum))? {
                                FragmentRandomAccessStep::Yield => {}
                                FragmentRandomAccessStep::End => break,
                                FragmentRandomAccessStep::Entry(e) => {
                                    if !entry_first {
                                        out.push(',');
                                    }
                                    entry_first = false;
                                    write!(out,"{{\"track_id\":{},\"ordinal\":{},\"time_ticks\":{},\"moof_offset\":{},\"traf\":{},\"trun\":{},\"sample\":{}}}",index.track_id(),e.ordinal,e.time_ticks,e.moof_offset,e.traf_number,e.trun_number,e.sample_number).expect("String write");
                                }
                            }
                        }
                    } else if child.header().kind == *b"mfro" {
                        if raw.header().end()? != Some(self.length) {
                            return Err(Error::Unsupported("nonterminal mfra tail audit").into());
                        }
                        MovieFragmentRandomAccessTail::parse(child.clone(), self.length)?
                            .require_mfra(raw)?;
                    }
                }
            }
            out.push_str("]}");
        }
        out.push_str("],\"raw_movie\":");
        protocol::raw_box(out, &model.raw_movie);
        out.push('}');
        Ok(())
    }
}

struct GroupOwner {
    first: u64,
    count: u64,
    fragment: Option<u64>,
    descriptions: Option<RecoveryDescriptions>,
    mappings: Option<RecoveryMappings>,
}
fn groups(
    raw: &BoxView,
    kind: RecoveryKind,
    syntax: RecoverySyntax,
    pool: &Budget,
) -> Result<(Option<RecoveryDescriptions>, Option<RecoveryMappings>)> {
    let (mut descriptions, mut mappings) = (None, None);
    let code = match kind {
        RecoveryKind::Roll => *b"roll",
        RecoveryKind::Prol => *b"prol",
    };
    for box_ in children(raw, 0, pool)?.as_slice() {
        if !matches!(&box_.header().kind, b"sgpd" | b"sbgp") {
            continue;
        }
        if box_.payload().get(4..8) != Some(code.as_slice()) {
            continue;
        }
        if box_.header().kind == *b"sgpd" {
            if descriptions.is_some() {
                return Err(Error::Invalid("duplicate recovery descriptions").into());
            }
            descriptions = Some(RecoveryDescriptions::parse(
                box_.clone(),
                syntax,
                triergon_isobmff::RecoveryLimits::default(),
            )?);
        } else {
            if mappings.is_some() {
                return Err(Error::Invalid("duplicate recovery mappings").into());
            }
            mappings = Some(RecoveryMappings::parse(
                box_.clone(),
                triergon_isobmff::RecoveryLimits::default(),
            )?);
        }
    }
    Ok((descriptions, mappings))
}
pub struct SourceRequest {
    pub track: u32,
    pub at: Time,
    pub following: u32,
    pub after_eof: bool,
    pub kind: RecoveryKind,
    pub syntax: RecoverySyntax,
}
impl Driver<'_> {
    fn source_row(
        &mut self,
        model: &Model,
        sample: Sample,
        out: &mut String,
        stats: &mut Stats,
    ) -> Result<()> {
        let packet = self.packet(sample, model.media.as_slice())?;
        let sample = &packet.sample;
        stats.packets = stats.packets.checked_add(1).ok_or(Error::Overflow)?;
        stats.bytes = stats
            .bytes
            .checked_add(u64::try_from(packet.payload.len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        if stats.packets > protocol::MAX_PACKETS || stats.bytes > protocol::MAX_DELIVERED_BYTES {
            return Err(Error::Limit {
                requested: usize::try_from(stats.bytes).map_err(|_| Error::Overflow)?,
                maximum: 256 * 1024 * 1024,
            }
            .into());
        }
        protocol::fold(&mut stats.digest, sample.ordinal);
        protocol::fold_framed(&mut stats.digest, packet.payload.as_ref());
        write!(
            out,
            "{{\"ordinal\":{},\"description\":{},\"range\":[{},{}],\"duration\":{},\"dts\":",
            sample.ordinal,
            sample.description_index,
            sample.range.start,
            sample.range.end,
            sample.duration_ticks
        )
        .expect("String write");
        match sample.dts_ticks {
            Some(v) => write!(out, "{v}").expect("String write"),
            None => out.push_str("null"),
        }
        out.push_str(",\"cts\":");
        match sample.cts_ticks {
            Some(v) => write!(out, "{v}").expect("String write"),
            None => out.push_str("null"),
        }
        match sample.composition_offset {
            Some(v) => write!(
                out,
                ",\"offset_version\":{},\"offset_bits\":{}",
                v.version(),
                v.raw_bits()
            )
            .expect("String write"),
            None => out.push_str(",\"offset_version\":null,\"offset_bits\":null"),
        }
        write!(
            out,
            ",\"access\":\"{:?}\",\"partial_sync\":{},\"dependency\":",
            sample.access, sample.partial_sync
        )
        .expect("String write");
        match sample.dependency {
            Some(v) => write!(out, "{v}").expect("String write"),
            None => out.push_str("null"),
        }
        out.push_str(",\"fragment_flags\":");
        match sample.fragment_flags {
            Some(v) => write!(out, "{v}").expect("String write"),
            None => out.push_str("null"),
        }
        write!(
            out,
            ",\"source\":{},\"generation\":{},\"request\":{},\"payload\":\"",
            packet.source.0,
            packet.token.generation(),
            packet.token.request()
        )
        .expect("String write");
        triergon_harness_protocol::push_hex(out, packet.payload.as_ref());
        out.push_str("\"}");
        Ok(())
    }
    pub fn source_seek(
        &mut self,
        model: &Model,
        request: SourceRequest,
        out: &mut String,
    ) -> Result<Stats> {
        if request.following > 64 {
            return Err(Error::Limit {
                requested: request.following as usize,
                maximum: 64,
            }
            .into());
        }
        let record = model
            .records
            .as_slice()
            .iter()
            .find(|r| r.header.track_id == request.track)
            .ok_or(Error::Invalid("source seek track absent"))?;
        let mut drain = Stats::default();
        if request.after_eof {
            for r in model.records.as_slice() {
                let mut cursor = TrackSampleCursor::new(SourceTrack::new(
                    r.header.track_id,
                    Some(&r.table),
                    model.fragments.as_slice(),
                )?);
                loop {
                    match cursor.poll(&mut WorkBudget::new(self.quantum))? {
                        SampleStep::Yield => {}
                        SampleStep::End => break,
                        SampleStep::Sample(s) => {
                            let p = self.packet(s, model.media.as_slice())?;
                            drain.packets = drain.packets.checked_add(1).ok_or(Error::Overflow)?;
                            drain.bytes = drain
                                .bytes
                                .checked_add(
                                    u64::try_from(p.payload.len()).map_err(|_| Error::Overflow)?,
                                )
                                .ok_or(Error::Overflow)?;
                            if drain.packets > protocol::MAX_PACKETS
                                || drain.bytes > protocol::MAX_DELIVERED_BYTES
                            {
                                return Err(
                                    Error::Unsupported("source drain consumer limits").into()
                                );
                            }
                            protocol::fold(&mut drain.digest, p.sample.ordinal);
                            protocol::fold_framed(&mut drain.digest, p.payload.as_ref());
                        }
                    }
                }
            }
            self.input.request_range(self.length)?;
            let mut byte = [0];
            loop {
                match self.input.read_into(
                    &mut byte,
                    &mut WorkBudget::new(self.quantum),
                    &self.cancel,
                )? {
                    Step::End(_) => break,
                    Step::NeedInput(r) => self.supply(r)?,
                    Step::Yield => {}
                    Step::Ready(_) => return Err(Error::Invalid("bytes after declared EOF").into()),
                }
            }
        }
        let (track_descriptions, mapping) = groups(
            record.table.raw(),
            request.kind,
            request.syntax,
            &self.budget,
        )?;
        let mut owners = BudgetVec::new(&self.budget);
        let mut next = record
            .table
            .sample_count()
            .checked_add(1)
            .ok_or(Error::Overflow)?;
        if record.table.sample_count() != 0 {
            append(
                &mut owners,
                GroupOwner {
                    first: 1,
                    count: record.table.sample_count(),
                    fragment: None,
                    descriptions: None,
                    mappings: mapping,
                },
            )?;
        }
        for (i, index) in model
            .fragments
            .as_slice()
            .iter()
            .enumerate()
            .filter(|(_, f)| f.track_id() == request.track)
        {
            let (descriptions, mappings) =
                groups(index.raw(), request.kind, request.syntax, &self.budget)?;
            append(
                &mut owners,
                GroupOwner {
                    first: next,
                    count: index.sample_count(),
                    fragment: Some(
                        u64::try_from(i)
                            .map_err(|_| Error::Overflow)?
                            .checked_add(1)
                            .ok_or(Error::Overflow)?,
                    ),
                    descriptions,
                    mappings,
                },
            )?;
            next = next
                .checked_add(index.sample_count())
                .ok_or(Error::Overflow)?;
        }
        let mut scopes = BudgetVec::new(&self.budget);
        for owner in owners.as_slice() {
            append(
                &mut scopes,
                RecoveryScope {
                    kind: request.kind,
                    first_ordinal: owner.first,
                    sample_count: owner.count,
                    fragment_ordinal: owner.fragment,
                    track_descriptions: track_descriptions.as_ref(),
                    local_descriptions: owner.descriptions.as_ref(),
                    mappings: owner.mappings.as_ref(),
                },
            )?;
        }
        let mut planner = MovieSourceSeekPlanner::new(
            MovieSourceSeekContext {
                source: SourceTrack::new(
                    request.track,
                    Some(&record.table),
                    model.fragments.as_slice(),
                )?,
                edits: record.edits.as_ref(),
                clocks: MovieSeekClocks {
                    movie_base: model.movie.time_base()?,
                    media_base: record.header.time_base()?,
                    media_duration: None,
                },
                media_extents: model.media.as_slice(),
                recovery_kind: request.kind,
                recovery_scopes: scopes.as_slice(),
            },
            request.at,
            MovieSeekLimits::default(),
        )?;
        while planner.poll(&mut WorkBudget::new(self.quantum), &self.cancel)? != BuildStatus::Ready
        {
        }
        let result = planner.finish()?;
        write!(out,"{{\"schema\":33,\"operation\":\"source-movie-seek-consumer-v33\",\"native_comparable\":false,\"status\":\"{:?}\",\"track_id\":{},\"after_eof\":{},\"source_point\":",result.status,request.track,request.after_eof).expect("String write");
        if let Some(point) = &result.point {
            write!(
                out,
                "[{},{}]",
                point.source_time.numerator(),
                point.source_time.denominator()
            )
            .expect("String write");
        } else {
            out.push_str("null");
        }
        out.push_str(",\"edit\":");
        match result.point.as_ref().and_then(|p| p.edit_ordinal) {
            Some(v) => write!(out, "{v}").expect("String write"),
            None => out.push_str("null"),
        }
        write!(
            out,
            ",\"source_drain\":{{\"packets\":{},\"bytes\":{},\"digest\":{}}}",
            drain.packets, drain.bytes, drain.digest
        )
        .expect("String write");
        protocol::source_receipts(out, &result);
        out.push_str(",\"raw_sources\":[");
        protocol::raw_box(out, record.table.raw());
        if let Some(edits) = &record.edits {
            out.push(',');
            protocol::raw_box(out, edits.raw());
        }
        for fragment in model
            .fragments
            .as_slice()
            .iter()
            .filter(|f| f.track_id() == request.track)
        {
            out.push(',');
            protocol::raw_box(out, fragment.raw());
        }
        out.push(']');
        out.push_str(",\"packets\":[");
        let mut stats = Stats::default();
        let mut first = true;
        if let Some(plan) = &result.plan {
            self.input.reset(plan.receipt().decode_start.range.start)?;
            let mut cursor = plan.cursor();
            loop {
                match cursor.poll(&mut WorkBudget::new(self.quantum), &self.cancel)? {
                    SampleStep::Yield => {}
                    SampleStep::End => break,
                    SampleStep::Sample(s) => {
                        if !first {
                            out.push(',');
                        }
                        first = false;
                        self.source_row(model, s, out, &mut stats)?;
                    }
                }
            }
            for offset in 1..=u64::from(request.following) {
                let ordinal = plan
                    .receipt()
                    .target
                    .ordinal
                    .checked_add(offset)
                    .ok_or(Error::Overflow)?;
                if ordinal >= next {
                    break;
                }
                let sample = if ordinal <= record.table.sample_count() {
                    let mut locate = record.table.locate(ordinal)?;
                    loop {
                        match locate.poll(&mut WorkBudget::new(self.quantum))? {
                            SampleStep::Sample(s) => break s,
                            SampleStep::Yield => {}
                            SampleStep::End => {
                                return Err(Error::Invalid("followup locator exhausted").into());
                            }
                        }
                    }
                } else {
                    let mut first_ordinal = record
                        .table
                        .sample_count()
                        .checked_add(1)
                        .ok_or(Error::Overflow)?;
                    let mut found = None;
                    for f in model
                        .fragments
                        .as_slice()
                        .iter()
                        .filter(|f| f.track_id() == request.track)
                    {
                        let end = first_ordinal
                            .checked_add(f.sample_count())
                            .ok_or(Error::Overflow)?;
                        if ordinal < end {
                            let mut locate = f.locate(ordinal)?;
                            loop {
                                match locate.poll(&mut WorkBudget::new(self.quantum))? {
                                    SampleStep::Sample(s) => {
                                        found = Some(s);
                                        break;
                                    }
                                    SampleStep::Yield => {}
                                    SampleStep::End => {
                                        return Err(Error::Invalid(
                                            "fragment followup locator exhausted",
                                        )
                                        .into());
                                    }
                                }
                            }
                            break;
                        }
                        first_ordinal = end;
                    }
                    found.ok_or(Error::Invalid("followup sample not indexed"))?
                };
                if !first {
                    out.push(',');
                }
                first = false;
                self.source_row(model, sample, out, &mut stats)?;
            }
        }
        out.push_str("],\"decoder_recovery\":\"unverified\",\"qualification\":false}");
        Ok(stats)
    }
}

#[cfg(test)]
mod refill_transcript42 {
    use super::*;
    use std::io::Cursor;

    struct TraceIo<'a> {
        input: Cursor<&'a [u8]>,
        events: Vec<(u8, u64, usize)>,
    }
    impl Read for TraceIo<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.events
                .push((b'R', self.input.position(), output.len()));
            self.input.read(output)
        }
    }
    impl Seek for TraceIo<'_> {
        fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
            let SeekFrom::Start(offset) = to else {
                panic!("consumer range seek must be explicit");
            };
            self.events.push((b'S', offset, 0));
            self.input.seek(SeekFrom::Start(offset))
        }
    }

    #[test]
    fn one_byte_refills_preserve_read_seek_requests_eof_and_short_source_failure() {
        let mut io = TraceIo {
            input: Cursor::new(&[11, 22, 33][..]),
            events: Vec::new(),
        };
        {
            let mut driver = Driver::new(&mut io, 3, 1, 3).unwrap();
            let request = driver.input.request();
            assert_eq!(request.maximum, 1);
            driver.supply(request).unwrap();
            let mut byte = [0];
            assert!(matches!(
                driver
                    .input
                    .read_into(&mut byte, &mut WorkBudget::new(1), &driver.cancel)
                    .unwrap(),
                Step::Ready(1)
            ));
            assert_eq!(byte, [11]);
            let generation = driver.input.generation();
            driver.input.request_range(2).unwrap();
            let request = driver.input.request();
            driver.supply(request).unwrap();
            assert!(matches!(
                driver
                    .input
                    .read_into(&mut byte, &mut WorkBudget::new(1), &driver.cancel)
                    .unwrap(),
                Step::Ready(1)
            ));
            assert_eq!(byte, [33]);
            assert_eq!(driver.input.generation(), generation);
            let request = driver.input.request();
            assert_eq!(request.maximum, 0);
            driver.supply(request).unwrap();
            assert!(
                matches!(driver.input.read_into(&mut byte, &mut WorkBudget::new(0), &driver.cancel).unwrap(), Step::End(end) if end.offset == 3)
            );
        }
        assert_eq!(
            io.events,
            [
                (b'S', 0, 0),
                (b'R', 0, 1),
                (b'S', 2, 0),
                (b'R', 2, 1),
                (b'R', 3, 0)
            ]
        );
        let mut short = TraceIo {
            input: Cursor::new(&[][..]),
            events: Vec::new(),
        };
        {
            let mut driver = Driver::new(&mut short, 1, 1, 3).unwrap();
            let request = driver.input.request();
            assert!(matches!(
                driver.supply(request),
                Err(Failure::Core(Error::Truncated))
            ));
            assert!(matches!(
                driver
                    .input
                    .read_into(&mut [0], &mut WorkBudget::new(0), &driver.cancel),
                Err(Error::Truncated)
            ));
        }
        assert_eq!(short.events, [(b'S', 0, 0), (b'R', 0, 1)]);
    }

    #[test]
    fn refill_tier_boundaries_preserve_exact_requests_ranges_work_and_eof() {
        for maximum in [12_usize, 13, 4096, 4097] {
            let bytes: Vec<u8> = (0..maximum + 7).map(|index| (index % 251) as u8).collect();
            let length = bytes.len() as u64;
            let mut io = TraceIo {
                input: Cursor::new(bytes.as_slice()),
                events: Vec::new(),
            };
            {
                let mut driver = Driver::new(&mut io, length, maximum, 3).unwrap();
                let request = driver.input.request();
                assert_eq!(request.maximum, maximum);
                driver.supply(request).unwrap();
                let mut output = vec![0_u8; maximum];
                let mut work = WorkBudget::new(maximum as u64);
                assert!(
                    matches!(driver.input.read_into(&mut output, &mut work, &driver.cancel).unwrap(), Step::Ready(count) if count == maximum)
                );
                assert_eq!(output, bytes[..maximum]);
                assert_eq!(work.remaining(), 0);
                assert_eq!(driver.input.position(), maximum as u64);
                let generation = driver.input.generation();
                driver.input.request_range((maximum - 2) as u64).unwrap();
                let request = driver.input.request();
                assert_eq!(request.maximum, 9);
                driver.supply(request).unwrap();
                let mut tail = [0_u8; 9];
                let mut work = WorkBudget::new(9);
                assert!(matches!(
                    driver
                        .input
                        .read_into(&mut tail, &mut work, &driver.cancel)
                        .unwrap(),
                    Step::Ready(9)
                ));
                assert_eq!(tail, bytes[maximum - 2..]);
                assert_eq!(work.remaining(), 0);
                assert_eq!(driver.input.generation(), generation);
                let request = driver.input.request();
                assert_eq!(request.maximum, 0);
                driver.supply(request).unwrap();
                assert!(matches!(
                    driver
                        .input
                        .read_into(&mut [], &mut WorkBudget::new(0), &driver.cancel)
                        .unwrap(),
                    Step::Ready(0)
                ));
                assert!(
                    matches!(driver.input.read_into(&mut [0], &mut WorkBudget::new(0), &driver.cancel).unwrap(), Step::End(end) if end.offset == length)
                );
            }
            assert_eq!(
                io.events,
                [
                    (b'S', 0, 0),
                    (b'R', 0, maximum),
                    (b'S', (maximum - 2) as u64, 0),
                    (b'R', (maximum - 2) as u64, 9),
                    (b'R', length, 0)
                ]
            );
            let mut short = TraceIo {
                input: Cursor::new(&[][..]),
                events: Vec::new(),
            };
            {
                let mut driver = Driver::new(&mut short, maximum as u64, maximum, 3).unwrap();
                let request = driver.input.request();
                assert!(matches!(
                    driver.supply(request),
                    Err(Failure::Core(Error::Truncated))
                ));
                assert!(matches!(
                    driver
                        .input
                        .read_into(&mut [0], &mut WorkBudget::new(0), &driver.cancel),
                    Err(Error::Truncated)
                ));
            }
            assert_eq!(short.events, [(b'S', 0, 0), (b'R', 0, maximum)]);
        }
    }
}
