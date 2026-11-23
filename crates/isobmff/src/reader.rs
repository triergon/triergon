//! [`Reader`], the source view through the neutral container model.
use crate::fragment::FragmentCursorPosition;
use crate::open::{
    FragmentTrack, MetadataReadScope, Movie, MovieOpen, MovieTrack, OpenLimits, OpenStep,
};
use crate::table::SampleCursorPosition;
use crate::{
    Access, BuildStatus, Next, Sample,
    boxes::{BoxHeader, BoxView},
    config::{ConfigurationLimits, EntryConfiguration, codec_id},
    fields,
    movie::{EditList, MovieHeader, SampleDescription, TrackHeader},
    table::SampleTable,
};
use forward::Forward;
use std::mem;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::container::{
    CodecId, ContainerReader, Event as Neutral, MediaKind, PacketInfo, RandomAccess, ReaderStep,
    SeekMethod, SeekPolicy, SeekReceipt, SeekRequest, TrackId, TrackInfo,
};
use triergon_core::{
    Backpressure, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputToken, InputWindow,
    Result, Rounding, SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, Time,
    TimeBase, WorkBudget,
};

mod forward;

/// The media kind of an `hdlr` handler code, or [`MediaKind::Unknown`].
pub fn media_kind(handler: &[u8; 4]) -> MediaKind {
    match handler {
        b"vide" => MediaKind::Video,
        b"soun" => MediaKind::Audio,
        b"sbtl" | b"text" | b"subt" | b"subp" => MediaKind::Subtitle,
        b"meta" | b"hint" | b"tmcd" => MediaKind::Data,
        _ => MediaKind::Unknown,
    }
}

/// The ISO 639-2/T code `mdhd` packs, or `None` for a QuickTime language code.
fn packed_language(mdhd: &[u8]) -> Result<Option<[u8; 3]>> {
    let (version, _) = fields::full(mdhd)?;
    let at = match version {
        0 => 20,
        1 => 32,
        _ => return Err(Error::Unsupported("clock header version")),
    };
    let packed = fields::u16_at(mdhd, at)?;
    let letter = |shift: u16| {
        let value = u8::try_from((packed >> shift) & 31).ok()?;
        (1..=26).contains(&value).then_some(value + 0x60)
    };
    if packed & 0x8000 != 0 {
        return Ok(None);
    }
    Ok(letter(10)
        .zip(letter(5))
        .zip(letter(0))
        .map(|((a, b), c)| [a, b, c]))
}

/// Bounds of one [`Reader`], on top of the caller's memory [`Budget`](triergon_core::Budget).
#[derive(Clone, Copy, Debug)]
pub struct ReaderLimits {
    /// Bounds of the movie open: root boxes, retained metadata and tables.
    pub open: OpenLimits,
    /// Maximum sample payload in bytes. The default is 64 MiB.
    pub packet_bytes: usize,
    /// Bounds of reading each codec configuration from its sample entry.
    pub configuration: ConfigurationLimits,
}
impl Default for ReaderLimits {
    fn default() -> Self {
        Self {
            open: OpenLimits::default(),
            packet_bytes: 64 << 20,
            configuration: ConfigurationLimits::default(),
        }
    }
}
impl ReaderLimits {
    /// The payload length of `sample`, within [`packet_bytes`](Self::packet_bytes).
    #[inline]
    fn payload_length(&self, sample: &Sample) -> Result<usize> {
        let length =
            usize::try_from(sample.range.end - sample.range.start).map_err(|_| Error::Overflow)?;
        if length > self.packet_bytes {
            return Err(Error::Limit {
                requested: length,
                maximum: self.packet_bytes,
            });
        }
        Ok(length)
    }
}

/// One sample and its payload. Timestamps are ticks of the media timescale,
/// without edits; the duration is the sample's decode delta.
#[derive(Clone, Debug)]
pub struct Packet {
    track: u32,
    base: TimeBase,
    pts: Option<i64>,
    dts: Option<i64>,
    sample: Sample,
    payload: Bytes,
}
impl Packet {
    /// The sample's source ordinal, description, range, clocks and flags.
    pub fn sample(&self) -> &Sample {
        &self.sample
    }
}
impl PacketInfo for Packet {
    fn track(&self) -> TrackId {
        TrackId(u64::from(self.track))
    }
    fn payload(&self) -> &[u8] {
        self.payload.as_ref()
    }
    fn time_base(&self) -> TimeBase {
        self.base
    }
    fn pts(&self) -> Option<i64> {
        self.pts
    }
    fn dts(&self) -> Option<i64> {
        self.dts
    }
    fn duration(&self) -> Option<i64> {
        Some(i64::from(self.sample.duration_ticks))
    }
    fn random_access(&self) -> RandomAccess {
        match self.sample.access {
            Access::ContainerSync => RandomAccess::Sync,
            Access::NonSync => RandomAccess::NotSync,
            Access::Unknown => RandomAccess::Unknown,
        }
    }
}

/// One `trak`: its headers, its index and the reader's place in it.
#[derive(Debug)]
pub struct Track {
    source: MovieTrack,
    base: TimeBase,
    language: Option<[u8; 3]>,
    /// The first sample entry's identity, once read.
    entry: Option<EntryConfiguration>,
    start: SampleCursorPosition,
    regular: Option<SampleCursorPosition>,
    fragment: usize,
    fragment_position: FragmentCursorPosition,
    head: Option<Sample>,
}
impl Track {
    /// The track's `tkhd`, `mdhd` and `hdlr` facts.
    pub fn header(&self) -> &TrackHeader {
        self.source.header()
    }
    /// The regular sample table, which holds no samples in a purely fragmented track.
    pub fn table(&self) -> &SampleTable {
        self.source.table()
    }
    /// Every sample entry; [`TrackInfo`] describes the first.
    pub fn descriptions(&self) -> &[SampleDescription] {
        self.table().descriptions()
    }
    /// The source edit list, which packet times do not apply.
    pub fn edits(&self) -> Option<&EditList> {
        self.source.edits()
    }
    /// The object type `esds` states for an `mp4a` entry.
    pub fn object_type(&self) -> Option<u8> {
        self.entry
            .as_ref()
            .and_then(EntryConfiguration::object_type)
    }
    fn rewind(&mut self) {
        self.regular = Some(self.start);
        self.fragment = 0;
        self.fragment_position = FragmentCursorPosition::default();
        self.head = None;
    }
    /// Forgets the fragments whose samples have all been taken, except the last, which
    /// states where the next fragment's decode clock starts.
    fn release_delivered(&mut self) {
        let fragments = &mut self.source.fragments;
        let delivered = self.fragment.min(fragments.len().saturating_sub(1));
        fragments.rotate_left(delivered);
        fragments.truncate(fragments.len() - delivered);
        self.fragment -= delivered;
    }
    /// Make the next sample the head. `false` means the work is spent; `true`
    /// with no head means the track has ended.
    fn fill(&mut self, work: &mut WorkBudget) -> Result<bool> {
        if self.head.is_some() {
            return Ok(true);
        }
        if let Some(position) = self.regular {
            let mut cursor = self.source.table().resume(position);
            match cursor.poll(work)? {
                Next::Item(sample) => {
                    self.regular = Some(cursor.position());
                    self.head = Some(sample);
                    return Ok(true);
                }
                Next::Yield => return Ok(false),
                Next::End => self.regular = None,
            }
        }
        while let Some(fragment) = self.source.fragments().get(self.fragment) {
            let mut cursor = fragment.resume(self.fragment_position);
            match cursor.poll(work)? {
                Next::Item(sample) => {
                    self.fragment_position = cursor.position();
                    self.head = Some(sample);
                    return Ok(true);
                }
                Next::Yield => return Ok(false),
                Next::End => {
                    self.fragment += 1;
                    self.fragment_position = FragmentCursorPosition::default();
                }
            }
        }
        Ok(true)
    }
}
impl FragmentTrack for Track {
    fn opened(&self) -> &MovieTrack {
        &self.source
    }
    fn opened_mut(&mut self) -> &mut MovieTrack {
        &mut self.source
    }
}
impl TrackInfo for Track {
    fn id(&self) -> TrackId {
        TrackId(u64::from(self.header().track_id))
    }
    fn kind(&self) -> MediaKind {
        media_kind(&self.header().handler)
    }
    /// The first sample entry code.
    fn codec_tag(&self) -> &[u8] {
        self.descriptions()
            .first()
            .map_or(&[], |entry| entry.format.as_slice())
    }
    fn codec(&self) -> CodecId {
        match &self.entry {
            Some(entry) => entry.codec(),
            None => self.descriptions().first().map_or(CodecId::Other, |entry| {
                codec_id(&self.header().handler, &entry.format, None)
            }),
        }
    }
    /// The configuration record's payload (AudioSpecificConfig for AAC), empty
    /// when it is absent, or the whole sample entry payload when the entry has
    /// no known record.
    fn configuration(&self) -> &[u8] {
        match (&self.entry, self.descriptions().first()) {
            (Some(identity), Some(entry)) => identity.bytes(entry),
            _ => &[],
        }
    }
    /// The `mdhd` code; `None` for a QuickTime language code.
    fn language(&self) -> Option<&str> {
        self.language
            .as_ref()
            .and_then(|code| std::str::from_utf8(code).ok())
    }
}

/// Reader events other than packets and seeks.
#[derive(Clone, Debug)]
pub enum ReaderEvent {
    /// A root box was passed. On a source without random access an `mdat` is passed once
    /// its samples have been delivered.
    Box(BoxHeader),
    /// The movie is indexed: tracks are available and packets follow.
    Movie,
}

/// Where a reader stands. The tag is explicit, so that the per-packet dispatch reads one
/// byte.
// Boxing the opener would allocate on every open and reset.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
#[repr(u8)]
enum Phase {
    /// Walking the root boxes.
    Open(MovieOpen) = 0,
    /// Giving each track of the opened movie its reading state.
    Tracks(BudgetVec<MovieTrack>) = 1,
    /// Identifying the first sample entry of track `entry` on.
    Entries {
        entry: usize,
    } = 2,
    /// Delivering packets by range requests.
    Packets = 3,
    Seek(Seeking) = 4,
    Ended = 5,
    /// Delivering packets as their bytes arrive, on a source without random access.
    Forward(Forward) = 6,
}
impl Phase {
    /// The phase that delivers the packets of `movie`. Its open chose how: an open that
    /// read every root box is followed by range requests, one that stopped at the first
    /// media box by reading on from there.
    fn delivery(movie: Option<&Movie>) -> Result<Self> {
        match movie {
            Some(Movie {
                read_scope: MetadataReadScope::FrontMetadataBeforeMdat(media),
                trex,
                ..
            }) => Ok(Self::Forward(Forward::new(*media, trex)?)),
            _ => Ok(Self::Packets),
        }
    }
}

#[derive(Debug)]
struct Reading {
    track: usize,
    sample: Sample,
    buffer: Option<BudgetVec<u8>>,
}

/// How far a payload read got.
enum Read {
    /// The payload is complete.
    Payload(Bytes),
    /// What to return, or `None` to read on.
    Pending(Option<Output>),
}

#[derive(Clone, Copy, Debug)]
struct Landing {
    ordinal: u64,
    time: Time,
}

#[derive(Debug)]
struct Seeking {
    track: usize,
    policy: SeekPolicy,
    requested: Time,
    /// The target in the track's ticks, rounded down, so `cts <= target` is exact.
    target: i128,
    stage: SeekStage,
}

/// What a seek is doing.
#[derive(Debug)]
enum SeekStage {
    /// Scanning the sought track for its landing: the ordinal and composition time of
    /// its first sync sample and of the latest one at or before the target.
    Search {
        first: Option<(u64, i128)>,
        best: Option<(u64, i128)>,
    },
    /// Positioning every track at the landing: the next track to position, the first
    /// tick it may resume at, and whether a sample was passed over on the way.
    Position {
        landing: Landing,
        at: usize,
        resume: Option<i128>,
        skipped: bool,
    },
}

type Output = Step<Neutral<Packet, ReaderEvent>, Backpressure>;

/// The source view through the neutral container model: the one type to open a file and
/// read its packets with. It walks the root boxes, indexes every track, then delivers
/// samples in ascending byte order across tracks and decode order within each. Times are
/// media ticks without edits, which stay on [`Track::edits`]; a fragment without `tfdt`
/// continues its track's decode clock. Boxes the neutral model has no field for stay
/// readable through [`Reader::movie_box`] and [`SampleDescription::raw`]. Errors are
/// sticky until [`Reader::reset`].
///
/// How the source is read is chosen once, from its capabilities:
///
/// - With random access the open reads every root box, wherever `moov` lies, and each
///   sample is asked for by its range.
/// - Without it the reader only moves forward. `moov` must precede the first `mdat`
///   (`Error::Unsupported` otherwise); the open ends there, and each later `moof` is
///   indexed when the cursor reaches it. A sample that starts before the read position,
///   as when one track's chunks go back in the file or a fragment's samples precede its
///   `moof`, is `Error::Unsupported("sample before the read position needs random
///   access")`. A `moof` is held until its samples are delivered, so memory does not grow
///   with the source. There is no seek.
#[derive(Debug)]
pub struct Reader {
    input: InputCursor,
    limits: ReaderLimits,
    phase: Phase,
    movie: Option<Movie>,
    tracks: BudgetVec<Track>,
    reading: Option<Reading>,
    failed: Option<Error>,
}

impl Reader {
    /// A reader for one source, which asks for at most `window` bytes at a time.
    /// `Error::Invalid` for limits outside the ranges [`OpenLimits`] states or a zero
    /// window.
    pub fn new(
        pool: &triergon_core::Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: ReaderLimits,
        window: usize,
    ) -> Result<Self> {
        let input = InputCursor::new(pool, source, capabilities, window)?;
        let opener = MovieOpen::for_source(&input, limits.open)?;
        Ok(Self {
            input,
            limits,
            phase: Phase::Open(opener),
            movie: None,
            tracks: BudgetVec::new(pool),
            reading: None,
            failed: None,
        })
    }
    /// Track catalogue in `moov` order, empty until [`ReaderEvent::Movie`].
    pub fn tracks(&self) -> &[Track] {
        if self.movie_open() {
            self.tracks.as_slice()
        } else {
            &[]
        }
    }
    fn movie_open(&self) -> bool {
        matches!(
            self.phase,
            Phase::Packets | Phase::Seek(_) | Phase::Ended | Phase::Forward(_)
        )
    }
    /// The `ftyp` major brand; that of the first `ftyp` where the source has several.
    pub fn major_brand(&self) -> Option<[u8; 4]> {
        self.movie.as_ref().and_then(Movie::major_brand)
    }
    /// The retained `moov`, including what the neutral view does not type.
    pub fn movie_box(&self) -> Option<&BoxView> {
        self.movie
            .as_ref()?
            .raw_roots()
            .iter()
            .find(|raw| raw.header().kind == *b"moov")
    }
    /// The `mvhd` facts, once the root boxes have been walked.
    pub fn movie_header(&self) -> Option<&MovieHeader> {
        self.movie.as_ref().map(Movie::header)
    }
    /// Payload extents of the `mdat` boxes the open read, in source order: every one with
    /// random access, the first without.
    pub fn media_extents(&self) -> &[Range<u64>] {
        self.movie
            .as_ref()
            .map_or(&[], |movie| movie.media.as_slice())
    }

    fn advance(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Output> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let output = match &mut self.phase {
                Phase::Open(opener) => match open_step(opener, &mut self.input, work, cancel)? {
                    OpenStep::Yield => Some(Step::Yield),
                    OpenStep::NeedInput(request) => Some(Step::NeedInput(request)),
                    OpenStep::Root(header) => {
                        Some(Step::Ready(Neutral::Other(ReaderEvent::Box(header))))
                    }
                    OpenStep::Ready => {
                        let mut movie = opener.finish(&self.input)?;
                        let sources = mem::replace(&mut movie.tracks, BudgetVec::new(&movie.pool));
                        self.movie = Some(movie);
                        self.phase = Phase::Tracks(sources);
                        None
                    }
                },
                Phase::Tracks(sources) => {
                    self.tracks.try_reserve_exact(sources.len())?;
                    work.take(sources.len() as u64);
                    let sources = mem::replace(sources, BudgetVec::new(self.tracks.budget()));
                    adopt_tracks(&mut self.tracks, sources)?;
                    self.phase = Phase::Entries { entry: 0 };
                    None
                }
                Phase::Entries { entry } => {
                    let output =
                        identify_entry(self.tracks.as_mut_slice(), entry, &self.limits, work)?;
                    // The movie is announced once every entry is identified.
                    if matches!(output, Some(Step::Ready(_))) {
                        self.phase = Phase::delivery(self.movie.as_ref())?;
                    }
                    output
                }
                Phase::Packets => self.packet(work, cancel)?,
                Phase::Forward(forward) => forward.poll(
                    &mut self.input,
                    self.tracks.as_mut_slice(),
                    &mut self.reading,
                    &self.limits,
                    work,
                    cancel,
                )?,
                Phase::Seek(seeking) => {
                    let output = seeking.poll(self.tracks.as_mut_slice(), work)?;
                    if matches!(output, Some(Step::Ready(_))) {
                        self.phase = Phase::Packets;
                    }
                    output
                }
                Phase::Ended => Some(Step::End(SourceLocation {
                    source: self.input.source(),
                    offset: self.input.position(),
                })),
            };
            if let Some(output) = output {
                return Ok(output);
            }
        }
    }

    fn packet(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Option<Output>> {
        let mut reading = if let Some(reading) = self.reading.take() {
            reading
        } else {
            for track in self.tracks.as_mut_slice() {
                if !track.fill(work)? {
                    return Ok(Some(Step::Yield));
                }
            }
            let Some((track, sample)) = take_earliest(self.tracks.as_mut_slice()) else {
                self.phase = Phase::Ended;
                return Ok(None);
            };
            if !work.try_charge(1) {
                self.tracks.as_mut_slice()[track].head = Some(sample);
                return Ok(Some(Step::Yield));
            }
            self.begin_reading(track, sample)?
        };
        match Self::read_payload(&mut self.input, &mut reading, work, cancel) {
            Ok(Read::Payload(payload)) => {
                Self::finish_packet(self.tracks.as_slice(), reading, payload).map(Some)
            }
            Ok(Read::Pending(output)) => {
                self.reading = Some(reading);
                Ok(output)
            }
            Err(error) => {
                self.reading = Some(reading);
                Err(error)
            }
        }
    }

    fn begin_reading(&mut self, track: usize, sample: Sample) -> Result<Reading> {
        let extents = self.media_extents();
        let after = extents.partition_point(|extent| extent.start <= sample.range.start);
        let extent = after
            .checked_sub(1)
            .and_then(|at| extents.get(at))
            .ok_or(Error::Invalid("sample outside media extent"))?;
        sample.require_within(extent.clone())?;
        let length = self.limits.payload_length(&sample)?;
        if length != 0 && self.input.position() != sample.range.start {
            self.input.request_range(sample.range.start)?;
        }
        Ok(Reading {
            track,
            sample,
            buffer: None,
        })
    }

    /// Reads on in the payload of `reading`, which starts at the cursor's position or has
    /// been read in part.
    // Part of each delivery loop: out of line, every packet pays this call and
    // `finish_packet`'s, both answering through memory (1.4% of an MP4 subtitle read).
    #[inline(always)]
    fn read_payload(
        input: &mut InputCursor,
        reading: &mut Reading,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Read> {
        let pool = input.budget().clone();
        let range = &reading.sample.range;
        let length = usize::try_from(range.end - range.start).map_err(|_| Error::Overflow)?;
        if length == 0 {
            return Ok(Read::Payload(Bytes::copy_from_slice(&pool, &[])?));
        }
        if let Some(buffer) = reading.buffer.take_if(|buffer| buffer.len() == length) {
            return match buffer.freeze() {
                Ok(payload) => Ok(Read::Payload(payload)),
                Err(rejected) => {
                    reading.buffer = Some(rejected.value);
                    Err(rejected.error)
                }
            };
        }
        if input.buffered().is_empty()
            && let Some(request) = input.pending_request(cancel)?
        {
            return Ok(Read::Pending(Some(Step::NeedInput(request))));
        }
        // A sample inside one window shares it; only a split sample is copied.
        if reading.buffer.is_none()
            && input.buffered().len() >= length
            && work.remaining() >= length as u64
        {
            return match input.read_span(length, work, cancel)? {
                Step::Ready(payload) => Ok(Read::Payload(payload)),
                Step::NeedInput(request) => Ok(Read::Pending(Some(Step::NeedInput(request)))),
                Step::Yield => Ok(Read::Pending(Some(Step::Yield))),
                Step::Backpressure(never) => match never {},
                Step::End(_) => Err(Error::Truncated),
            };
        }
        let buffer = if let Some(buffer) = &mut reading.buffer {
            buffer
        } else {
            let mut buffer = BudgetVec::new(&pool);
            buffer.try_reserve_exact(length)?;
            reading.buffer.insert(buffer)
        };
        let filled = buffer.len();
        Ok(Read::Pending(
            match input.read_append(buffer, length - filled, work, cancel)? {
                Step::Ready(_) => None,
                Step::NeedInput(request) => Some(Step::NeedInput(request)),
                Step::Yield => Some(Step::Yield),
                Step::Backpressure(never) => match never {},
                Step::End(_) => return Err(Error::Truncated),
            },
        ))
    }

    #[inline(always)]
    fn finish_packet(tracks: &[Track], reading: Reading, payload: Bytes) -> Result<Output> {
        let Reading { track, sample, .. } = reading;
        let track = &tracks[track];
        let ticks = |value: Option<i128>| {
            value
                .map(|ticks| i64::try_from(ticks).map_err(|_| Error::Overflow))
                .transpose()
        };
        Ok(Step::Ready(Neutral::Packet(Packet {
            track: track.header().track_id,
            base: track.base,
            pts: ticks(sample.cts_ticks)?,
            dts: ticks(sample.dts_ticks)?,
            sample,
            payload,
        })))
    }
}

/// One step of the open.
// Out of line: inlined, the opener's frame becomes the frame of every packet step.
#[inline(never)]
fn open_step(
    opener: &mut MovieOpen,
    input: &mut InputCursor,
    work: &mut WorkBudget,
    cancel: &AtomicBool,
) -> Result<OpenStep> {
    opener.advance(input, work, cancel)
}

/// Takes the head sample that starts first in the file, with its track's index; the
/// earlier track on a tie.
fn take_earliest(tracks: &mut [Track]) -> Option<(usize, Sample)> {
    let (_, at) = tracks
        .iter()
        .enumerate()
        .filter_map(|(at, track)| track.head.as_ref().map(|s| (s.range.start, at)))
        .min()?;
    tracks[at].head.take().map(|sample| (at, sample))
}

/// Gives each opened track its reading state, within the opener's track limit.
fn adopt_tracks(tracks: &mut BudgetVec<Track>, sources: BudgetVec<MovieTrack>) -> Result<()> {
    for source in sources {
        let start = source.table().cursor().position();
        let adopted = Track {
            base: source.header().time_base()?,
            language: packed_language(source.header().raw_mdhd.payload())?,
            entry: None,
            start,
            regular: Some(start),
            fragment: 0,
            fragment_position: FragmentCursorPosition::default(),
            head: None,
            source,
        };
        tracks.try_push(adopted).map_err(|e| e.error)?;
    }
    Ok(())
}

/// Identifies the first sample entry of track `entry`, moving `entry` past it once done;
/// past the last track, announces the movie.
fn identify_entry(
    tracks: &mut [Track],
    entry: &mut usize,
    limits: &ReaderLimits,
    work: &mut WorkBudget,
) -> Result<Option<Output>> {
    let Some(track) = tracks.get_mut(*entry) else {
        return Ok(Some(Step::Ready(Neutral::Other(ReaderEvent::Movie))));
    };
    let identity = if let Some(identity) = &mut track.entry {
        identity
    } else {
        if !work.try_charge(1) {
            return Ok(Some(Step::Yield));
        }
        let first = track
            .source
            .table()
            .descriptions()
            .first()
            .ok_or(Error::Invalid("empty sample descriptions"))?;
        let identity = EntryConfiguration::new(
            track.source.header().handler,
            first,
            limits.configuration,
            limits.open.format,
        );
        track.entry.insert(identity)
    };
    if identity.poll(work)? == BuildStatus::Yield {
        return Ok(Some(Step::Yield));
    }
    *entry += 1;
    Ok(None)
}

impl Seeking {
    /// Finds the landing on the sought track, then positions every track after it;
    /// `Some(Step::Ready)` with the receipt once done.
    fn poll(&mut self, tracks: &mut [Track], work: &mut WorkBudget) -> Result<Option<Output>> {
        match &mut self.stage {
            SeekStage::Search { first, best } => {
                let track = &mut tracks[self.track];
                if !track.fill(work)? {
                    return Ok(Some(Step::Yield));
                }
                if let Some(sample) = track.head.take() {
                    if let (Access::ContainerSync, Some(cts)) = (sample.access, sample.cts_ticks) {
                        first.get_or_insert((sample.ordinal, cts));
                        if cts <= self.target && best.is_none_or(|(_, best)| cts >= best) {
                            *best = Some((sample.ordinal, cts));
                        }
                    }
                    return Ok(None);
                }
                let (ordinal, ticks) = best.or(*first).ok_or(Error::Unsupported(
                    "track states no timed random-access sample",
                ))?;
                let landing = Landing {
                    ordinal,
                    time: Time::from_ticks(ticks, track.base)?,
                };
                for track in tracks.iter_mut() {
                    track.rewind();
                }
                self.stage = SeekStage::Position {
                    landing,
                    at: 0,
                    resume: None,
                    skipped: false,
                };
                Ok(None)
            }
            SeekStage::Position {
                landing,
                at,
                resume,
                skipped,
            } => {
                if let Some(track) = tracks.get_mut(*at) {
                    if !track.fill(work)? {
                        return Ok(Some(Step::Yield));
                    }
                    if *at != self.track && resume.is_none() {
                        *resume = Some(landing.time.to_ticks(track.base, Rounding::Ceil)?);
                    }
                    let reached = track.head.as_ref().is_none_or(|head| {
                        if *at == self.track {
                            head.ordinal == landing.ordinal
                        } else {
                            // A sample with no time cannot be placed, so it is not dropped.
                            head.dts_ticks
                                .or(head.cts_ticks)
                                .zip(*resume)
                                .is_none_or(|(ticks, resume)| ticks >= resume)
                        }
                    });
                    if reached {
                        *at += 1;
                        *resume = None;
                    } else {
                        track.head = None;
                        *skipped = true;
                    }
                    return Ok(None);
                }
                let discard_until = match self.policy {
                    SeekPolicy::PreviousRandomAccess => landing.time,
                    SeekPolicy::PlaybackPreroll => landing.time.max(self.requested),
                };
                Ok(Some(Step::Ready(Neutral::Seek(SeekReceipt {
                    requested: self.requested,
                    landing: landing.time,
                    discard_until,
                    method: SeekMethod::Index,
                    scanned_bytes: 0,
                    replays_from_start: !*skipped,
                }))))
            }
        }
    }
}

impl ContainerReader for Reader {
    type Packet = Packet;
    type Track = Track;
    /// Root boxes and the end of opening.
    type Other = ReaderEvent;

    fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.input.provide(window)
    }
    fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        self.input.finish(source, token)
    }
    fn fail(&mut self, source: SourceId, token: InputToken, failure: SourceFailure) -> Result<()> {
        self.input.fail(source, token, failure)
    }
    /// Keeps nothing: the source is read and indexed again from its start, which a
    /// source without random access has to be able to supply again.
    fn reset(&mut self) -> Result<InputRequest> {
        let request = self.input.reset(0)?;
        self.phase = Phase::Open(MovieOpen::for_source(&self.input, self.limits.open)?);
        self.movie = None;
        self.tracks.clear();
        self.reading = None;
        self.failed = None;
        Ok(request)
    }
    fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ReaderStep<Self>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        self.advance(work, cancel).or_else(|error| {
            error
                .into_step()
                .inspect_err(|error| self.failed = Some(*error))
        })
    }
    /// The track must be open. The target is on the track's media clock, without
    /// edits, and delivery restarts at the latest stated sync sample presented at
    /// or before it, or the first one if none is. Other tracks resume at their
    /// first sample at or after that time, which may not be a sync sample. The
    /// search reads no media and may scan every sample of the track. Failing to
    /// find a sync sample is reported by `step` and needs [`Reader::reset`]. A source
    /// without random access refuses the request with the cursor's
    /// `Error::Unsupported` and goes on delivering.
    ///
    /// The returned request is the current position; a later `NeedInput` names
    /// the landing range.
    fn request_seek(&mut self, request: SeekRequest) -> Result<InputRequest> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let track = self
            .tracks()
            .iter()
            .position(|track| track.id() == request.track)
            .ok_or(Error::Invalid("seek track is not in the movie"))?;
        let target = request
            .target
            .to_ticks(self.tracks.as_slice()[track].base, Rounding::Floor)?;
        let position = self.input.request_range(self.input.position())?;
        for track in self.tracks.as_mut_slice() {
            track.rewind();
        }
        self.reading = None;
        self.phase = Phase::Seek(Seeking {
            track,
            policy: request.policy,
            requested: request.target,
            target,
            stage: SeekStage::Search {
                first: None,
                best: None,
            },
        });
        Ok(position)
    }
    fn track_count(&self) -> usize {
        self.tracks().len()
    }
    fn track(&self, index: usize) -> Option<&Track> {
        self.tracks().get(index)
    }
    fn capabilities(&self) -> SourceCapabilities {
        self.input.capabilities()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_packed_iso_code_is_a_language() {
        let mdhd = |packed: u16| {
            let mut body = vec![0; 24];
            body[20..22].copy_from_slice(&packed.to_be_bytes());
            body
        };
        assert_eq!(packed_language(&mdhd(0x55c4)).unwrap(), Some(*b"und"));
        assert_eq!(packed_language(&mdhd(0x15c7)).unwrap(), Some(*b"eng"));
        assert_eq!(packed_language(&mdhd(0)).unwrap(), None);
        assert_eq!(packed_language(&mdhd(0x8000)).unwrap(), None);
    }
}
