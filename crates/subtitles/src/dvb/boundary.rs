//! A DVB subtitle service's configuration and the packets bound to it after
//! demux. Matroska `CodecPrivate` follows the codec mapping Internet-Draft
//! draft-ietf-cellar-codec-21 §5.7.1, the subtitling descriptor EN 300 468
//! §6.2.41; times convert exactly through `Time`. There is no I/O, container
//! demux or detection of a packet's envelope.
use triergon_core::{Budget, Bytes, Error, Result, SourceLocation, Time, TimeBase};

/// One subtitle service: the pages its segments are on and its type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Service {
    /// The composition page id.
    pub composition_page: u16,
    /// The ancillary page id, whose segments services may share.
    pub ancillary_page: u16,
    /// The `subtitling_type` byte, unchecked.
    pub subtitling_type: u8,
}

/// How a packet's payload holds its segments; declared, never detected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketEnvelope {
    /// Bare subtitling segments, as a Matroska block holds them.
    Segments,
    /// A whole PES packet data field: data identifier, stream id, segments
    /// and end marker.
    PesDataField,
}

/// One DVB subtitle packet with its time and identity.
#[derive(Clone, Debug)]
pub struct DvbPacket {
    /// The packet bytes, framed as `envelope` says.
    pub payload: Bytes,
    /// The presentation time, converted exactly from the source clock;
    /// `None` when unknown.
    pub pts: Option<Time>,
    /// The reader generation the packet was read in.
    pub generation: u64,
    /// The caller's sequence number of the packet.
    pub sequence: u64,
    /// How `payload` holds its segments.
    pub envelope: PacketEnvelope,
    /// The configuration, raw clock values and container locator, when
    /// [`SourceConfiguration::packet`] bound the packet.
    pub source: Option<PacketSourceFacts>,
}

/// A source time base as declared, kept beside its exact reduced
/// `TimeBase`: 1/90000 and 2/180000 stay distinct.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceClock {
    numerator: u64,
    denominator: u64,
    exact: TimeBase,
}

impl SourceClock {
    /// `numerator / denominator` seconds per tick. A zero in either is
    /// `Error::Invalid`.
    pub fn new(numerator: u64, denominator: u64) -> Result<Self> {
        Ok(Self {
            numerator,
            denominator,
            exact: TimeBase::new(numerator, denominator)?,
        })
    }
    /// The numerator as declared, not reduced.
    pub fn raw_numerator(self) -> u64 {
        self.numerator
    }
    /// The denominator as declared, not reduced.
    pub fn raw_denominator(self) -> u64 {
        self.denominator
    }
    /// The exact reduced time base.
    pub fn time_base(self) -> TimeBase {
        self.exact
    }
}

/// Where a clock value comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockOrigin {
    /// Encoded in the source.
    Source,
    /// Derived by the container or the caller; never taken for an encoded
    /// value, or for a DTS the source does not give.
    Inferred,
}

/// A raw clock value and where it comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockValue {
    /// Ticks of the [`SourceClock`].
    pub ticks: i128,
    /// Whether the source encodes the value.
    pub origin: ClockOrigin,
}

impl ClockValue {
    /// The exact time of `ticks` at `clock`; `Error::Overflow` when it does
    /// not fit a `Time`.
    pub fn to_time(self, clock: SourceClock) -> Result<Time> {
        Time::from_ticks(self.ticks, clock.time_base())
    }
}

/// A packet's raw clock values. `None` is unknown; no value is a sentinel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PacketTiming {
    /// The presentation time stamp.
    pub pts: Option<ClockValue>,
    /// The decoding time stamp.
    pub dts: Option<ClockValue>,
    /// The duration.
    pub duration: Option<ClockValue>,
}

/// Where a packet sits in the caller's reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketIdentity {
    /// The reader generation the packet was read in.
    pub generation: u64,
    /// The caller's sequence number of the packet.
    pub sequence: u64,
    /// A container locator, not a claim that the payload's first byte occurs
    /// here or that an assembled TS payload occupies a contiguous source range.
    pub container_position: Option<SourceLocation>,
}

/// Where a [`SourceConfiguration`] was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigurationSource {
    /// A Matroska track's five-byte `CodecPrivate`.
    MatroskaCodecPrivate,
    /// An MPEG-TS subtitling descriptor.
    SubtitlingDescriptor {
        /// The selected service's index among the descriptor's entries.
        entry: u8,
    },
}

/// One service's configuration, parsed, with the bytes it was read from.
/// Reading a service's type does not mean every feature it signals is
/// decoded.
#[derive(Clone, Debug)]
pub struct SourceConfiguration {
    original: Bytes,
    source: ConfigurationSource,
    service: Service,
    clock: SourceClock,
    envelope: PacketEnvelope,
}

impl SourceConfiguration {
    /// Reads a Matroska `CodecPrivate`: composition page, ancillary page and
    /// subtitling type in five bytes; its packets hold bare segments.
    /// `Error::ForeignPool` when `original` belongs to another pool,
    /// `Error::Truncated` below five bytes, `Error::Invalid` above.
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

    /// Reads service `entry` of a whole subtitling descriptor, tag and length
    /// included. Its packets hold their segments as `envelope` declares,
    /// which is never detected from their bytes. `Error::ForeignPool` when
    /// `original` belongs to another pool, `Error::Truncated` when it is
    /// shorter than its length, `Error::Invalid` for another tag than 0x59,
    /// bytes past the length, a length that is not a nonzero multiple of 8, or
    /// an `entry` past the last service.
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

    /// The bytes the configuration was read from.
    pub fn original(&self) -> &Bytes {
        &self.original
    }
    /// Where the configuration was read from.
    pub fn source(&self) -> ConfigurationSource {
        self.source
    }
    /// The selected service.
    pub fn service(&self) -> Service {
        self.service
    }
    /// The clock of the packets' time stamps.
    pub fn clock(&self) -> SourceClock {
        self.clock
    }
    /// How the packets hold their segments.
    pub fn envelope(&self) -> PacketEnvelope {
        self.envelope
    }

    /// The service's three-byte ISO 639-2 language code from the
    /// descriptor; `None` for a Matroska configuration.
    pub fn language_bytes(&self) -> Option<&[u8]> {
        match self.source {
            ConfigurationSource::MatroskaCodecPrivate => None,
            ConfigurationSource::SubtitlingDescriptor { entry } => {
                let at = 2 + usize::from(entry) * 8;
                self.original.as_ref().get(at..at + 3)
            }
        }
    }

    /// The service as the five bytes of a Matroska `CodecPrivate`:
    /// composition page, ancillary page, subtitling type. The descriptor
    /// bytes are unchanged.
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

    /// Binds `payload` and its timing to this configuration without
    /// allocating, stripping or rescaling; an unknown time stays unknown.
    /// `Error::ForeignPool` when the configuration or `payload` belongs to
    /// another pool than `budget`; a PTS that does not convert fails as
    /// [`ClockValue::to_time`] does.
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

/// What [`SourceConfiguration::packet`] bound a packet with.
#[derive(Clone, Debug)]
pub struct PacketSourceFacts {
    configuration: SourceConfiguration,
    timing: PacketTiming,
    container_position: Option<SourceLocation>,
}

impl PacketSourceFacts {
    /// The configuration the packet is bound to.
    pub fn configuration(&self) -> &SourceConfiguration {
        &self.configuration
    }
    /// The raw clock values.
    pub fn timing(&self) -> PacketTiming {
        self.timing
    }
    /// The container locator, as [`PacketIdentity::container_position`].
    pub fn container_position(&self) -> Option<SourceLocation> {
        self.container_position
    }
}
