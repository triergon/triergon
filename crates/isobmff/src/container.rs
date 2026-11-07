//! The source view through the neutral container model.
//!
//! [`Reader`] walks the root boxes first, so it needs a seekable source: `moov`
//! and every `moof` are retained, every `mdat` extent is recorded, and packets
//! follow once the tracks are indexed. Packets are sample wire facts in ascending
//! byte order across tracks and decode order within a track. Edit lists are not
//! applied: times are the media clock of the track, and the list stays on
//! [`Track::edits`]. A fragment without `tfdt` has unknown times, which stay
//! `None`.
//!
//! Track metadata describes the first sample entry. [`Packet::sample`] names the
//! entry of each sample and [`Track::descriptions`] keeps them all, so a track
//! that changes entry is visible there. Movie boxes the neutral view has no field
//! for, such as colour, `udta` chapters and metadata, stay readable as bounded
//! bytes through [`Reader::movie_box`] and [`SampleDescription::raw`].
use crate::encoded_stream::ObjectTypeScan;
use crate::fragment::FragmentCursorPosition;
use crate::open::{Movie, MovieOpen, MovieTrack, OpenLimits, OpenStep};
use crate::regular::SampleCursorPosition;
use crate::{
    Access, BoxChildren, BoxHeader, BoxStep, BoxView, ConfigurationLimits, EditList, EntryLayout,
    Limits, MovieHeader, Sample, SampleDescription, SampleStep, SampleTable, TrackHeader,
    dts_description::dts_entry_children, fields,
};
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

/// The one mapping from a track handler and sample entry code. `raw ` needs the
/// handler, and `mp4a` needs its stated object type: without one it stays
/// [`CodecId::Other`].
fn codec_id<'a>(handler: &[u8; 4], format: &'a [u8; 4], object_type: Option<u8>) -> CodecId<'a> {
    match (handler, format) {
        (b"vide", b"avc1" | b"avc3") => CodecId::H264,
        (b"vide", b"hvc1" | b"hev1") => CodecId::Hevc,
        (b"vide", b"av01") => CodecId::Av1,
        (b"vide", b"vp09") => CodecId::Vp9,
        (b"vide", b"raw ") => CodecId::RawVideo,
        (b"soun", b"mp4a") => match object_type {
            Some(0x40) => CodecId::Aac,
            Some(0xa9 | 0xaa) => CodecId::Dts,
            _ => CodecId::Other(format),
        },
        (b"soun", b"ac-3") => CodecId::Ac3,
        (b"soun", b"ec-3") => CodecId::Eac3,
        (b"soun", b"dtsc" | b"dtsh" | b"dtsl") => CodecId::Dts,
        (b"soun", b"Opus") => CodecId::Opus,
        (b"soun", b"raw " | b"twos") => CodecId::Pcm,
        (b"sbtl" | b"text" | b"subt", b"tx3g") => CodecId::Tx3g,
        _ => CodecId::Other(format),
    }
}

/// The sample entry child holding a codec's configuration record.
fn configuration_box(codec: CodecId<'_>) -> Option<[u8; 4]> {
    Some(match codec {
        CodecId::H264 => *b"avcC",
        CodecId::Hevc => *b"hvcC",
        CodecId::Av1 => *b"av1C",
        CodecId::Vp9 => *b"vpcC",
        CodecId::Ac3 => *b"dac3",
        CodecId::Eac3 => *b"dec3",
        CodecId::Dts => *b"ddts",
        CodecId::Opus => *b"dOps",
        _ => return None,
    })
}

fn media_kind(handler: &[u8; 4]) -> MediaKind {
    match handler {
        b"vide" => MediaKind::Video,
        b"soun" => MediaKind::Audio,
        b"sbtl" | b"text" | b"subt" => MediaKind::Subtitle,
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
    pub open: OpenLimits,
    /// Largest sample.
    pub packet_bytes: usize,
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
    /// The sample's wire facts: ordinal, sample entry, byte range, flags.
    pub fn sample(&self) -> &Sample {
        &self.sample
    }
    /// Shared payload storage, without copying.
    pub fn bytes(&self) -> &Bytes {
        &self.payload
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

#[derive(Debug)]
enum Configuration {
    Record(Bytes),
    /// The codec's record is absent or could not be read.
    Absent,
    /// The entry's layout is not known, so the whole entry payload stands in.
    Entry,
}

/// One `trak`: its headers, its index and the reader's place in it.
#[derive(Debug)]
pub struct Track {
    source: MovieTrack,
    base: TimeBase,
    language: Option<[u8; 3]>,
    object_type: Option<u8>,
    configuration: Option<Configuration>,
    start: SampleCursorPosition,
    regular: Option<SampleCursorPosition>,
    fragment: usize,
    fragment_position: FragmentCursorPosition,
    head: Option<Sample>,
}
impl Track {
    pub fn header(&self) -> &TrackHeader {
        self.source.header()
    }
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
        self.object_type
    }
    fn rewind(&mut self) {
        self.regular = Some(self.start);
        self.fragment = 0;
        self.fragment_position = FragmentCursorPosition::default();
        self.head = None;
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
                SampleStep::Sample(sample) => {
                    self.regular = Some(cursor.position());
                    self.head = Some(sample);
                    return Ok(true);
                }
                SampleStep::Yield => return Ok(false),
                SampleStep::End => self.regular = None,
            }
        }
        while let Some(fragment) = self.source.fragments().get(self.fragment) {
            let mut cursor = fragment.resume(self.fragment_position);
            match cursor.poll(work)? {
                SampleStep::Sample(sample) => {
                    self.fragment_position = cursor.position();
                    self.head = Some(sample);
                    return Ok(true);
                }
                SampleStep::Yield => return Ok(false),
                SampleStep::End => {
                    self.fragment += 1;
                    self.fragment_position = FragmentCursorPosition::default();
                }
            }
        }
        Ok(true)
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
    fn codec(&self) -> CodecId<'_> {
        self.descriptions()
            .first()
            .map_or(CodecId::Other(&[]), |entry| {
                codec_id(&self.header().handler, &entry.format, self.object_type)
            })
    }
    /// The configuration record's payload (AudioSpecificConfig for AAC), empty
    /// when it is absent, or the whole sample entry payload when the entry has
    /// no known record.
    fn configuration(&self) -> &[u8] {
        match self
            .configuration
            .as_ref()
            .unwrap_or(&Configuration::Absent)
        {
            Configuration::Record(bytes) => bytes.as_ref(),
            Configuration::Absent => &[],
            Configuration::Entry => self
                .descriptions()
                .first()
                .map_or(&[], |entry| entry.raw().payload()),
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
    /// A root box was passed. `moov`, `moof` and `ftyp` are retained.
    Box(BoxHeader),
    /// The movie is indexed: tracks are available and packets follow.
    Movie,
}

// One scan runs per track during opening, so its size does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum EntryScan {
    ObjectType(ObjectTypeScan),
    Record(BoxChildren, [u8; 4]),
}

fn missing(reason: &'static str) -> Error {
    Error::Invalid(reason)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Open,
    Tracks,
    Entries,
    Packets,
    Seek,
    Ended,
}

#[derive(Debug)]
struct Reading {
    track: usize,
    sample: Sample,
    buffer: Option<BudgetVec<u8>>,
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
    first: Option<(u64, i128)>,
    best: Option<(u64, i128)>,
    landing: Option<Landing>,
    /// The next track to position, and the first tick it may resume at.
    at: usize,
    resume: Option<i128>,
    skipped: bool,
}

type Output = Step<Neutral<Packet, ReaderEvent>, Backpressure>;

fn pressure(error: Error) -> Result<Output> {
    match error {
        Error::Backpressure {
            requested,
            available,
        } => Ok(Step::Backpressure(Backpressure {
            requested,
            available,
        })),
        other => Err(other),
    }
}

/// A resumable ISO BMFF reader over the source view; see the module
/// documentation for what it reports. Errors are sticky until [`Reader::reset`].
#[derive(Debug)]
pub struct Reader {
    input: InputCursor,
    limits: ReaderLimits,
    phase: Phase,
    opener: Option<MovieOpen>,
    movie: Option<Movie>,
    tracks: BudgetVec<Track>,
    entry: usize,
    scan: Option<EntryScan>,
    reading: Option<Reading>,
    seeking: Option<Seeking>,
    failed: Option<Error>,
}

impl Reader {
    /// A reader for one source. It refuses a source without random access,
    /// because `mdat` may precede `moov` and is passed before any sample is known.
    pub fn new(
        pool: &triergon_core::Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: ReaderLimits,
        window: usize,
    ) -> Result<Self> {
        if !capabilities.random_access {
            return Err(Error::Unsupported("ISO BMFF reader needs random access"));
        }
        let input = InputCursor::new(pool, source, capabilities, window)?;
        let opener = MovieOpen::new(&input, limits.open)?;
        Ok(Self {
            input,
            limits,
            phase: Phase::Open,
            opener: Some(opener),
            movie: None,
            tracks: BudgetVec::new(pool),
            entry: 0,
            scan: None,
            reading: None,
            seeking: None,
            failed: None,
        })
    }
    /// Every track once [`ReaderEvent::Movie`] has been delivered, otherwise none.
    pub fn tracks(&self) -> &[Track] {
        if self.movie_open() {
            self.tracks.as_slice()
        } else {
            &[]
        }
    }
    fn movie_open(&self) -> bool {
        matches!(self.phase, Phase::Packets | Phase::Seek | Phase::Ended)
    }
    /// The first `ftyp` major brand.
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
    pub fn movie_header(&self) -> Option<&MovieHeader> {
        self.movie.as_ref().map(Movie::header)
    }
    /// Payload extents of the `mdat` boxes, in source order.
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
            let output = match self.phase {
                Phase::Open => self.open(work, cancel)?,
                Phase::Tracks => self.adopt_tracks(work)?,
                Phase::Entries => self.identify_entry(work)?,
                Phase::Packets => self.packet(work, cancel)?,
                Phase::Seek => self.seek(work)?,
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

    fn open(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Option<Output>> {
        let opener = self.opener.as_mut().ok_or(missing("missing opener"))?;
        Ok(match opener.advance(&mut self.input, work, cancel)? {
            OpenStep::Yield => Some(Step::Yield),
            OpenStep::NeedInput(request) => Some(Step::NeedInput(request)),
            OpenStep::Root(header) => Some(Step::Ready(Neutral::Other(ReaderEvent::Box(header)))),
            OpenStep::Ready => {
                self.movie = Some(opener.finish(&self.input)?);
                self.opener = None;
                self.phase = Phase::Tracks;
                None
            }
        })
    }

    /// Give each opened track its reading state, within the opener's track limit.
    fn adopt_tracks(&mut self, work: &mut WorkBudget) -> Result<Option<Output>> {
        let movie = self.movie.as_mut().ok_or(missing("missing movie"))?;
        self.tracks.try_reserve_exact(movie.tracks.len())?;
        work.take(movie.tracks.len() as u64);
        let tracks = std::mem::replace(&mut movie.tracks, BudgetVec::new(&movie.pool));
        for source in tracks {
            let start = source.table().cursor().position();
            let adopted = Track {
                base: source.header().time_base()?,
                language: packed_language(source.header().raw_mdhd.payload())?,
                object_type: None,
                configuration: None,
                start,
                regular: Some(start),
                fragment: 0,
                fragment_position: FragmentCursorPosition::default(),
                head: None,
                source,
            };
            self.tracks.try_push(adopted).map_err(|e| e.error)?;
        }
        self.phase = Phase::Entries;
        Ok(None)
    }

    fn identify_entry(&mut self, work: &mut WorkBudget) -> Result<Option<Output>> {
        let Some(track) = self.tracks.as_mut_slice().get_mut(self.entry) else {
            self.phase = Phase::Packets;
            return Ok(Some(Step::Ready(Neutral::Other(ReaderEvent::Movie))));
        };
        if track.configuration.is_none() {
            if !work.try_charge(1) {
                return Ok(Some(Step::Yield));
            }
            let entry = track
                .descriptions()
                .first()
                .ok_or(missing("empty sample descriptions"))?;
            let (scan, configuration) = entry_scan(track.header(), entry, &self.limits);
            self.scan = scan;
            track.configuration = Some(configuration);
        }
        if self.scan.is_none() {
            self.entry += 1;
        } else if step_entry_scan(&mut self.scan, track, work)? {
            return Ok(Some(Step::Yield));
        }
        Ok(None)
    }

    fn packet(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Option<Output>> {
        if self.reading.is_none() {
            for track in self.tracks.as_mut_slice() {
                if !track.fill(work)? {
                    return Ok(Some(Step::Yield));
                }
            }
            let next = self
                .tracks
                .as_slice()
                .iter()
                .enumerate()
                .filter_map(|(at, track)| track.head.as_ref().map(|s| (s.range.start, at)))
                .min();
            let Some((_, track)) = next else {
                self.phase = Phase::Ended;
                return Ok(None);
            };
            if !work.try_charge(1) {
                return Ok(Some(Step::Yield));
            }
            let sample = self.tracks.as_mut_slice()[track]
                .head
                .take()
                .ok_or(missing("missing sample"))?;
            self.begin_reading(track, sample)?;
        }
        self.read_payload(work, cancel)
    }

    fn begin_reading(&mut self, track: usize, sample: Sample) -> Result<()> {
        let extents = self.media_extents();
        let after = extents.partition_point(|extent| extent.start <= sample.range.start);
        let extent = after
            .checked_sub(1)
            .and_then(|at| extents.get(at))
            .ok_or(Error::Invalid("sample outside media extent"))?;
        sample.require_within(extent.clone())?;
        let length =
            usize::try_from(sample.range.end - sample.range.start).map_err(|_| Error::Overflow)?;
        if length > self.limits.packet_bytes {
            return Err(Error::Limit {
                requested: length,
                maximum: self.limits.packet_bytes,
            });
        }
        if length != 0 && self.input.position() != sample.range.start {
            self.input.request_range(sample.range.start)?;
        }
        self.reading = Some(Reading {
            track,
            sample,
            buffer: None,
        });
        Ok(())
    }

    fn read_payload(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Output>> {
        let pool = self.input.budget().clone();
        let reading = self.reading.as_mut().ok_or(missing("missing read"))?;
        let range = &reading.sample.range;
        let length = usize::try_from(range.end - range.start).map_err(|_| Error::Overflow)?;
        if length == 0 {
            return self
                .finish_packet(Bytes::copy_from_slice(&pool, &[])?)
                .map(Some);
        }
        let filled = reading.buffer.as_ref().map_or(0, BudgetVec::len);
        if filled == length {
            let buffer = reading
                .buffer
                .take()
                .ok_or(missing("missing read buffer"))?;
            return match buffer.freeze() {
                Ok(payload) => self.finish_packet(payload).map(Some),
                Err(rejected) => {
                    reading.buffer = Some(rejected.buffer);
                    Err(rejected.error)
                }
            };
        }
        if self.input.buffered().is_empty()
            && let Some(request) = self.input.pending_request(cancel)?
        {
            return Ok(Some(Step::NeedInput(request)));
        }
        // A sample inside one window shares it; only a split sample is copied.
        if reading.buffer.is_none()
            && self.input.buffered().len() >= length
            && work.remaining() >= length as u64
        {
            return match self.input.read_span(length, work, cancel)? {
                Step::Ready(payload) => self.finish_packet(payload).map(Some),
                Step::NeedInput(request) => Ok(Some(Step::NeedInput(request))),
                Step::Yield => Ok(Some(Step::Yield)),
                Step::Backpressure(never) => match never {},
                Step::End(_) => Err(Error::Truncated),
            };
        }
        if reading.buffer.is_none() {
            let mut buffer = BudgetVec::new(&pool);
            buffer.try_reserve_exact(length)?;
            reading.buffer = Some(buffer);
        }
        let buffer = reading
            .buffer
            .as_mut()
            .ok_or(missing("missing read buffer"))?;
        match self
            .input
            .read_append(buffer, length - filled, work, cancel)?
        {
            Step::Ready(_) => Ok(None),
            Step::NeedInput(request) => Ok(Some(Step::NeedInput(request))),
            Step::Yield => Ok(Some(Step::Yield)),
            Step::Backpressure(never) => match never {},
            Step::End(_) => Err(Error::Truncated),
        }
    }

    fn finish_packet(&mut self, payload: Bytes) -> Result<Output> {
        let Reading { track, sample, .. } = self.reading.take().ok_or(missing("missing read"))?;
        let track = &self.tracks.as_slice()[track];
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

    fn seek(&mut self, work: &mut WorkBudget) -> Result<Option<Output>> {
        let seeking = self.seeking.as_mut().ok_or(missing("missing seek"))?;
        let tracks = self.tracks.as_mut_slice();
        let Some(landing) = seeking.landing else {
            let track = &mut tracks[seeking.track];
            if !track.fill(work)? {
                return Ok(Some(Step::Yield));
            }
            if let Some(sample) = track.head.take() {
                if let (Access::ContainerSync, Some(cts)) = (sample.access, sample.cts_ticks) {
                    seeking.first.get_or_insert((sample.ordinal, cts));
                    if cts <= seeking.target && seeking.best.is_none_or(|(_, best)| cts >= best) {
                        seeking.best = Some((sample.ordinal, cts));
                    }
                }
                return Ok(None);
            }
            let (ordinal, ticks) = seeking.best.or(seeking.first).ok_or(Error::Unsupported(
                "track states no timed random-access sample",
            ))?;
            seeking.landing = Some(Landing {
                ordinal,
                time: Time::from_ticks(ticks, tracks[seeking.track].base)?,
            });
            for track in tracks.iter_mut() {
                track.rewind();
            }
            return Ok(None);
        };
        if let Some(track) = tracks.get_mut(seeking.at) {
            if !track.fill(work)? {
                return Ok(Some(Step::Yield));
            }
            if seeking.at != seeking.track && seeking.resume.is_none() {
                seeking.resume = Some(landing.time.to_ticks(track.base, Rounding::Ceil)?);
            }
            let reached = track.head.as_ref().is_none_or(|head| {
                if seeking.at == seeking.track {
                    head.ordinal == landing.ordinal
                } else {
                    // A sample with no time cannot be placed, so it is not dropped.
                    head.dts_ticks
                        .or(head.cts_ticks)
                        .zip(seeking.resume)
                        .is_none_or(|(ticks, resume)| ticks >= resume)
                }
            });
            if reached {
                seeking.at += 1;
                seeking.resume = None;
            } else {
                track.head = None;
                seeking.skipped = true;
            }
            return Ok(None);
        }
        let discard_until = match seeking.policy {
            SeekPolicy::PreviousRandomAccess => landing.time,
            SeekPolicy::PlaybackPreroll => landing.time.max(seeking.requested),
        };
        let receipt = SeekReceipt {
            requested: seeking.requested,
            landing: landing.time,
            discard_until,
            method: SeekMethod::Index,
            scanned_bytes: 0,
            replays_from_start: !seeking.skipped,
        };
        self.seeking = None;
        self.phase = Phase::Packets;
        Ok(Some(Step::Ready(Neutral::Seek(receipt))))
    }
}

/// What identifying a track's first entry still has to read, and what its
/// configuration is if that reading finds nothing.
fn entry_scan(
    header: &TrackHeader,
    entry: &SampleDescription,
    limits: &ReaderLimits,
) -> (Option<EntryScan>, Configuration) {
    if (&header.handler, &entry.format) == (b"soun", b"mp4a") {
        let scan = ObjectTypeScan::new(entry, limits.configuration).ok();
        return (scan.map(EntryScan::ObjectType), Configuration::Entry);
    }
    let codec = codec_id(&header.handler, &entry.format, None);
    let Some(record) = configuration_box(codec) else {
        return (None, Configuration::Entry);
    };
    let child_limits = Limits {
        max_boxes: limits.configuration.max_child_boxes,
        ..limits.open.format
    };
    let children = match codec {
        CodecId::Dts => dts_entry_children(entry, child_limits),
        CodecId::Ac3 | CodecId::Eac3 | CodecId::Opus => {
            entry.extensions(EntryLayout::AudioV0, child_limits)
        }
        _ => entry.extensions(EntryLayout::Visual, child_limits),
    };
    (
        children
            .ok()
            .map(|children| EntryScan::Record(children, record)),
        Configuration::Absent,
    )
}

/// Advance the scan. `true` means the work is spent. A sample entry that cannot
/// be read keeps the identity and configuration its code alone gives.
fn step_entry_scan(
    active: &mut Option<EntryScan>,
    track: &mut Track,
    work: &mut WorkBudget,
) -> Result<bool> {
    match active.as_mut() {
        Some(EntryScan::ObjectType(scan)) => {
            let found = match scan.poll(work) {
                Ok(None) => return Ok(true),
                Ok(Some(found)) => Some(found),
                Err(_) => None,
            };
            if let Some((object_type, specific)) = found {
                track.object_type = Some(object_type);
                if !matches!(track.codec(), CodecId::Other(_)) {
                    track.configuration =
                        Some(specific.map_or(Configuration::Absent, Configuration::Record));
                }
            }
            *active = None;
        }
        Some(EntryScan::Record(children, record)) => match children.poll(work) {
            Ok(BoxStep::Yield) => return Ok(true),
            Ok(BoxStep::Box(raw)) if raw.header().kind == *record => {
                let start = usize::from(raw.header().header_size);
                let bytes = raw.bytes().slice(start..raw.bytes().len())?;
                track.configuration = Some(Configuration::Record(bytes));
                *active = None;
            }
            Ok(BoxStep::Box(_)) => {}
            Ok(BoxStep::End) | Err(_) => *active = None,
        },
        None => {}
    }
    Ok(false)
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
    /// Keeps nothing: the source is read and indexed again from its start.
    fn reset(&mut self) -> Result<InputRequest> {
        let request = self.input.reset(0)?;
        self.opener = Some(MovieOpen::new(&self.input, self.limits.open)?);
        self.phase = Phase::Open;
        self.movie = None;
        self.tracks.clear();
        self.entry = 0;
        self.scan = None;
        self.reading = None;
        self.seeking = None;
        self.failed = None;
        Ok(request)
    }
    fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ReaderStep<Self>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        self.advance(work, cancel)
            .or_else(|error| pressure(error).inspect_err(|error| self.failed = Some(*error)))
    }
    /// The track must be open. The target is on the track's media clock, without
    /// edits, and delivery restarts at the latest stated sync sample presented at
    /// or before it, or the first one if none is. Other tracks resume at their
    /// first sample at or after that time, which may not be a sync sample. The
    /// search reads no media and may scan every sample of the track. Failing to
    /// find a sync sample is reported by `step` and needs [`Reader::reset`].
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
        self.seeking = Some(Seeking {
            track,
            policy: request.policy,
            requested: request.target,
            target,
            first: None,
            best: None,
            landing: None,
            at: 0,
            resume: None,
            skipped: false,
        });
        self.phase = Phase::Seek;
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
    fn every_recognised_entry_has_one_name() {
        for (handler, format, object_type, id) in [
            (b"vide", b"avc1", None, CodecId::H264),
            (b"vide", b"avc3", None, CodecId::H264),
            (b"vide", b"hvc1", None, CodecId::Hevc),
            (b"vide", b"hev1", None, CodecId::Hevc),
            (b"vide", b"av01", None, CodecId::Av1),
            (b"vide", b"vp09", None, CodecId::Vp9),
            (b"vide", b"raw ", None, CodecId::RawVideo),
            (b"soun", b"mp4a", Some(0x40), CodecId::Aac),
            (b"soun", b"mp4a", Some(0xa9), CodecId::Dts),
            (b"soun", b"mp4a", Some(0xaa), CodecId::Dts),
            (b"soun", b"ac-3", None, CodecId::Ac3),
            (b"soun", b"ec-3", None, CodecId::Eac3),
            (b"soun", b"dtsc", None, CodecId::Dts),
            (b"soun", b"dtsh", None, CodecId::Dts),
            (b"soun", b"dtsl", None, CodecId::Dts),
            (b"soun", b"Opus", None, CodecId::Opus),
            (b"soun", b"raw ", None, CodecId::Pcm),
            (b"soun", b"twos", None, CodecId::Pcm),
            (b"sbtl", b"tx3g", None, CodecId::Tx3g),
            (b"text", b"tx3g", None, CodecId::Tx3g),
            (b"subt", b"tx3g", None, CodecId::Tx3g),
        ] {
            assert_eq!(codec_id(handler, format, object_type), id);
        }
    }

    #[test]
    fn an_unrecognised_entry_is_reported_with_its_own_code() {
        for (handler, format, object_type) in [
            (b"vide", b"xyz1", None),
            (b"soun", b"mp4a", None),
            (b"soun", b"mp4a", Some(0x6b)),
            (b"soun", b"mp4a", Some(0xac)),
            (b"soun", b"dtse", None),
            (b"soun", b"dtsx", None),
            (b"vide", b"Opus", None),
            (b"vide", b"mp4a", Some(0x40)),
            (b"soun", b"avc1", None),
            (b"meta", b"tx3g", None),
        ] {
            assert_eq!(
                codec_id(handler, format, object_type),
                CodecId::Other(format)
            );
        }
    }

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
