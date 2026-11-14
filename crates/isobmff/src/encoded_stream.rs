//! Explicit regular encoded catalogue view. Decoder configuration is retained
//! alongside the catalogue projection.
use crate::{
    BoxChildren, BoxStep, BoxView, CursorStatus, DtsCoreDescription, EntryLayout, Limits,
    MediaClock, SampleDescription, SampleTable, StreamView, TrackHeader,
    dts_description::{
        check_dts_sample_entry, check_mp4a_dts, dts_entry_children, dts_object_type,
    },
    fields,
};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, InputCursor, InputToken, Result, SourceId, WorkBudget};
use triergon_framing::{OPUS_RATE, OpusHead};

#[derive(Clone, Copy, Debug)]
pub struct EncodedStreamProfile;
/// Limits for inspecting an encoded sample description and its configuration.
#[derive(Clone, Copy, Debug)]
pub struct ConfigurationLimits {
    pub max_config_bytes: usize,
    pub max_child_boxes: u32,
    pub max_descriptors: u32,
}
impl Default for ConfigurationLimits {
    fn default() -> Self {
        Self {
            max_config_bytes: 1 << 20,
            max_child_boxes: 4096,
            max_descriptors: 4096,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodedEntryKind {
    Avc,
    Hevc,
    Mpeg4Visual,
    Av1,
    Vp9,
    Aac,
    Ac3,
    Eac3,
    Dts,
    Opus,
}

impl EncodedEntryKind {
    pub(crate) const fn is_video(self) -> bool {
        matches!(
            self,
            Self::Avc | Self::Hevc | Self::Mpeg4Visual | Self::Av1 | Self::Vp9
        )
    }

    pub(crate) fn require_regular_codec(self) -> Result<()> {
        if matches!(self, Self::Av1 | Self::Vp9 | Self::Dts | Self::Opus) {
            return Err(Error::Unsupported("encoded codec requires range playback"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub struct EncodedStream {
    demux: StreamView,
    entry: SampleDescription,
    configuration: BoxView,
    pub(crate) dts: Option<DtsCoreDescription>,
    encoder: Option<Bytes>,
    kind: EncodedEntryKind,
    raw_headers: [BoxView; 3],
    receiver_source: Option<crate::EncodedSource>,
    wave_source: Option<crate::WaveObservation>,
}
impl EncodedStream {
    pub fn wave_source(&self) -> Option<&crate::WaveObservation> {
        self.wave_source.as_ref()
    }

    pub fn demux(&self) -> &StreamView {
        &self.demux
    }
    pub fn source_description(&self) -> &SampleDescription {
        &self.entry
    }
    /// The source configuration box, or the original sample entry for the
    /// selected QuickTime DTS description with in-band configuration.
    pub fn configuration_box(&self) -> &BoxView {
        &self.configuration
    }
    pub fn encoder_name(&self) -> Option<&Bytes> {
        self.encoder.as_ref()
    }
    pub fn kind(&self) -> EncodedEntryKind {
        self.kind
    }
    pub fn receiver_source(&self) -> Option<&crate::EncodedSource> {
        self.receiver_source.as_ref()
    }
    pub fn raw_headers(&self) -> &[BoxView; 3] {
        &self.raw_headers
    }
}

// A bounded storage-relative TLV walk. Each header/prefix byte is inspected once;
// opaque bodies are only range checked and borrowed, never parsed on a countdown.
#[derive(Debug, Clone, Copy)]
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

#[derive(Debug, Clone, Copy)]
enum EsdsProfile {
    Aac,
    AacDescription,
    AudioDescription,
    RangeAudio,
    Visual,
    ObjectType,
    /// Any object type of any stream type: an `mp4s` system stream entry.
    SystemStream,
}

#[derive(Debug)]
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
    object_type: Option<u8>,
    profile: EsdsProfile,
    completed: bool,
}
impl EsdsWalk {
    fn new(raw: Bytes, limit: u32) -> Result<Self> {
        Self::with_profile(raw, limit, EsdsProfile::Aac)
    }

    fn with_profile(raw: Bytes, limit: u32, profile: EsdsProfile) -> Result<Self> {
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
            object_type: None,
            profile,
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
                    // ISO/IEC 14496-14 §6.7.2 gives esds one ES descriptor.
                    if self.depth == 0
                        && (tag != 3 || self.rows != 1)
                        && matches!(
                            self.profile,
                            EsdsProfile::AacDescription | EsdsProfile::AudioDescription
                        )
                    {
                        return Err(Error::Invalid("AAC elementary stream descriptor root"));
                    }
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
                        if index == 0 {
                            let allowed = match self.profile {
                                EsdsProfile::Aac | EsdsProfile::AacDescription => b == 0x40,
                                EsdsProfile::RangeAudio => b == 0x40 || dts_object_type(b)?,
                                EsdsProfile::Visual => b == 0x20,
                                EsdsProfile::AudioDescription
                                | EsdsProfile::ObjectType
                                | EsdsProfile::SystemStream => true,
                            };
                            if !allowed {
                                return Err(Error::Unsupported(
                                    if matches!(self.profile, EsdsProfile::Visual) {
                                        "MPEG-4 Visual object type"
                                    } else {
                                        "encoded mp4a object-type is not selected AAC"
                                    },
                                ));
                            }
                            self.object_type = Some(b);
                        }
                        let stream_type = if matches!(self.profile, EsdsProfile::Visual) {
                            4
                        } else {
                            5
                        };
                        if index == 1
                            && !matches!(self.profile, EsdsProfile::SystemStream)
                            && (b >> 2 != stream_type || b & 1 == 0)
                        {
                            if matches!(self.profile, EsdsProfile::Visual) {
                                return Err(Error::Unsupported(
                                    "MPEG-4 Visual decoder stream type",
                                ));
                            }
                            return Err(Error::Unsupported(if self.object_type == Some(0x40) {
                                "encoded AAC decoder stream-type"
                            } else {
                                "encoded mp4a decoder stream-type"
                            }));
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
                        next >= 3 && skip == 0 && es_flags & 0x60 == 0
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

/// AudioSpecificConfig from an MPEG-4 Audio sample entry.
/// Validate the bytes to identify the audio object type before decoding.
#[derive(Debug)]
pub struct AacConfiguration {
    /// Absolute source offset of the first configuration byte.
    pub offset: u64,
    /// Shared view into the sample description, without copying the payload.
    pub bytes: Bytes,
}

/// Progress when reading a sample description already in memory.
#[derive(Debug)]
pub enum AacConfigurationStep {
    Yield,
    Ready(AacConfiguration),
    End,
}

pub(crate) enum AudioDescriptionStep {
    Yield,
    Aac(AacConfiguration),
    Other,
}

pub(crate) struct AudioDescription(EsdsDescriptionWalk);

impl AudioDescription {
    pub(crate) fn new(entry: &SampleDescription, limits: ConfigurationLimits) -> Result<Self> {
        EsdsDescriptionWalk::with_profile(entry, limits, EsdsProfile::AudioDescription).map(Self)
    }

    pub(crate) fn poll(&mut self, work: &mut WorkBudget) -> Result<AudioDescriptionStep> {
        if !self.0.scan_configuration(work)? {
            return Ok(AudioDescriptionStep::Yield);
        }
        let (_, walker) = self
            .0
            .esds
            .as_ref()
            .ok_or(Error::Invalid("missing elementary stream descriptor"))?;
        match walker.object_type {
            Some(0x40) => self
                .0
                .take_aac_configuration()
                .map(AudioDescriptionStep::Aac),
            Some(1..) => Ok(AudioDescriptionStep::Other),
            _ => Err(Error::Invalid("missing or forbidden decoder object type")),
        }
    }
}

/// Extracts AudioSpecificConfig from an `mp4a` description independently
/// of its sample table. Supports ISO sound entries and the selected
/// QuickTime sound layout with a `wave` container.
#[derive(Debug)]
pub struct AacConfigurationJob {
    walk: Option<EsdsDescriptionWalk>,
    failed: Option<Error>,
}

impl AacConfigurationJob {
    pub fn new(entry: &SampleDescription, limits: ConfigurationLimits) -> Result<Self> {
        if entry.format != entry.raw().header().kind {
            return Err(Error::Invalid(
                "sample entry format differs from raw source",
            ));
        }
        Ok(Self {
            walk: Some(EsdsDescriptionWalk::with_profile(
                entry,
                limits,
                EsdsProfile::AacDescription,
            )?),
            failed: None,
        })
    }

    /// Returns the configuration once, yielding when the work budget runs out.
    /// Later polls return `End`. Errors, including cancellation, are sticky.
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AacConfigurationStep> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let Some(walk) = self.walk.as_mut() else {
            return Ok(AacConfigurationStep::End);
        };
        let result = Self::advance(walk, work, cancel);
        match result {
            Ok(AacConfigurationStep::Ready(_)) => self.walk = None,
            Err(error) => {
                self.failed = Some(error);
                self.walk = None;
            }
            _ => {}
        }
        result
    }

    fn advance(
        walk: &mut EsdsDescriptionWalk,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AacConfigurationStep> {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if !walk.scan_configuration(work)? {
            return Ok(AacConfigurationStep::Yield);
        }
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        walk.take_aac_configuration()
            .map(AacConfigurationStep::Ready)
    }
}

fn quicktime_aac_extensions(entry: &SampleDescription) -> Result<usize> {
    let raw = entry.raw();
    let body = raw.payload();
    fields::slice(body, 0, 64)?;
    // The version-2 sizeOfStructOnly includes the sample entry's box header.
    let extensions = usize::try_from(fields::u32_at(body, 28)?).map_err(|_| Error::Overflow)?;
    let offset = extensions
        .checked_sub(usize::from(raw.header().header_size))
        .filter(|&offset| offset >= 64)
        .ok_or(Error::Invalid("QuickTime sound extension offset"))?;
    fields::slice(body, 0, offset)?;
    let sample_rate = f64::from_bits(fields::u64_at(body, 32)?);
    if fields::u32_at(body, 16)? != 0x0003_0010
        || fields::u32_at(body, 20)? != 0xfffe_0000
        || fields::u32_at(body, 24)? != 65_536
        || fields::u32_at(body, 44)? != 0x7f00_0000
        || !sample_rate.is_finite()
        || sample_rate <= 0.0
        || fields::u32_at(body, 40)? == 0
    {
        return Err(Error::Invalid("QuickTime version-2 sound description"));
    }
    Ok(offset)
}

#[derive(Debug)]
struct EsdsDescriptionWalk {
    children: BoxChildren,
    wave: Option<BoxChildren>,
    seen_wave: bool,
    quicktime_extended_sound: bool,
    wave_fields: AacWaveFields,
    scanned: bool,
    limits: ConfigurationLimits,
    inspected_boxes: u32,
    profile: EsdsProfile,
    esds: Option<(u64, EsdsWalk)>,
}

impl EsdsDescriptionWalk {
    fn take_aac_configuration(&mut self) -> Result<AacConfiguration> {
        let (base, walker) = self
            .esds
            .as_mut()
            .ok_or(Error::Invalid("missing AAC elementary stream descriptor"))?;
        let (relative, bytes) = walker
            .dsi
            .take()
            .ok_or(Error::Invalid("missing AAC decoder-specific bytes"))?;
        if bytes.is_empty() {
            return Err(Error::Invalid("empty AAC decoder-specific bytes"));
        }
        let offset = base.checked_add(relative as u64).ok_or(Error::Overflow)?;
        Ok(AacConfiguration { offset, bytes })
    }

    fn with_profile(
        entry: &SampleDescription,
        limits: ConfigurationLimits,
        profile: EsdsProfile,
    ) -> Result<Self> {
        let child_limits = Limits {
            max_boxes: limits.max_child_boxes,
            ..Limits::default()
        };
        let (children, quicktime_extended_sound) = match &entry.format {
            // MPEG-4 system streams (DVD subpictures) have a plain sample entry.
            b"mp4s" => (entry.raw().children(8, child_limits)?, false),
            b"mp4v" => (entry.extensions(EntryLayout::Visual, child_limits)?, false),
            b"mp4a" => match fields::u16_at(entry.raw().payload(), 8)? {
                1 => {
                    crate::SoundV1Source::parse_contexts(entry)?;
                    (entry.raw().children(44, child_limits)?, false)
                }
                2 => (
                    entry
                        .raw()
                        .children(quicktime_aac_extensions(entry)?, child_limits)?,
                    true,
                ),
                _ => (entry.extensions(EntryLayout::AudioV0, child_limits)?, false),
            },
            _ => {
                return Err(Error::Invalid("sample entry does not use an ES descriptor"));
            }
        };
        Ok(Self {
            children,
            wave: None,
            seen_wave: false,
            quicktime_extended_sound,
            wave_fields: AacWaveFields::default(),
            scanned: false,
            limits,
            inspected_boxes: 0,
            profile,
            esds: None,
        })
    }

    fn scan_configuration(&mut self, work: &mut WorkBudget) -> Result<bool> {
        while !self.scanned {
            let nested = self.wave.is_some();
            let children = self.wave.as_mut().unwrap_or(&mut self.children);
            match children.poll(work)? {
                BoxStep::Yield => return Ok(false),
                BoxStep::End if nested => {
                    if !self.wave_fields.complete(!self.quicktime_extended_sound)
                        || self.esds.is_none()
                    {
                        return Err(Error::Unsupported("incomplete AAC wave context"));
                    }
                    self.wave = None;
                }
                BoxStep::End => {
                    if self.quicktime_extended_sound && !self.seen_wave {
                        return Err(Error::Unsupported(
                            "QuickTime AAC requires wave configuration",
                        ));
                    }
                    self.scanned = true;
                }
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
        let (_, walker) = self
            .esds
            .as_mut()
            .ok_or(Error::Invalid("missing AAC elementary stream descriptor"))?;
        walker.poll(work)
    }

    fn accept_descriptor(&mut self, raw: BoxView) -> Result<()> {
        if self.esds.is_some() {
            return Err(Error::Invalid("duplicate AAC elementary stream descriptor"));
        }
        check_configuration_size(&raw, self.limits)?;
        let header = raw.header();
        let offset = header
            .start
            .checked_add(u64::from(header.header_size))
            .ok_or(Error::Overflow)?;
        let bytes = raw
            .bytes()
            .slice(usize::from(header.header_size)..raw.bytes().len())?;
        self.esds = Some((
            offset,
            EsdsWalk::with_profile(bytes, self.limits.max_descriptors, self.profile)?,
        ));
        Ok(())
    }
}

/// Check a selected sample description for DTS core playback. Direct DTS
/// entries and DTS object types in `mp4a` are checked within the caller's
/// box, configuration-byte and descriptor limits. Other formats and valid
/// non-DTS object types return `None`. A positive result describes container
/// signalling only. The caller must verify each packet's usable core.
pub fn check_dts_core_description(
    description: &SampleDescription,
    limits: ConfigurationLimits,
) -> Result<Option<DtsCoreDescription>> {
    if description.format != description.raw().header().kind {
        return Err(Error::Invalid(
            "sample entry format differs from raw source",
        ));
    }
    // Each box/descriptor consumes a bounded number of work units. Opaque
    // bodies are skipped without iterating their bytes.
    let mut work = WorkBudget::new(u64::MAX);
    match &description.format {
        b"mp4a" => {
            let mut job =
                EsdsDescriptionWalk::with_profile(description, limits, EsdsProfile::ObjectType)?;
            if !job.scan_configuration(&mut work)? {
                return Err(Error::Overflow);
            }
            let object_type = job
                .esds
                .and_then(|(_, walk)| walk.object_type)
                .ok_or(Error::Invalid("missing mp4a decoder object type"))?;
            if object_type == 0 {
                return Err(Error::Invalid("forbidden mp4a decoder object type"));
            }
            if !dts_object_type(object_type)? {
                return Ok(None);
            }
            if job.seen_wave {
                return Err(Error::Unsupported("DTS mp4a wave configuration"));
            }
            Ok(Some(check_mp4a_dts(description)?))
        }
        b"dtsc" | b"dtsh" | b"dtsl" => {
            let mut children = dts_entry_children(
                description,
                Limits {
                    max_boxes: limits.max_child_boxes,
                    ..Limits::default()
                },
            )?;
            let mut config = None;
            loop {
                match children.poll(&mut work)? {
                    BoxStep::Box(raw) if raw.header().kind == *b"ddts" => {
                        check_configuration_size(&raw, limits)?;
                        if config.replace(raw).is_some() {
                            return Err(Error::Unsupported("encoded duplicate configuration box"));
                        }
                    }
                    BoxStep::Box(_) => {}
                    BoxStep::End => break,
                    BoxStep::Yield => return Err(Error::Overflow),
                }
            }
            Ok(Some(check_dts_sample_entry(description, config.as_ref())?))
        }
        b"dtse" | b"dtsx" => Err(Error::Unsupported("DTS entry without selected core")),
        _ => Ok(None),
    }
}

fn check_configuration_size(raw: &BoxView, limits: ConfigurationLimits) -> Result<()> {
    if raw.payload().len() > limits.max_config_bytes {
        return Err(Error::Limit {
            requested: raw.payload().len(),
            maximum: limits.max_config_bytes,
        });
    }
    Ok(())
}

#[derive(Debug, Default)]
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

    fn complete(&self, require_format_and_marker: bool) -> bool {
        self.terminator && (!require_format_and_marker || (self.format && self.marker))
    }
}

// One nested, charged-storage-relative wave context; no recursive parser or queue.
#[derive(Debug)]
struct Wave {
    raw: BoxView,
    children: Option<BoxChildren>,
    format: Option<BoxView>,
    marker: Option<BoxView>,
    terminator: Option<BoxView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DescriptionView {
    Regular,
    Metadata,
    Contexts,
    RangePlayback,
    DemuxCatalogue,
}
impl DescriptionView {
    fn observes_contexts(self) -> bool {
        self == Self::Contexts
    }

    fn extended_configurations(self) -> bool {
        matches!(self, Self::RangePlayback | Self::DemuxCatalogue)
    }
}

/// Metadata-only job over the original charged source storage. Source/request
/// generations, work, cancellation and transport checks use the shared core.
#[derive(Debug)]
pub struct EncodedStreamJob<'a> {
    header: &'a TrackHeader,
    entry: SampleDescription,
    children: Option<BoxChildren>,
    configuration: Option<BoxView>,
    esds: Option<EsdsWalk>,
    kind: EncodedEntryKind,
    config_kind: [u8; 4],
    limits: ConfigurationLimits,
    pool: Budget,
    source: SourceId,
    token: InputToken,
    result: Option<EncodedStream>,
    failed: Option<Error>,
    finished: bool,
    view: DescriptionView,
    major: Option<[u8; 4]>,
    sound_source: Option<crate::SoundV1Source>,
    requires_wave: bool,
    wave_fields: Option<Wave>,
    inspected_boxes: u32,
    wave_source: Option<crate::WaveObservation>,
    pending_esds: Option<(BoxView, EsdsWalk)>,
    descriptors_left: u32,
}
impl<'a> EncodedStreamJob<'a> {
    pub fn new(
        header: &'a TrackHeader,
        table: &SampleTable,
        input: &InputCursor,
        major: Option<[u8; 4]>,
        limits: ConfigurationLimits,
        _profile: EncodedStreamProfile,
    ) -> Result<Self> {
        Self::new_description_inner(
            header,
            table,
            input,
            crate::EncodedSelection {
                major_brand: major,
                limits,
                description: 1,
            },
            DescriptionView::Regular,
        )
    }
    pub fn new_description(
        header: &'a TrackHeader,
        table: &SampleTable,
        input: &InputCursor,
        major: Option<[u8; 4]>,
        limits: ConfigurationLimits,
        description: u32,
        _profile: crate::EncodedConfigProfile,
    ) -> Result<Self> {
        Self::new_description_inner(
            header,
            table,
            input,
            crate::EncodedSelection {
                major_brand: major,
                limits,
                description,
            },
            DescriptionView::Regular,
        )
    }
    /// Admits the independently sourced QuickTime VBR AudioV1/wave layout
    /// and separate legacy metadata presence rules. Existing profiles stay exact.
    pub fn new_description_with_metadata(
        header: &'a TrackHeader,
        table: &SampleTable,
        input: &InputCursor,
        selection: crate::EncodedSelection,
        _profile: crate::EncodedMetadataProfile,
    ) -> Result<Self> {
        Self::new_description_inner(header, table, input, selection, DescriptionView::Metadata)
    }
    /// Named receiver context: source wave words/ordering and the old strict
    /// profile's first fault remain owned; no valid-source result is fabricated.
    pub fn new_description_with_contexts(
        header: &'a TrackHeader,
        table: &SampleTable,
        input: &InputCursor,
        selection: crate::EncodedSelection,
        _profile: crate::EncodedContextProfile,
    ) -> Result<Self> {
        Self::new_description_inner(header, table, input, selection, DescriptionView::Contexts)
    }
    /// Range playback requires unambiguous audio configuration and admits
    /// AV1, VP9 and selected DTS core descriptions in addition to the reference codecs.
    pub fn new_range_playback(
        header: &'a TrackHeader,
        table: &SampleTable,
        input: &InputCursor,
        selection: crate::EncodedSelection,
    ) -> Result<Self> {
        Self::new_description_inner(
            header,
            table,
            input,
            selection,
            DescriptionView::RangePlayback,
        )
    }

    /// Read an encoded stream's demux metadata and configuration. Also accepts
    /// a description from an empty fragment initialization sample table.
    pub fn new_demux_catalogue(
        header: &'a TrackHeader,
        table: &SampleTable,
        input: &InputCursor,
        selection: crate::EncodedSelection,
    ) -> Result<Self> {
        Self::new_description_inner(
            header,
            table,
            input,
            selection,
            DescriptionView::DemuxCatalogue,
        )
    }

    fn new_description_inner(
        header: &'a TrackHeader,
        table: &SampleTable,
        input: &InputCursor,
        selection: crate::EncodedSelection,
        view: DescriptionView,
    ) -> Result<Self> {
        let crate::EncodedSelection {
            major_brand,
            limits,
            description,
        } = selection;
        let metadata_receiver = view != DescriptionView::Regular;
        let catalogue = view == DescriptionView::DemuxCatalogue;
        // QuickTime movies may precede the definition of a file type atom.
        let major = major_brand.or_else(|| catalogue.then_some(*b"qt  "));
        if !table.raw().bytes().belongs_to(input.budget())
            || [&header.raw_tkhd, &header.raw_mdhd, &header.raw_hdlr]
                .iter()
                .any(|x| !x.bytes().belongs_to(input.budget()))
        {
            return Err(Error::ForeignPool);
        }
        if !catalogue
            && !matches!(major,Some(x) if x==*b"isom"||x==*b"iso2"||x==*b"mp41"||x==*b"mp42"||(metadata_receiver&&x==*b"qt  "))
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
        if table.sample_count() == 0 && !catalogue {
            return Err(Error::Unsupported(
                "encoded nonempty regular source required",
            ));
        }
        if entry.data_reference_index != 1 {
            return Err(Error::Unsupported("encoded external data reference"));
        }
        let (kind, config_kind, layout) = match (&header.handler, &entry.format) {
            (b"vide", b"avc1" | b"avc3") => (EncodedEntryKind::Avc, *b"avcC", EntryLayout::Visual),
            (b"vide", b"hvc1" | b"hev1") => (EncodedEntryKind::Hevc, *b"hvcC", EntryLayout::Visual),
            (b"vide", b"mp4v") if catalogue => {
                (EncodedEntryKind::Mpeg4Visual, *b"esds", EntryLayout::Visual)
            }
            (b"vide", b"dvh1" | b"dvhe") if view.extended_configurations() => {
                (EncodedEntryKind::Hevc, *b"hvcC", EntryLayout::Visual)
            }
            (b"vide", b"av01" | b"dav1") if view.extended_configurations() => {
                (EncodedEntryKind::Av1, *b"av1C", EntryLayout::Visual)
            }
            (b"vide", b"vp09") if view.extended_configurations() => {
                (EncodedEntryKind::Vp9, *b"vpcC", EntryLayout::Visual)
            }
            (b"soun", b"mp4a") => (EncodedEntryKind::Aac, *b"esds", EntryLayout::AudioV0),
            (b"soun", b"ac-3") => (EncodedEntryKind::Ac3, *b"dac3", EntryLayout::AudioV0),
            (b"soun", b"ec-3") => (EncodedEntryKind::Eac3, *b"dec3", EntryLayout::AudioV0),
            (b"soun", b"Opus") if view.extended_configurations() => {
                (EncodedEntryKind::Opus, *b"dOps", EntryLayout::AudioV0)
            }
            (b"soun", b"dtsc" | b"dtsh" | b"dtsl") if view.extended_configurations() => {
                (EncodedEntryKind::Dts, *b"ddts", EntryLayout::AudioV0)
            }
            _ => return Err(Error::Unsupported("encoded catalogue namespace/handler")),
        };
        let child_limits = Limits {
            max_boxes: limits.max_child_boxes,
            ..Limits::default()
        };
        let quicktime = metadata_receiver && major == Some(*b"qt  ");
        let sound_version = if quicktime && kind == EncodedEntryKind::Aac {
            fields::u16_at(entry.raw().payload(), 8)?
        } else {
            0
        };
        let sound_source = if sound_version == 1 {
            Some(match view {
                DescriptionView::Contexts
                | DescriptionView::RangePlayback
                | DescriptionView::DemuxCatalogue => crate::SoundV1Source::parse_contexts(&entry)?,
                _ => crate::SoundV1Source::parse(&entry)?,
            })
        } else {
            None
        };
        let wave_offset = match sound_version {
            1 => Some(44),
            2 if view.extended_configurations() => Some(quicktime_aac_extensions(&entry)?),
            _ => None,
        };
        let quicktime_visual =
            view.extended_configurations() && matches!(layout, EntryLayout::Visual);
        if quicktime
            && !catalogue
            && wave_offset.is_none()
            && !quicktime_visual
            && kind != EncodedEntryKind::Dts
        {
            return Err(Error::Unsupported(
                if view == DescriptionView::RangePlayback {
                    "QuickTime range playback sample entry"
                } else {
                    "encoded V4 QuickTime AudioV1/mp4a profile only"
                },
            ));
        }
        let children = if kind == EncodedEntryKind::Dts {
            if major != Some(*b"qt  ") && fields::u16_at(entry.raw().payload(), 8)? != 0 {
                return Err(Error::Unsupported("DTS QuickTime sample entry brand"));
            }
            dts_entry_children(&entry, child_limits)?
        } else if let Some(offset) = wave_offset {
            entry.raw().children(offset, child_limits)?
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
            sound_source,
            requires_wave: wave_offset.is_some(),
            wave_fields: None,
            inspected_boxes: 0,
            wave_source: None,
            pending_esds: None,
            descriptors_left: limits.max_descriptors,
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
        check_configuration_size(&raw, self.limits)?;
        if self.view.observes_contexts()
            && self.wave_fields.is_some()
            && self.kind == EncodedEntryKind::Aac
        {
            if let Some(report) = &mut self.wave_source {
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
            self.pending_esds = Some((raw, EsdsWalk::new(bytes, self.descriptors_left)?));
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
    ) -> Result<CursorStatus> {
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
    ) -> Result<CursorStatus> {
        self.check(input, work, cancel)?;
        if self.result.is_some() {
            return Ok(CursorStatus::Ready);
        }
        if self.finished {
            return Ok(CursorStatus::End);
        }
        loop {
            self.check(input, work, cancel)?;
            if let Some((_, walker)) = &mut self.pending_esds {
                let outcome = walker.poll(work);
                match outcome {
                    Err(Error::Limit { requested, .. }) => {
                        return Err(Error::Limit {
                            requested: (self.limits.max_descriptors - self.descriptors_left)
                                as usize
                                + requested,
                            maximum: self.limits.max_descriptors as usize,
                        });
                    }
                    Err(error) => return Err(error),
                    Ok(false) => return Ok(CursorStatus::Yield),
                    Ok(true) => {}
                }
                let (raw, walker) = self
                    .pending_esds
                    .take()
                    .ok_or(Error::Invalid("V7 config pending invariant"))?;
                if walker.dsi.is_none() {
                    return Err(Error::Unsupported(
                        "encoded AAC decoder-specific config absent",
                    ));
                }
                self.descriptors_left = self
                    .descriptors_left
                    .checked_sub(walker.rows)
                    .ok_or(Error::Overflow)?;
                self.configuration = Some(raw);
                self.esds = Some(walker);
                continue;
            }
            if work.remaining() == 0 {
                return Ok(CursorStatus::Yield);
            }
            let wave_step = if let Some(children) =
                self.wave_fields.as_mut().and_then(|w| w.children.as_mut())
            {
                Some(children.poll(work)?)
            } else {
                None
            };
            if let Some(step) = wave_step {
                match step {
                    BoxStep::Yield => return Ok(CursorStatus::Yield),
                    BoxStep::Box(raw) => {
                        self.inspect_box()?;
                        let w = self
                            .wave_fields
                            .as_mut()
                            .ok_or(Error::Invalid("wave state absent"))?;
                        let ordinal = if let Some(report) = &mut self.wave_source {
                            report.child_count =
                                report.child_count.checked_add(1).ok_or(Error::Overflow)?;
                            report.child_count
                        } else {
                            0
                        };
                        if w.terminator.is_some() {
                            let error = Error::Invalid("encoded wave child after terminator");
                            if let Some(report) = &mut self.wave_source {
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
                                    if let Some(r) = &mut self.wave_source {
                                        r.fault(e, raw.clone(), ordinal)?;
                                    } else {
                                        return Err(e);
                                    }
                                }
                                if raw.payload() != b"mp4a" {
                                    let e = Error::Unsupported(
                                        "encoded wave format conflicts with entry",
                                    );
                                    if let Some(r) = &mut self.wave_source {
                                        r.fault(e, raw.clone(), ordinal)?;
                                    } else {
                                        return Err(e);
                                    }
                                }
                                if let Some(r) = &mut self.wave_source {
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
                                if let Some(r) = &mut self.wave_source {
                                    r.marker = Some(raw.clone());
                                }
                                w.marker = Some(raw);
                            }
                            [0, 0, 0, 0] => {
                                if !raw.payload().is_empty() {
                                    let e = Error::Invalid("encoded wave terminator data");
                                    if let Some(r) = &mut self.wave_source {
                                        r.fault(e, raw.clone(), ordinal)?;
                                    } else {
                                        return Err(e);
                                    }
                                }
                                if let Some(r) = &mut self.wave_source
                                    && r.terminator.is_none()
                                {
                                    r.terminator = Some(raw.clone());
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
                            .wave_fields
                            .as_mut()
                            .ok_or(Error::Invalid("wave state absent"))?;
                        if w.terminator.is_none()
                            || (self.sound_source.is_some()
                                && (w.format.is_none() || w.marker.is_none()))
                        {
                            let error = Error::Unsupported("encoded incomplete mp4a wave context");
                            if let Some(report) = &mut self.wave_source {
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
                    BoxStep::Yield => return Ok(CursorStatus::Yield),
                    BoxStep::Box(raw) => {
                        self.inspect_box()?;
                        if raw.header().kind == *b"wave" && self.requires_wave {
                            if self.wave_fields.is_some() {
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
                                let mut report = crate::WaveObservation::new(raw.clone());
                                if self
                                    .sound_source
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
                                self.wave_source = Some(report);
                            }
                            self.wave_fields = Some(Wave {
                                raw,
                                children: Some(nested),
                                format: None,
                                marker: None,
                                terminator: None,
                            });
                        } else if raw.header().kind == *b"wave"
                            && self.view.extended_configurations()
                            && self.kind == EncodedEntryKind::Aac
                        {
                            return Err(Error::Unsupported(
                                "encoded AAC wave requires a QuickTime sound description",
                            ));
                        } else if raw.header().kind == self.config_kind {
                            if self.requires_wave {
                                return Err(Error::Unsupported(if self.sound_source.is_some() {
                                    "encoded sound-v1 config outside wave"
                                } else {
                                    "encoded QuickTime AAC configuration outside wave"
                                }));
                            }
                            self.accept_configuration(raw)?;
                        }
                    }
                    BoxStep::End => {
                        self.children = None;
                        if self.kind == EncodedEntryKind::Dts {
                            continue;
                        }
                        let cfg = self.configuration.as_ref().ok_or(Error::Unsupported(
                            "encoded configuration path or absence not established",
                        ))?;
                        if matches!(
                            self.kind,
                            EncodedEntryKind::Aac | EncodedEntryKind::Mpeg4Visual
                        ) && self.esds.is_none()
                        {
                            let start = usize::from(cfg.header().header_size);
                            let bytes = cfg.bytes().slice(start..cfg.bytes().len())?;
                            let profile = match self.kind {
                                EncodedEntryKind::Mpeg4Visual => EsdsProfile::Visual,
                                _ if self.view.extended_configurations() && !self.requires_wave => {
                                    EsdsProfile::RangeAudio
                                }
                                _ => EsdsProfile::Aac,
                            };
                            self.esds = Some(EsdsWalk::with_profile(
                                bytes,
                                self.limits.max_descriptors,
                                profile,
                            )?);
                        }
                    }
                }
                continue;
            }
            if let Some(esds) = &mut self.esds
                && !esds.poll(work)?
            {
                return Ok(CursorStatus::Yield);
            }
            if !work.try_charge(1) {
                return Ok(CursorStatus::Yield);
            }
            let cfg = match self.configuration.as_ref() {
                Some(cfg) => cfg,
                None if self.kind == EncodedEntryKind::Dts => self.entry.raw(),
                None => return Err(Error::Invalid("encoded config missing at publication")),
            };
            let dts = match self.kind {
                EncodedEntryKind::Dts => Some(check_dts_sample_entry(
                    &self.entry,
                    self.configuration.as_ref(),
                )?),
                EncodedEntryKind::Aac if self.view.extended_configurations() => {
                    match self.esds.as_ref().and_then(|esds| esds.object_type) {
                        Some(oti) if dts_object_type(oti)? => {
                            self.kind = EncodedEntryKind::Dts;
                            Some(check_mp4a_dts(&self.entry)?)
                        }
                        _ => None,
                    }
                }
                _ => None,
            };
            if let Some(dts) = dts {
                dts.media_clock(self.header.time_base()?)?;
            }
            let extradata = match self.kind {
                EncodedEntryKind::Aac | EncodedEntryKind::Mpeg4Visual => self
                    .esds
                    .as_ref()
                    .and_then(|x| x.dsi.as_ref().map(|(_, bytes)| bytes.clone()))
                    .ok_or(Error::Unsupported(if self.kind == EncodedEntryKind::Aac {
                        "encoded AAC decoder-specific config absent"
                    } else {
                        "MPEG-4 Visual decoder-specific config absent"
                    }))?,
                EncodedEntryKind::Avc | EncodedEntryKind::Hevc | EncodedEntryKind::Av1 => cfg
                    .bytes()
                    .slice(usize::from(cfg.header().header_size)..cfg.bytes().len())?,
                EncodedEntryKind::Opus => opus_configuration(&self.pool, &self.entry, cfg)?,
                EncodedEntryKind::Ac3
                | EncodedEntryKind::Eac3
                | EncodedEntryKind::Vp9
                | EncodedEntryKind::Dts => {
                    cfg.bytes().slice(cfg.bytes().len()..cfg.bytes().len())?
                }
            };
            if self.kind == EncodedEntryKind::Aac {
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
            let (mut demux, encoder, receiver_source) = metadata(
                self.header,
                &self.entry,
                self.kind,
                extradata,
                self.view,
                self.major,
            )?;
            let receiver_source = receiver_source.map(|mut source| {
                source.sound = self.sound_source.clone();
                source.wave = self.wave_fields.as_ref().map(|w| w.raw.clone());
                source
            });
            if self.view != DescriptionView::Regular && self.major == Some(*b"qt  ") {
                let vendor = fields::fourcc(self.entry.raw().payload(), 12)?;
                if self.requires_wave && vendor != [0; 4] {
                    return Err(Error::Unsupported(
                        "encoded QuickTime nonzero vendor profile",
                    ));
                }
                demux.vendor_id = Some(vendor);
            }
            self.result = Some(EncodedStream {
                demux,
                entry: self.entry.clone(),
                configuration: cfg.clone(),
                dts,
                encoder,
                kind: self.kind,
                receiver_source,
                wave_source: self.wave_source.take(),
                raw_headers: [
                    self.header.raw_tkhd.clone(),
                    self.header.raw_mdhd.clone(),
                    self.header.raw_hdlr.clone(),
                ],
            });
            return Ok(CursorStatus::Ready);
        }
    }
    pub fn take(&mut self, input: &mut InputCursor, cancel: &AtomicBool) -> Result<EncodedStream> {
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

type MetadataParts = (StreamView, Option<Bytes>, Option<crate::EncodedSource>);
fn metadata(
    header: &TrackHeader,
    entry: &SampleDescription,
    kind: EncodedEntryKind,
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
    let clock = MediaClock::from_source(header.time_base()?)?;
    let metadata_source = (view != DescriptionView::Regular)
        .then(|| -> Result<_> {
            Ok(crate::EncodedSource {
                language: crate::encoded_metadata::language(
                    header,
                    view.extended_configurations() && major == Some(*b"qt  "),
                )?,
                handler: crate::encoded_metadata::handler(header, major)?,
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
        if metadata_source.is_none() && (!(1..=26).contains(&c) || packed & 0x8000 != 0) {
            return Err(Error::Unsupported("encoded native language profile"));
        }
        *v = c + 0x60;
    }
    if let Some(source) = &metadata_source {
        language = source.language.demux;
    }
    let h = header.raw_hdlr.payload();
    if metadata_source.is_none()
        && (fields::u32_at(h, 4)? != 0 || fields::slice(h, 12, 12)? != [0; 12])
    {
        return Err(Error::Unsupported("encoded legacy handler profile"));
    }
    let tail = h.get(24..).ok_or(Error::Truncated)?;
    if tail.len() > 256 {
        return Err(Error::Limit {
            requested: tail.len(),
            maximum: 256,
        });
    }
    let handler_name = if let Some(source) = &metadata_source {
        if let Some(name) = &source.handler.demux {
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
    let video = kind.is_video();
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
        EncodedEntryKind::Avc => "h264",
        EncodedEntryKind::Hevc => "hevc",
        EncodedEntryKind::Mpeg4Visual => "mpeg4",
        EncodedEntryKind::Av1 => "av1",
        EncodedEntryKind::Vp9 => "vp9",
        EncodedEntryKind::Aac => "aac",
        EncodedEntryKind::Ac3 => "ac3",
        EncodedEntryKind::Eac3 => "eac3",
        EncodedEntryKind::Dts => "dts",
        EncodedEntryKind::Opus => "opus",
    };
    Ok((
        StreamView {
            track_id: header.track_id,
            codec,
            media_type: u8::from(!video),
            time_base: clock.demux_base,
            clock,
            extradata,
            language,
            handler_name,
            vendor_id: None,
        },
        encoder,
        metadata_source,
    ))
}

fn opus_configuration(pool: &Budget, entry: &SampleDescription, config: &BoxView) -> Result<Bytes> {
    // Opus in ISO BMFF, sections 4.3.1 and 4.3.2.
    let head = OpusHead::from_dops(config.payload())?;
    let sample_entry = entry.raw().payload();
    let coded_channels = u16::from(head.streams) + u16::from(head.coupled_streams);
    if fields::u16_at(sample_entry, 16)? != coded_channels
        || fields::u16_at(sample_entry, 18)? != 16
        || fields::u32_at(sample_entry, 24)? != OPUS_RATE << 16
    {
        return Err(Error::Invalid("Opus sample entry audio fields"));
    }
    let mut packet = [0; OpusHead::MAX_BYTES];
    let len = head.write(&mut packet);
    if config.payload().len() != len - b"OpusHead".len() {
        return Err(Error::Invalid("Opus specific box trailing bytes"));
    }
    Bytes::copy_from_slice(pool, &packet[..len])
}

/// The object type and decoder-specific bytes an `mp4a`, `mp4v` or `mp4s` description
/// states in its `esds` (ISO/IEC 14496-1 `DecoderConfigDescriptor`), read
/// under a work budget.
#[derive(Debug)]
pub struct ObjectTypeScan(EsdsDescriptionWalk);

impl ObjectTypeScan {
    pub fn new(entry: &SampleDescription, limits: ConfigurationLimits) -> Result<Self> {
        let profile = match &entry.format {
            b"mp4s" => EsdsProfile::SystemStream,
            b"mp4v" => EsdsProfile::Visual,
            _ => EsdsProfile::ObjectType,
        };
        EsdsDescriptionWalk::with_profile(entry, limits, profile).map(Self)
    }

    /// The object type and the decoder-specific information, if any; `None`
    /// until the walk completes, yielding with `work` spent.
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<Option<(u8, Option<Bytes>)>> {
        if !self.0.scan_configuration(work)? {
            return Ok(None);
        }
        let (_, walk) = self
            .0
            .esds
            .as_ref()
            .ok_or(Error::Invalid("missing elementary stream descriptor"))?;
        let object_type = walk
            .object_type
            .ok_or(Error::Invalid("missing decoder configuration"))?;
        Ok(Some((
            object_type,
            walk.dsi.as_ref().map(|(_, bytes)| bytes.clone()),
        )))
    }
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
            let mut job = AacConfigurationJob::new(&entry, ConfigurationLimits::default()).unwrap();
            let cancel = AtomicBool::new(false);
            assert!(matches!(
                job.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
                AacConfigurationStep::Yield
            ));
            let config = (0..200)
                .find_map(
                    |_| match job.poll(&mut WorkBudget::new(16), &cancel).unwrap() {
                        AacConfigurationStep::Ready(config) => Some(config),
                        AacConfigurationStep::Yield => None,
                        AacConfigurationStep::End => panic!("AAC configuration missing"),
                    },
                )
                .expect("AAC configuration");
            let trailing = if wave { 8 } else { 0 };
            assert_eq!(
                config.offset,
                START + (bytes.len() - ASC.len() - trailing) as u64
            );
            assert_eq!(config.bytes.as_ref(), ASC);
            assert!(matches!(
                job.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
                AacConfigurationStep::End
            ));
        }
    }

    #[test]
    fn quicktime_aac_uses_the_declared_sound_extension_offset() {
        let mut body = vec![0; 68];
        body[7] = 1;
        body[9] = 2;
        body[16..20].copy_from_slice(&0x0003_0010_u32.to_be_bytes());
        body[20..24].copy_from_slice(&0xfffe_0000_u32.to_be_bytes());
        body[24..28].copy_from_slice(&65_536_u32.to_be_bytes());
        body[28..32].copy_from_slice(&76_u32.to_be_bytes());
        body[32..40].copy_from_slice(&48_000_f64.to_be_bytes());
        body[40..44].copy_from_slice(&2_u32.to_be_bytes());
        body[44..48].copy_from_slice(&0x7f00_0000_u32.to_be_bytes());
        body.extend(atom(b"wave", &wave_config(b"mp4a", false)));
        let pool = Budget::new(4096).unwrap();
        let entry = |body: &[u8]| {
            let bytes = Bytes::copy_from_slice(&pool, &atom(b"mp4a", body)).unwrap();
            SampleDescription::parse(BoxView::new(bytes, START).unwrap(), 1).unwrap()
        };
        let limits = ConfigurationLimits::default();
        let cancel = AtomicBool::new(false);
        let mut job = AacConfigurationJob::new(&entry(&body), limits).unwrap();
        let config = (0..200)
            .find_map(
                |_| match job.poll(&mut WorkBudget::new(16), &cancel).unwrap() {
                    AacConfigurationStep::Ready(config) => Some(config),
                    AacConfigurationStep::Yield => None,
                    AacConfigurationStep::End => panic!("AAC configuration missing"),
                },
            )
            .unwrap();
        assert_eq!(config.bytes.as_ref(), ASC);
        assert_eq!(config.offset, START + (body.len() - ASC.len()) as u64);

        for (offset, expected) in [
            (71_u32, Error::Invalid("QuickTime sound extension offset")),
            (u32::MAX, Error::Truncated),
        ] {
            body[28..32].copy_from_slice(&offset.to_be_bytes());
            assert_eq!(
                AacConfigurationJob::new(&entry(&body), limits).unwrap_err(),
                expected
            );
        }
    }

    #[test]
    fn cancellation_during_descriptor_scan_ends_the_job() {
        let (entry, _) = description(&esds(&ASC), false);
        let mut job = AacConfigurationJob::new(&entry, ConfigurationLimits::default()).unwrap();
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            job.poll(&mut WorkBudget::new(16), &cancel).unwrap(),
            AacConfigurationStep::Yield
        ));
        assert_eq!(
            job.poll(&mut WorkBudget::new(1024), &AtomicBool::new(true))
                .unwrap_err(),
            Error::Cancelled
        );
        assert_eq!(
            job.poll(&mut WorkBudget::new(1024), &cancel).unwrap_err(),
            Error::Cancelled
        );
    }

    #[test]
    fn refuses_a_second_top_level_elementary_stream_descriptor() {
        let mut children = esds(&ASC);
        children.extend([3, 3, 0, 2, 0]);
        let size = u32::try_from(children.len()).unwrap();
        children[..4].copy_from_slice(&size.to_be_bytes());
        let (entry, _) = description(&children, false);
        let mut job = AacConfigurationJob::new(&entry, ConfigurationLimits::default()).unwrap();
        assert_eq!(
            job.poll(&mut WorkBudget::new(1024), &AtomicBool::new(false))
                .unwrap_err(),
            Error::Invalid("AAC elementary stream descriptor root")
        );
    }

    #[test]
    fn refuses_duplicate_descriptors_and_enforces_the_descriptor_work_limit() {
        let mut duplicate = esds(&ASC);
        duplicate.extend(esds(&ASC));
        let (entry, _) = description(&duplicate, false);
        let mut job = AacConfigurationJob::new(&entry, ConfigurationLimits::default()).unwrap();
        let cancel = AtomicBool::new(false);
        let duplicate_error = Error::Invalid("duplicate AAC elementary stream descriptor");
        for _ in 0..2 {
            assert_eq!(
                job.poll(&mut WorkBudget::new(1024), &cancel).unwrap_err(),
                duplicate_error
            );
        }

        let (entry, _) = description(&esds(&ASC), false);
        let mut job = AacConfigurationJob::new(
            &entry,
            ConfigurationLimits {
                max_descriptors: 2,
                ..ConfigurationLimits::default()
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
            let mut job = AacConfigurationJob::new(&entry, ConfigurationLimits::default()).unwrap();
            assert_eq!(
                job.poll(&mut WorkBudget::new(1024), &cancel).unwrap_err(),
                error
            );
        }
    }

    #[test]
    fn counts_outer_and_wave_children_against_one_box_limit() {
        let (entry, _) = description(&wave_config(b"mp4a", false), true);
        let limits = ConfigurationLimits {
            max_child_boxes: 4,
            ..ConfigurationLimits::default()
        };
        let mut job = AacConfigurationJob::new(&entry, limits).unwrap();
        assert!(matches!(
            job.poll(&mut WorkBudget::new(1024), &AtomicBool::new(false)),
            Err(Error::Limit {
                requested: 5,
                maximum: 4
            })
        ));
    }
}
