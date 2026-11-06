//! Original EN 300 468 6.2.41/Table 99 source views. No inner subtitle parsing.
use super::{Descriptor, ElementaryStream};
use triergon_core::{Error, Result};

/// Out-of-band interpretation, kept separate from source descriptor bytes.
/// Neither a tag, a filename, registration nor a private_data_specifier proves DVB.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DescriptorNamespace {
    #[default]
    Unspecified,
    Dvb(DvbNamespaceEvidence),
}

/// The coordinator must retain the actual service/profile evidence in its
/// operation contract. These variants record its declaration, not on-wire proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DvbNamespaceEvidence {
    CallerDeclaredTransportProfile,
    OutOfBandServiceContract,
}

/// Original descriptor TLV and body borrowed from charged PMT storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvbSubtitling<'a> {
    pub source: &'a [u8],
    pub data: &'a [u8],
}
impl<'a> DvbSubtitling<'a> {
    pub fn service_count(self) -> usize {
        self.data.len() / 8
    }
    pub fn services(self) -> DvbSubtitlingServices<'a> {
        DvbSubtitlingServices {
            remaining: self.data,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvbSubtitlingService<'a> {
    pub source: &'a [u8],
    /// Raw ISO 639-2/ISO 8859-1 bytes, including unrecognised/malformed codes.
    pub language_code: [u8; 3],
    /// Original byte, including reserved/unrecognised values.
    pub subtitling_type: u8,
    pub composition_page_id: u16,
    pub ancillary_page_id: u16,
}
impl<'a> DvbSubtitlingService<'a> {
    fn read(source: &'a [u8]) -> Option<Self> {
        let source = source.get(..8)?;
        Some(Self {
            source,
            language_code: [source[0], source[1], source[2]],
            subtitling_type: source[3],
            composition_page_id: u16::from_be_bytes([source[4], source[5]]),
            ancillary_page_id: u16::from_be_bytes([source[6], source[7]]),
        })
    }
    /// Syntax annotation only; reserved types remain observable. This does not
    /// validate the inner stream or establish that an ancillary page is present.
    pub fn has_dvb_subtitling_type(self) -> bool {
        (0x10..=0x2f).contains(&self.subtitling_type)
    }
}

#[derive(Clone, Debug)]
pub struct DvbSubtitlingServices<'a> {
    remaining: &'a [u8],
}
impl<'a> Iterator for DvbSubtitlingServices<'a> {
    type Item = DvbSubtitlingService<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        let service = DvbSubtitlingService::read(self.remaining)?;
        self.remaining = &self.remaining[8..];
        Some(service)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let count = self.remaining.len() / 8;
        (count, Some(count))
    }
}
impl ExactSizeIterator for DvbSubtitlingServices<'_> {}
impl std::iter::FusedIterator for DvbSubtitlingServices<'_> {}

impl<'a> Descriptor<'a> {
    /// Explicit DVB interpretation; ordinary unscoped descriptor parsing keeps
    /// this tag opaque. A u8 descriptor length allows at most 31 complete entries.
    pub fn dvb_subtitling(self) -> Result<DvbSubtitling<'a>> {
        if self.tag != 0x59 {
            return Err(Error::Invalid("DVB subtitling descriptor tag"));
        }
        if self.data.len() > 255 || !self.data.len().is_multiple_of(8) {
            return Err(Error::Invalid("DVB subtitling descriptor length"));
        }
        Ok(DvbSubtitling {
            source: self.source,
            data: self.data,
        })
    }
}

/// Source signalling, not an assertion of payload/decoder conformance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecIdentification {
    Unknown,
    DvbSubtitles,
    ConflictingDvbSignalling,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DvbSubtitlingSignalling {
    /// All occurrences, including empty and duplicate descriptors.
    pub descriptors: usize,
    /// All entries in source order, without deduplication/selection.
    pub services: usize,
    pub audio_conflict: bool,
}

/// A development native view, not a complete container/decoder operation.
/// Multi-service behaviour is unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DvbNativeViewProfile {
    D0SingleServiceV1,
}
impl DvbNativeViewProfile {
    pub fn source_pin(self) -> &'static str {
        match self {
            Self::D0SingleServiceV1 => "d32b387f2b0a484599d4587d651891f0c63c4238",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeDvbSingleServiceView<'a> {
    pub profile: DvbNativeViewProfile,
    pub source: DvbSubtitlingService<'a>,
    /// compID_be, ancillaryID_be, type. Raw language stays in `source`.
    pub bytes: [u8; 5],
}

/// Constant-sized facts collected inside PmtReader's charged descriptor steps.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct DvbSummary {
    descriptors: usize,
    services: usize,
    first_service_offset: Option<usize>,
    audio_present: bool,
}
impl DvbSummary {
    pub(super) fn observe(&mut self, descriptor: Descriptor<'_>, offset: usize) -> Result<()> {
        match descriptor.tag {
            0x59 => {
                let count = descriptor.dvb_subtitling()?.service_count();
                let descriptors = self.descriptors.checked_add(1).ok_or(Error::Overflow)?;
                let services = self.services.checked_add(count).ok_or(Error::Overflow)?;
                let entry = offset.checked_add(2).ok_or(Error::Overflow)?;
                if count != 0 && self.first_service_offset.is_none() {
                    self.first_service_offset = Some(entry);
                }
                self.descriptors = descriptors;
                self.services = services;
            }
            0x6a | 0x7a => self.audio_present = true,
            _ => {}
        }
        Ok(())
    }
}

impl ElementaryStream {
    pub fn descriptor_namespace(&self) -> DescriptorNamespace {
        self.namespace
    }
    /// None means the operation did not establish DVB context, not no services.
    /// Raw ordered TLVs (including private namespace changes) remain in descriptors().
    pub fn dvb_subtitling_signalling(&self) -> Option<DvbSubtitlingSignalling> {
        if !matches!(self.namespace, DescriptorNamespace::Dvb(_)) {
            return None;
        }
        Some(DvbSubtitlingSignalling {
            descriptors: self.dvb.descriptors,
            services: self.dvb.services,
            audio_conflict: self.dvb.services != 0 && self.dvb.audio_present,
        })
    }
    pub fn codec_identification(&self) -> CodecIdentification {
        let Some(facts) = self.dvb_subtitling_signalling() else {
            return CodecIdentification::Unknown;
        };
        if self.stream_type != 0x06 || facts.services == 0 {
            CodecIdentification::Unknown
        } else if facts.audio_conflict {
            CodecIdentification::ConflictingDvbSignalling
        } else {
            CodecIdentification::DvbSubtitles
        }
    }
    /// Explicit single-descriptor/single-service D0 codecpar view. Additional ES
    /// context TLVs are unprobed and unsupported by this view. No first-service
    /// fallback, language conversion, PES envelope stripping or DTS inference.
    /// Wider native field values are unverified; only six D0 inputs were probed.
    pub fn native_dvb_single_service_view(
        &self,
        profile: DvbNativeViewProfile,
    ) -> Result<NativeDvbSingleServiceView<'_>> {
        if self.codec_identification() != CodecIdentification::DvbSubtitles
            || self.dvb.descriptors != 1
            || self.dvb.services != 1
            || self.descriptor_bytes.len() != 10
        {
            return Err(Error::Unsupported(
                "D0 single-service DVB native view context",
            ));
        }
        let start = self
            .dvb
            .first_service_offset
            .ok_or(Error::Invalid("DVB service offset"))?;
        let end = start.checked_add(8).ok_or(Error::Overflow)?;
        let raw = self
            .descriptor_bytes
            .as_ref()
            .get(start..end)
            .ok_or(Error::Invalid("DVB service extent"))?;
        let source = DvbSubtitlingService::read(raw).ok_or(Error::Invalid("DVB service extent"))?;
        Ok(NativeDvbSingleServiceView {
            profile,
            source,
            bytes: [raw[4], raw[5], raw[6], raw[7], raw[3]],
        })
    }
}
