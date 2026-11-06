//! Raw Tracks transaction. Text, Colour, encodings, private restoration and
//! duplicate checks advance under the caller's work budget before publication.
use super::{
    colour::ColourJob,
    configuration::{Configuration, ConfigurationJob},
    syntax::{ChildCursor, check},
};
use crate::content_encoding::{Encodings, typed::EncodingsJob};
use crate::matroska::typed::syntax::Required;
use crate::zlib::typed_restore::{RestoreJob, RestorePlan};
use crate::{
    Error, Limits, Result, ebml,
    incremental::ParseStep,
    matroska::Track,
    storage::{SharedBytes, SharedText, text_job::TextJob},
};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, BudgetVec, WorkBudget};

#[derive(Debug)]
enum TrackPhase {
    Fields,
    Video,
    Audio,
    Colour,
    Text,
    Encodings,
    PrivateStart,
    Private,
    ExtensionsStart,
    Extensions,
    Finish,
    Done,
}
#[derive(Debug)]
pub(crate) struct TrackResult {
    pub track: Track,
    pub private_bytes: usize,
}
#[derive(Debug)]
struct TrackJob {
    source: SharedBytes,
    pool: Budget,
    limits: Limits,
    private_budget: usize,
    out: Option<Track>,
    root: ChildCursor,
    inner: Option<ChildCursor>,
    phase: TrackPhase,
    text: Option<TextJob>,
    text_id: u64,
    codec_nonempty: bool,
    colour: Option<ColourJob>,
    encodings: Option<EncodingsJob>,
    encoded: bool,
    prefixes: Encodings,
    flags_seen: u16,
    languages_seen: u8,
    private: Option<RestoreJob>,
    compressed_private: bool,
    has_extensions: bool,
    extensions: Option<super::mapping::TrackExtensionsJob>,
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
            return Err(triergon_core::Error::ForeignPool);
        }
        let end = source.len();
        let out = Track {
            packet_prefix: SharedBytes::default(),
            packet_decoder: None,
            source_metadata: source.clone(),
            number: 0,
            uid: 0,
            kind: 0,
            codec: SharedText::constant(""),
            codec_private: SharedBytes::default(),
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
        };
        Ok(Self {
            source,
            pool: pool.clone(),
            limits,
            private_budget,
            out: Some(out),
            root: ChildCursor::new(0..end),
            inner: None,
            phase: TrackPhase::Fields,
            text: None,
            text_id: 0,
            codec_nonempty: false,
            colour: None,
            encodings: None,
            encoded: false,
            prefixes: Encodings::default(),
            flags_seen: 0,
            languages_seen: 0,
            private: None,
            compressed_private: false,
            has_extensions: false,
            extensions: None,
            failed: false,
        })
    }
    fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<TrackResult>> {
        if self.failed {
            return Err(Error::Invalid(
                "TrackEntry requires replacement after error",
            ));
        }
        let result = self.advance(work, cancel);
        match result {
            Err(Error::Backpressure {
                requested,
                available,
            }) => Ok(ParseStep::Backpressure {
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
    ) -> Result<ParseStep<TrackResult>> {
        loop {
            check(cancel)?;
            match self.phase {
                TrackPhase::Fields => match self.root.step(&self.source, work, cancel)? {
                    ParseStep::Yield => return Ok(ParseStep::Yield),
                    ParseStep::Event(None) => self.phase = TrackPhase::PrivateStart,
                    ParseStep::Event(Some(child)) => {
                        let t = self.out.as_mut().required("missing TrackEntry builder")?;
                        let bytes = &self.source[child.payload.clone()];
                        match child.id {
                            0xd7 => t.number = ebml::uint(bytes)?,
                            0x73c5 => t.uid = ebml::uint(bytes)?,
                            0x83 => t.kind = ebml::uint(bytes)?,
                            0x63a2 => t.codec_private = self.source.slice(child.payload)?,
                            0x55ee | 0x41e4 => self.has_extensions = true,
                            0x86 | 0x536e | 0x22b59c | 0x22b59d => {
                                if matches!(child.id, 0x22b59c | 0x22b59d) {
                                    let mask = if child.id == 0x22b59c { 1 } else { 2 };
                                    if self.languages_seen & mask != 0 {
                                        return Err(Error::Invalid("duplicate track language"));
                                    }
                                    self.languages_seen |= mask;
                                    if child.id == 0x22b59c && bytes.is_empty() {
                                        t.language = SharedText::constant("eng");
                                        continue;
                                    }
                                }
                                if matches!(child.id, 0x86 | 0x22b59c)
                                    && let Some(value) = super::super::static_metadata_text(bytes)
                                {
                                    if child.id == 0x86 {
                                        t.codec = SharedText::constant(value);
                                        self.codec_nonempty = true;
                                    } else {
                                        t.language = SharedText::constant(value);
                                    }
                                    continue;
                                }
                                self.text =
                                    Some(TextJob::new(self.source.clone(), child.payload, false)?);
                                self.text_id = child.id;
                                self.phase = TrackPhase::Text;
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
                                    return Err(Error::Invalid("duplicate track flag"));
                                }
                                self.flags_seen |= bit;
                                let value =
                                    if bytes.is_empty() && matches!(child.id, 0xb9 | 0x9c | 0x88) {
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
                            0x23e383 => t.default_duration_ns = Some(ebml::uint(bytes)?),
                            0x56aa => t.codec_delay_ns = ebml::uint(bytes)?,
                            0x56bb => t.seek_preroll_ns = ebml::uint(bytes)?,
                            0x23314f
                                if bytes != 1_f64.to_be_bytes() && bytes != 1_f32.to_be_bytes() =>
                            {
                                return Err(Error::Unsupported("TrackTimestampScale"));
                            }
                            0x6d80 => {
                                if self.encoded {
                                    return Err(Error::Invalid("duplicate ContentEncodings"));
                                }
                                self.encoded = true;
                                self.encodings = Some(EncodingsJob::new(
                                    self.source.clone(),
                                    child.payload,
                                    self.limits.metadata_bytes,
                                    &self.pool,
                                )?);
                                self.phase = TrackPhase::Encodings;
                            }
                            0xe0 | 0xe1 => {
                                self.inner = Some(ChildCursor::new(child.payload));
                                self.phase = if child.id == 0xe0 {
                                    TrackPhase::Video
                                } else {
                                    TrackPhase::Audio
                                };
                            }
                            _ => (),
                        }
                    }
                    _ => return Err(Error::Invalid("invalid TrackEntry transition")),
                },
                TrackPhase::Text => match self
                    .text
                    .as_mut()
                    .required("missing track text")?
                    .step(work, cancel)?
                {
                    ParseStep::Event(value) => {
                        let t = self.out.as_mut().required("missing TrackEntry builder")?;
                        match self.text_id {
                            0x86 => {
                                self.codec_nonempty = !value.range.is_empty();
                                t.codec = value.text;
                            }
                            0x536e => t.name = value.text,
                            0x22b59c => t.language = value.text,
                            0x22b59d => t.language_bcp47 = Some(value.text),
                            _ => return Err(Error::Invalid("invalid track text destination")),
                        }
                        self.text = None;
                        self.phase = TrackPhase::Fields;
                    }
                    other => return super::pending(other),
                },
                TrackPhase::Video | TrackPhase::Audio => match self
                    .inner
                    .as_mut()
                    .required("missing audio/video cursor")?
                    .step(&self.source, work, cancel)?
                {
                    ParseStep::Yield => return Ok(ParseStep::Yield),
                    ParseStep::Event(None) => {
                        self.inner = None;
                        self.phase = TrackPhase::Fields;
                    }
                    ParseStep::Event(Some(child)) => {
                        let t = self.out.as_mut().required("missing TrackEntry builder")?;
                        let bytes = &self.source[child.payload.clone()];
                        if matches!(self.phase, TrackPhase::Video) {
                            match child.id {
                                0xb0 => t.width = ebml::uint(bytes)?,
                                0xba => t.height = ebml::uint(bytes)?,
                                0x55b0 => {
                                    if t.colour.is_some() {
                                        return Err(Error::Invalid("duplicate Colour"));
                                    }
                                    self.colour =
                                        Some(ColourJob::new(self.source.clone(), child.payload));
                                    self.phase = TrackPhase::Colour;
                                }
                                _ => (),
                            }
                        } else {
                            match child.id {
                                0x9f => t.channels = ebml::uint(bytes)?,
                                0xb5 => {
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
                                            return Err(Error::Invalid("sampling frequency width"));
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
                },
                TrackPhase::Colour => match self
                    .colour
                    .as_mut()
                    .required("missing Colour job")?
                    .step(work, cancel)?
                {
                    ParseStep::Event(value) => {
                        self.out
                            .as_mut()
                            .required("missing TrackEntry builder")?
                            .colour = Some(value);
                        self.colour = None;
                        self.phase = TrackPhase::Video;
                    }
                    other => return super::pending(other),
                },
                TrackPhase::Encodings => match self
                    .encodings
                    .as_mut()
                    .required("missing encoding job")?
                    .step(work, cancel)?
                {
                    ParseStep::Event(value) => {
                        self.prefixes = value;
                        self.encodings = None;
                        self.phase = TrackPhase::Fields;
                    }
                    other => return super::pending(other),
                },
                TrackPhase::PrivateStart => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    self.compressed_private = self
                        .prefixes
                        .private
                        .as_ref()
                        .is_some_and(crate::content_encoding::Decoder::contains_zlib);
                    let t = self.out.as_mut().required("missing TrackEntry builder")?;
                    t.packet_prefix = self.prefixes.frame_prefix.clone();
                    t.packet_decoder = self.prefixes.frame.clone();
                    if let Some(decoder) = &self.prefixes.private {
                        let limit = if self.compressed_private {
                            self.limits.metadata_bytes.min(self.private_budget)
                        } else {
                            self.limits.metadata_bytes
                        };
                        self.private = Some(RestoreJob::new(
                            t.codec_private.clone(),
                            RestorePlan::from_decoder(decoder),
                            self.limits,
                            limit,
                            &self.pool,
                        )?);
                        self.phase = TrackPhase::Private;
                    } else {
                        self.phase = if self.has_extensions {
                            TrackPhase::ExtensionsStart
                        } else {
                            TrackPhase::Finish
                        };
                    }
                }
                TrackPhase::Private => match self
                    .private
                    .as_mut()
                    .required("missing private decode job")?
                    .step(work, cancel)?
                {
                    ParseStep::Event(value) => {
                        self.out
                            .as_mut()
                            .required("missing TrackEntry builder")?
                            .codec_private = value;
                        self.private = None;
                        self.phase = if self.has_extensions {
                            TrackPhase::ExtensionsStart
                        } else {
                            TrackPhase::Finish
                        };
                    }
                    other => return super::pending(other),
                },
                TrackPhase::ExtensionsStart => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    self.extensions = Some(super::mapping::TrackExtensionsJob::new(
                        self.source.clone(),
                        self.limits,
                        &self.pool,
                    )?);
                    self.phase = TrackPhase::Extensions;
                }
                TrackPhase::Extensions => match self
                    .extensions
                    .as_mut()
                    .required("missing TrackEntry extension validation")?
                    .step(work, cancel)?
                {
                    ParseStep::Event(_) => {
                        self.extensions = None;
                        self.phase = TrackPhase::Finish;
                    }
                    other => return super::pending(other),
                },
                TrackPhase::Finish => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let t = self.out.as_ref().required("missing TrackEntry builder")?;
                    if t.number == 0 || t.uid == 0 || t.kind == 0 || !self.codec_nonempty {
                        return Err(Error::Invalid("missing track fields"));
                    }
                    let private_bytes = if self.compressed_private {
                        t.codec_private.retained_capacity()
                    } else {
                        0
                    };
                    if private_bytes > self.private_budget {
                        return Err(Error::Policy("decoded private metadata capacity"));
                    }
                    let track = self.out.take().required("missing completed TrackEntry")?;
                    self.phase = TrackPhase::Done;
                    return Ok(ParseStep::Event(TrackResult {
                        track,
                        private_bytes,
                    }));
                }
                TrackPhase::Done => return Ok(ParseStep::Finished),
            }
        }
    }
}

#[derive(Debug)]
enum Phase {
    Count,
    Reserve,
    CopyPrevious,
    Root,
    Track,
    Number,
    Uid,
    Store,
    Seal,
    Done,
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
    count: usize,
    counter: ChildCursor,
    cursor: ChildCursor,
    tracks: Option<BudgetVec<Track>>,
    old: usize,
    entry: Option<TrackJob>,
    ready: Option<TrackResult>,
    compare: usize,
    private_bytes: usize,
    seal: Option<ConfigurationJob>,
    result: Option<Configuration>,
    failed: bool,
    webm_vp9_t35: bool,
}
impl TracksJob {
    pub fn metadata_bytes(&self) -> Result<usize> {
        self.source
            .len()
            .checked_add(self.private_bytes)
            .ok_or(Error::Overflow)
    }
    #[inline(always)]
    pub fn new(
        source: SharedBytes,
        previous: Option<Configuration>,
        limits: Limits,
        metadata_left: usize,
        pool: &Budget,
        generation: u64,
        revision: u64,
    ) -> Result<Self> {
        if !source.belongs_to(pool) || previous.as_ref().is_some_and(|old| !old.belongs_to(pool)) {
            return Err(triergon_core::Error::ForeignPool);
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
            count: 0,
            counter: ChildCursor::new(0..end),
            cursor: ChildCursor::new(0..end),
            tracks: Some(BudgetVec::new(pool)),
            old: 0,
            entry: None,
            ready: None,
            compare: 0,
            private_bytes: 0,
            seal: None,
            result: None,
            failed: false,
            webm_vp9_t35: false,
        })
    }
    pub(crate) fn with_webm_vp9_t35(mut self, enabled: bool) -> Self {
        self.webm_vp9_t35 = enabled;
        self
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Configuration>> {
        if self.failed {
            return Err(Error::Invalid("Tracks requires replacement after error"));
        }
        let result = self.advance(work, cancel);
        match result {
            Err(Error::Backpressure {
                requested,
                available,
            }) => Ok(ParseStep::Backpressure {
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
                    self.tracks
                        .as_mut()
                        .required("missing Tracks arena")?
                        .try_reserve_exact(needed)?;
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
                        .as_mut()
                        .required("missing Tracks arena")?
                        .try_push(old[self.old].clone())
                        .map_err(|error| error.error)?;
                    self.old += 1;
                }
                Phase::Root => match self.cursor.step(&self.source, work, cancel)? {
                    ParseStep::Yield => return Ok(ParseStep::Yield),
                    ParseStep::Event(None) => {
                        let tracks = self.tracks.take().required("missing Tracks arena")?;
                        self.seal = Some(
                            ConfigurationJob::from_validated_vec(
                                tracks,
                                &self.pool,
                                self.generation,
                                self.revision,
                            )
                            .inherit_states(self.previous.as_ref())
                            .with_webm_vp9_t35(self.webm_vp9_t35),
                        );
                        self.phase = Phase::Seal;
                    }
                    ParseStep::Event(Some(child)) => {
                        if child.id == 0xae {
                            if self.tracks.as_ref().required("missing Tracks arena")?.len()
                                >= self.limits.tracks
                            {
                                return Err(Error::Policy("track count"));
                            }
                            let available = self
                                .metadata_left
                                .checked_sub(self.source.len())
                                .and_then(|n| n.checked_sub(self.private_bytes))
                                .ok_or(Error::Policy("decoded private metadata"))?;
                            self.entry = Some(TrackJob::new(
                                self.source.slice(child.payload)?,
                                self.limits,
                                available,
                                &self.pool,
                            )?);
                            self.phase = Phase::Track;
                        }
                    }
                    _ => return Err(Error::Invalid("invalid Tracks transition")),
                },
                Phase::Track => match self
                    .entry
                    .as_mut()
                    .required("missing TrackEntry job")?
                    .step(work, cancel)?
                {
                    ParseStep::Event(value) => {
                        self.ready = Some(value);
                        self.entry = None;
                        self.compare = 0;
                        self.phase = Phase::Number;
                    }
                    other => return super::pending(other),
                },
                Phase::Number | Phase::Uid => {
                    let tracks = self.tracks.as_ref().required("missing Tracks arena")?;
                    if self.compare == tracks.len() {
                        self.compare = 0;
                        self.phase = if matches!(self.phase, Phase::Number) {
                            Phase::Uid
                        } else {
                            Phase::Store
                        };
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let track = &self
                        .ready
                        .as_ref()
                        .required("missing validated TrackEntry")?
                        .track;
                    let old = &tracks.as_slice()[self.compare];
                    if matches!(self.phase, Phase::Number) && track.number == old.number {
                        return Err(Error::Invalid("duplicate track number"));
                    }
                    if matches!(self.phase, Phase::Uid) && track.uid == old.uid {
                        return Err(Error::Invalid("duplicate track UID"));
                    }
                    self.compare += 1;
                }
                Phase::Store => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let ready = self.ready.take().required("missing validated TrackEntry")?;
                    let bytes = ready.private_bytes;
                    match self
                        .tracks
                        .as_mut()
                        .required("missing Tracks arena")?
                        .try_push(ready.track)
                    {
                        Ok(()) => {
                            self.private_bytes = self
                                .private_bytes
                                .checked_add(bytes)
                                .ok_or(Error::Overflow)?;
                            self.phase = Phase::Root;
                        }
                        Err(error) => {
                            self.ready = Some(TrackResult {
                                track: error.value,
                                private_bytes: bytes,
                            });
                            return Err(error.error);
                        }
                    }
                }
                Phase::Seal => match self
                    .seal
                    .as_mut()
                    .required("missing configuration seal")?
                    .step(work, cancel)?
                {
                    ParseStep::Event(value) => {
                        self.result = Some(value);
                        self.seal = None;
                        self.phase = Phase::Done;
                    }
                    other => return super::pending(other),
                },
                Phase::Done => {
                    return Ok(ParseStep::Event(
                        self.result
                            .as_ref()
                            .required("missing configuration result")?
                            .clone(),
                    ));
                }
            }
        }
    }
}

super::usage::storage!(TrackResult, |s| [s.track]);
super::usage::storage!(TrackJob, |s| [
    s.source,
    s.out,
    s.text,
    s.colour,
    s.encodings,
    s.prefixes,
    s.private,
    s.extensions
]);
super::usage::storage!(TracksJob, |s| [
    s.source,
    s.previous,
    s.tracks,
    super::usage::Tracks(s.tracks.as_ref().map_or(&[][..], BudgetVec::as_slice)),
    s.entry,
    s.ready,
    s.seal,
    s.result
]);
