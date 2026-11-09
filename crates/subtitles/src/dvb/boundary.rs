//! Original source configuration and explicit post-demux DVB packet binding.
//! Codec mapping draft-ietf-cellar-codec-21 5.7.1; EN300468 6.2.41;
//! exact shared Time conversion. The codec mapping is a numbered Internet-Draft.
//! This module performs no I/O, container demux, envelope guessing or native call.
use triergon_core::{Budget, Bytes, Error, Result, SourceLocation, Time, TimeBase};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Service {
    pub composition_page: u16,
    pub ancillary_page: u16,
    pub subtitling_type: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketEnvelope {
    Segments,
    PesDataField,
}

#[derive(Clone, Debug)]
pub struct DvbPacket {
    pub payload: Bytes,
    pub pts: Option<Time>,
    pub generation: u64,
    pub sequence: u64,
    pub envelope: PacketEnvelope,
    /// Original configuration, raw clock values and container locator, when
    /// the packet was bound by [`SourceConfiguration::packet`].
    pub source: Option<PacketSourceFacts>,
}

/// Keep the declared positive ratio as well as core's exact reduced conversion
/// base. Equivalent ratios do not erase their original numeric representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceClock {
    numerator: u64,
    denominator: u64,
    exact: TimeBase,
}

impl SourceClock {
    pub fn new(numerator: u64, denominator: u64) -> Result<Self> {
        Ok(Self {
            numerator,
            denominator,
            exact: TimeBase::new(numerator, denominator)?,
        })
    }
    pub fn raw_numerator(self) -> u64 {
        self.numerator
    }
    pub fn raw_denominator(self) -> u64 {
        self.denominator
    }
    pub fn time_base(self) -> TimeBase {
        self.exact
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockOrigin {
    Source,
    /// A container/caller-derived view; never substituted for an encoded source
    /// value or unknown source DTS without retaining this distinction.
    Inferred,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockValue {
    pub ticks: i128,
    pub origin: ClockOrigin,
}

impl ClockValue {
    pub fn to_time(self, clock: SourceClock) -> Result<Time> {
        Time::from_ticks(self.ticks, clock.time_base())
    }
}

/// None means unknown. No integer is a sentinel in this independent API.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PacketTiming {
    pub pts: Option<ClockValue>,
    pub dts: Option<ClockValue>,
    pub duration: Option<ClockValue>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketIdentity {
    pub generation: u64,
    pub sequence: u64,
    /// A container locator, not a claim that the payload's first byte occurs
    /// here or that an assembled TS payload occupies a contiguous source range.
    pub container_position: Option<SourceLocation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigurationSource {
    MatroskaCodecPrivate,
    SubtitlingDescriptor { entry: u8 },
}

/// Fixed-size parsed facts plus charged immutable original configuration.
/// Parsing service metadata does not certify all features of its signalled type.
#[derive(Clone, Debug)]
pub struct SourceConfiguration {
    original: Bytes,
    source: ConfigurationSource,
    service: Service,
    clock: SourceClock,
    envelope: PacketEnvelope,
}

impl SourceConfiguration {
    pub fn from_matroska_private(
        budget: &Budget,
        original: &Bytes,
        clock: SourceClock,
    ) -> Result<Self> {
        if !original.belongs_to(budget) {
            return Err(Error::ForeignPool);
        }
        let bytes = original.as_ref();
        if bytes.len() < 5 {
            return Err(Error::Truncated);
        }
        if bytes.len() != 5 {
            return Err(Error::Invalid("DVB source private length"));
        }
        Ok(Self {
            original: original.clone(),
            source: ConfigurationSource::MatroskaCodecPrivate,
            service: Service {
                composition_page: u16::from_be_bytes([bytes[0], bytes[1]]),
                ancillary_page: u16::from_be_bytes([bytes[2], bytes[3]]),
                subtitling_type: bytes[4],
            },
            clock,
            envelope: PacketEnvelope::Segments,
        })
    }

    /// Takes the complete descriptor, including tag and length, and an explicit
    /// selected service index. Post-demux bare segments and source PES data fields
    /// are different declared inputs; neither is autodetected from payload bytes.
    pub fn from_subtitling_descriptor(
        budget: &Budget,
        original: &Bytes,
        entry: usize,
        clock: SourceClock,
        envelope: PacketEnvelope,
    ) -> Result<Self> {
        if !original.belongs_to(budget) {
            return Err(Error::ForeignPool);
        }
        let bytes = original.as_ref();
        if bytes.len() < 2 {
            return Err(Error::Truncated);
        }
        if bytes[0] != 0x59 {
            return Err(Error::Invalid("DVB subtitling descriptor tag"));
        }
        let length = usize::from(bytes[1]);
        let end = length + 2;
        if bytes.len() < end {
            return Err(Error::Truncated);
        }
        if bytes.len() != end {
            return Err(Error::Invalid("DVB subtitling descriptor length"));
        }
        if length == 0 || length % 8 != 0 {
            return Err(Error::Invalid("DVB subtitling descriptor entries"));
        }
        if entry >= length / 8 {
            return Err(Error::Invalid("DVB descriptor service index"));
        }
        let at = 2 + entry * 8;
        Ok(Self {
            original: original.clone(),
            source: ConfigurationSource::SubtitlingDescriptor {
                entry: u8::try_from(entry).map_err(|_| Error::Overflow)?,
            },
            service: Service {
                composition_page: u16::from_be_bytes([bytes[at + 4], bytes[at + 5]]),
                ancillary_page: u16::from_be_bytes([bytes[at + 6], bytes[at + 7]]),
                subtitling_type: bytes[at + 3],
            },
            clock,
            envelope,
        })
    }

    pub fn original(&self) -> &Bytes {
        &self.original
    }
    pub fn source(&self) -> ConfigurationSource {
        self.source
    }
    pub fn service(&self) -> Service {
        self.service
    }
    pub fn clock(&self) -> SourceClock {
        self.clock
    }
    pub fn envelope(&self) -> PacketEnvelope {
        self.envelope
    }

    pub fn language_bytes(&self) -> Option<&[u8]> {
        match self.source {
            ConfigurationSource::MatroskaCodecPrivate => None,
            ConfigurationSource::SubtitlingDescriptor { entry } => {
                let at = 2 + usize::from(entry) * 8;
                self.original.as_ref().get(at..at + 3)
            }
        }
    }

    /// Explicit five-byte source mapping, not an assertion that every native
    /// codec parameter block has this representation. Raw descriptor stays intact.
    pub fn service_private_view(&self) -> [u8; 5] {
        let composition = self.service.composition_page.to_be_bytes();
        let ancillary = self.service.ancillary_page.to_be_bytes();
        [
            composition[0],
            composition[1],
            ancillary[0],
            ancillary[1],
            self.service.subtitling_type,
        ]
    }

    /// Alias payload/configuration without allocating, stripping, rescaling to
    /// an integer output clock, filling unknown timestamps or changing order.
    pub fn packet(
        &self,
        budget: &Budget,
        payload: &Bytes,
        timing: PacketTiming,
        identity: PacketIdentity,
    ) -> Result<DvbPacket> {
        self.check_pool(budget)?;
        if !payload.belongs_to(budget) {
            return Err(Error::ForeignPool);
        }
        let pts = timing
            .pts
            .map(|value| value.to_time(self.clock))
            .transpose()?;
        Ok(DvbPacket {
            payload: payload.clone(),
            pts,
            generation: identity.generation,
            sequence: identity.sequence,
            envelope: self.envelope,
            source: Some(PacketSourceFacts {
                configuration: self.clone(),
                timing,
                container_position: identity.container_position,
            }),
        })
    }

    fn check_pool(&self, budget: &Budget) -> Result<()> {
        if self.original.belongs_to(budget) {
            Ok(())
        } else {
            Err(Error::ForeignPool)
        }
    }
}

#[derive(Clone, Debug)]
pub struct PacketSourceFacts {
    configuration: SourceConfiguration,
    timing: PacketTiming,
    container_position: Option<SourceLocation>,
}

impl PacketSourceFacts {
    pub fn configuration(&self) -> &SourceConfiguration {
        &self.configuration
    }
    pub fn timing(&self) -> PacketTiming {
        self.timing
    }
    pub fn container_position(&self) -> Option<SourceLocation> {
        self.container_position
    }
}
