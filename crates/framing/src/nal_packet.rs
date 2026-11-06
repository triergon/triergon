//! Complete-packet, source-preserving projection of the pinned d32 public BSFs.
//! NAL bodies stay opaque; this does not establish access-unit/decoder validity.
mod phases;

use crate::header::NalHeaderError;
use crate::{AvcConfigArrayKind, AvcConfiguration, BuildStep, Codec, HvcConfiguration};
use std::ops::Range;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, SourceLocation, WorkBudget};

/// Literal caller fields. A numeric value here is not a known-source-clock proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NalPacketFields {
    pub pts: i64,
    pub dts: i64,
    pub duration: i64,
    pub position: i64,
    pub flags: u32,
    pub stream_index: i32,
}

#[derive(Clone, Debug)]
pub struct NalPacket<M = NalPacketFields> {
    pub bytes: Bytes,
    pub location: SourceLocation,
    /// Caller source generation; independent of the receiver configuration epoch.
    pub generation: u64,
    /// Opaque caller metadata is moved intact, including exact/unknown clocks.
    /// The default is only the literal native-observation scalar structure.
    pub fields: M,
    /// Exact input side-data bytes, including a present empty value.
    pub new_extradata: Option<Bytes>,
}

pub enum NalPacketConfiguration {
    Avc(AvcConfiguration),
    Hevc(HvcConfiguration),
    /// Explicit observed framing; any strict ISO refusal remains attached.
    ObservedAvc(crate::NalObservedAvcConfiguration),
    ObservedHevc(crate::NalObservedHevcConfiguration),
    /// Explicit native passthrough mode. This is not Annex-B validation.
    Passthrough {
        codec: Codec,
        raw: Bytes,
    },
}
impl NalPacketConfiguration {
    pub fn bytes(&self) -> &Bytes {
        match self {
            Self::Avc(v) => v.source_bytes(),
            Self::Hevc(v) => v.source_bytes(),
            Self::ObservedAvc(v) => v.source_bytes(),
            Self::ObservedHevc(v) => v.source_bytes(),
            Self::Passthrough { raw, .. } => raw,
        }
    }
    pub fn codec(&self) -> Codec {
        match self {
            Self::Avc(_) | Self::ObservedAvc(_) => Codec::Avc,
            Self::Hevc(_) | Self::ObservedHevc(_) => Codec::Hevc,
            Self::Passthrough { codec, .. } => *codec,
        }
    }
    pub fn strict_source_refusal(&self) -> Option<Error> {
        self.compact_source_refusal().error()
    }
    pub(crate) fn compact_source_refusal(&self) -> NalConfigurationRefusal {
        match self {
            Self::ObservedAvc(v) if v.fields().length_size_minus_one() == 2 => {
                NalConfigurationRefusal::AvcWidthThree
            }
            Self::ObservedHevc(v) if v.fields().length_size_minus_one() == 2 => {
                NalConfigurationRefusal::HevcWidthThree
            }
            _ => NalConfigurationRefusal::None,
        }
    }
    pub(crate) fn avc_record(&self) -> Option<&AvcConfiguration> {
        match self {
            Self::Avc(v) => Some(v),
            Self::ObservedAvc(v) => Some(v.record()),
            _ => None,
        }
    }
    pub(crate) fn hevc_record(&self) -> Option<&HvcConfiguration> {
        match self {
            Self::Hevc(v) => Some(v),
            Self::ObservedHevc(v) => Some(v.record()),
            _ => None,
        }
    }
    /// Literal source identity, independently of a record-validity conclusion.
    pub fn source_identity(&self) -> Option<(SourceLocation, u64)> {
        if let Some(c) = self.avc_record() {
            let s = c.source();
            Some((
                SourceLocation {
                    source: s.source,
                    offset: s.offset,
                },
                s.generation,
            ))
        } else if let Some(c) = self.hevc_record() {
            let s = c.source();
            Some((
                SourceLocation {
                    source: s.source,
                    offset: s.offset,
                },
                s.generation,
            ))
        } else {
            None
        }
    }
    fn descriptor_storage_lower_bound(&self) -> Result<usize> {
        if let Some(c) = self.avc_record() {
            c.nals()
                .len()
                .checked_mul(size_of::<crate::AvcConfigNal>())
                .ok_or(Error::Overflow)
        } else if let Some(c) = self.hevc_record() {
            c.nals()
                .len()
                .checked_mul(size_of::<crate::HvcConfigNal>())
                .and_then(|n| {
                    c.arrays()
                        .len()
                        .checked_mul(size_of::<crate::HvcConfigArray>())
                        .and_then(|a| a.checked_add(n))
                })
                .ok_or(Error::Overflow)
        } else {
            Ok(0)
        }
    }
    fn width(&self) -> Option<usize> {
        self.avc_record()
            .map(|v| usize::from(v.sample_length_size().bytes()))
            .or_else(|| {
                self.hevc_record()
                    .map(|v| usize::from(v.length_size().bytes()))
            })
    }
}

/// These are the only strict-source configuration refusals carried by the
/// observed receiver. Raw configuration bytes/identity remain separate owners.
#[derive(Clone, Copy)]
pub(crate) enum NalConfigurationRefusal {
    None,
    AvcWidthThree,
    HevcWidthThree,
}
impl NalConfigurationRefusal {
    pub(crate) fn error(self) -> Option<Error> {
        match self {
            Self::None => None,
            Self::AvcWidthThree => Some(Error::Invalid("AVC three-byte sample length")),
            Self::HevcWidthThree => Some(Error::Unsupported("hvcC three-byte sample length")),
        }
    }
}

/// Effective receiver configuration viewed through one compact descriptor
/// table and one raw owner. Packet and initialization framing are identical to
/// [`NalPacketConfiguration`]; borrowed NAL inspection creates no per-NAL owner.
pub enum NalPacketViewConfiguration {
    Avc(crate::AvcConfigurationView),
    Hevc(crate::HvcConfigurationView),
    /// Explicit observed framing; any strict ISO refusal remains attached.
    ObservedAvc(crate::NalObservedAvcConfigurationView),
    ObservedHevc(crate::NalObservedHevcConfigurationView),
    /// Explicit native passthrough mode. This is not Annex-B validation.
    Passthrough {
        codec: Codec,
        raw: Bytes,
    },
}
impl NalPacketViewConfiguration {
    pub fn bytes(&self) -> &Bytes {
        PacketConfig::raw(self)
    }
    pub fn codec(&self) -> Codec {
        PacketConfig::codec_kind(self)
    }
    pub fn strict_source_refusal(&self) -> Option<Error> {
        PacketConfig::refusal(self).error()
    }
    /// Literal source identity, independently of a record-validity conclusion.
    pub fn source_identity(&self) -> Option<(SourceLocation, u64)> {
        PacketConfig::identity(self)
    }
    fn avc_view(&self) -> Option<&crate::AvcConfigurationView> {
        match self {
            Self::Avc(v) => Some(v),
            Self::ObservedAvc(v) => Some(v.view()),
            _ => None,
        }
    }
    fn hevc_view(&self) -> Option<&crate::HvcConfigurationView> {
        match self {
            Self::Hevc(v) => Some(v),
            Self::ObservedHevc(v) => Some(v.view()),
            _ => None,
        }
    }
}

/// One configuration NAL as the packet and initialization writers read it.
pub(crate) struct ConfigNal<'a> {
    /// AVC array kind; HEVC records select every NAL.
    pub(crate) kind: Option<AvcConfigArrayKind>,
    /// Eager records: the NAL's own owner. Views: the raw record, whose
    /// `range` addresses the NAL; compact segments read it through the range.
    pub(crate) owner: &'a Bytes,
    pub(crate) range: Range<usize>,
    /// True when `owner` is exactly the NAL payload (eager records).
    pub(crate) owner_is_payload: bool,
}

impl<'a> ConfigNal<'a> {
    /// The NAL payload. An eager owner is exactly that payload; a view owner is
    /// the raw record that the validated range addresses.
    pub(crate) fn payload(&self) -> &'a [u8] {
        if self.owner_is_payload {
            self.owner.as_ref()
        } else {
            // The parser validated this range against the same raw record.
            &self.owner[self.range.clone()]
        }
    }
}

/// Facts the shared packet/initialization state engines read from an effective
/// configuration. Both representations answer from the same parsed record.
pub(crate) trait PacketConfig {
    fn raw(&self) -> &Bytes;
    fn codec_kind(&self) -> Codec;
    /// Sample length width; None for explicit passthrough.
    fn length_width(&self) -> Option<usize>;
    fn refusal(&self) -> NalConfigurationRefusal;
    fn identity(&self) -> Option<(SourceLocation, u64)>;
    fn descriptor_lower_bound(&self) -> Result<usize>;
    /// Declared HEVC array type, or None past the last array or for non-HEVC.
    fn hevc_array_type(&self, index: usize) -> Option<u8>;
    /// Configuration NAL, or None past the last NAL or for passthrough.
    fn config_nal(&self, index: usize) -> Option<ConfigNal<'_>>;
    /// Number of configuration NALs; an upper bound on one injection.
    fn config_nal_count(&self) -> usize;
}
impl PacketConfig for NalPacketConfiguration {
    fn raw(&self) -> &Bytes {
        NalPacketConfiguration::bytes(self)
    }
    fn codec_kind(&self) -> Codec {
        NalPacketConfiguration::codec(self)
    }
    fn length_width(&self) -> Option<usize> {
        NalPacketConfiguration::width(self)
    }
    fn refusal(&self) -> NalConfigurationRefusal {
        NalPacketConfiguration::compact_source_refusal(self)
    }
    fn identity(&self) -> Option<(SourceLocation, u64)> {
        NalPacketConfiguration::source_identity(self)
    }
    fn descriptor_lower_bound(&self) -> Result<usize> {
        NalPacketConfiguration::descriptor_storage_lower_bound(self)
    }
    fn hevc_array_type(&self, index: usize) -> Option<u8> {
        self.hevc_record()?
            .arrays()
            .get(index)
            .map(super::hevc_config::HvcConfigArray::declared_nal_type)
    }
    fn config_nal(&self, index: usize) -> Option<ConfigNal<'_>> {
        if let Some(c) = self.avc_record() {
            c.nals().get(index).map(|n| ConfigNal {
                kind: Some(n.array_kind()),
                owner: n.source_bytes(),
                range: n.source_range(),
                owner_is_payload: true,
            })
        } else if let Some(c) = self.hevc_record() {
            c.nals().get(index).map(|n| ConfigNal {
                kind: None,
                owner: n.source_bytes(),
                range: n.source_range(),
                owner_is_payload: true,
            })
        } else {
            None
        }
    }
    fn config_nal_count(&self) -> usize {
        self.avc_record()
            .map(|c| c.nals().len())
            .or_else(|| self.hevc_record().map(|c| c.nals().len()))
            .unwrap_or(0)
    }
}
impl PacketConfig for NalPacketViewConfiguration {
    fn raw(&self) -> &Bytes {
        match self {
            Self::Avc(v) => v.source_bytes(),
            Self::Hevc(v) => v.source_bytes(),
            Self::ObservedAvc(v) => v.source_bytes(),
            Self::ObservedHevc(v) => v.source_bytes(),
            Self::Passthrough { raw, .. } => raw,
        }
    }
    fn codec_kind(&self) -> Codec {
        match self {
            Self::Avc(_) | Self::ObservedAvc(_) => Codec::Avc,
            Self::Hevc(_) | Self::ObservedHevc(_) => Codec::Hevc,
            Self::Passthrough { codec, .. } => *codec,
        }
    }
    fn length_width(&self) -> Option<usize> {
        self.avc_view()
            .map(|v| usize::from(v.sample_length_size().bytes()))
            .or_else(|| {
                self.hevc_view()
                    .map(|v| usize::from(v.length_size().bytes()))
            })
    }
    fn refusal(&self) -> NalConfigurationRefusal {
        match self {
            Self::ObservedAvc(v) if v.fields().length_size_minus_one() == 2 => {
                NalConfigurationRefusal::AvcWidthThree
            }
            Self::ObservedHevc(v) if v.fields().length_size_minus_one() == 2 => {
                NalConfigurationRefusal::HevcWidthThree
            }
            _ => NalConfigurationRefusal::None,
        }
    }
    fn identity(&self) -> Option<(SourceLocation, u64)> {
        if let Some(c) = self.avc_view() {
            let s = c.source();
            Some((
                SourceLocation {
                    source: s.source,
                    offset: s.offset,
                },
                s.generation,
            ))
        } else if let Some(c) = self.hevc_view() {
            let s = c.source();
            Some((
                SourceLocation {
                    source: s.source,
                    offset: s.offset,
                },
                s.generation,
            ))
        } else {
            None
        }
    }
    fn descriptor_lower_bound(&self) -> Result<usize> {
        if let Some(c) = self.avc_view() {
            c.descriptor_lower_bound()
        } else if let Some(c) = self.hevc_view() {
            c.descriptor_lower_bound()
        } else {
            Ok(0)
        }
    }
    fn hevc_array_type(&self, index: usize) -> Option<u8> {
        self.hevc_view()?
            .array(index)
            .map(|a| a.declared_nal_type())
    }
    fn config_nal(&self, index: usize) -> Option<ConfigNal<'_>> {
        if let Some(c) = self.avc_view() {
            c.nal(index).map(|n| ConfigNal {
                kind: Some(n.array_kind()),
                owner: c.source_bytes(),
                range: n.source_range(),
                owner_is_payload: false,
            })
        } else if let Some(c) = self.hevc_view() {
            c.nal(index).map(|n| ConfigNal {
                kind: None,
                owner: c.source_bytes(),
                range: n.source_range(),
                owner_is_payload: false,
            })
        } else {
            None
        }
    }
    fn config_nal_count(&self) -> usize {
        self.avc_view()
            .map(super::avc_config::AvcConfigurationView::nal_count)
            .or_else(|| {
                self.hevc_view()
                    .map(super::hevc_config::HvcConfigurationView::nal_count)
            })
            .unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct NalPacketLimits {
    pub maximum_packet_bytes: usize,
    pub maximum_output_bytes: usize,
    pub maximum_nals: usize,
    pub maximum_segments: usize,
}

#[derive(Clone, Copy, Debug)]
struct AvcState {
    sps: bool,
    pps: bool,
    new_idr: bool,
}
impl Default for AvcState {
    fn default() -> Self {
        Self {
            sps: false,
            pps: false,
            new_idr: true,
        }
    }
}

/// Source and native configuration lifetimes are separate. Failed/cancelled
/// jobs never publish partial packets or commit partial in-band state. A valid
/// AVC side-data update survives a later physical payload error, as an explicit
/// receiver fact; cancellation/resource refusal does not adopt that update.
pub struct NalPacketSession<C = NalPacketConfiguration> {
    pool: Budget,
    configuration: C,
    limits: NalPacketLimits,
    avc: AvcState,
    receiver_epoch: u64,
    drained: bool,
}
impl NalPacketSession {
    pub fn new(
        pool: &Budget,
        configuration: NalPacketConfiguration,
        limits: NalPacketLimits,
    ) -> Result<Self> {
        Self::with_configuration(pool, configuration, limits)
    }
}
impl NalPacketSession<NalPacketViewConfiguration> {
    /// The same receiver over a compact configuration view. Packet framing,
    /// work, validation and errors match [`NalPacketSession::new`].
    pub fn new_view(
        pool: &Budget,
        configuration: NalPacketViewConfiguration,
        limits: NalPacketLimits,
    ) -> Result<Self> {
        Self::with_configuration(pool, configuration, limits)
    }
    /// Observed receiver view with compact descriptors; the packet job is the
    /// same state engine as [`NalPacketSession::packet_observed_compact`].
    pub fn packet_observed_view<M>(
        &mut self,
        source: NalPacket<M>,
        update: Option<crate::NalObservedAvcConfigurationView>,
    ) -> Result<NalPacketViewJob<'_, M>> {
        self.begin_packet(
            source,
            update.map(NalPacketViewConfiguration::ObservedAvc),
            true,
        )
        .map(|inner| NalPacketViewJob { inner })
    }
}
impl<C> NalPacketSession<C> {
    fn with_configuration(pool: &Budget, configuration: C, limits: NalPacketLimits) -> Result<Self>
    where
        C: PacketConfig,
    {
        if !configuration.raw().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        if limits.maximum_nals == 0 || limits.maximum_segments == 0 {
            return Err(Error::Invalid("zero packet policy"));
        }
        Ok(Self {
            pool: pool.clone(),
            configuration,
            limits,
            avc: AvcState::default(),
            receiver_epoch: 0,
            drained: false,
        })
    }
}
impl<C> NalPacketSession<C> {
    pub fn configuration(&self) -> &C {
        &self.configuration
    }
    pub fn receiver_epoch(&self) -> u64 {
        self.receiver_epoch
    }
    /// No pending queue exists: the exclusive job borrow must end first.
    pub fn drain(&mut self) {
        self.drained = true;
    }
    /// Keeps the current configuration/epoch, clears in-band injection state.
    pub fn flush(&mut self) {
        self.avc = AvcState::default();
        self.drained = false;
    }
}
impl NalPacketSession {
    /// A nonempty AVC side-data record is parsed by the existing cooperative
    /// AvcConfigParser before this call. Exact raw equality is then work-bounded
    /// inside the job. HEVC retains but does not adopt the side-data record.
    pub fn packet<M>(
        &mut self,
        source: NalPacket<M>,
        avc_update: Option<AvcConfiguration>,
    ) -> Result<NalPacketJob<'_, M>> {
        self.begin_packet(source, avc_update.map(NalPacketConfiguration::Avc), false)
            .map(|inner| NalPacketJob { inner })
    }
    /// Separately typed native-framing side-data view. The source record error
    /// stays in the observed configuration and in every output/rejection.
    pub fn packet_observed<M>(
        &mut self,
        source: NalPacket<M>,
        update: Option<crate::NalObservedAvcConfiguration>,
    ) -> Result<NalPacketJob<'_, M>> {
        self.begin_packet(
            source,
            update.map(NalPacketConfiguration::ObservedAvc),
            true,
        )
        .map(|inner| NalPacketJob { inner })
    }
    /// The same framing operation with range descriptors instead of one Bytes
    /// clone per segment. The output still owns the complete packet and raw
    /// effective configuration; all packet work and validation are unchanged.
    pub fn packet_compact<M>(
        &mut self,
        source: NalPacket<M>,
        avc_update: Option<AvcConfiguration>,
    ) -> Result<NalPacketCompactJob<'_, M>> {
        self.begin_packet(source, avc_update.map(NalPacketConfiguration::Avc), false)
            .map(|inner| NalPacketCompactJob { inner })
    }
    /// Observed receiver view with compact descriptors, preserving both source
    /// refusals and the independently retained raw configuration facts.
    pub fn packet_observed_compact<M>(
        &mut self,
        source: NalPacket<M>,
        update: Option<crate::NalObservedAvcConfiguration>,
    ) -> Result<NalPacketCompactJob<'_, M>> {
        self.begin_packet(
            source,
            update.map(NalPacketConfiguration::ObservedAvc),
            true,
        )
        .map(|inner| NalPacketCompactJob { inner })
    }
}
impl<C> NalPacketSession<C> {
    fn begin_packet<M, S: PacketSegment>(
        &mut self,
        source: NalPacket<M>,
        avc_update: Option<C>,
        observed_packet: bool,
    ) -> Result<PacketJob<'_, M, S, C>>
    where
        C: PacketConfig,
    {
        if self.drained {
            return Err(Error::Invalid("packet after drain requires flush"));
        }
        if !source.bytes.belongs_to(&self.pool)
            || source
                .new_extradata
                .as_ref()
                .is_some_and(|v| !v.belongs_to(&self.pool))
            || avc_update
                .as_ref()
                .is_some_and(|v| !v.raw().belongs_to(&self.pool))
        {
            return Err(Error::ForeignPool);
        }
        if source.bytes.len() > self.limits.maximum_packet_bytes {
            return Err(Error::Limit {
                requested: source.bytes.len(),
                maximum: self.limits.maximum_packet_bytes,
            });
        }
        source
            .location
            .offset
            .checked_add(u64::try_from(source.bytes.len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        let is_avc = self.configuration.codec_kind() == Codec::Avc;
        let changes = is_avc && source.new_extradata.as_ref().is_some_and(|v| !v.is_empty());
        if changes != avc_update.is_some() {
            return Err(Error::Invalid("AVC update and literal side data disagree"));
        }
        let epoch = if changes {
            self.receiver_epoch.checked_add(1).ok_or(Error::Overflow)?
        } else {
            self.receiver_epoch
        };
        let avc = if changes {
            AvcState::default()
        } else {
            self.avc
        };
        // Both vectors are created before the session borrow moves into the job,
        // so no extra pool handle is cloned per packet.
        let segments = BudgetVec::new(&self.pool);
        let output = BudgetVec::new(&self.pool);
        Ok(PacketJob {
            session: self,
            source: Some(source),
            update: avc_update,
            avc,
            epoch,
            phase: Phase::Verify,
            verify: 0,
            update_verified: false,
            array: 0,
            offset: 0,
            length: 0,
            prefix_read: 0,
            nals: 0,
            nal: None,
            injection: None,
            injected_irap: false,
            has_irap: false,
            look_offset: 0,
            look_length: 0,
            look_prefix: 0,
            look_nals: 0,
            live_base: None,
            segment_hint: None,
            segments: Some(segments),
            output: Some(output),
            total: 0,
            copy_segment: 0,
            copied: 0,
            failure: None,
            receiver_failure: None,
            observed_packet,
            packet_source_refusal: None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NalPacketOrigin {
    Packet {
        source_range: Range<usize>,
    },
    Configuration {
        nal_index: usize,
        source_range: Range<usize>,
    },
}
#[derive(Clone, Debug)]
pub struct NalPacketSegment {
    bytes: Bytes,
    pub origin: NalPacketOrigin,
    pub prefix_bytes: u8,
    pub output_offset: usize,
}
impl NalPacketSegment {
    pub fn source_bytes(&self) -> &Bytes {
        &self.bytes
    }
}

/// Small progress result for publication into an empty caller-owned slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NalPacketProgress {
    Yield,
    Ready,
    Complete,
}

pub struct NalPacketOutput<M = NalPacketFields> {
    inner: PacketOutput<M, NalPacketSegment>,
}

/// An immutable source range. Its backing bytes are owned by the compact output,
/// rather than independently cloned into each descriptor.
#[derive(Clone, PartialEq, Eq)]
pub struct NalPacketRangeSegment {
    source_range: Range<usize>,
    // The high byte carries the existing source/prefix tag. The low24 bits
    // carry a configuration NAL index: avcC has at most31+255+255 NALs;
    // hvcC has at most255 arrays of65535 NALs. Packet descriptors use index0.
    // Source ranges and output offsets remain full usize values.
    nal_and_prefix: u32,
    output_offset: usize,
}
impl std::fmt::Debug for NalPacketRangeSegment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Preserve the existing public diagnostic fields after private packing.
        let nal_index = (self.nal_and_prefix & Self::NAL_INDEX_MASK) as usize;
        let prefix_and_source = (self.nal_and_prefix >> 24) as u8;
        formatter
            .debug_struct("NalPacketRangeSegment")
            .field("source_range", &self.source_range)
            .field("nal_index", &nal_index)
            .field("prefix_and_source", &prefix_and_source)
            .field("output_offset", &self.output_offset)
            .finish()
    }
}
impl NalPacketRangeSegment {
    const CONFIGURATION_SOURCE: u8 = 0x80;
    const NAL_INDEX_MASK: u32 = 0x00ff_ffff;
    pub fn origin(&self) -> NalPacketOrigin {
        if (self.nal_and_prefix >> 24) as u8 & Self::CONFIGURATION_SOURCE == 0 {
            NalPacketOrigin::Packet {
                source_range: self.source_range.clone(),
            }
        } else {
            NalPacketOrigin::Configuration {
                nal_index: (self.nal_and_prefix & Self::NAL_INDEX_MASK) as usize,
                source_range: self.source_range.clone(),
            }
        }
    }
    pub fn prefix_bytes(&self) -> u8 {
        (self.nal_and_prefix >> 24) as u8 & !Self::CONFIGURATION_SOURCE
    }
    pub fn output_offset(&self) -> usize {
        self.output_offset
    }
    fn range(&self) -> &Range<usize> {
        &self.source_range
    }
    fn owner<'a>(&self, packet: &'a Bytes, configuration: &'a Bytes) -> &'a Bytes {
        if (self.nal_and_prefix >> 24) as u8 & Self::CONFIGURATION_SOURCE == 0 {
            packet
        } else {
            configuration
        }
    }
}

/// Complete output with compact descriptors. Payload, source, side data and raw
/// effective configuration remain owned and charged after producer teardown.
/// Borrowed segment bytes cannot outlive that output:
/// ```compile_fail
/// use triergon_framing::NalPacketCompactOutput;
/// fn invalid(output: NalPacketCompactOutput) -> &'static [u8] {
///     output.segment_bytes(0).unwrap()
/// }
/// ```
pub struct NalPacketCompactOutput<M = NalPacketFields> {
    inner: PacketOutput<M, NalPacketRangeSegment>,
}

struct PacketOutput<M, S> {
    bytes: Bytes,
    source: NalPacket<M>,
    configuration: Bytes,
    // Stored unwrapped with a separate presence flag: the flag packs with the
    // other one-byte fields instead of adding an eight-byte Option tag to every
    // retained output.
    configuration_identity: (SourceLocation, u64),
    has_configuration_identity: bool,
    configuration_source_refusal: NalConfigurationRefusal,
    packet_source_refusal: Option<NalHeaderError>,
    segments: BudgetVec<S>,
    receiver_epoch: u64,
    forward_new_extradata: bool,
}

/// A physical complete-packet rejection observed at the pinned d32 receive
/// boundary. Resource, cancellation and strict fixed-header refusals are not
/// codec errors and do not map to one of these kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NalPacketReceiverErrorKind {
    IncompleteLengthPrefix,
    IncompleteNalPayload,
    EmptyAvcPacket,
    EmptyHevcNal,
}

/// A separately typed receiver view of a real failed packet operation. The
/// original source error, bytes, caller metadata and receiver configuration
/// remain available; this does not certify the source as valid.
pub struct NalPacketRejection<M = NalPacketFields> {
    kind: NalPacketReceiverErrorKind,
    error: Error,
    source: NalPacket<M>,
    configuration: Bytes,
    configuration_identity: Option<(SourceLocation, u64)>,
    configuration_source_refusal: NalConfigurationRefusal,
    packet_source_refusal: Option<NalHeaderError>,
    receiver_epoch: u64,
}
impl<M> NalPacketRejection<M> {
    pub fn kind(&self) -> NalPacketReceiverErrorKind {
        self.kind
    }
    pub fn source_error(&self) -> Error {
        self.error
    }
    /// Pinned public BSF AVERROR_INVALIDDATA, not an executed native call.
    pub fn native_receive_result(&self) -> i32 {
        -1_094_995_529
    }
    pub fn source(&self) -> &NalPacket<M> {
        &self.source
    }
    pub fn configuration_bytes(&self) -> &Bytes {
        &self.configuration
    }
    pub fn configuration_source_identity(&self) -> Option<(SourceLocation, u64)> {
        self.configuration_identity
    }
    pub fn configuration_source_refusal(&self) -> Option<Error> {
        self.configuration_source_refusal.error()
    }
    pub fn packet_source_refusal(&self) -> Option<Error> {
        self.packet_source_refusal.map(NalHeaderError::error)
    }
    pub fn receiver_epoch(&self) -> u64 {
        self.receiver_epoch
    }
}
impl<M> NalPacketOutput<M> {
    pub fn bytes(&self) -> &Bytes {
        &self.inner.bytes
    }
    pub fn source(&self) -> &NalPacket<M> {
        &self.inner.source
    }
    pub fn configuration_bytes(&self) -> &Bytes {
        &self.inner.configuration
    }
    pub fn configuration_source_identity(&self) -> Option<(SourceLocation, u64)> {
        self.inner
            .has_configuration_identity
            .then_some(self.inner.configuration_identity)
    }
    pub fn segments(&self) -> &[NalPacketSegment] {
        self.inner.segments.as_slice()
    }
    pub fn configuration_source_refusal(&self) -> Option<Error> {
        self.inner.configuration_source_refusal.error()
    }
    pub fn packet_source_refusal(&self) -> Option<Error> {
        self.inner.packet_source_refusal.map(NalHeaderError::error)
    }
    pub fn receiver_epoch(&self) -> u64 {
        self.inner.receiver_epoch
    }
    pub fn forward_new_extradata(&self) -> bool {
        self.inner.forward_new_extradata
    }
}
impl<M> NalPacketCompactOutput<M> {
    pub fn bytes(&self) -> &Bytes {
        &self.inner.bytes
    }
    pub fn source(&self) -> &NalPacket<M> {
        &self.inner.source
    }
    pub fn configuration_bytes(&self) -> &Bytes {
        &self.inner.configuration
    }
    pub fn configuration_source_identity(&self) -> Option<(SourceLocation, u64)> {
        self.inner
            .has_configuration_identity
            .then_some(self.inner.configuration_identity)
    }
    pub fn segments(&self) -> &[NalPacketRangeSegment] {
        self.inner.segments.as_slice()
    }
    pub fn configuration_source_refusal(&self) -> Option<Error> {
        self.inner.configuration_source_refusal.error()
    }
    pub fn packet_source_refusal(&self) -> Option<Error> {
        self.inner.packet_source_refusal.map(NalHeaderError::error)
    }
    pub fn receiver_epoch(&self) -> u64 {
        self.inner.receiver_epoch
    }
    pub fn forward_new_extradata(&self) -> bool {
        self.inner.forward_new_extradata
    }
    /// Borrow a segment payload through the output's retained packet/configuration
    /// owner. No Bytes clone or allocation occurs.
    pub fn segment_bytes(&self, index: usize) -> Option<&[u8]> {
        let part = self.inner.segments.as_slice().get(index)?;
        part.owner(&self.inner.source.bytes, &self.inner.configuration)
            .as_ref()
            .get(part.range().clone())
    }
    /// Explicit compatibility materialization of one owned segment. This clones
    /// the existing immutable owner; descriptor collection remains caller-owned.
    pub fn materialize_segment(&self, index: usize) -> Result<NalPacketSegment> {
        let part = self
            .inner
            .segments
            .as_slice()
            .get(index)
            .ok_or(Error::Invalid("packet segment index"))?;
        Ok(NalPacketSegment {
            bytes: part
                .owner(&self.inner.source.bytes, &self.inner.configuration)
                .slice(part.range().clone())?,
            origin: part.origin(),
            prefix_bytes: part.prefix_bytes(),
            output_offset: part.output_offset,
        })
    }
}

// A compile-time storage choice for one parser/state engine. Neither job nor
// output has a runtime storage tag, spare vector or inline segment array.
trait PacketSegment: Sized {
    type Output<M>;
    fn packet(source: &Bytes, range: Range<usize>, prefix: u8, offset: usize) -> Result<Self>;
    fn configuration(source: &Bytes, index: usize, range: Range<usize>, offset: usize) -> Self;
    fn passthrough(source: &Bytes, offset: usize) -> Self;
    fn len(&self) -> usize;
    fn prefix_bytes(&self) -> u8;
    fn payload<'a>(&'a self, packet: &'a Bytes, configuration: &'a Bytes) -> Result<&'a [u8]>;
    fn finish<M>(output: PacketOutput<M, Self>) -> Self::Output<M>;
}
impl PacketSegment for NalPacketSegment {
    type Output<M> = NalPacketOutput<M>;
    fn packet(source: &Bytes, range: Range<usize>, prefix: u8, offset: usize) -> Result<Self> {
        Ok(Self {
            bytes: source.slice(range.clone())?,
            origin: NalPacketOrigin::Packet {
                source_range: range,
            },
            prefix_bytes: prefix,
            output_offset: offset,
        })
    }
    fn configuration(source: &Bytes, index: usize, range: Range<usize>, offset: usize) -> Self {
        Self {
            bytes: source.clone(),
            origin: NalPacketOrigin::Configuration {
                nal_index: index,
                source_range: range,
            },
            prefix_bytes: 4,
            output_offset: offset,
        }
    }
    fn passthrough(source: &Bytes, offset: usize) -> Self {
        Self {
            bytes: source.clone(),
            origin: NalPacketOrigin::Packet {
                source_range: 0..source.len(),
            },
            prefix_bytes: 0,
            output_offset: offset,
        }
    }
    fn len(&self) -> usize {
        self.bytes.len()
    }
    fn prefix_bytes(&self) -> u8 {
        self.prefix_bytes
    }
    fn payload<'a>(&'a self, _: &'a Bytes, _: &'a Bytes) -> Result<&'a [u8]> {
        Ok(self.bytes.as_ref())
    }
    fn finish<M>(inner: PacketOutput<M, Self>) -> Self::Output<M> {
        NalPacketOutput { inner }
    }
}
impl PacketSegment for NalPacketRangeSegment {
    type Output<M> = NalPacketCompactOutput<M>;
    fn packet(_: &Bytes, range: Range<usize>, prefix: u8, offset: usize) -> Result<Self> {
        Ok(Self {
            source_range: range,
            nal_and_prefix: u32::from(prefix) << 24,
            output_offset: offset,
        })
    }
    fn configuration(_: &Bytes, index: usize, range: Range<usize>, offset: usize) -> Self {
        // Called only for an entry in the parsed avcC/hvcC descriptor slice.
        // Their literal count fields bound every index below255*65535<2^24;
        // this does not narrow source ranges, output lengths or packet offsets.
        debug_assert!(index < usize::from(u8::MAX) * usize::from(u16::MAX));
        Self {
            source_range: range,
            nal_and_prefix: (index as u32) | (u32::from(Self::CONFIGURATION_SOURCE | 4) << 24),
            output_offset: offset,
        }
    }
    fn passthrough(source: &Bytes, offset: usize) -> Self {
        Self {
            source_range: 0..source.len(),
            nal_and_prefix: 0,
            output_offset: offset,
        }
    }
    fn len(&self) -> usize {
        self.range().end - self.range().start
    }
    fn prefix_bytes(&self) -> u8 {
        NalPacketRangeSegment::prefix_bytes(self)
    }
    fn payload<'a>(&'a self, packet: &'a Bytes, configuration: &'a Bytes) -> Result<&'a [u8]> {
        self.owner(packet, configuration)
            .as_ref()
            .get(self.range().clone())
            .ok_or(Error::Invalid("packet segment range"))
    }
    fn finish<M>(inner: PacketOutput<M, Self>) -> Self::Output<M> {
        NalPacketCompactOutput { inner }
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Verify,
    Arrays,
    Lookahead,
    Scan,
    Inject,
    Nal,
    Reserve,
    Copy,
    Freeze,
    Complete,
}
#[derive(Clone, Copy)]
enum Injection {
    All,
    Sps,
    Pps,
}
struct PendingNal {
    range: Range<usize>,
    kind: u8,
}
pub struct NalPacketJob<'a, M = NalPacketFields> {
    inner: PacketJob<'a, M, NalPacketSegment, NalPacketConfiguration>,
}
/// The compact descriptor variant of the same cooperative packet job.
pub struct NalPacketCompactJob<'a, M = NalPacketFields> {
    inner: PacketJob<'a, M, NalPacketRangeSegment, NalPacketConfiguration>,
}
/// The compact descriptor job over a configuration view.
pub struct NalPacketViewJob<'a, M = NalPacketFields> {
    inner: PacketJob<'a, M, NalPacketRangeSegment, NalPacketViewConfiguration>,
}
struct PacketJob<'a, M, S: PacketSegment, C> {
    session: &'a mut NalPacketSession<C>,
    source: Option<NalPacket<M>>,
    update: Option<C>,
    avc: AvcState,
    epoch: u64,
    phase: Phase,
    verify: usize,
    update_verified: bool,
    array: usize,
    offset: usize,
    length: usize,
    prefix_read: usize,
    nals: usize,
    nal: Option<PendingNal>,
    injection: Option<(Injection, usize)>,
    injected_irap: bool,
    has_irap: bool,
    look_offset: usize,
    look_length: usize,
    look_prefix: usize,
    look_nals: usize,
    /// Job-invariant part of `minimum_live_bytes`: control storage, both
    /// descriptor lower bounds and the deduplicated raw owners. None of these
    /// change while segments are appended, so the sum is computed once.
    live_base: Option<Result<usize>>,
    /// Segment count known before the first append: width-framed HEVC after its
    /// lookahead, or a packet whose first NAL is also its last. Only sizes the
    /// first allocation; a refused reservation falls back to ordinary growth.
    segment_hint: Option<usize>,
    segments: Option<BudgetVec<S>>,
    output: Option<BudgetVec<u8>>,
    total: usize,
    copy_segment: usize,
    copied: usize,
    failure: Option<Error>,
    receiver_failure: Option<NalPacketReceiverErrorKind>,
    observed_packet: bool,
    packet_source_refusal: Option<NalHeaderError>,
}
impl<M, S: PacketSegment, C: PacketConfig> PacketJob<'_, M, S, C> {
    fn config(&self) -> &C {
        self.update.as_ref().unwrap_or(&self.session.configuration)
    }
    pub fn source(&self) -> Option<&NalPacket<M>> {
        self.source.as_ref()
    }
    /// Move the original failed source into an observed receiver-error view.
    /// Only the physical error sites below can enable this transition.
    /// The job's source error remains sticky after the one-time move.
    pub fn take_receiver_rejection(&mut self) -> Option<NalPacketRejection<M>> {
        let kind = self.receiver_failure?;
        let error = self.failure?;
        let source = self.source.take()?;
        Some(NalPacketRejection {
            kind,
            error,
            source,
            configuration: self.session.configuration.raw().clone(),
            configuration_identity: self.session.configuration.identity(),
            configuration_source_refusal: self.session.configuration.refusal(),
            packet_source_refusal: self.packet_source_refusal,
            receiver_epoch: self.session.receiver_epoch,
        })
    }
    // Known owner/capacity lower bound, deduplicating raw caller aliases. A
    // larger allocator replacement peak may still require retryable pressure.
    // All terms are non-negative, so summing the job-invariant terms first
    // overflows exactly when the original left-to-right sum overflowed.
    fn minimum_live_bytes(&mut self, segments: usize, output: usize) -> Result<usize> {
        let base = match self.live_base {
            Some(base) => base,
            None => *self.live_base.insert(self.fixed_live_bytes()),
        }?;
        let copied_storage = if self.config().length_width().is_some() {
            Bytes::owner_overhead_bytes()
                .checked_add(output)
                .ok_or(Error::Overflow)?
        } else {
            0
        };
        segments
            .checked_mul(size_of::<S>())
            .and_then(|p| p.checked_add(copied_storage))
            .and_then(|n| n.checked_add(base))
            .ok_or(Error::Overflow)
    }
    fn fixed_live_bytes(&self) -> Result<usize> {
        let packet = self
            .source
            .as_ref()
            .ok_or(Error::Invalid("missing packet source"))?;
        let mut total = Budget::control_storage_bytes()
            .checked_add(self.session.configuration.descriptor_lower_bound()?)
            .ok_or(Error::Overflow)?;
        if let Some(c) = &self.update {
            total = total
                .checked_add(c.descriptor_lower_bound()?)
                .ok_or(Error::Overflow)?;
        }
        let owners = [
            Some(self.session.configuration.raw()),
            self.update.as_ref().map(C::raw),
            Some(&packet.bytes),
            packet.new_extradata.as_ref(),
        ];
        for (i, owner) in owners.iter().enumerate() {
            if let Some(b) = owner
                && !owners[..i]
                    .iter()
                    .flatten()
                    .any(|old| old.shares_storage_with(b))
            {
                total = total
                    .checked_add(b.retained_storage_bytes())
                    .ok_or(Error::Overflow)?;
            }
        }
        Ok(total)
    }
    fn append(&mut self, segment: S) -> Result<()> {
        let parts = self
            .segments
            .as_ref()
            .ok_or(Error::Invalid("missing packet plan"))?;
        if parts.len() == self.session.limits.maximum_segments {
            return Err(Error::Limit {
                requested: parts.len().saturating_add(1),
                maximum: self.session.limits.maximum_segments,
            });
        }
        let count = parts.len().checked_add(1).ok_or(Error::Overflow)?;
        let total = self
            .total
            .checked_add(usize::from(segment.prefix_bytes()))
            .and_then(|v| v.checked_add(segment.len()))
            .ok_or(Error::Overflow)?;
        if total > self.session.limits.maximum_output_bytes {
            return Err(Error::Limit {
                requested: total,
                maximum: self.session.limits.maximum_output_bytes,
            });
        }
        let minimum = self.minimum_live_bytes(count, total)?;
        if minimum > self.session.pool.limit() {
            return Err(Error::Limit {
                requested: minimum,
                maximum: self.session.pool.limit(),
            });
        }
        let parts = self
            .segments
            .as_mut()
            .ok_or(Error::Invalid("missing packet plan"))?;
        if let Some(planned) = self.segment_hint.take().filter(|_| parts.capacity() == 0) {
            // Exact first allocation instead of the default four slots. Any
            // refusal leaves the vector empty and try_push applies its own
            // growth and errors exactly as before.
            let _ = parts.try_reserve_exact(planned);
        }
        parts.try_push(segment).map_err(|e| e.error)?;
        self.total = total;
        Ok(())
    }
    /// Compatibility adapter over the same state engine used by `step_into`.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<S::Output<M>>> {
        let mut ready = None;
        match self.step_into(work, cancel, &mut ready)? {
            NalPacketProgress::Yield => Ok(BuildStep::Yield),
            NalPacketProgress::Complete => Ok(BuildStep::Complete),
            NalPacketProgress::Ready => Ok(BuildStep::Ready(
                ready.ok_or(Error::Invalid("missing published packet"))?,
            )),
        }
    }
    /// Publish a complete output directly into an empty caller-owned slot.
    /// Occupied slots are rejected before any job, work or owner change and do
    /// not poison the job. Yield, completion and errors leave the slot empty.
    pub fn step_into(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        ready: &mut Option<S::Output<M>>,
    ) -> Result<NalPacketProgress> {
        if ready.is_some() {
            return Err(Error::Invalid("packet output sink occupied"));
        }
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = self.advance_into(work, cancel, ready);
        if let Err(e) = result
            && !matches!(e, Error::Backpressure { .. } | Error::Allocation)
        {
            // A valid side-data configuration precedes payload parsing in
            // the pinned receiver. Its change survives a physical packet
            // error, while partial in-band availability never commits.
            if matches!(e, Error::Invalid(_) | Error::Truncated)
                && self.update_verified
                && let Some(configuration) = self.update.take()
            {
                self.session.configuration = configuration;
                self.session.receiver_epoch = self.epoch;
                self.session.avc = AvcState::default();
            }
            self.failure = Some(e);
            self.output = None;
            self.segments = None;
        }
        result
    }
}

impl<M> NalPacketJob<'_, M> {
    pub fn source(&self) -> Option<&NalPacket<M>> {
        self.inner.source()
    }
    pub fn take_receiver_rejection(&mut self) -> Option<NalPacketRejection<M>> {
        self.inner.take_receiver_rejection()
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<NalPacketOutput<M>>> {
        self.inner.step(work, cancel)
    }
    /// The sink must be empty. An occupied sink is rejected without job,
    /// owner, work or cancellation side effects; all other semantics match step.
    pub fn step_into(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        ready: &mut Option<NalPacketOutput<M>>,
    ) -> Result<NalPacketProgress> {
        self.inner.step_into(work, cancel, ready)
    }
}

impl<M> NalPacketCompactJob<'_, M> {
    pub fn source(&self) -> Option<&NalPacket<M>> {
        self.inner.source()
    }
    pub fn take_receiver_rejection(&mut self) -> Option<NalPacketRejection<M>> {
        self.inner.take_receiver_rejection()
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<NalPacketCompactOutput<M>>> {
        self.inner.step(work, cancel)
    }
    /// The sink must be empty. An occupied sink is rejected without job,
    /// owner, work or cancellation side effects; all other semantics match step.
    pub fn step_into(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        ready: &mut Option<NalPacketCompactOutput<M>>,
    ) -> Result<NalPacketProgress> {
        self.inner.step_into(work, cancel, ready)
    }
}

impl<M> NalPacketViewJob<'_, M> {
    pub fn source(&self) -> Option<&NalPacket<M>> {
        self.inner.source()
    }
    pub fn take_receiver_rejection(&mut self) -> Option<NalPacketRejection<M>> {
        self.inner.take_receiver_rejection()
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<NalPacketCompactOutput<M>>> {
        self.inner.step(work, cancel)
    }
    /// The sink must be empty. An occupied sink is rejected without job,
    /// owner, work or cancellation side effects; all other semantics match step.
    pub fn step_into(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        ready: &mut Option<NalPacketCompactOutput<M>>,
    ) -> Result<NalPacketProgress> {
        self.inner.step_into(work, cancel, ready)
    }
}

// It exercises the existing private constructor, not a new test-only API.
#[cfg(test)]
mod packed_plan_boundary_guards {
    use super::*;

    #[test]
    fn every_configuration_index_boundary_preserves_full_source_ranges() {
        let pool = Budget::new(4096).unwrap();
        let raw = Bytes::copy_from_slice(&pool, &[0x40, 1]).unwrap();
        // A hvcC has at most255 arrays with65535 NALs each. The last possible
        // index is checked without allocating a multi-gigabyte record.
        for index in [0, 31, 255, 65_535, 255 * 65_535 - 1] {
            let range = usize::MAX - 1..usize::MAX;
            let part = <NalPacketRangeSegment as PacketSegment>::configuration(
                &raw,
                index,
                range.clone(),
                usize::MAX,
            );
            assert_eq!(
                part.origin(),
                NalPacketOrigin::Configuration {
                    nal_index: index,
                    source_range: range,
                }
            );
            assert_eq!(part.prefix_bytes(), 4);
            assert_eq!(part.output_offset(), usize::MAX);
            assert_eq!(
                format!("{part:?}"),
                format!(
                    "NalPacketRangeSegment {{ source_range: {}..{}, nal_index: {index}, prefix_and_source: 132, output_offset: {} }}",
                    usize::MAX - 1,
                    usize::MAX,
                    usize::MAX,
                )
            );
        }
    }

    #[test]
    fn packet_tags_keep_all_prefix_values_and_full_offsets() {
        let pool = Budget::new(4096).unwrap();
        let raw = Bytes::copy_from_slice(&pool, &[0x41]).unwrap();
        for prefix in [0, 3, 4] {
            let part =
                <NalPacketRangeSegment as PacketSegment>::packet(&raw, 0..1, prefix, usize::MAX)
                    .unwrap();
            assert_eq!(
                part.origin(),
                NalPacketOrigin::Packet { source_range: 0..1 }
            );
            assert_eq!(part.prefix_bytes(), prefix);
            assert_eq!(part.output_offset(), usize::MAX);
            assert_eq!(part.payload(&raw, &raw).unwrap(), &[0x41]);
            assert_eq!(
                format!("{part:?}"),
                format!(
                    "NalPacketRangeSegment {{ source_range: 0..1, nal_index: 0, prefix_and_source: {prefix}, output_offset: {} }}",
                    usize::MAX,
                )
            );
        }
    }
}
