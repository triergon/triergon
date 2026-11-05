//! Explicit regular encoded catalogue view. Config records stay opaque; NAL,
//! ASC/ADTS and decoder validation belong to their existing separate adapters.
use crate::{
    BoxChildren, BoxStep, BoxView, EntryLayout, Limits, Mov9CursorStatus, Mov9MediaClock,
    Mov9StreamView, SampleDescription, SampleTable, TrackHeader, fields,
};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, InputCursor, InputToken, Result, SourceId, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub struct PinnedMovD32EncodedRegularV1;
#[derive(Clone, Copy, Debug)]
pub struct Mov9EncodedLimitsV1 {
    pub max_config_bytes: usize,
    pub max_child_boxes: u32,
    pub max_descriptors: u32,
}
impl Default for Mov9EncodedLimitsV1 {
    fn default() -> Self {
        Self {
            max_config_bytes: 1 << 20,
            max_child_boxes: 4096,
            max_descriptors: 4096,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodedEntryKindV1 {
    Avc,
    Hevc,
    Aac,
    Ac3,
    Eac3,
}
#[derive(Clone, Debug)]
pub struct Mov9EncodedStreamV1 {
    native: Mov9StreamView,
    entry: SampleDescription,
    configuration: BoxView,
    encoder: Option<Bytes>,
    kind: EncodedEntryKindV1,
    raw_headers: [BoxView; 3],
    receiver_source_v4: Option<crate::Mov9EncodedSourceV4>,
    wave_source_v7: Option<crate::Mov9WaveObservationV7>,
}
impl Mov9EncodedStreamV1 {
    pub fn wave_source_v7(&self) -> Option<&crate::Mov9WaveObservationV7> {
        self.wave_source_v7.as_ref()
    }

    pub fn native(&self) -> &Mov9StreamView {
        &self.native
    }
    pub fn source_description(&self) -> &SampleDescription {
        &self.entry
    }
    pub fn configuration_box(&self) -> &BoxView {
        &self.configuration
    }
    pub fn encoder_name(&self) -> Option<&Bytes> {
        self.encoder.as_ref()
    }
    pub fn kind(&self) -> EncodedEntryKindV1 {
        self.kind
    }
    pub fn receiver_source_v4(&self) -> Option<&crate::Mov9EncodedSourceV4> {
        self.receiver_source_v4.as_ref()
    }
    pub fn raw_headers(&self) -> &[BoxView; 3] {
        &self.raw_headers
    }
}

// A bounded owner-relative TLV walk. Each header/prefix byte is inspected once;
// opaque bodies are only range checked and borrowed, never parsed on a countdown.
#[derive(Clone, Copy)]
enum DescriptorPhase {
    Tag,
    Length {
        tag: u8,
        value: usize,
        bytes: u8,
    },
    Prefix {
        tag: u8,
        end: usize,
        index: usize,
        es_flags: u8,
        skip: usize,
    },
    Body {
        tag: u8,
        end: usize,
    },
}
struct EsdsWalk {
    raw: Bytes,
    at: usize,
    ends: [usize; 3],
    depth: usize,
    phase: DescriptorPhase,
    rows: u32,
    limit: u32,
    dsi: Option<(usize, Bytes)>,
    decoder_seen: bool,
    completed: bool,
}
impl EsdsWalk {
    fn new(raw: Bytes, limit: u32) -> Result<Self> {
        if raw.as_ref().get(..4) != Some(&[0; 4]) {
            return Err(Error::Unsupported("encoded esds full-box version/flags"));
        }
        let end = raw.len();
        Ok(Self {
            raw,
            at: 4,
            ends: [end, 0, 0],
            depth: 0,
            phase: DescriptorPhase::Tag,
            rows: 0,
            limit,
            dsi: None,
            decoder_seen: false,
            completed: false,
        })
    }
    fn byte(&mut self) -> Result<u8> {
        if self.at >= self.ends[self.depth] {
            return Err(Error::Truncated);
        }
        let value = *self.raw.as_ref().get(self.at).ok_or(Error::Truncated)?;
        self.at += 1;
        Ok(value)
    }
    fn poll(&mut self, work: &mut WorkBudget) -> Result<bool> {
        if self.completed {
            return Ok(true);
        }
        loop {
            if !work.try_charge(1) {
                return Ok(false);
            }
            match self.phase {
                DescriptorPhase::Tag => {
                    if self.at == self.ends[self.depth] {
                        if self.depth == 0 {
                            self.completed = true;
                            return Ok(true);
                        }
                        self.depth -= 1;
                        continue;
                    }
                    self.rows = self.rows.checked_add(1).ok_or(Error::Overflow)?;
                    if self.rows > self.limit {
                        return Err(Error::Limit {
                            requested: self.rows as usize,
                            maximum: self.limit as usize,
                        });
                    }
                    let tag = self.byte()?;
                    self.phase = DescriptorPhase::Length {
                        tag,
                        value: 0,
                        bytes: 0,
                    };
                }
                DescriptorPhase::Length { tag, value, bytes } => {
                    let b = self.byte()?;
                    let value = value
                        .checked_mul(128)
                        .and_then(|x| x.checked_add(usize::from(b & 127)))
                        .ok_or(Error::Overflow)?;
                    let bytes = bytes.checked_add(1).ok_or(Error::Overflow)?;
                    if b & 128 != 0 {
                        if bytes == 4 {
                            return Err(Error::Unsupported(
                                "encoded unterminated descriptor length",
                            ));
                        }
                        self.phase = DescriptorPhase::Length { tag, value, bytes };
                    } else {
                        let end = self.at.checked_add(value).ok_or(Error::Overflow)?;
                        if end > self.ends[self.depth] {
                            return Err(Error::Truncated);
                        }
                        self.phase =
                            if (tag == 3 && self.depth == 0) || (tag == 4 && self.depth == 1) {
                                if tag == 4 {
                                    if self.decoder_seen {
                                        return Err(Error::Unsupported(
                                            "encoded multiple decoder configs",
                                        ));
                                    }
                                    self.decoder_seen = true;
                                }
                                DescriptorPhase::Prefix {
                                    tag,
                                    end,
                                    index: 0,
                                    es_flags: 0,
                                    skip: 0,
                                }
                            } else {
                                DescriptorPhase::Body { tag, end }
                            };
                    }
                }
                DescriptorPhase::Prefix {
                    tag,
                    end,
                    index,
                    mut es_flags,
                    mut skip,
                } => {
                    if self.at >= end {
                        return Err(Error::Truncated);
                    }
                    let b = self.byte()?;
                    let next = index.checked_add(1).ok_or(Error::Overflow)?;
                    let done = if tag == 4 {
                        if index == 0 && b != 0x40 {
                            return Err(Error::Unsupported(
                                "encoded mp4a object-type is not selected AAC",
                            ));
                        }
                        if index == 1 && (b >> 2 != 5 || b & 1 == 0) {
                            return Err(Error::Unsupported("encoded AAC decoder stream-type"));
                        }
                        next == 13
                    } else {
                        // ES_ID(2), flags(1), optional dependsOn_ES_ID(2),
                        // URL length+bytes, OCR_ES_ID(2). The raw bytes survive.
                        if index == 2 {
                            es_flags = b;
                            skip = if b & 128 != 0 { 2 } else { 0 };
                        } else if index > 2 {
                            if skip != 0 {
                                skip -= 1;
                            } else if es_flags & 64 != 0 {
                                es_flags &= !64;
                                skip = usize::from(b);
                            } else if es_flags & 32 != 0 {
                                es_flags &= !32;
                                skip = 1;
                            }
                        }
                        next >= 3 && skip == 0 && es_flags & 96 == 0
                    };
                    if done {
                        if self.depth == 2 {
                            return Err(Error::Unsupported("encoded descriptor nesting"));
                        }
                        self.depth += 1;
                        self.ends[self.depth] = end;
                        self.phase = DescriptorPhase::Tag;
                    } else {
                        self.phase = DescriptorPhase::Prefix {
                            tag,
                            end,
                            index: next,
                            es_flags,
                            skip,
                        };
                    }
                }
                DescriptorPhase::Body { tag, end } => {
                    if tag == 5 && self.depth == 2 {
                        if self.dsi.is_some() {
                            return Err(Error::Unsupported(
                                "encoded duplicate decoder-specific config",
                            ));
                        }
                        self.dsi = Some((self.at, self.raw.slice(self.at..end)?));
                    }
                    self.at = end;
                    self.phase = DescriptorPhase::Tag;
                }
            }
        }
    }
}

/// Decoder-specific bytes borrowed from the original AAC sample entry.
#[derive(Debug)]
pub struct Mp4AacConfig {
    pub offset: u64,
    pub bytes: Bytes,
}

/// Extracts AudioSpecificConfig from an `mp4a` description independently
/// of its sample table. A QuickTime sound description may carry one
/// `wave` container around the elementary stream descriptor.
pub struct Mp4AacConfigJob {
    children: BoxChildren,
    wave: Option<BoxChildren>,
    seen_wave: bool,
    wave_fields: AacWaveFields,
    scanned: bool,
    limits: Mov9EncodedLimitsV1,
    inspected_boxes: u32,
    esds: Option<(u64, EsdsWalk)>,
    failed: Option<Error>,
}

impl Mp4AacConfigJob {
    pub fn new(entry: &SampleDescription, limits: Mov9EncodedLimitsV1) -> Result<Self> {
        if entry.format != *b"mp4a" {
            return Err(Error::Invalid(
                "AAC configuration requires an mp4a description",
            ));
        }
        let child_limits = Limits {
            max_boxes: limits.max_child_boxes,
            ..Limits::default()
        };
        let children = if fields::u16_at(entry.raw().payload(), 8)? == 1 {
            crate::Mov9SoundV1SourceV4::parse_v7(entry)?;
            entry.raw().children(44, child_limits)?
        } else {
            entry.extensions(EntryLayout::AudioV0, child_limits)?
        };
        Ok(Self {
            children,
            wave: None,
            seen_wave: false,
            wave_fields: AacWaveFields::default(),
            scanned: false,
            limits,
            inspected_boxes: 0,
            esds: None,
            failed: None,
        })
    }

    /// Yields when the work budget runs out. A parse error ends this job.
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Mp4AacConfig>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.advance(work, cancel);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }

    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Mp4AacConfig>> {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        while !self.scanned {
            let nested = self.wave.is_some();
            let children = self.wave.as_mut().unwrap_or(&mut self.children);
            match children.poll(work)? {
                BoxStep::Yield => return Ok(None),
                BoxStep::End if nested => {
                    if !self.wave_fields.complete() || self.esds.is_none() {
                        return Err(Error::Unsupported("incomplete AAC wave context"));
                    }
                    self.wave = None;
                }
                BoxStep::End => self.scanned = true,
                BoxStep::Box(raw) => {
                    self.inspected_boxes =
                        self.inspected_boxes.checked_add(1).ok_or(Error::Overflow)?;
                    if self.inspected_boxes > self.limits.max_child_boxes {
                        return Err(Error::Limit {
                            requested: self.inspected_boxes as usize,
                            maximum: self.limits.max_child_boxes as usize,
                        });
                    }
                    if nested {
                        self.wave_fields.inspect(&raw)?;
                    }
                    match &raw.header().kind {
                        b"esds" => self.accept_descriptor(raw)?,
                        b"wave" => {
                            if self.seen_wave || self.esds.is_some() {
                                return Err(Error::Unsupported("multiple AAC configuration paths"));
                            }
                            self.seen_wave = true;
                            self.wave = Some(raw.children(
                                0,
                                Limits {
                                    max_boxes: self.limits.max_child_boxes,
                                    ..Limits::default()
                                },
                            )?);
                        }
                        _ => {}
                    }
                }
            }
        }
        let (base, walker) = self
            .esds
            .as_mut()
            .ok_or(Error::Invalid("missing AAC elementary stream descriptor"))?;
        if !walker.poll(work)? {
            return Ok(None);
        }
        let (relative, bytes) = walker
            .dsi
            .take()
            .ok_or(Error::Invalid("missing AAC decoder-specific bytes"))?;
        if bytes.is_empty() {
            return Err(Error::Invalid("empty AAC decoder-specific bytes"));
        }
        let offset = base.checked_add(relative as u64).ok_or(Error::Overflow)?;
        Ok(Some(Mp4AacConfig { offset, bytes }))
    }

    fn accept_descriptor(&mut self, raw: BoxView) -> Result<()> {
        if self.esds.is_some() {
            return Err(Error::Invalid("duplicate AAC elementary stream descriptor"));
        }
        let length = raw.payload().len();
        if length > self.limits.max_config_bytes {
            return Err(Error::Limit {
                requested: length,
                maximum: self.limits.max_config_bytes,
            });
        }
        let header = raw.header();
        let offset = header
            .start
            .checked_add(u64::from(header.header_size))
            .ok_or(Error::Overflow)?;
        let bytes = raw
            .bytes()
            .slice(usize::from(header.header_size)..raw.bytes().len())?;
        self.esds = Some((offset, EsdsWalk::new(bytes, self.limits.max_descriptors)?));
        Ok(())
    }
}

#[derive(Default)]
struct AacWaveFields {
    format: bool,
    marker: bool,
    terminator: bool,
}

impl AacWaveFields {
    fn inspect(&mut self, raw: &BoxView) -> Result<()> {
        if self.terminator {
            return Err(Error::Invalid("AAC wave child after terminator"));
        }
        match &raw.header().kind {
            b"frma" => {
                if self.format || raw.payload() != b"mp4a" {
                    return Err(Error::Invalid("AAC wave format"));
                }
                self.format = true;
            }
            b"mp4a" => {
                if self.marker || raw.payload() != [0; 4] {
                    return Err(Error::Invalid("AAC wave marker"));
                }
                self.marker = true;
            }
            [0, 0, 0, 0] => {
                if !raw.payload().is_empty() {
                    return Err(Error::Invalid("AAC wave terminator data"));
                }
                self.terminator = true;
            }
            _ => {}
        }
        Ok(())
    }

    fn complete(&self) -> bool {
        self.format && self.marker && self.terminator
    }
}

// One nested, charged-owner-relative wave context; no recursive parser or queue.
struct WaveV4 {
    raw: BoxView,
    children: Option<BoxChildren>,
    format: Option<BoxView>,
    marker: Option<BoxView>,
    terminator: Option<BoxView>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DescriptionView {
    Regular,
    MetadataV4,
    ContextsV7,
    RangePlayback,
}
impl DescriptionView {
    fn observes_contexts(self) -> bool {
        self == Self::ContextsV7
    }
}

/// Metadata-only job over the original charged source owners. Source/request
/// generations, work, cancellation and transport checks use the shared core.
pub struct Mov9EncodedStreamJobV1<'a> {
    header: &'a TrackHeader,
    entry: SampleDescription,
    children: Option<BoxChildren>,
    configuration: Option<BoxView>,
    esds: Option<EsdsWalk>,
    kind: EncodedEntryKindV1,
    config_kind: [u8; 4],
    limits: Mov9EncodedLimitsV1,
    pool: Budget,
    source: SourceId,
    token: InputToken,
    result: Option<Mov9EncodedStreamV1>,
    failed: Option<Error>,
    finished: bool,
    view: DescriptionView,
    major: Option<[u8; 4]>,
    sound_v4: Option<crate::Mov9SoundV1SourceV4>,
    wave_v4: Option<WaveV4>,
    inspected_boxes: u32,
    wave_source_v7: Option<crate::Mov9WaveObservationV7>,
    pending_esds_v7: Option<(BoxView, EsdsWalk)>,
    descriptors_left_v7: u32,
}
impl<'a> Mov9EncodedStreamJobV1<'a> {
    pub fn new(
        header: &'a TrackHeader,
        table: &SampleTable,
        input: &InputCursor,
        major: Option<[u8; 4]>,
        limits: Mov9EncodedLimitsV1,
        _profile: PinnedMovD32EncodedRegularV1,
    ) -> Result<Self> {
        Self::new_description_inner(
            header,
            table,
            input,
            crate::Mov9EncodedSelectionV4 {
                major_brand: major,
                limits,
                description: 1,
            },
            DescriptionView::Regular,
        )
    }
    pub fn new_description_v3(
        header: &'a TrackHeader,
        table: &SampleTable,
        input: &InputCursor,
        major: Option<[u8; 4]>,
        limits: Mov9EncodedLimitsV1,
        description: u32,
        _profile: crate::PinnedMovD32EncodedRegularV3,
    ) -> Result<Self> {
        Self::new_description_inner(
            header,
            table,
            input,
            crate::Mov9EncodedSelectionV4 {
                major_brand: major,
                limits,
                description,
            },
            DescriptionView::Regular,
        )
    }
    /// V4 admits the independently sourced QuickTime VBR AudioV1/wave layout
    /// and separate legacy metadata presence rules. Existing profiles stay exact.
    pub fn new_description_v4(
        header: &'a TrackHeader,
        table: &SampleTable,
        input: &InputCursor,
        selection: crate::Mov9EncodedSelectionV4,
        _profile: crate::PinnedMovD32EncodedRegularV4,
    ) -> Result<Self> {
        Self::new_description_inner(header, table, input, selection, DescriptionView::MetadataV4)
    }
    /// Named receiver context: source wave words/ordering and the old strict
    /// profile's first fault remain owned; no valid-source result is fabricated.
    pub fn new_description_v7(
        header: &'a TrackHeader,
        table: &SampleTable,
        input: &InputCursor,
        selection: crate::Mov9EncodedSelectionV4,
        _profile: crate::PinnedMovD32EncodedContextsV7,
    ) -> Result<Self> {
        Self::new_description_inner(header, table, input, selection, DescriptionView::ContextsV7)
    }
    /// Range playback requires an unambiguous AAC configuration and admits
    /// AVC and HEVC visual entries in a QuickTime movie.
    pub fn new_range_playback(
        header: &'a TrackHeader,
        table: &SampleTable,
        input: &InputCursor,
        selection: crate::Mov9EncodedSelectionV4,
    ) -> Result<Self> {
        Self::new_description_inner(
            header,
            table,
            input,
            selection,
            DescriptionView::RangePlayback,
        )
    }
    fn new_description_inner(
        header: &'a TrackHeader,
        table: &SampleTable,
        input: &InputCursor,
        selection: crate::Mov9EncodedSelectionV4,
        view: DescriptionView,
    ) -> Result<Self> {
        let crate::Mov9EncodedSelectionV4 {
            major_brand: major,
            limits,
            description,
        } = selection;
        let receiver_v4 = view != DescriptionView::Regular;
        if !table.raw().bytes().belongs_to(input.budget())
            || [&header.raw_tkhd, &header.raw_mdhd, &header.raw_hdlr]
                .iter()
                .any(|x| !x.bytes().belongs_to(input.budget()))
        {
            return Err(Error::ForeignPool);
        }
        if !matches!(major,Some(x) if x==*b"isom"||x==*b"iso2"||x==*b"mp41"||x==*b"mp42"||(receiver_v4&&x==*b"qt  "))
        {
            return Err(Error::Unsupported("encoded native major-brand profile"));
        }
        let at = usize::try_from(
            description
                .checked_sub(1)
                .ok_or(Error::Invalid("encoded sample description zero"))?,
        )
        .map_err(|_| Error::Overflow)?;
        let entry = table
            .descriptions()
            .get(at)
            .ok_or(Error::Invalid("encoded initial sample description absent"))?
            .clone();
        if table.sample_count() == 0 {
            return Err(Error::Unsupported(
                "encoded nonempty regular source required",
            ));
        }
        if entry.data_reference_index != 1 {
            return Err(Error::Unsupported("encoded external data reference"));
        }
        let (kind, config_kind, layout) = match (&header.handler, &entry.format) {
            (b"vide", b"avc1" | b"avc3") => {
                (EncodedEntryKindV1::Avc, *b"avcC", EntryLayout::Visual)
            }
            (b"vide", b"hvc1" | b"hev1") => {
                (EncodedEntryKindV1::Hevc, *b"hvcC", EntryLayout::Visual)
            }
            (b"soun", b"mp4a") => (EncodedEntryKindV1::Aac, *b"esds", EntryLayout::AudioV0),
            (b"soun", b"ac-3") => (EncodedEntryKindV1::Ac3, *b"dac3", EntryLayout::AudioV0),
            (b"soun", b"ec-3") => (EncodedEntryKindV1::Eac3, *b"dec3", EntryLayout::AudioV0),
            _ => return Err(Error::Unsupported("encoded catalogue namespace/handler")),
        };
        let child_limits = Limits {
            max_boxes: limits.max_child_boxes,
            ..Limits::default()
        };
        let sound_v4 = if receiver_v4
            && major == Some(*b"qt  ")
            && kind == EncodedEntryKindV1::Aac
            && fields::u16_at(entry.raw().payload(), 8)? == 1
        {
            Some(match view {
                DescriptionView::ContextsV7 | DescriptionView::RangePlayback => {
                    crate::Mov9SoundV1SourceV4::parse_v7(&entry)?
                }
                _ => crate::Mov9SoundV1SourceV4::parse(&entry)?,
            })
        } else {
            None
        };
        let quicktime_visual =
            view == DescriptionView::RangePlayback && matches!(layout, EntryLayout::Visual);
        if receiver_v4 && major == Some(*b"qt  ") && sound_v4.is_none() && !quicktime_visual {
            return Err(Error::Unsupported(
                "encoded V4 QuickTime AudioV1/mp4a profile only",
            ));
        }
        let children = if sound_v4.is_some() {
            entry.raw().children(44, child_limits)?
        } else {
            entry.extensions(layout, child_limits)?
        };
        Ok(Self {
            header,
            entry,
            children: Some(children),
            configuration: None,
            esds: None,
            kind,
            config_kind,
            limits,
            pool: input.budget().clone(),
            source: input.source(),
            token: input.token(),
            result: None,
            failed: None,
            finished: false,
            view,
            major,
            sound_v4,
            wave_v4: None,
            inspected_boxes: 0,
            wave_source_v7: None,
            pending_esds_v7: None,
            descriptors_left_v7: limits.max_descriptors,
        })
    }
    fn inspect_box(&mut self) -> Result<()> {
        if self.view != DescriptionView::Regular {
            self.inspected_boxes = self.inspected_boxes.checked_add(1).ok_or(Error::Overflow)?;
            if self.inspected_boxes > self.limits.max_child_boxes {
                return Err(Error::Limit {
                    requested: self.inspected_boxes as usize,
                    maximum: self.limits.max_child_boxes as usize,
                });
            }
        }
        Ok(())
    }
    fn accept_configuration(&mut self, raw: BoxView) -> Result<()> {
        if raw.payload().len() > self.limits.max_config_bytes {
            return Err(Error::Limit {
                requested: raw.payload().len(),
                maximum: self.limits.max_config_bytes,
            });
        }
        if self.view.observes_contexts()
            && self.wave_v4.is_some()
            && self.kind == EncodedEntryKindV1::Aac
        {
            if let Some(report) = &mut self.wave_source_v7 {
                report.configuration_count = report
                    .configuration_count
                    .checked_add(1)
                    .ok_or(Error::Overflow)?;
                if self.configuration.is_some() {
                    report.fault(
                        Error::Unsupported("encoded duplicate configuration box"),
                        raw.clone(),
                        report.child_count,
                    )?;
                }
            }
            let at = usize::from(raw.header().header_size);
            let bytes = raw.bytes().slice(at..raw.bytes().len())?;
            self.pending_esds_v7 = Some((raw, EsdsWalk::new(bytes, self.descriptors_left_v7)?));
        } else {
            if self.configuration.is_some() {
                return Err(Error::Unsupported("encoded duplicate configuration box"));
            }
            self.configuration = Some(raw);
        }
        Ok(())
    }
    fn check(
        &self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<()> {
        if !input.budget().same_pool(&self.pool) {
            return Err(Error::ForeignPool);
        }
        if input.source() != self.source {
            return Err(Error::SourceMismatch);
        }
        if input.token().generation() != self.token.generation() {
            return Err(Error::StaleGeneration {
                expected: self.token.generation(),
                received: input.token().generation(),
            });
        }
        input.read_into(&mut [], work, cancel)?;
        Ok(())
    }
    pub fn poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Mov9CursorStatus> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        let answer = self.inner(input, work, cancel);
        if let Err(e) = answer {
            self.failed = Some(e);
            self.result = None;
        }
        answer
    }
    fn inner(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Mov9CursorStatus> {
        self.check(input, work, cancel)?;
        if self.result.is_some() {
            return Ok(Mov9CursorStatus::Ready);
        }
        if self.finished {
            return Ok(Mov9CursorStatus::End);
        }
        loop {
            self.check(input, work, cancel)?;
            if let Some((_, walker)) = &mut self.pending_esds_v7 {
                let outcome = walker.poll(work);
                match outcome {
                    Err(Error::Limit { requested, .. }) => {
                        return Err(Error::Limit {
                            requested: (self.limits.max_descriptors - self.descriptors_left_v7)
                                as usize
                                + requested,
                            maximum: self.limits.max_descriptors as usize,
                        });
                    }
                    Err(error) => return Err(error),
                    Ok(false) => return Ok(Mov9CursorStatus::Yield),
                    Ok(true) => {}
                }
                let (raw, walker) = self
                    .pending_esds_v7
                    .take()
                    .ok_or(Error::Invalid("V7 config pending invariant"))?;
                if walker.dsi.is_none() {
                    return Err(Error::Unsupported(
                        "encoded AAC decoder-specific config absent",
                    ));
                }
                self.descriptors_left_v7 = self
                    .descriptors_left_v7
                    .checked_sub(walker.rows)
                    .ok_or(Error::Overflow)?;
                self.configuration = Some(raw);
                self.esds = Some(walker);
                continue;
            }
            if work.remaining() == 0 {
                return Ok(Mov9CursorStatus::Yield);
            }
            let wave_step =
                if let Some(children) = self.wave_v4.as_mut().and_then(|w| w.children.as_mut()) {
                    Some(children.poll(work)?)
                } else {
                    None
                };
            if let Some(step) = wave_step {
                match step {
                    BoxStep::Yield => return Ok(Mov9CursorStatus::Yield),
                    BoxStep::Box(raw) => {
                        self.inspect_box()?;
                        let w = self
                            .wave_v4
                            .as_mut()
                            .ok_or(Error::Invalid("wave state absent"))?;
                        let ordinal = if let Some(report) = &mut self.wave_source_v7 {
                            report.child_count =
                                report.child_count.checked_add(1).ok_or(Error::Overflow)?;
                            report.child_count
                        } else {
                            0
                        };
                        if w.terminator.is_some() {
                            let error = Error::Invalid("encoded wave child after terminator");
                            if let Some(report) = &mut self.wave_source_v7 {
                                report.fault(error, raw.clone(), ordinal)?;
                            } else {
                                return Err(error);
                            }
                        }
                        match raw.header().kind {
                            x if x == *b"frma" => {
                                if raw.payload().len() != 4 {
                                    return Err(Error::Truncated);
                                }
                                if w.format.is_some() {
                                    let e = Error::Invalid("encoded duplicate wave format");
                                    if let Some(r) = &mut self.wave_source_v7 {
                                        r.fault(e, raw.clone(), ordinal)?;
                                    } else {
                                        return Err(e);
                                    }
                                }
                                if raw.payload() != b"mp4a" {
                                    let e = Error::Unsupported(
                                        "encoded wave format conflicts with entry",
                                    );
                                    if let Some(r) = &mut self.wave_source_v7 {
                                        r.fault(e, raw.clone(), ordinal)?;
                                    } else {
                                        return Err(e);
                                    }
                                }
                                if let Some(r) = &mut self.wave_source_v7 {
                                    r.format_count =
                                        r.format_count.checked_add(1).ok_or(Error::Overflow)?;
                                    if r.format.is_none() {
                                        r.format = Some(raw.clone());
                                    }
                                }
                                if w.format.is_none() {
                                    w.format = Some(raw);
                                }
                            }
                            x if x == *b"mp4a" => {
                                if w.marker.is_some() {
                                    return Err(Error::Invalid(
                                        "encoded duplicate wave mp4a marker",
                                    ));
                                }
                                if raw.payload() != [0; 4] {
                                    return Err(Error::Unsupported(
                                        "encoded wave mp4a marker profile",
                                    ));
                                }
                                if let Some(r) = &mut self.wave_source_v7 {
                                    r.marker = Some(raw.clone());
                                }
                                w.marker = Some(raw);
                            }
                            [0, 0, 0, 0] => {
                                if !raw.payload().is_empty() {
                                    let e = Error::Invalid("encoded wave terminator data");
                                    if let Some(r) = &mut self.wave_source_v7 {
                                        r.fault(e, raw.clone(), ordinal)?;
                                    } else {
                                        return Err(e);
                                    }
                                }
                                if let Some(r) = &mut self.wave_source_v7 {
                                    if r.terminator.is_none() {
                                        r.terminator = Some(raw.clone());
                                    }
                                }
                                w.terminator = Some(raw);
                            }
                            x if x == self.config_kind => self.accept_configuration(raw)?,
                            x if x == *b"wave" => {
                                return Err(Error::Unsupported("encoded nested wave profile"));
                            }
                            _ => {}
                        }
                    }
                    BoxStep::End => {
                        let w = self
                            .wave_v4
                            .as_mut()
                            .ok_or(Error::Invalid("wave state absent"))?;
                        if w.format.is_none() || w.marker.is_none() || w.terminator.is_none() {
                            let error = Error::Unsupported("encoded incomplete mp4a wave context");
                            if let Some(report) = &mut self.wave_source_v7 {
                                report.fault(error, w.raw.clone(), report.child_count)?;
                            } else {
                                return Err(error);
                            }
                        }
                        w.children = None;
                    }
                }
                continue;
            }
            if let Some(children) = &mut self.children {
                match children.poll(work)? {
                    BoxStep::Yield => return Ok(Mov9CursorStatus::Yield),
                    BoxStep::Box(raw) => {
                        self.inspect_box()?;
                        if raw.header().kind == *b"wave" && self.sound_v4.is_some() {
                            if self.wave_v4.is_some() {
                                return Err(Error::Invalid("encoded duplicate wave context"));
                            }
                            let nested = raw.children(
                                0,
                                Limits {
                                    max_boxes: self.limits.max_child_boxes,
                                    ..Limits::default()
                                },
                            )?;
                            if self.view.observes_contexts() {
                                let mut report = crate::Mov9WaveObservationV7::new(raw.clone());
                                if self
                                    .sound_v4
                                    .as_ref()
                                    .is_some_and(|s| s.compression_id() != -2)
                                {
                                    report.fault(
                                        Error::Unsupported(
                                            "encoded sound-v1 frame-table compression ID",
                                        ),
                                        self.entry.raw().clone(),
                                        0,
                                    )?;
                                }
                                self.wave_source_v7 = Some(report);
                            }
                            self.wave_v4 = Some(WaveV4 {
                                raw,
                                children: Some(nested),
                                format: None,
                                marker: None,
                                terminator: None,
                            });
                        } else if raw.header().kind == *b"wave"
                            && self.view == DescriptionView::RangePlayback
                            && self.kind == EncodedEntryKindV1::Aac
                        {
                            return Err(Error::Unsupported(
                                "encoded AAC wave requires QuickTime AudioV1",
                            ));
                        } else if raw.header().kind == self.config_kind {
                            if self.sound_v4.is_some() {
                                return Err(Error::Unsupported(
                                    "encoded sound-v1 config outside wave",
                                ));
                            }
                            self.accept_configuration(raw)?;
                        }
                    }
                    BoxStep::End => {
                        self.children = None;
                        let cfg = self.configuration.as_ref().ok_or(Error::Unsupported(
                            "encoded configuration path or absence not established",
                        ))?;
                        if self.kind == EncodedEntryKindV1::Aac && self.esds.is_none() {
                            let start = usize::from(cfg.header().header_size);
                            let bytes = cfg.bytes().slice(start..cfg.bytes().len())?;
                            self.esds = Some(EsdsWalk::new(bytes, self.limits.max_descriptors)?);
                        }
                    }
                }
                continue;
            }
            if let Some(esds) = &mut self.esds {
                if !esds.poll(work)? {
                    return Ok(Mov9CursorStatus::Yield);
                }
            }
            if !work.try_charge(1) {
                return Ok(Mov9CursorStatus::Yield);
            }
            let cfg = self
                .configuration
                .as_ref()
                .ok_or(Error::Invalid("encoded config missing at publication"))?;
            let extradata = match self.kind {
                EncodedEntryKindV1::Aac => self
                    .esds
                    .as_ref()
                    .and_then(|x| x.dsi.as_ref().map(|(_, bytes)| bytes.clone()))
                    .ok_or(Error::Unsupported(
                        "encoded AAC decoder-specific config absent",
                    ))?,
                EncodedEntryKindV1::Avc | EncodedEntryKindV1::Hevc => cfg
                    .bytes()
                    .slice(usize::from(cfg.header().header_size)..cfg.bytes().len())?,
                EncodedEntryKindV1::Ac3 | EncodedEntryKindV1::Eac3 => {
                    cfg.bytes().slice(cfg.bytes().len()..cfg.bytes().len())?
                }
            };
            if self.kind == EncodedEntryKindV1::Aac {
                let first = *extradata
                    .as_ref()
                    .first()
                    .ok_or(Error::Unsupported("encoded empty AAC configuration"))?;
                if !matches!(first >> 3, 2 | 5 | 29) {
                    return Err(Error::Unsupported(
                        "encoded unselected MPEG-4 Audio object type",
                    ));
                }
            }
            let (mut native, encoder, receiver_source_v4) = metadata(
                self.header,
                &self.entry,
                self.kind,
                extradata,
                self.view,
                self.major,
            )?;
            let receiver_source_v4 = receiver_source_v4.map(|mut source| {
                source.sound = self.sound_v4.clone();
                source.wave = self.wave_v4.as_ref().map(|w| w.raw.clone());
                source
            });
            if self.view != DescriptionView::Regular && self.major == Some(*b"qt  ") {
                let vendor = fields::fourcc(self.entry.raw().payload(), 12)?;
                if self.sound_v4.is_some() && vendor != [0; 4] {
                    return Err(Error::Unsupported(
                        "encoded QuickTime nonzero vendor profile",
                    ));
                }
                native.vendor_id = Some(vendor);
            }
            self.result = Some(Mov9EncodedStreamV1 {
                native,
                entry: self.entry.clone(),
                configuration: cfg.clone(),
                encoder,
                kind: self.kind,
                receiver_source_v4,
                wave_source_v7: self.wave_source_v7.take(),
                raw_headers: [
                    self.header.raw_tkhd.clone(),
                    self.header.raw_mdhd.clone(),
                    self.header.raw_hdlr.clone(),
                ],
            });
            return Ok(Mov9CursorStatus::Ready);
        }
    }
    pub fn take(
        &mut self,
        input: &mut InputCursor,
        cancel: &AtomicBool,
    ) -> Result<Mov9EncodedStreamV1> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        let answer = (|| {
            self.check(input, &mut WorkBudget::new(0), cancel)?;
            self.result
                .take()
                .ok_or(Error::Invalid("encoded catalogue not ready"))
        })();
        match answer {
            Ok(value) => {
                self.finished = true;
                Ok(value)
            }
            Err(error) => {
                self.failed = Some(error);
                self.result = None;
                Err(error)
            }
        }
    }
}

type MetadataParts = (
    Mov9StreamView,
    Option<Bytes>,
    Option<crate::Mov9EncodedSourceV4>,
);
fn metadata(
    header: &TrackHeader,
    entry: &SampleDescription,
    kind: EncodedEntryKindV1,
    extradata: Bytes,
    view: DescriptionView,
    major: Option<[u8; 4]>,
) -> Result<MetadataParts> {
    let parsed = TrackHeader::parse(&header.raw_tkhd, &header.raw_mdhd, &header.raw_hdlr)?;
    if (
        parsed.track_id,
        parsed.timescale,
        parsed.duration,
        parsed.handler,
        parsed.enabled,
    ) != (
        header.track_id,
        header.timescale,
        header.duration,
        header.handler,
        header.enabled,
    ) {
        return Err(Error::Invalid(
            "encoded public header differs from raw source",
        ));
    }
    let clock = Mov9MediaClock::from_source(header.time_base()?)?;
    let source_v4 = (view != DescriptionView::Regular)
        .then(|| -> Result<_> {
            Ok(crate::Mov9EncodedSourceV4 {
                language: crate::native_encoded_metadata_v4::language(
                    header,
                    view == DescriptionView::RangePlayback && major == Some(*b"qt  "),
                )?,
                handler: crate::native_encoded_metadata_v4::handler(header, major)?,
                sound: None,
                wave: None,
            })
        })
        .transpose()?;
    let b = header.raw_mdhd.payload();
    let at = match fields::full(b)?.0 {
        0 => 20,
        1 => 32,
        _ => return Err(Error::Unsupported("encoded media header version")),
    };
    let packed = fields::u16_at(b, at)?;
    let mut language = [0; 3];
    for (i, v) in language.iter_mut().enumerate() {
        let c = ((packed >> (10 - 5 * i)) & 31) as u8;
        if source_v4.is_none() && (!(1..=26).contains(&c) || packed & 0x8000 != 0) {
            return Err(Error::Unsupported("encoded native language profile"));
        }
        *v = c + 0x60;
    }
    if let Some(source) = &source_v4 {
        language = source.language.native;
    }
    let h = header.raw_hdlr.payload();
    if source_v4.is_none() && (fields::u32_at(h, 4)? != 0 || fields::slice(h, 12, 12)? != [0; 12]) {
        return Err(Error::Unsupported("encoded legacy handler profile"));
    }
    let tail = h.get(24..).ok_or(Error::Truncated)?;
    if tail.len() > 256 {
        return Err(Error::Limit {
            requested: tail.len(),
            maximum: 256,
        });
    }
    let handler_name = if let Some(source) = &source_v4 {
        if let Some(name) = &source.handler.native {
            name.clone()
        } else {
            header
                .raw_hdlr
                .bytes()
                .slice(header.raw_hdlr.bytes().len()..header.raw_hdlr.bytes().len())?
        }
    } else {
        let n = tail
            .iter()
            .position(|b| *b == 0)
            .ok_or(Error::Unsupported("encoded unterminated handler"))?;
        if !tail[..n].iter().all(u8::is_ascii) {
            return Err(Error::Unsupported("encoded handler text profile"));
        }
        let start = usize::from(header.raw_hdlr.header().header_size) + 24;
        header.raw_hdlr.bytes().slice(start..start + n)?
    };
    let video = matches!(kind, EncodedEntryKindV1::Avc | EncodedEntryKindV1::Hevc);
    let encoder = if video {
        let body = entry.raw().payload();
        let n = usize::from(*body.get(42).ok_or(Error::Truncated)?);
        if n > 31 {
            return Err(Error::Unsupported("encoded compressor-name width"));
        }
        if n == 0 {
            None
        } else {
            let at = usize::from(entry.raw().header().header_size) + 43;
            let name = entry.raw().bytes().slice(at..at + n)?;
            if !name.as_ref().iter().all(|b| b.is_ascii() && *b != 0) {
                return Err(Error::Unsupported("encoded compressor-name encoding"));
            }
            Some(name)
        }
    } else {
        None
    };
    let codec = match kind {
        EncodedEntryKindV1::Avc => "h264",
        EncodedEntryKindV1::Hevc => "hevc",
        EncodedEntryKindV1::Aac => "aac",
        EncodedEntryKindV1::Ac3 => "ac3",
        EncodedEntryKindV1::Eac3 => "eac3",
    };
    Ok((
        Mov9StreamView {
            track_id: header.track_id,
            codec,
            media_type: u8::from(!video),
            time_base: clock.native_base,
            clock,
            extradata,
            language,
            handler_name,
            vendor_id: None,
        },
        encoder,
        source_v4,
    ))
}

#[cfg(test)]
mod aac_entry_tests {
    use super::*;

    const ASC: [u8; 2] = [0x11, 0x90];
    const START: u64 = 1024;

    fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(body);
        bytes
    }

    fn esds(config: &[u8]) -> Vec<u8> {
        let length = u8::try_from(config.len()).unwrap();
        let mut body = vec![0; 4];
        body.extend([3, 20 + length, 0, 1, 0, 4, 15 + length, 0x40, 0x15]);
        body.extend([0; 11]);
        body.extend([5, length]);
        body.extend_from_slice(config);
        atom(b"esds", &body)
    }

    fn wave_config(format: &[u8; 4], trailing_child: bool) -> Vec<u8> {
        let mut children = atom(b"frma", format);
        children.extend(atom(b"mp4a", &[0; 4]));
        children.extend(esds(&ASC));
        children.extend(atom(&[0; 4], &[]));
        if trailing_child {
            children.extend(atom(b"free", &[]));
        }
        children
    }

    fn description(children: &[u8], wave: bool) -> (SampleDescription, Vec<u8>) {
        let mut body = vec![0; if wave { 44 } else { 28 }];
        body[7] = 1;
        if wave {
            body[9] = 1;
            body[20..22].copy_from_slice(&0xfffeu16.to_be_bytes());
            body.extend(atom(b"wave", children));
        } else {
            body.extend_from_slice(children);
        }
        let bytes = atom(b"mp4a", &body);
        let pool = Budget::new(4096).unwrap();
        let raw = BoxView::new(Bytes::copy_from_slice(&pool, &bytes).unwrap(), START).unwrap();
        (SampleDescription::parse(raw, 1).unwrap(), bytes)
    }

    #[test]
    fn extracts_aac_from_iso_and_quicktime_entries_without_a_sample_table() {
        for wave in [false, true] {
            let children = if wave {
                wave_config(b"mp4a", false)
            } else {
                esds(&ASC)
            };
            let (entry, bytes) = description(&children, wave);
            let mut job = Mp4AacConfigJob::new(&entry, Mov9EncodedLimitsV1::default()).unwrap();
            let cancel = AtomicBool::new(false);
            assert!(
                job.poll(&mut WorkBudget::new(0), &cancel)
                    .unwrap()
                    .is_none()
            );
            let config = (0..200)
                .find_map(|_| job.poll(&mut WorkBudget::new(16), &cancel).unwrap())
                .expect("AAC configuration");
            let trailing = if wave { 8 } else { 0 };
            assert_eq!(
                config.offset,
                START + (bytes.len() - ASC.len() - trailing) as u64
            );
            assert_eq!(config.bytes.as_ref(), ASC);
        }
    }

    #[test]
    fn refuses_duplicate_descriptors_and_enforces_the_descriptor_work_limit() {
        let mut duplicate = esds(&ASC);
        duplicate.extend(esds(&ASC));
        let (entry, _) = description(&duplicate, false);
        let mut job = Mp4AacConfigJob::new(&entry, Mov9EncodedLimitsV1::default()).unwrap();
        let cancel = AtomicBool::new(false);
        let duplicate_error = Error::Invalid("duplicate AAC elementary stream descriptor");
        for _ in 0..2 {
            assert_eq!(
                job.poll(&mut WorkBudget::new(1024), &cancel).unwrap_err(),
                duplicate_error
            );
        }

        let (entry, _) = description(&esds(&ASC), false);
        let mut job = Mp4AacConfigJob::new(
            &entry,
            Mov9EncodedLimitsV1 {
                max_descriptors: 2,
                ..Mov9EncodedLimitsV1::default()
            },
        )
        .unwrap();
        assert!(matches!(
            job.poll(&mut WorkBudget::new(1024), &cancel),
            Err(Error::Limit {
                requested: 3,
                maximum: 2
            })
        ));
    }

    #[test]
    fn refuses_inconsistent_wave_and_empty_decoder_configuration() {
        let cancel = AtomicBool::new(false);
        for (children, wave, error) in [
            (
                wave_config(b"ac-3", false),
                true,
                Error::Invalid("AAC wave format"),
            ),
            (
                wave_config(b"mp4a", true),
                true,
                Error::Invalid("AAC wave child after terminator"),
            ),
            (
                esds(&[]),
                false,
                Error::Invalid("empty AAC decoder-specific bytes"),
            ),
        ] {
            let (entry, _) = description(&children, wave);
            let mut job = Mp4AacConfigJob::new(&entry, Mov9EncodedLimitsV1::default()).unwrap();
            assert_eq!(
                job.poll(&mut WorkBudget::new(1024), &cancel).unwrap_err(),
                error
            );
        }
    }

    #[test]
    fn counts_outer_and_wave_children_against_one_box_limit() {
        let (entry, _) = description(&wave_config(b"mp4a", false), true);
        let limits = Mov9EncodedLimitsV1 {
            max_child_boxes: 4,
            ..Mov9EncodedLimitsV1::default()
        };
        let mut job = Mp4AacConfigJob::new(&entry, limits).unwrap();
        assert!(matches!(
            job.poll(&mut WorkBudget::new(1024), &AtomicBool::new(false)),
            Err(Error::Limit {
                requested: 5,
                maximum: 4
            })
        ));
    }
}
