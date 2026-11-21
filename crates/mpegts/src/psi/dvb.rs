//! Original EN 300 468 6.2.41/Table 99 source views. No inner subtitle parsing.
use super::{Descriptor, ElementaryStream};
use triergon_core::{Error, Result};

/// Out-of-band interpretation, kept separate from source descriptor bytes.
/// Neither a tag, a filename, registration nor a private_data_specifier proves DVB.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DescriptorNamespace {
    /// No out-of-band context: DVB and ATSC private descriptors stay opaque.
    #[default]
    Unspecified,
    /// The caller has identified the transport as DVB, with the evidence it relies on.
    Dvb(DvbNamespaceEvidence),
    /// The caller has identified the transport as ATSC. Private audio
    /// stream types follow A/53 Part 3 §5.7 in this namespace.
    Atsc,
}

/// The coordinator must retain the actual service/profile evidence in its
/// operation contract. These variants record its declaration, not on-wire proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DvbNamespaceEvidence {
    /// The caller declared a DVB transport profile for the input.
    CallerDeclaredTransportProfile,
    /// An out-of-band service contract establishes DVB signalling.
    OutOfBandServiceContract,
}

/// Original descriptor TLV and body borrowed from charged PMT storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvbSubtitling<'a> {
    /// The whole descriptor: tag, length and body.
    pub source: &'a [u8],
    /// The descriptor body: a whole number of 8-byte service entries.
    pub data: &'a [u8],
}
impl<'a> DvbSubtitling<'a> {
    /// The number of 8-byte service entries, at most 31.
    pub fn service_count(self) -> usize {
        self.data.len() / 8
    }
    /// Iterates the service entries in source order.
    pub fn services(self) -> DvbSubtitlingServices<'a> {
        DvbSubtitlingServices {
            remaining: self.data,
        }
    }
}

/// One 8-byte service entry of a DVB subtitling descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvbSubtitlingService<'a> {
    /// The entry's 8 source bytes.
    pub source: &'a [u8],
    /// Raw ISO 639-2/ISO 8859-1 bytes, including unrecognised/malformed codes.
    pub language_code: [u8; 3],
    /// Original byte, including reserved/unrecognised values.
    pub subtitling_type: u8,
    /// The 16-bit composition page ID.
    pub composition_page_id: u16,
    /// The 16-bit ancillary page ID.
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

/// Iterator over the service entries of a DVB subtitling descriptor.
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
    /// The signalling does not identify the codec.
    Unknown,
    /// Stream type 0x06 with DVB subtitling services and no DVB audio descriptor.
    DvbSubtitles,
    /// DVB subtitling services together with a DVB AC-3 or E-AC-3 descriptor on the same stream.
    ConflictingDvbSignalling,
}

/// Counts of the DVB subtitling signalling on one elementary stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DvbSubtitlingSignalling {
    /// All occurrences, including empty and duplicate descriptors.
    pub descriptors: usize,
    /// All entries in source order, without deduplication/selection.
    pub services: usize,
    /// Whether the stream also carries a DVB AC-3 or E-AC-3 descriptor alongside subtitling
    /// services.
    pub audio_conflict: bool,
}

/// A partial demux view, not a complete container/decoder operation.
/// Multi-service behaviour is unknown.
#[cfg(feature = "reference-trace")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DvbViewProfile {
    /// The view of a stream that signals a single subtitling service.
    SingleService,
}
#[cfg(feature = "reference-trace")]
impl DvbViewProfile {
    /// The revision of the reference demuxer that this view profile reproduces.
    pub fn reference_revision(self) -> &'static str {
        match self {
            Self::SingleService => "d32b387f2b0a484599d4587d651891f0c63c4238",
        }
    }
}

/// The single-service view of a DVB subtitling descriptor, as the reference demuxer exports it.
#[cfg(feature = "reference-trace")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvbSingleServiceView<'a> {
    /// The view profile this view follows.
    pub profile: DvbViewProfile,
    /// The service entry the view is taken from.
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
    /// The descriptor namespace the PMT was parsed with.
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
    /// What the stream's DVB signalling identifies, from its stream type and subtitling
    /// descriptors.
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
    /// Explicit single-descriptor/single-service codec parameters view. Additional
    /// ES context TLVs are unprobed and unsupported by this view. No first-service
    /// fallback, language conversion, PES envelope stripping or DTS inference.
    /// Wider field values are unverified; only six inputs were probed.
    #[cfg(feature = "reference-trace")]
    pub fn dvb_single_service_view(
        &self,
        profile: DvbViewProfile,
    ) -> Result<DvbSingleServiceView<'_>> {
        if self.codec_identification() != CodecIdentification::DvbSubtitles
            || self.dvb.descriptors != 1
            || self.dvb.services != 1
            || self.descriptor_bytes.len() != 10
        {
            return Err(Error::Unsupported(
                "DVB codec parameters require one subtitling descriptor with one service",
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
        Ok(DvbSingleServiceView {
            profile,
            source,
            bytes: [raw[4], raw[5], raw[6], raw[7], raw[3]],
        })
    }
}
