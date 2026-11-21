//! Codec configuration in sample entries: AudioSpecificConfig and the object type
//! of an `esds` elementary stream descriptor, and DTS signalling.
use crate::{
    Limits,
    boxes::{BoxChildren, BoxStep, BoxView},
    fields,
    movie::{EntryLayout, SampleDescription},
};
use std::sync::atomic::AtomicBool;
use triergon_core::{Bytes, Error, Result, WorkBudget};

mod dts;
pub use dts::DtsCoreDescription;
pub(crate) use dts::{check_dts_sample_entry, check_mp4a_dts, dts_entry_children, dts_object_type};

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
pub(crate) enum EsdsProfile {
    Aac,
    AacDescription,
    AudioDescription,
    RangeAudio,
    ObjectType,
    /// Any object type of any stream type: an `mp4s` system stream entry.
    SystemStream,
}

#[derive(Debug)]
pub(crate) struct EsdsWalk {
    raw: Bytes,
    at: usize,
    ends: [usize; 3],
    depth: usize,
    phase: DescriptorPhase,
    pub(crate) rows: u32,
    limit: u32,
    pub(crate) dsi: Option<(usize, Bytes)>,
    decoder_seen: bool,
    pub(crate) object_type: Option<u8>,
    profile: EsdsProfile,
    completed: bool,
}
impl EsdsWalk {
    pub(crate) fn new(raw: Bytes, limit: u32) -> Result<Self> {
        Self::with_profile(raw, limit, EsdsProfile::Aac)
    }

    pub(crate) fn with_profile(raw: Bytes, limit: u32, profile: EsdsProfile) -> Result<Self> {
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
    pub(crate) fn poll(&mut self, work: &mut WorkBudget) -> Result<bool> {
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
                                EsdsProfile::AudioDescription
                                | EsdsProfile::ObjectType
                                | EsdsProfile::SystemStream => true,
                            };
                            if !allowed {
                                return Err(Error::Unsupported(
                                    "encoded mp4a object-type is not selected AAC",
                                ));
                            }
                            self.object_type = Some(b);
                        }
                        if index == 1
                            && !matches!(self.profile, EsdsProfile::SystemStream)
                            && (b >> 2 != 5 || b & 1 == 0)
                        {
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

pub(crate) fn quicktime_aac_extensions(entry: &SampleDescription) -> Result<usize> {
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
            b"mp4a" => match fields::u16_at(entry.raw().payload(), 8)? {
                1 => {
                    crate::compat::mov::encoded_metadata::SoundV1Source::parse_contexts(entry)?;
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
                return Err(Error::Invalid(
                    "AAC configuration requires an mp4a description",
                ));
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

pub(crate) fn check_configuration_size(raw: &BoxView, limits: ConfigurationLimits) -> Result<()> {
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

/// The object type and decoder-specific bytes an `mp4a` or `mp4s` description
/// states in its `esds` (ISO/IEC 14496-1 `DecoderConfigDescriptor`), read
/// under a work budget.
#[derive(Debug)]
pub struct ObjectTypeScan(EsdsDescriptionWalk);

impl ObjectTypeScan {
    pub fn new(entry: &SampleDescription, limits: ConfigurationLimits) -> Result<Self> {
        let profile = if entry.format == *b"mp4s" {
            EsdsProfile::SystemStream
        } else {
            EsdsProfile::ObjectType
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
    use triergon_core::Budget;

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
