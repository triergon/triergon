//! Raw Tracks transaction. Text, Colour, encodings, private restoration and
//! duplicate checks advance under the caller's work budget before publication.
use super::{Track, static_metadata_text};
use crate::colour::parse::ColourJob;
use crate::configuration::{Configuration, ConfigurationJob};
use crate::content_encoding::{Encodings, parse::EncodingsJob};
use crate::ebml::{ChildCursor, once};
use crate::step::check;
use crate::zlib::restore::{RestoreJob, RestorePlan};
use crate::{
    Error, Limits, Result, ebml,
    step::ParseStep,
    storage::{SharedBytes, SharedText, text::TextJob},
};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, BudgetVec, WorkBudget};

pub(crate) const VIDEO: u64 = 0xe0;

/// Where the reading of one TrackEntry stands. A phase owns no storage, so a
/// phase change never drops one; the jobs it starts run in `TrackJob::side`.
#[derive(Clone, Copy, Debug)]
enum TrackPhase {
    /// The children of the TrackEntry.
    Fields,
    /// The children of its Video or Audio master.
    Video(ChildCursor),
    Audio(ChildCursor),
    /// Starting the restoration of CodecPrivate through the private encodings.
    PrivateStart,
    /// Starting the validation of the block addition mappings.
    ExtensionsStart,
    /// Checking the required fields and the decoded private size.
    Finish,
    /// Comparing the TrackNumber, then the TrackUID, with track `at` of the
    /// configuration read so far.
    Number {
        at: usize,
    },
    Uid {
        at: usize,
    },
    /// Adding the track to the configuration.
    Store,
    Done,
}
/// The track field a text child fills.
#[derive(Clone, Copy, Debug)]
enum TextField {
    Codec,
    Name,
    Language,
    LanguageBcp47,
}
/// A job a TrackEntry phase started, which finishes before the phase goes on.
// An explicit tag byte, as the packet batch's side jobs measured.
#[expect(
    clippy::large_enum_variant,
    reason = "one side job at a time, inline in the entry job; boxing it would allocate per track"
)]
#[derive(Debug)]
#[repr(u8)]
enum TrackSide {
    Idle,
    /// The text of a CodecID, Name, Language or LanguageBCP47 child.
    Text {
        field: TextField,
        job: TextJob,
    },
    Colour(ColourJob),
    Encodings(EncodingsJob),
    Private(RestoreJob),
    Extensions(crate::track::extensions::TrackExtensionsJob),
}
impl TrackSide {
    fn text(&self) -> Option<&TextJob> {
        match self {
            Self::Text { job, .. } => Some(job),
            _ => None,
        }
    }
    fn colour(&self) -> Option<&ColourJob> {
        match self {
            Self::Colour(job) => Some(job),
            _ => None,
        }
    }
    fn encodings(&self) -> Option<&EncodingsJob> {
        match self {
            Self::Encodings(job) => Some(job),
            _ => None,
        }
    }
    fn private(&self) -> Option<&RestoreJob> {
        match self {
            Self::Private(job) => Some(job),
            _ => None,
        }
    }
    fn extensions(&self) -> Option<&crate::track::extensions::TrackExtensionsJob> {
        match self {
            Self::Extensions(job) => Some(job),
            _ => None,
        }
    }
}
/// A track as TrackJob starts it: every field at its default.
fn new_track(source_metadata: SharedBytes) -> Track {
    Track {
        packet_prefix: SharedBytes::default(),
        packet_decoder: None,
        source_metadata,
        number: 0,
        uid: 0,
        kind: 0,
        codec: SharedText::constant(""),
        codec_private: SharedBytes::default(),
        codec_private_present: false,
        audio_settings_present: false,
        name: SharedText::constant(""),
        language: SharedText::constant("eng"),
        language_bcp47: None,
        enabled: true,
        lacing: true,
        hearing_impaired: None,
        visual_impaired: None,
        text_descriptions: None,
        original: None,
        commentary: None,
        default: true,
        forced: false,
        default_duration_ns: None,
        codec_delay_ns: 0,
        seek_preroll_ns: 0,
        width: 0,
        height: 0,
        sample_rate: 8000.,
        channels: 1,
        colour: None,
    }
}
/// One TrackEntry: its fields, encodings, private data and mappings, checked
/// against the tracks read before it and added to them.
#[derive(Debug)]
struct TrackJob {
    source: SharedBytes,
    pool: Budget,
    limits: Limits,
    private_budget: usize,
    out: Track,
    root: ChildCursor,
    phase: TrackPhase,
    side: TrackSide,
    codec_nonempty: bool,
    encoded: bool,
    prefixes: Encodings,
    flags_seen: u16,
    languages_seen: u8,
    // The single-occurrence children read so far, one bit each (`once`).
    seen: u16,
    compressed_private: bool,
    has_extensions: bool,
    private_bytes: usize,
    failed: bool,
}
impl TrackJob {
    #[inline(always)]
    fn new(
        source: SharedBytes,
        limits: Limits,
        private_budget: usize,
        pool: &Budget,
    ) -> Result<Self> {
        if !source.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        let end = source.len();
        let out = new_track(source.clone());
        Ok(Self {
            source,
            pool: pool.clone(),
            limits,
            private_budget,
            out,
            root: ChildCursor::new(0..end),
            phase: TrackPhase::Fields,
            side: TrackSide::Idle,
            codec_nonempty: false,
            encoded: false,
            prefixes: Encodings::default(),
            flags_seen: 0,
            languages_seen: 0,
            seen: 0,
            compressed_private: false,
            has_extensions: false,
            private_bytes: 0,
            failed: false,
        })
    }
    /// Reads the TrackEntry and adds its track to `tracks`; the event is the
    /// decoded private bytes it charges to the metadata allowance.
    fn step(
        &mut self,
        tracks: &mut BudgetVec<Track>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<usize>> {
        if self.failed {
            return Err(Error::Invalid(
                "TrackEntry requires replacement after error",
            ));
        }
        let result = self.advance(tracks, work, cancel);
        match result {
            Err(Error::Backpressure(triergon_core::Backpressure {
                requested,
                available,
            })) => Ok(ParseStep::Backpressure {
                requested,
                available,
            }),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
            other => other,
        }
    }
    fn advance(
        &mut self,
        tracks: &mut BudgetVec<Track>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<usize>> {
        'entry: loop {
            if !matches!(self.side, TrackSide::Idle) {
                check(cancel)?;
                match &mut self.side {
                    TrackSide::Idle => {}
                    TrackSide::Text { field, job } => match job.step(work, cancel)? {
                        ParseStep::Event(value) => {
                            let t = &mut self.out;
                            match field {
                                TextField::Codec => {
                                    self.codec_nonempty = !value.range.is_empty();
                                    t.codec = value.text;
                                }
                                TextField::Name => t.name = value.text,
                                TextField::Language => t.language = value.text,
                                TextField::LanguageBcp47 => t.language_bcp47 = Some(value.text),
                            }
                            self.side = TrackSide::Idle;
                        }
                        other => return crate::step::pending(other),
                    },
                    TrackSide::Colour(job) => match job.step(work, cancel)? {
                        ParseStep::Event(value) => {
                            self.out.colour = Some(value);
                            self.side = TrackSide::Idle;
                        }
                        other => return crate::step::pending(other),
                    },
                    TrackSide::Encodings(job) => match job.step(work, cancel)? {
                        ParseStep::Event(value) => {
                            self.prefixes = value;
                            self.side = TrackSide::Idle;
                        }
                        other => return crate::step::pending(other),
                    },
                    TrackSide::Private(job) => match job.step(work, cancel)? {
                        ParseStep::Event(value) => {
                            self.out.codec_private = value;
                            self.side = TrackSide::Idle;
                        }
                        other => return crate::step::pending(other),
                    },
                    TrackSide::Extensions(job) => match job.step_job(work, cancel)? {
                        ParseStep::Event(_) => self.side = TrackSide::Idle,
                        other => return crate::step::pending(other),
                    },
                }
            }
            loop {
                check(cancel)?;
                match &mut self.phase {
                    TrackPhase::Fields => match self.root.step(&self.source, work, cancel)? {
                        ParseStep::Yield => return Ok(ParseStep::Yield),
                        ParseStep::Event(None) => self.phase = TrackPhase::PrivateStart,
                        ParseStep::Event(Some(child)) => {
                            let t = &mut self.out;
                            let bytes = &self.source[child.payload.clone()];
                            match child.id {
                                0xd7 if once(&mut self.seen, 0) => t.number = ebml::uint(bytes)?,
                                0x73c5 if once(&mut self.seen, 1) => t.uid = ebml::uint(bytes)?,
                                0x83 if once(&mut self.seen, 2) => t.kind = ebml::uint(bytes)?,
                                0x63a2 if once(&mut self.seen, 3) => {
                                    t.codec_private = self.source.slice(child.payload)?;
                                    t.codec_private_present = true;
                                }
                                0x55ee | 0x41e4 => self.has_extensions = true,
                                0x86 | 0x536e | 0x22b59c | 0x22b59d => {
                                    if matches!(child.id, 0x86 | 0x536e)
                                        && !once(
                                            &mut self.seen,
                                            if child.id == 0x86 { 4 } else { 5 },
                                        )
                                    {
                                        continue;
                                    }
                                    if matches!(child.id, 0x22b59c | 0x22b59d) {
                                        // A repeated Language or LanguageBCP47 is passed over
                                        // (RFC 8794 §12).
                                        let mask = if child.id == 0x22b59c { 1 } else { 2 };
                                        if self.languages_seen & mask != 0 {
                                            continue;
                                        }
                                        self.languages_seen |= mask;
                                        if child.id == 0x22b59c && bytes.is_empty() {
                                            t.language = SharedText::constant("eng");
                                            continue;
                                        }
                                    }
                                    if matches!(child.id, 0x86 | 0x22b59c)
                                        && let Some(value) = static_metadata_text(bytes)
                                    {
                                        if child.id == 0x86 {
                                            t.codec = SharedText::constant(value);
                                            self.codec_nonempty = true;
                                        } else {
                                            t.language = SharedText::constant(value);
                                        }
                                        continue;
                                    }
                                    let field = match child.id {
                                        0x86 => TextField::Codec,
                                        0x536e => TextField::Name,
                                        0x22b59c => TextField::Language,
                                        _ => TextField::LanguageBcp47,
                                    };
                                    self.side = TrackSide::Text {
                                        field,
                                        job: TextJob::new(
                                            self.source.clone(),
                                            child.payload,
                                            false,
                                        )?,
                                    };
                                    continue 'entry;
                                }
                                0xb9 | 0x9c | 0x88 | 0x55aa..=0x55af => {
                                    let index = match child.id {
                                        0xb9 => 0,
                                        0x9c => 1,
                                        0x88 => 2,
                                        _ => 3 + (child.id - 0x55aa) as u32,
                                    };
                                    let bit = 1 << index;
                                    if self.flags_seen & bit != 0 {
                                        continue;
                                    }
                                    self.flags_seen |= bit;
                                    let value = if bytes.is_empty()
                                        && matches!(child.id, 0xb9 | 0x9c | 0x88)
                                    {
                                        1
                                    } else {
                                        ebml::uint(bytes)?
                                    };
                                    if value > 1 {
                                        return Err(Error::Invalid("track flag range"));
                                    }
                                    let value = value != 0;
                                    match child.id {
                                        0xb9 => t.enabled = value,
                                        0x9c => t.lacing = value,
                                        0x88 => t.default = value,
                                        0x55aa => t.forced = value,
                                        0x55ab => t.hearing_impaired = Some(value),
                                        0x55ac => t.visual_impaired = Some(value),
                                        0x55ad => t.text_descriptions = Some(value),
                                        0x55ae => t.original = Some(value),
                                        _ => t.commentary = Some(value),
                                    }
                                }
                                0x23e383 if once(&mut self.seen, 6) => {
                                    t.default_duration_ns = Some(ebml::uint(bytes)?);
                                }
                                0x56aa if once(&mut self.seen, 7) => {
                                    t.codec_delay_ns = ebml::uint(bytes)?;
                                }
                                0x56bb if once(&mut self.seen, 8) => {
                                    t.seek_preroll_ns = ebml::uint(bytes)?;
                                }
                                0x23314f
                                    if once(&mut self.seen, 9)
                                        && bytes != 1_f64.to_be_bytes()
                                        && bytes != 1_f32.to_be_bytes() =>
                                {
                                    return Err(Error::Unsupported("TrackTimestampScale"));
                                }
                                0x6d80 => {
                                    if self.encoded {
                                        continue;
                                    }
                                    self.encoded = true;
                                    self.side = TrackSide::Encodings(EncodingsJob::new(
                                        self.source.clone(),
                                        child.payload,
                                        self.limits.metadata_bytes,
                                        &self.pool,
                                    )?);
                                    continue 'entry;
                                }
                                VIDEO | 0xe1
                                    if once(
                                        &mut self.seen,
                                        if child.id == VIDEO { 10 } else { 11 },
                                    ) =>
                                {
                                    let cursor = ChildCursor::new(child.payload);
                                    self.phase = if child.id == VIDEO {
                                        TrackPhase::Video(cursor)
                                    } else {
                                        t.audio_settings_present = true;
                                        TrackPhase::Audio(cursor)
                                    };
                                }
                                _ => (),
                            }
                        }
                        _ => return Err(Error::Invalid("invalid TrackEntry transition")),
                    },
                    TrackPhase::Video(inner) | TrackPhase::Audio(inner) => {
                        let step = inner.step(&self.source, work, cancel)?;
                        let video = matches!(self.phase, TrackPhase::Video(_));
                        match step {
                            ParseStep::Yield => return Ok(ParseStep::Yield),
                            ParseStep::Event(None) => self.phase = TrackPhase::Fields,
                            ParseStep::Event(Some(child)) => {
                                let t = &mut self.out;
                                let bytes = &self.source[child.payload.clone()];
                                if video {
                                    match child.id {
                                        0xb0 if once(&mut self.seen, 12) => {
                                            t.width = ebml::uint(bytes)?;
                                        }
                                        0xba if once(&mut self.seen, 13) => {
                                            t.height = ebml::uint(bytes)?;
                                        }
                                        // A repeated Colour is ignored (RFC 8794 §12).
                                        crate::colour::COLOUR if t.colour.is_none() => {
                                            self.side = TrackSide::Colour(ColourJob::new(
                                                self.source.clone(),
                                                child.payload,
                                            ));
                                            continue 'entry;
                                        }
                                        _ => (),
                                    }
                                } else {
                                    match child.id {
                                        0x9f if once(&mut self.seen, 14) => {
                                            t.channels = ebml::uint(bytes)?;
                                        }
                                        0xb5 if once(&mut self.seen, 15) => {
                                            t.sample_rate = match bytes.len() {
                                                4 => f64::from(f32::from_be_bytes(
                                                    bytes
                                                        .try_into()
                                                        .map_err(|_| Error::Invalid("float"))?,
                                                )),
                                                8 => f64::from_be_bytes(
                                                    bytes
                                                        .try_into()
                                                        .map_err(|_| Error::Invalid("float"))?,
                                                ),
                                                _ => {
                                                    return Err(Error::Invalid(
                                                        "sampling frequency width",
                                                    ));
                                                }
                                            };
                                            if !t.sample_rate.is_finite() || t.sample_rate <= 0. {
                                                return Err(Error::Invalid("sampling frequency"));
                                            }
                                        }
                                        _ => (),
                                    }
                                }
                            }
                            _ => return Err(Error::Invalid("invalid audio/video transition")),
                        }
                    }
                    TrackPhase::PrivateStart => {
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        self.compressed_private = self
                            .prefixes
                            .private
                            .as_ref()
                            .is_some_and(crate::content_encoding::Decoder::contains_zlib);
                        let t = &mut self.out;
                        t.packet_prefix = self.prefixes.frame_prefix.clone();
                        t.packet_decoder.clone_from(&self.prefixes.frame);
                        self.phase = if self.has_extensions {
                            TrackPhase::ExtensionsStart
                        } else {
                            TrackPhase::Finish
                        };
                        if let Some(decoder) = &self.prefixes.private {
                            let limit = if self.compressed_private {
                                self.limits.metadata_bytes.min(self.private_budget)
                            } else {
                                self.limits.metadata_bytes
                            };
                            self.side = TrackSide::Private(RestoreJob::new(
                                t.codec_private.clone(),
                                RestorePlan::from_decoder(decoder),
                                self.limits,
                                limit,
                                &self.pool,
                            )?);
                            continue 'entry;
                        }
                    }
                    TrackPhase::ExtensionsStart => {
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        self.side = TrackSide::Extensions(
                            crate::track::extensions::TrackExtensionsJob::new(
                                self.source.clone(),
                                self.limits,
                                &self.pool,
                            )?,
                        );
                        self.phase = TrackPhase::Finish;
                        continue 'entry;
                    }
                    TrackPhase::Finish => {
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        let t = &self.out;
                        if t.number == 0 || t.uid == 0 || t.kind == 0 || !self.codec_nonempty {
                            return Err(Error::Invalid("missing track fields"));
                        }
                        self.private_bytes = if self.compressed_private {
                            t.codec_private.retained_capacity()
                        } else {
                            0
                        };
                        if self.private_bytes > self.private_budget {
                            return Err(Error::Policy("decoded private metadata capacity"));
                        }
                        self.phase = TrackPhase::Number { at: 0 };
                    }
                    TrackPhase::Number { at } | TrackPhase::Uid { at } => {
                        let at = *at;
                        let number = matches!(self.phase, TrackPhase::Number { .. });
                        if at == tracks.len() {
                            self.phase = if number {
                                TrackPhase::Uid { at: 0 }
                            } else {
                                TrackPhase::Store
                            };
                            continue;
                        }
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        let old = &tracks.as_slice()[at];
                        self.phase = if number {
                            if self.out.number == old.number {
                                return Err(Error::Invalid("duplicate track number"));
                            }
                            TrackPhase::Number { at: at + 1 }
                        } else {
                            if self.out.uid == old.uid {
                                return Err(Error::Invalid("duplicate track UID"));
                            }
                            TrackPhase::Uid { at: at + 1 }
                        };
                    }
                    TrackPhase::Store => {
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        let track =
                            std::mem::replace(&mut self.out, new_track(SharedBytes::default()));
                        if let Err(error) = tracks.try_push(track) {
                            self.out = error.value;
                            return Err(error.error);
                        }
                        self.phase = TrackPhase::Done;
                        return Ok(ParseStep::Event(self.private_bytes));
                    }
                    TrackPhase::Done => return Ok(ParseStep::Finished),
                }
            }
        }
    }
}

/// Where the reading of Tracks stands; the TrackEntry and seal jobs run in
/// `TracksJob::side`.
#[derive(Clone, Copy, Debug)]
enum Phase {
    Count,
    Reserve,
    CopyPrevious,
    Root,
    Done,
}
/// A job a Tracks phase started, which finishes before the phase goes on.
#[expect(
    clippy::large_enum_variant,
    reason = "one side job at a time, inline in the Tracks job; boxing it would allocate per track"
)]
#[derive(Debug)]
#[repr(u8)]
enum TracksSide {
    Idle,
    Entry(TrackJob),
    Seal(ConfigurationJob),
}
impl TracksSide {
    fn entry(&self) -> Option<&TrackJob> {
        match self {
            Self::Entry(job) => Some(job),
            _ => None,
        }
    }
    fn seal(&self) -> Option<&ConfigurationJob> {
        match self {
            Self::Seal(job) => Some(job),
            _ => None,
        }
    }
}
#[derive(Debug)]
pub(crate) struct TracksJob {
    source: SharedBytes,
    pool: Budget,
    previous: Option<Configuration>,
    limits: Limits,
    metadata_left: usize,
    generation: u64,
    revision: u64,
    phase: Phase,
    side: TracksSide,
    count: usize,
    counter: ChildCursor,
    cursor: ChildCursor,
    tracks: BudgetVec<Track>,
    old: usize,
    private_bytes: usize,
    failed: bool,
    webm_vp9_t35: bool,
}
impl TracksJob {
    pub(crate) fn metadata_bytes(&self) -> Result<usize> {
        self.source
            .len()
            .checked_add(self.private_bytes)
            .ok_or(Error::Overflow)
    }
    #[inline(always)]
    pub(crate) fn new(
        source: SharedBytes,
        previous: Option<Configuration>,
        limits: Limits,
        metadata_left: usize,
        pool: &Budget,
        generation: u64,
        revision: u64,
    ) -> Result<Self> {
        if !source.belongs_to(pool) || previous.as_ref().is_some_and(|old| !old.belongs_to(pool)) {
            return Err(Error::ForeignPool);
        }
        if source.len() > limits.metadata_bytes {
            return Err(Error::Policy("metadata"));
        }
        let end = source.len();
        Ok(Self {
            source,
            pool: pool.clone(),
            previous,
            limits,
            metadata_left,
            generation,
            revision,
            phase: Phase::Count,
            side: TracksSide::Idle,
            count: 0,
            counter: ChildCursor::new(0..end),
            cursor: ChildCursor::new(0..end),
            tracks: BudgetVec::new(pool),
            old: 0,
            private_bytes: 0,
            failed: false,
            webm_vp9_t35: false,
        })
    }
    pub(crate) fn with_webm_vp9_t35(mut self, enabled: bool) -> Self {
        self.webm_vp9_t35 = enabled;
        self
    }
    pub(crate) fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Configuration>> {
        if self.failed {
            return Err(Error::Invalid("Tracks requires replacement after error"));
        }
        let result = self.advance(work, cancel);
        match result {
            Err(Error::Backpressure(triergon_core::Backpressure {
                requested,
                available,
            })) => Ok(ParseStep::Backpressure {
                requested,
                available,
            }),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
            other => other,
        }
    }
    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Configuration>> {
        'tracks: loop {
            match &mut self.side {
                TracksSide::Idle => {}
                TracksSide::Entry(job) => {
                    check(cancel)?;
                    match job.step(&mut self.tracks, work, cancel)? {
                        ParseStep::Event(bytes) => {
                            self.private_bytes = self
                                .private_bytes
                                .checked_add(bytes)
                                .ok_or(Error::Overflow)?;
                            self.side = TracksSide::Idle;
                        }
                        other => return crate::step::pending(other),
                    }
                }
                TracksSide::Seal(job) => {
                    check(cancel)?;
                    match job.step(work, cancel)? {
                        ParseStep::Event(configuration) => {
                            self.side = TracksSide::Idle;
                            self.phase = Phase::Done;
                            return Ok(ParseStep::Event(configuration));
                        }
                        other => return crate::step::pending(other),
                    }
                }
            }
            loop {
                check(cancel)?;
                match self.phase {
                    Phase::Count => match self.counter.step(&self.source, work, cancel) {
                        Ok(ParseStep::Yield) => return Ok(ParseStep::Yield),
                        Ok(ParseStep::Event(Some(child))) => {
                            if child.id == 0xae {
                                self.count = self.count.checked_add(1).ok_or(Error::Overflow)?;
                            }
                        }
                        Ok(ParseStep::Event(None)) => self.phase = Phase::Reserve,
                        // Admission scanning does not publish a later syntax error
                        // before an earlier TrackEntry semantic error. The real
                        // source-order cursor below will encounter it again.
                        Err(Error::Cancelled) => return Err(Error::Cancelled),
                        Err(_) => self.phase = Phase::Reserve,
                        _ => return Err(Error::Invalid("invalid Tracks admission transition")),
                    },
                    Phase::Reserve => {
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        let old = self.previous.as_ref().map_or(0, |old| old.tracks().len());
                        let needed = old
                            .checked_add(self.count.min(self.limits.tracks.saturating_sub(old)))
                            .ok_or(Error::Overflow)?;
                        self.tracks.try_reserve_exact(needed)?;
                        self.phase = Phase::CopyPrevious;
                    }
                    Phase::CopyPrevious => {
                        let old = self
                            .previous
                            .as_ref()
                            .map_or(&[][..], Configuration::tracks);
                        if self.old == old.len() {
                            self.phase = Phase::Root;
                            continue;
                        }
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        self.tracks
                            .try_push(old[self.old].clone())
                            .map_err(|error| error.error)?;
                        self.old += 1;
                    }
                    Phase::Root => match self.cursor.step(&self.source, work, cancel)? {
                        ParseStep::Yield => return Ok(ParseStep::Yield),
                        ParseStep::Event(None) => {
                            let tracks =
                                std::mem::replace(&mut self.tracks, BudgetVec::new(&self.pool));
                            self.side = TracksSide::Seal(
                                ConfigurationJob::from_validated_vec(
                                    tracks,
                                    &self.pool,
                                    self.generation,
                                    self.revision,
                                )
                                .inherit_states(self.previous.as_ref())
                                .with_webm_vp9_t35(self.webm_vp9_t35),
                            );
                            continue 'tracks;
                        }
                        ParseStep::Event(Some(child)) => {
                            if child.id == 0xae {
                                if self.tracks.len() >= self.limits.tracks {
                                    return Err(Error::Policy("track count"));
                                }
                                let available = self
                                    .metadata_left
                                    .checked_sub(self.source.len())
                                    .and_then(|n| n.checked_sub(self.private_bytes))
                                    .ok_or(Error::Policy("decoded private metadata"))?;
                                self.side = TracksSide::Entry(TrackJob::new(
                                    self.source.slice(child.payload)?,
                                    self.limits,
                                    available,
                                    &self.pool,
                                )?);
                                continue 'tracks;
                            }
                        }
                        _ => return Err(Error::Invalid("invalid Tracks transition")),
                    },
                    Phase::Done => return Ok(ParseStep::Finished),
                }
            }
        }
    }
}

crate::storage::usage::storage!(TrackJob, |s| [
    s.source,
    s.out,
    s.side.text(),
    s.side.colour(),
    s.side.encodings(),
    s.prefixes,
    s.side.private(),
    s.side.extensions()
]);
crate::storage::usage::storage!(TracksJob, |s| [
    s.source,
    s.previous,
    s.tracks,
    crate::storage::usage::Tracks(s.tracks.as_slice()),
    s.side.entry(),
    s.side.seal()
]);

#[cfg(test)]
mod tests;
