//! Blocking research consumer over core InputCursor and existing format jobs.
//! Parsing helper provenance: this repo's independently authored source-trace
//! adapter. No FFmpeg/third-party implementation code or new core framework.
use super::protocol::{self, Packet, Retention, Stats};
use std::{
    fmt,
    fmt::Write as _,
    io::{self, Read, Seek},
    ops::Range,
    sync::atomic::AtomicBool,
};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, SourceCapabilities, SourceId, Step,
    Time, TimeBase, WorkBudget, blocking::Feed,
};
use triergon_isobmff::{
    BoxEvent, BoxHeader, BoxReader, BoxStep, BoxView, BuildStatus, ConfigurationLimits, EditList,
    FragmentBuilder, FragmentContext, FragmentIndex, ImplicitBase, Limits, MediaDataPolicy,
    MovieHeader, MovieSeekClocks, MovieSeekLimits, MovieSourceSeekContext, MovieSourceSeekPlanner,
    RecoveryDescriptions, RecoveryKind, RecoveryMappings, RecoveryScope, RecoverySyntax, Sample,
    SampleDescription, SampleReader, SampleStep, SampleTable, SampleTableBuilder,
    SampleTablePolicy, SourcePacket, SourceTrack, TrackHeader, TrackSampleCursor, Trex,
    compat::mov::{
        edits::{
            EncodedEditContext, EncodedEditJob, EncodedEditLimits, EncodedEditPlan,
            EncodedEditProfile,
        },
        encoded::{EncodedSelection, EncodedStream, EncodedStreamJob},
        packets::{CursorStatus, DemuxProfile, DemuxTrack, PacketCursor, PacketLimits, StreamView},
        recovery::{
            DescriptionContext, DescriptionRead, DescriptionRecoveryBuilder,
            DescriptionRecoveryProfile, PartialSampleReader, PayloadBounds, RecoveryTrackHeader,
            RunRecoveryProfile, complete_edit_rows,
        },
        seek::{GlobalSeekJob, IndexCursor, IndexLimits, SeekLimits},
    },
    reference_trace::mov::{
        fragment_clock::{FragmentClockProfile, check_signed_decode_runs},
        sample_entry::{RawVisualEntriesJob, SampleEntryHeaderProfile},
        transport::{RawVideoTransportBuilder, RawVideoTransportProfile},
    },
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
pub struct DemuxSeekRequest {
    pub track: i32,
    pub ticks: i64,
    pub count: u32,
    pub after_eof: bool,
}
#[derive(Clone, Copy)]
pub enum DemuxOperation {
    Open,
    Index,
    Seek(DemuxSeekRequest),
}

#[derive(Clone, Copy)]
enum Delivery {
    All,
    ApplyEncodedTrim,
    AfterSeek(u32),
}

impl DemuxOperation {
    fn trace_name(self) -> &'static str {
        match self {
            Self::Open => "mov-native-open-consumer-v33",
            Self::Index => "mov-native-index-consumer-v33",
            Self::Seek(_) => "mov-native-seek-consumer-v33",
        }
    }
}
fn append<T>(target: &mut BudgetVec<T>, value: T) -> Result<()> {
    target.try_push(value).map_err(|e| Failure::Core(e.error))
}
fn demux_catalogue(
    streams: &[protocol::CatalogueStream],
    stats: &mut Stats,
    mut trace: Option<&mut String>,
) -> Result<()> {
    if let Some(out) = trace.as_deref_mut() {
        out.push_str("],\"streams\":[");
    }
    protocol::fold(&mut stats.digest, 0x43);
    protocol::fold(
        &mut stats.digest,
        u64::try_from(streams.len()).map_err(|_| Error::Overflow)?,
    );
    for (i, stream) in streams.iter().enumerate() {
        protocol::consume_stream(&mut stats.digest, i, stream)?;
        if let Some(out) = trace.as_deref_mut() {
            if i != 0 {
                out.push(',');
            }
            protocol::trace_stream(out, i, stream);
        }
    }
    protocol::fold(&mut stats.digest, 0x47);
    protocol::fold(&mut stats.digest, 0);
    if let Some(out) = trace {
        out.push_str("],\"programs\":[]");
    }
    Ok(())
}

fn demux_result(eof: bool, stats: &mut Stats, trace: Option<&mut String>) {
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
    }
}

struct IndexRow {
    ordinal: u64,
    position: i64,
    timestamp: i64,
    flags: i32,
    size: u32,
    min_distance: i32,
}

fn demux_index_stream(
    stream: usize,
    track_id: u32,
    stats: &mut Stats,
    mut trace: Option<&mut String>,
    mut next_entry: impl FnMut() -> Result<Option<IndexRow>>,
) -> Result<()> {
    let (mut count, mut digest) = (0u64, 0u64);
    if let Some(out) = trace.as_deref_mut() {
        if stream != 0 {
            out.push(',');
        }
        write!(
            out,
            "{{\"stream\":{stream},\"pid\":{track_id},\"entries\":["
        )
        .expect("String write");
    }
    while let Some(entry) = next_entry()? {
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
    protocol::fold(
        &mut stats.digest,
        u64::try_from(stream).map_err(|_| Error::Overflow)?,
    );
    protocol::fold(&mut stats.digest, count);
    protocol::append_words(
        &mut stats.digest,
        digest,
        u32::try_from(count.checked_mul(5).ok_or(Error::Overflow)?).map_err(|_| Error::Overflow)?,
    );
    if let Some(out) = trace {
        out.push_str("]}");
    }
    Ok(())
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
/// The child of each kind in `kinds`, found in one walk of `raw`. Each kind's
/// result is its own: a kind that occurs twice is an error only when taken.
fn lookup<const N: usize>(
    raw: &BoxView,
    kinds: [&[u8; 4]; N],
) -> Result<[Result<Option<BoxView>>; N]> {
    let mut cursor = raw.children(0, Limits::default())?;
    let mut found: [Option<BoxView>; N] = std::array::from_fn(|_| None);
    let mut duplicate = [false; N];
    loop {
        match cursor.poll(&mut WorkBudget::new(4096))? {
            BoxStep::Box(child) => {
                if let Some(at) = kinds.iter().position(|&kind| child.header().kind == *kind) {
                    duplicate[at] |= found[at].replace(child).is_some();
                }
            }
            BoxStep::End => break,
            BoxStep::Yield => {}
        }
    }
    Ok(std::array::from_fn(|at| {
        if duplicate[at] {
            Err(Error::Invalid("duplicate required child").into())
        } else {
            Ok(found[at].take())
        }
    }))
}
fn required(child: Result<Option<BoxView>>) -> Result<BoxView> {
    child?.ok_or(Failure::Core(Error::Invalid("missing required child")))
}
fn optional(raw: &BoxView, kind: &[u8; 4]) -> Result<Option<BoxView>> {
    let [child] = lookup(raw, [kind])?;
    child
}
fn child(raw: &BoxView, kind: &[u8; 4]) -> Result<BoxView> {
    required(optional(raw, kind))
}
fn track_header(track: &BoxView) -> Result<(TrackHeader, BoxView)> {
    let [mdia, tkhd] = lookup(track, [b"mdia", b"tkhd"])?;
    let mdia = required(mdia)?;
    let tkhd = required(tkhd)?;
    let [mdhd, hdlr] = lookup(&mdia, [b"mdhd", b"hdlr"])?;
    let header = TrackHeader::parse(&tkhd, &required(mdhd)?, &required(hdlr)?)?;
    Ok((header, mdia))
}
fn self_contained_table(
    mdia: &BoxView,
    budget: &Budget,
    quantum: u64,
    table_policy: SampleTablePolicy,
) -> Result<SampleTable> {
    let minf = child(mdia, b"minf")?;
    let refs = data_references(&minf, budget)?;
    let mut builder = SampleTableBuilder::with_policy(
        child(&minf, b"stbl")?,
        budget.clone(),
        Limits::default(),
        table_policy,
    )?;
    while builder.poll(&mut WorkBudget::new(quantum))? != BuildStatus::Ready {}
    let table = builder.finish()?;
    require_local_references(refs.as_slice(), table.descriptions())?;
    Ok(table)
}

fn data_references(minf: &BoxView, budget: &Budget) -> Result<BudgetVec<BoxView>> {
    let dref = child(&child(minf, b"dinf")?, b"dref")?;
    if word(dref.payload(), 0)? != 0 {
        return Err(Error::Unsupported("data reference version/flags").into());
    }
    let refs = children(&dref, 8, budget)?;
    if refs.len() != usize::try_from(word(dref.payload(), 4)?).map_err(|_| Error::Overflow)? {
        return Err(Error::Invalid("data reference count").into());
    }
    Ok(refs)
}

fn require_local_references(refs: &[BoxView], descriptions: &[SampleDescription]) -> Result<()> {
    for description in descriptions {
        let reference = usize::from(description.data_reference_index)
            .checked_sub(1)
            .ok_or(Error::Invalid("zero data ref"))?;
        let target = refs
            .get(reference)
            .ok_or(Error::Invalid("dangling data reference"))?;
        if target.header().kind != *b"url " || target.payload() != [0, 0, 0, 1] {
            return Err(Error::Unsupported(
                "external/unknown data reference; no file or URL opened",
            )
            .into());
        }
    }
    Ok(())
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
    let extends = child(movie, b"mvex")?;
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
        let (header, media) = track_header(track)?;
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
        let stbl = child(&child(&media, b"minf")?, b"stbl")?;
        let [stsd, stsz, stz2] = lookup(&stbl, [b"stsd", b"stsz", b"stz2"])?;
        let descriptions = word(required(stsd)?.payload(), 4)?;
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
        let sizes = match (stsz?, stz2?) {
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
        let mfhd = child(moof, b"mfhd")?;
        if mfhd.payload().len() != 8 || word(mfhd.payload(), 0)? != 0 {
            return Err(Error::Unsupported("mfhd version/flags/layout").into());
        }
        let mut previous: Option<triergon_core::Result<u64>> = None;
        for traf in children(moof, 0, budget)?
            .as_slice()
            .iter()
            .filter(|b| b.header().kind == *b"traf")
        {
            let tfhd = child(traf, b"tfhd")?;
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

/// Drives the readers over one source, fed through the library's [`Feed`].
pub struct Driver<'a, T> {
    feed: Feed<&'a mut T>,
    pub input: InputCursor,
    pub budget: Budget,
    length: u64,
    window: usize,
    quantum: u64,
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
struct RootWalk {
    headers: BudgetVec<BoxHeader>,
    file_type: Option<BoxView>,
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
    pub regular_recovery: bool,
    pub edit_rows: bool,
}
impl<'a, T: Read + Seek> Driver<'a, T> {
    pub fn new(source: &'a mut T, length: u64, window: usize, quantum: u64) -> Result<Self> {
        if window == 0 || window > 65536 || quantum == 0 {
            return Err(Error::Invalid("consumer window/work policy").into());
        }
        let budget = Budget::new(64 * 1024 * 1024)?;
        let feed = Feed::new(source, &budget)?;
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
            feed,
            input,
            budget,
            length,
            window,
            quantum,
            cancel: AtomicBool::new(false),
        })
    }
    // The sample readers' polling loops call this once per window.
    #[inline(always)]
    fn supply(&mut self, request: InputRequest) -> Result<()> {
        Ok(self.feed.supply(&mut self.input, request)?)
    }
    fn scan(
        &mut self,
        media_data: MediaDataPolicy,
        mut roots: Option<&mut RootWalk>,
    ) -> Result<Scanned> {
        let mut reader =
            BoxReader::with_media_data_policy(&self.input, 16 * 1024 * 1024, media_data)?;
        let (mut movie, mut brand) = (None, None);
        let mut media = BudgetVec::new(&self.budget);
        let mut truncated_media = None;
        let mut moofs = BudgetVec::new(&self.budget);
        let mut indexes = BudgetVec::new(&self.budget);
        while let Some(event) =
            reader.next_event(&mut self.input, &mut self.feed, self.quantum, &self.cancel)?
        {
            match event {
                BoxEvent::Header(header) => {
                    if let Some(roots) = roots.as_deref_mut() {
                        if roots.headers.len() == 64 {
                            return Err(Error::Limit {
                                requested: 65,
                                maximum: 64,
                            }
                            .into());
                        }
                        append(&mut roots.headers, header)?;
                    }
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
                BoxEvent::Skipped(header) => {
                    if header.kind == *b"mdat" {
                        append(&mut media, header.payload_range()?)?;
                    }
                }
                BoxEvent::Metadata(raw) => match &raw.header().kind {
                    b"ftyp" => {
                        if brand.is_some() {
                            return Err(Error::Invalid("duplicate file type box").into());
                        }
                        let b = raw.payload();
                        if b.len() < 8 || b.len() % 4 != 0 {
                            return Err(Error::Invalid("file type box extent").into());
                        }
                        brand = Some(b[..4].try_into().map_err(|_| Error::Truncated)?);
                        if let Some(roots) = roots.as_deref_mut() {
                            roots.file_type = Some(raw);
                        }
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
        let scanned = self.scan(policy.media_data, None)?;
        let movie = MovieHeader::parse(&child(&scanned.movie, b"mvhd")?)?;
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
            let (header, mdia) = track_header(track)?;
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
            let edits = optional(track, b"edts")?
                .map(|raw| {
                    child(&raw, b"elst").and_then(|raw| {
                        let parse = if policy.edit_rows {
                            complete_edit_rows
                        } else {
                            EditList::parse
                        };
                        parse(raw, Limits::default()).map_err(Failure::Core)
                    })
                })
                .transpose()?;
            let edits = edits.filter(|list| !policy.edit_rows || !list.is_empty());
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
    pub fn demux_open(
        &mut self,
        model: &Model,
        stats: &mut Stats,
        retention: &mut Retention,
        trace: Option<&mut String>,
    ) -> Result<()> {
        self.demux_operation(model, stats, retention, DemuxOperation::Open, trace)
    }

    pub fn recovered_regular_operation(
        &mut self,
        operation: DemuxOperation,
        stats: &mut Stats,
        retention: &mut Retention,
        mut trace: Option<&mut String>,
    ) -> Result<()> {
        if matches!(operation, DemuxOperation::Seek(_)) {
            return Err(Error::Unsupported("seek with regular table recovery").into());
        }
        let mut roots = RootWalk {
            headers: BudgetVec::new(&self.budget),
            file_type: None,
        };
        let scanned = self.scan(MediaDataPolicy::Strict, Some(&mut roots))?;
        let children = children(&scanned.movie, 0, &self.budget)?;
        let mut tracks = children
            .as_slice()
            .iter()
            .filter(|raw| raw.header().kind == *b"trak");
        let track = tracks.next().ok_or(Error::Invalid("missing track"))?;
        if tracks.next().is_some()
            || !scanned.moofs.is_empty()
            || scanned.media.len() != 1
            || optional(track, b"edts")?.is_some()
        {
            return Err(
                Error::Unsupported("regular recovery needs one unedited regular track").into(),
            );
        }
        MovieHeader::parse(&child(&scanned.movie, b"mvhd")?)?;
        let mdia = child(track, b"mdia")?;
        let header = RecoveryTrackHeader::parse(
            &child(track, b"tkhd")?,
            &child(&mdia, b"mdhd")?,
            &child(&mdia, b"hdlr")?,
            RunRecoveryProfile,
        )?;
        let minf = child(&mdia, b"minf")?;
        let refs = data_references(&minf, &self.budget)?;
        let profile = DescriptionRecoveryProfile;
        let mut builder = DescriptionRecoveryBuilder::new(
            child(&minf, b"stbl")?,
            self.budget.clone(),
            Limits::default(),
            profile,
        )?;
        while builder.poll(&mut WorkBudget::new(self.quantum), &self.cancel)? != BuildStatus::Ready
        {
        }
        let table = builder.finish()?;
        require_local_references(refs.as_slice(), table.descriptions())?;
        let stream = table.stream_view_for_recovery_header(&header, scanned.brand)?;
        let track_id = header.source().track_id;
        let context = DescriptionContext {
            physical_length: self.length,
            custom_read_maximum: self.window,
        };
        let media = scanned.media.as_slice();
        if let Some(out) = trace.as_deref_mut() {
            let name = operation.trace_name();
            write!(
                out,
                "{{\"schema\":33,\"operation\":\"{name}\",\"packets\":["
            )
            .expect("String write");
        }
        let mut cursor = table.description_packet_cursor(
            &self.budget,
            &header,
            media,
            PacketLimits::default(),
            context,
            profile,
        )?;
        loop {
            match cursor.poll(&mut WorkBudget::new(self.quantum), &self.cancel)? {
                CursorStatus::Yield => continue,
                CursorStatus::End => break,
                CursorStatus::Ready => {}
            }
            let event = cursor.take_event()?;
            if event.read == DescriptionRead::NoPacketForAbsentCatalogue {
                continue;
            }
            let view = event
                .packet
                .ok_or(Error::Invalid("missing projected packet"))?;
            if event.read == DescriptionRead::UnavailablePhysicalStart
                && event.declaration.ordinal == 1
                && view.source_ordinal == 1
                && stats.packets == 0
            {
                let file_type = roots
                    .file_type
                    .take()
                    .ok_or(Error::Invalid("missing file type"))?;
                let mut transport = RawVideoTransportBuilder::new(
                    file_type,
                    scanned.movie.clone(),
                    roots.headers.as_slice(),
                    &table,
                    &self.budget,
                    context,
                    RawVideoTransportProfile,
                )?;
                while transport.poll(&mut WorkBudget::new(self.quantum), &self.cancel)?
                    != BuildStatus::Ready
                {}
                if transport.finish()?.models_initial_eof(view.source_range) {
                    break;
                }
                return Err(Error::Invalid("unreachable initial sample offset").into());
            }
            if event.read != DescriptionRead::Available {
                return Err(Error::Invalid("unreachable sample offset").into());
            }
            let sample = Sample {
                range: view.source_range.clone(),
                ..event.declaration
            };
            let payload = self.packet(sample, media)?.payload;
            let packet = Packet::from_view(0, track_id, &view, payload)?;
            stats.packet(&packet)?;
            if let Some(out) = trace.as_deref_mut() {
                if stats.packets > 1 {
                    out.push(',');
                }
                packet.trace(out);
            }
            retention.retain(packet);
        }
        demux_catalogue(
            &[protocol::CatalogueStream::from(stream)],
            stats,
            trace.as_deref_mut(),
        )?;
        if matches!(operation, DemuxOperation::Index) {
            let mut index = table.description_index_cursor(
                &self.budget,
                &header,
                media,
                IndexLimits::default(),
                context,
                profile,
            )?;
            protocol::fold(&mut stats.digest, 0x49);
            protocol::fold(&mut stats.digest, 1);
            if let Some(out) = trace.as_deref_mut() {
                out.push_str(",\"native_indexes\":[");
            }
            demux_index_stream(0, track_id, stats, trace.as_deref_mut(), || {
                loop {
                    match index.poll(&mut WorkBudget::new(self.quantum), &self.cancel)? {
                        CursorStatus::Yield => {}
                        CursorStatus::End => return Ok(None),
                        CursorStatus::Ready => {
                            if let Some(entry) = index.take_event()?.demux_entry {
                                return Ok(Some(IndexRow {
                                    ordinal: entry.ordinal,
                                    position: entry.position,
                                    timestamp: entry.timestamp,
                                    flags: entry.flags,
                                    size: entry.size,
                                    min_distance: entry.min_distance,
                                }));
                            }
                        }
                    }
                }
            })?;
            if let Some(out) = trace.as_deref_mut() {
                out.push(']');
            }
        }
        demux_result(true, stats, trace);
        Ok(())
    }

    #[cold]
    fn terminal_payload(&mut self, sample: Sample, declared: &Range<u64>) -> Result<Bytes> {
        let profile = RunRecoveryProfile;
        let mut reader = PartialSampleReader::new(
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

    pub fn demux_operation(
        &mut self,
        model: &Model,
        stats: &mut Stats,
        retention: &mut Retention,
        operation: DemuxOperation,
        mut trace: Option<&mut String>,
    ) -> Result<()> {
        if matches!(operation, DemuxOperation::Seek(_)) && model.truncated_media.is_some() {
            return Err(Error::Truncated.into());
        }
        if matches!(operation, DemuxOperation::Open | DemuxOperation::Index) {
            check_signed_decode_runs(model.fragments.as_slice(), FragmentClockProfile)?;
        }
        let mut tracks = BudgetVec::new(&self.budget);
        let mut streams = BudgetVec::new(&self.budget);
        let mut encoded_edits = BudgetVec::new(&self.budget);
        let movie_base = model.movie.time_base()?;
        for record in model.records.as_slice() {
            let mut entries = RawVisualEntriesJob::new(
                &record.header,
                &record.table,
                Limits::default(),
                SampleEntryHeaderProfile,
            );
            while entries.poll(&mut WorkBudget::new(self.quantum), &self.cancel)?
                != BuildStatus::Ready
            {}
            let has_fragments = model
                .fragments
                .as_slice()
                .iter()
                .any(|f| f.track_id() == record.header.track_id);
            let media_base = record.header.time_base()?;
            let mut encoded_edit = None;
            let stream = match record
                .table
                .descriptions()
                .first()
                .map(|entry| entry.format)
            {
                Some(b) if b != *b"raw " && b != *b"tx3g" => {
                    let mut job = EncodedStreamJob::new_demux_catalogue(
                        &record.header,
                        &record.table,
                        &self.input,
                        EncodedSelection {
                            major_brand: model.brand,
                            description: 1,
                            limits: ConfigurationLimits::default(),
                        },
                    )?;
                    while job.poll(
                        &mut self.input,
                        &mut WorkBudget::new(self.quantum),
                        &self.cancel,
                    )? != CursorStatus::Ready
                    {}
                    let configuration = job.take(&mut self.input, &self.cancel)?;
                    let stream =
                        protocol::CatalogueStream::encoded(&configuration, &record.header)?;
                    if !has_fragments
                        && matches!(operation, DemuxOperation::Open | DemuxOperation::Index)
                        && let Some(edits) = record.edits.as_ref()
                        && edits.len() == 1
                        && !record.table.has_avc_composition_offsets()
                    {
                        encoded_edit =
                            Some(self.encoded_edit(record, edits, configuration, movie_base)?);
                    }
                    stream
                }
                _ => StreamView::for_source(
                    &record.header,
                    &record.table,
                    has_fragments,
                    model.brand,
                )?
                .into(),
            };
            append(&mut streams, stream)?;
            if encoded_edit.is_some() && encoded_edits.is_empty() {
                for _ in 0..tracks.len() {
                    append(&mut encoded_edits, None)?;
                }
            }
            if encoded_edit.is_some() || !encoded_edits.is_empty() {
                append(&mut encoded_edits, encoded_edit)?;
            }
            append(
                &mut tracks,
                DemuxTrack {
                    track_id: record.header.track_id,
                    table: &record.table,
                    fragments: if has_fragments {
                        model.fragments.as_slice()
                    } else {
                        &[]
                    },
                    media_base,
                    edits: record.edits.as_ref().map(|e| (e, movie_base)),
                },
            )?;
        }
        let mut drain = Stats::default();
        let seek = match operation {
            DemuxOperation::Seek(r) => Some(r),
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
                let mut cursor = PacketCursor::new_complete_source(
                    &self.budget,
                    tracks.as_slice(),
                    model.media.as_slice(),
                    PacketLimits::default(),
                    DemuxProfile::NoParseNoFillin,
                )?;
                loop {
                    match cursor.poll(&mut WorkBudget::new(self.quantum), &self.cancel)? {
                        CursorStatus::Yield => {}
                        CursorStatus::End => break,
                        CursorStatus::Ready => {
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
                                &row.demux,
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
            let mut job = GlobalSeekJob::new_complete_source(
                &self.budget,
                tracks.as_slice(),
                model.media.as_slice(),
                u32::try_from(r.track).map_err(|_| Error::Unsupported("native signed track ID"))?,
                r.ticks,
                SeekLimits::default(),
                DemuxProfile::NoParseNoFillin,
            )?;
            while job.poll(&mut WorkBudget::new(self.quantum), &self.cancel)? != BuildStatus::Ready
            {
            }
            let plan = job.finish()?;
            self.input
                .reset(plan.landing().selected.occurrence.source.range.start)?;
            protocol::fold(&mut stats.digest, 0x51);
            protocol::fold(&mut stats.digest, 0);
            Some(plan)
        } else {
            None
        };
        if let Some(out) = trace.as_deref_mut() {
            let name = operation.trace_name();
            write!(out, "{{\"schema\":33,\"operation\":\"{name}\"").expect("String write");
            if let Some(r) = seek {
                write!(out,",\"track_id\":{},\"native_ticks\":{},\"after_eof\":{},\"seek_result\":0,\"native_drain\":{{\"packets\":{},\"bytes\":{},\"digest\":{}}}",r.track,r.ticks,r.after_eof,drain.packets,drain.bytes,drain.digest).expect("String write");
            }
            out.push_str(",\"packets\":[");
        }
        let mut completed_packets = None;
        let (reads, eof) = if let Some(plan) = &plan {
            let mut cursor = plan.cursor()?;
            self.deliver_rows(
                model,
                &mut cursor,
                Delivery::AfterSeek(seek.ok_or(Error::Invalid("missing seek request"))?.count),
                stats,
                retention,
                trace.as_deref_mut(),
            )?
        } else {
            let mut cursor = if encoded_edits.is_empty() {
                PacketCursor::new_complete_source(
                    &self.budget,
                    tracks.as_slice(),
                    model.media.as_slice(),
                    PacketLimits::default(),
                    DemuxProfile::NoParseNoFillin,
                )?
            } else {
                PacketCursor::new_encoded_regular(
                    &self.budget,
                    tracks.as_slice(),
                    model.media.as_slice(),
                    PacketLimits::default(),
                    encoded_edits.as_slice(),
                    EncodedEditProfile,
                )?
            };
            let result = self.deliver_rows(
                model,
                &mut cursor,
                if encoded_edits.is_empty() {
                    Delivery::All
                } else {
                    Delivery::ApplyEncodedTrim
                },
                stats,
                retention,
                trace.as_deref_mut(),
            )?;
            if matches!(operation, DemuxOperation::Index) && result.1 {
                completed_packets = Some(cursor);
            }
            result
        };
        demux_catalogue(streams.as_slice(), stats, trace.as_deref_mut())?;
        if matches!(operation, DemuxOperation::Index) {
            self.demux_indexes(
                model,
                tracks.as_slice(),
                completed_packets.as_ref(),
                stats,
                trace.as_deref_mut(),
            )?;
        }
        demux_result(eof, stats, trace.as_deref_mut());
        if let (Some(out), Some(r)) = (trace, seek) {
            write!(
                out,
                ",\"read_calls_after_seek\":{reads},\"delivery_limit_reached\":{}",
                !eof && stats.packets >= u64::from(r.count)
            )
            .expect("String write");
        }
        Ok(())
    }
    fn encoded_edit(
        &mut self,
        record: &Record,
        edits: &EditList,
        configuration: EncodedStream,
        movie_base: TimeBase,
    ) -> Result<EncodedEditPlan> {
        let configs = [configuration];
        let mut job = EncodedEditJob::new_range_playback(
            EncodedEditContext {
                table: &record.table,
                edits,
                stream: &configs[0],
                media_base: record.header.time_base()?,
                movie_base,
            },
            &configs,
            &self.input,
            EncodedEditLimits::default(),
        )?;
        loop {
            match job.poll(
                &mut self.input,
                &mut WorkBudget::new(self.quantum),
                &self.cancel,
            ) {
                Ok(CursorStatus::Ready) => {
                    return Ok(job.take(&mut self.input, &self.cancel)?);
                }
                Ok(CursorStatus::Yield) => {}
                Ok(CursorStatus::End) => {
                    return Err(Error::Invalid("encoded edit ended without a plan").into());
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn deliver_rows(
        &mut self,
        model: &Model,
        cursor: &mut PacketCursor<'_>,
        delivery: Delivery,
        stats: &mut Stats,
        retention: &mut Retention,
        mut trace: Option<&mut String>,
    ) -> Result<(u64, bool)> {
        let limit = match delivery {
            Delivery::AfterSeek(count) => Some(count),
            _ => None,
        };
        let (mut reads, mut first) = (0u64, true);
        let mut short_end = None;
        while limit.is_none_or(|maximum| stats.packets < u64::from(maximum)) {
            if limit.is_some() {
                reads = reads.checked_add(1).ok_or(Error::Overflow)?;
            }
            loop {
                match cursor.poll(&mut WorkBudget::new(self.quantum), &self.cancel)? {
                    CursorStatus::Yield => {}
                    CursorStatus::End => return Ok((reads, true)),
                    CursorStatus::Ready => break,
                }
            }
            let skip = if matches!(delivery, Delivery::ApplyEncodedTrim) {
                cursor.ready_encoded_skip()?
            } else {
                None
            };
            let item = cursor.take_occurrence()?;
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
                    PayloadBounds::from_source(
                        &item.source,
                        declared.clone(),
                        self.length,
                        MAX_SAMPLE_BYTES,
                        RunRecoveryProfile,
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
            let mut packet = Packet::from_view(index, item.track_id, &item.demux, payload)?;
            packet.skip_samples = skip;
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
    fn demux_indexes<'model>(
        &self,
        model: &'model Model,
        tracks: &[DemuxTrack<'model>],
        completed_packets: Option<&PacketCursor<'model>>,
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
            let limits = IndexLimits::default();
            let mut index = if let Some(cursor) = completed_packets {
                IndexCursor::replay_track(cursor, track.track_id, limits.max_entries)?
            } else {
                IndexCursor::new_complete_source(
                    &self.budget,
                    *track,
                    model.media.as_slice(),
                    limits,
                    DemuxProfile::NoParseNoFillin,
                )?
            };
            demux_index_stream(i, track.track_id, stats, trace.as_deref_mut(), || {
                loop {
                    match index.poll(&mut WorkBudget::new(self.quantum), &self.cancel)? {
                        CursorStatus::Yield => {}
                        CursorStatus::End => return Ok(None),
                        CursorStatus::Ready => {
                            let entry = index.take_entry()?;
                            return Ok(Some(IndexRow {
                                ordinal: entry.ordinal,
                                position: entry.position,
                                timestamp: entry.timestamp,
                                flags: entry.flags,
                                size: entry.size,
                                min_distance: entry.min_distance,
                            }));
                        }
                    }
                }
            })?;
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
impl<T: Read + Seek> Driver<'_, T> {
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
        let mut backings = BudgetVec::new(&self.budget);
        let mut next = record
            .table
            .sample_count()
            .checked_add(1)
            .ok_or(Error::Overflow)?;
        if record.table.sample_count() != 0 {
            append(
                &mut backings,
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
                &mut backings,
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
        for backing in backings.as_slice() {
            append(
                &mut scopes,
                RecoveryScope {
                    kind: request.kind,
                    first_ordinal: backing.first,
                    sample_count: backing.count,
                    fragment_ordinal: backing.fragment,
                    track_descriptions: track_descriptions.as_ref(),
                    local_descriptions: backing.descriptions.as_ref(),
                    mappings: backing.mappings.as_ref(),
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
        protocol::source_landings(out, &result);
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
            self.input.reset(plan.landing().decode_start.range.start)?;
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
                    .landing()
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
    use std::io::{Cursor, SeekFrom};

    #[test]
    fn child_lookup_validates_the_entire_parent() {
        let budget = Budget::new(4096).unwrap();
        let parent = |wire: &[u8]| {
            BoxView::new(Bytes::copy_from_slice(&budget, wire).unwrap(), 100).unwrap()
        };
        let valid = parent(b"\0\0\0\x18moov\0\0\0\x08trak\0\0\0\x08free");
        assert_eq!(child(&valid, b"trak").unwrap().header().start, 108);
        assert!(optional(&valid, b"mvhd").unwrap().is_none());

        let duplicate = parent(b"\0\0\0\x18moov\0\0\0\x08trak\0\0\0\x08trak");
        assert!(matches!(
            child(&duplicate, b"trak"),
            Err(Failure::Core(Error::Invalid("duplicate required child")))
        ));

        let trailing = parent(b"\0\0\0\x19moov\0\0\0\x08trak\0\0\0\x08trak\0");
        assert!(matches!(
            child(&trailing, b"trak"),
            Err(Failure::Core(Error::Truncated))
        ));
    }

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
    fn regular_recovery_preserves_packet_and_index_boundaries() {
        for (bytes, packets) in [
            (
                include_bytes!("../../tests/fixtures/bad-zero-timescale.mov").as_slice(),
                6,
            ),
            (
                include_bytes!("../../tests/fixtures/bad-stsc-description-zero.mov").as_slice(),
                5,
            ),
            (
                include_bytes!("../../tests/fixtures/bad-chunk-offset-outside-source.mov")
                    .as_slice(),
                0,
            ),
        ] {
            for (window, quantum) in [(1, 3), (65536, 4096)] {
                let mut input = Cursor::new(bytes);
                let mut driver =
                    Driver::new(&mut input, bytes.len() as u64, window, quantum).unwrap();
                let mut retained = Retention::new(&driver.budget, 0).unwrap();
                let mut stats = Stats::default();
                let mut trace = String::new();
                driver
                    .recovered_regular_operation(
                        DemuxOperation::Index,
                        &mut stats,
                        &mut retained,
                        Some(&mut trace),
                    )
                    .unwrap();
                assert_eq!(stats.packets, packets);
                assert!(trace.contains("\"confirmed_eof\":true"));
                if packets == 0 {
                    assert!(trace.contains("\"entries\":[{\"ordinal\":0,\"position\":4843,"));
                }
            }
        }
    }

    #[test]
    fn regular_recovery_refuses_an_unreachable_initial_offset() {
        let mut bytes = include_bytes!("../../tests/fixtures/r-front-stco.mov").to_vec();
        let stco = bytes.windows(4).position(|kind| kind == b"stco").unwrap();
        bytes[stco + 12..stco + 16].copy_from_slice(&40_000u32.to_be_bytes());
        let length = bytes.len() as u64;
        let mut input = Cursor::new(bytes);
        let mut driver = Driver::new(&mut input, length, 65536, 4096).unwrap();
        let mut retained = Retention::new(&driver.budget, 0).unwrap();
        assert!(matches!(
            driver.recovered_regular_operation(
                DemuxOperation::Open,
                &mut Stats::default(),
                &mut retained,
                None
            ),
            Err(Failure::Core(Error::Invalid(
                "unreachable initial sample offset"
            )))
        ));
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

#[cfg(test)]
mod private_configuration_comparison {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn malformed_opus_configuration_refuses_open_and_index() {
        let original = include_bytes!("../../tests/fixtures/r-private-carriage.mov");
        let mut control = original.to_vec();
        for kind in [b"dOps", b"ddts"] {
            let at = control.windows(4).position(|bytes| bytes == kind).unwrap();
            control[at..at + 4].copy_from_slice(b"free");
        }
        for (bytes, rejected) in [(original.as_slice(), true), (control.as_slice(), false)] {
            for (window, quantum) in [(65536, 4096), (1, 3)] {
                let mut input = Cursor::new(bytes);
                let mut driver =
                    Driver::new(&mut input, bytes.len() as u64, window, quantum).unwrap();
                let model = driver.open().unwrap();
                let entry = model.records.as_slice()[0].table.descriptions()[0].raw();
                let start = entry.header().start as usize;
                assert_eq!(
                    entry.bytes().as_ref(),
                    &bytes[start..start + entry.bytes().len()]
                );
                for operation in [DemuxOperation::Open, DemuxOperation::Index] {
                    let mut stats = Stats::default();
                    let mut retention = Retention::new(&driver.budget, 0).unwrap();
                    let mut trace = String::new();
                    let result = driver.demux_operation(
                        &model,
                        &mut stats,
                        &mut retention,
                        operation,
                        Some(&mut trace),
                    );
                    if rejected {
                        assert!(matches!(
                            result,
                            Err(Failure::Core(Error::Invalid("sample entry configuration")))
                        ));
                        assert!(trace.is_empty());
                    } else {
                        result.unwrap();
                        assert_eq!(stats.packets, 6);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod signed_decode_comparison {
    use super::*;
    use std::io::Cursor;

    fn check(bytes: &[u8], decode_end: Option<i128>, admitted: bool) {
        for (window, quantum) in [(65536, 4096), (1, 3)] {
            for operation in [DemuxOperation::Open, DemuxOperation::Index] {
                let mut input = Cursor::new(bytes);
                let mut driver = Driver::new(
                    &mut input,
                    u64::try_from(bytes.len()).unwrap(),
                    window,
                    quantum,
                )
                .unwrap();
                let model = driver.open().unwrap();
                if let Some(end) = decode_end {
                    let [fragment] = model.fragments.as_slice() else {
                        panic!("expected one fragment");
                    };
                    assert_eq!(fragment.decode_end(), Some(end));
                }
                let mut stats = Stats::default();
                let mut retention = Retention::new(&driver.budget, 0).unwrap();
                let mut trace = String::new();
                let result = driver.demux_operation(
                    &model,
                    &mut stats,
                    &mut retention,
                    operation,
                    Some(&mut trace),
                );
                if admitted {
                    result.unwrap();
                    assert_eq!(stats.packets, 6);
                    assert!(trace.contains("\"confirmed_eof\":true"));
                } else {
                    assert!(matches!(
                        result,
                        Err(Failure::Core(Error::Invalid(
                            "native signed decode run overflow"
                        )))
                    ));
                    assert!(trace.is_empty());
                }
            }
        }
    }

    #[test]
    fn signed_comparison_accepts_an_exact_end_and_rejects_one_tick_beyond() {
        let original = include_bytes!("../../tests/fixtures/native-clock28/clock27-i63-step12.mp4");
        let header = b"\x00\x00\x00\x14tfdt\x01\x00\x00\x00";
        let at = original
            .windows(header.len())
            .position(|window| window == header)
            .unwrap()
            + header.len();
        let mut bytes = original.to_vec();
        // The six sample durations sum to nine, including the final sample.
        for (distance, admitted) in [(9, true), (8, false)] {
            let start = i64::MAX.unsigned_abs() - distance;
            bytes[at..at + 8].copy_from_slice(&start.to_be_bytes());
            check(&bytes, Some(i128::from(start) + 9), admitted);
        }
    }

    #[test]
    fn signed_comparison_preserves_large_legacy_and_modern_starts() {
        for bytes in [
            include_bytes!("../../tests/fixtures/clock-boundary27/f-tfdt-above-i64.mp4").as_slice(),
            include_bytes!("../../tests/fixtures/clock-boundary27/modern-iso6-tfdt-above-i64.mp4")
                .as_slice(),
            include_bytes!("../../tests/fixtures/native-clock28/clock27-u64-minus1-step12.mp4")
                .as_slice(),
        ] {
            check(bytes, None, true);
        }
    }
}
