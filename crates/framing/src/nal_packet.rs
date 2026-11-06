//! Complete-packet, source-preserving projection of the pinned d32 public BSFs.
//! NAL bodies stay opaque; this does not establish access-unit/decoder validity.
use crate::header::NalHeaderError;
use crate::{AvcConfigArrayKind, AvcConfiguration, BuildStep, Codec, HvcConfiguration, NalHeader};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, Result, SourceId, SourceLocation, WorkBudget,
};

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
            .map(|a| a.declared_nal_type())
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
            .map(|c| c.nal_count())
            .or_else(|| self.hevc_view().map(|c| c.nal_count()))
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
/// codec errors and do not acquire one of these receipts.
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
    /// Only the physical error sites below can authorize this transition.
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
    fn advance_into(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        ready: &mut Option<S::Output<M>>,
    ) -> Result<NalPacketProgress> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if matches!(self.phase, Phase::Complete) {
                return Ok(NalPacketProgress::Complete);
            }
            match self.phase {
                Phase::Verify => {
                    if let Some(update) = &self.update {
                        let extra = self
                            .source
                            .as_ref()
                            .and_then(|s| s.new_extradata.as_ref())
                            .ok_or(Error::Invalid("missing source update"))?;
                        let raw = update.raw();
                        if extra.len() != raw.len() {
                            return Err(Error::Invalid("configuration update bytes differ"));
                        }
                        if self.verify < raw.len() {
                            let count = (raw.len() - self.verify)
                                .min(4096)
                                .min(usize::try_from(work.remaining()).unwrap_or(usize::MAX));
                            if count == 0 {
                                return Ok(NalPacketProgress::Yield);
                            }
                            if extra[self.verify..self.verify + count]
                                != raw[self.verify..self.verify + count]
                            {
                                return Err(Error::Invalid("configuration update bytes differ"));
                            }
                            work.take(count as u64);
                            self.verify += count;
                            continue;
                        }
                    }
                    self.update_verified = true;
                    self.phase = Phase::Arrays;
                }
                Phase::Arrays => {
                    if let Some(declared) = self.config().hevc_array_type(self.array) {
                        if !work.try_charge(1) {
                            return Ok(NalPacketProgress::Yield);
                        }
                        if !matches!(declared, 32 | 33 | 34 | 39 | 40) {
                            return Err(Error::Unsupported("pinned HEVC decoder array type"));
                        }
                        self.array += 1;
                        continue;
                    }
                    self.phase = if self.config().codec_kind() == Codec::Hevc
                        && self.config().length_width().is_some()
                    {
                        Phase::Lookahead
                    } else {
                        Phase::Scan
                    };
                }
                Phase::Lookahead => {
                    let source = self
                        .source
                        .as_ref()
                        .ok_or(Error::Invalid("missing packet source"))?;
                    let width = self
                        .config()
                        .length_width()
                        .ok_or(Error::Invalid("missing packet width"))?;
                    if self.look_offset == source.bytes.len() && self.look_prefix == 0 {
                        self.phase = Phase::Scan;
                        continue;
                    }
                    if self.look_prefix < width {
                        let count = (width - self.look_prefix)
                            .min(usize::try_from(work.remaining()).unwrap_or(usize::MAX))
                            .min(source.bytes.len().saturating_sub(self.look_offset));
                        if count == 0 {
                            // Missing input still consumes the old failed-read
                            // token; exhausted work yields before that refusal.
                            if !work.try_charge(1) {
                                return Ok(NalPacketProgress::Yield);
                            }
                            self.receiver_failure =
                                Some(NalPacketReceiverErrorKind::IncompleteLengthPrefix);
                            return Err(Error::Truncated);
                        }
                        work.take(count as u64);
                        // Width is at most four. Every reachable accumulated
                        // prefix fits usize on the supported32/64-bit targets.
                        for &byte in &source.bytes[self.look_offset..self.look_offset + count] {
                            // A prefix is at most four bytes, so each partial value
                            // fits usize on supported32/64-bit targets.
                            self.look_length = (self.look_length << 8) | usize::from(byte);
                        }
                        self.look_offset += count;
                        self.look_prefix += count;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(NalPacketProgress::Yield);
                    }
                    if self.look_length == 0 {
                        self.receiver_failure = Some(NalPacketReceiverErrorKind::EmptyHevcNal);
                        return Err(Error::Invalid("pinned HEVC empty NAL"));
                    }
                    if self.look_nals == self.session.limits.maximum_nals {
                        return Err(Error::Limit {
                            requested: self.look_nals.saturating_add(1),
                            maximum: self.session.limits.maximum_nals,
                        });
                    }
                    let end = self
                        .look_offset
                        .checked_add(self.look_length)
                        .ok_or(Error::Overflow)?;
                    if end > source.bytes.len() {
                        self.receiver_failure =
                            Some(NalPacketReceiverErrorKind::IncompleteNalPayload);
                        return Err(Error::Truncated);
                    }
                    let kind = match NalHeader::parse_compact(
                        Codec::Hevc,
                        &source.bytes[self.look_offset..end],
                    ) {
                        Ok(h) => h.nal_unit_type(),
                        Err(e) if self.observed_packet => {
                            self.packet_source_refusal.get_or_insert(e);
                            (source.bytes[self.look_offset] >> 1) & 63
                        }
                        Err(e) => return Err(e.error()),
                    };
                    self.has_irap |= (16..=23).contains(&kind);
                    self.look_offset = end;
                    self.look_prefix = 0;
                    self.look_length = 0;
                    self.look_nals += 1;
                }
                Phase::Scan => {
                    let source = self
                        .source
                        .as_ref()
                        .ok_or(Error::Invalid("missing packet source"))?;
                    let Some(width) = self.config().length_width() else {
                        if !work.try_charge(1) {
                            return Ok(NalPacketProgress::Yield);
                        }
                        self.segment_hint = Some(1);
                        self.append(S::passthrough(&source.bytes, self.total))?;
                        self.phase = Phase::Reserve;
                        continue;
                    };
                    if self.offset == 0
                        && source.bytes.is_empty()
                        && self.config().codec_kind() == Codec::Avc
                    {
                        self.receiver_failure = Some(NalPacketReceiverErrorKind::EmptyAvcPacket);
                        return Err(Error::Invalid("pinned AVC empty packet"));
                    }
                    if self.offset == source.bytes.len() && self.prefix_read == 0 {
                        self.phase = Phase::Reserve;
                        continue;
                    }
                    if self.prefix_read < width {
                        let count = (width - self.prefix_read)
                            .min(usize::try_from(work.remaining()).unwrap_or(usize::MAX))
                            .min(source.bytes.len().saturating_sub(self.offset));
                        if count == 0 {
                            // Missing input still consumes the old failed-read
                            // token; exhausted work yields before that refusal.
                            if !work.try_charge(1) {
                                return Ok(NalPacketProgress::Yield);
                            }
                            self.receiver_failure =
                                Some(NalPacketReceiverErrorKind::IncompleteLengthPrefix);
                            return Err(Error::Truncated);
                        }
                        work.take(count as u64);
                        // Width is at most four. Every reachable accumulated
                        // prefix fits usize on the supported32/64-bit targets.
                        for &byte in &source.bytes[self.offset..self.offset + count] {
                            // A prefix is at most four bytes, so each partial value
                            // fits usize on supported32/64-bit targets.
                            self.length = (self.length << 8) | usize::from(byte);
                        }
                        self.offset += count;
                        self.prefix_read += count;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(NalPacketProgress::Yield);
                    }
                    let end = self
                        .offset
                        .checked_add(self.length)
                        .ok_or(Error::Overflow)?;
                    if end > source.bytes.len() {
                        self.receiver_failure =
                            Some(NalPacketReceiverErrorKind::IncompleteNalPayload);
                        return Err(Error::Truncated);
                    }
                    if self.length == 0 {
                        if self.config().codec_kind() == Codec::Hevc {
                            self.receiver_failure = Some(NalPacketReceiverErrorKind::EmptyHevcNal);
                            return Err(Error::Invalid("pinned HEVC empty NAL"));
                        }
                        self.prefix_read = 0;
                        continue;
                    }
                    if self.nals == self.session.limits.maximum_nals {
                        return Err(Error::Limit {
                            requested: self.nals.saturating_add(1),
                            maximum: self.session.limits.maximum_nals,
                        });
                    }
                    let codec = self.config().codec_kind();
                    let kind = if codec == Codec::Hevc {
                        // Width-framed HEVC reaches Scan only after Lookahead
                        // accepted every header or retained the first observed
                        // refusal. The exclusively borrowed source is immutable.
                        (source.bytes[self.offset] >> 1) & 63
                    } else {
                        match NalHeader::parse_compact(codec, &source.bytes[self.offset..end]) {
                            Ok(header) => header.nal_unit_type(),
                            Err(e) if self.observed_packet => {
                                self.packet_source_refusal.get_or_insert(e);
                                match codec {
                                    Codec::Avc => source.bytes[self.offset] & 31,
                                    Codec::Hevc => (source.bytes[self.offset] >> 1) & 63,
                                }
                            }
                            Err(e) => return Err(e.error()),
                        }
                    };
                    let mut inject = None;
                    if codec == Codec::Hevc {
                        if self.has_irap && matches!(kind,16..=23|32..=34) && !self.injected_irap {
                            inject = Some(Injection::All);
                        }
                    } else {
                        match kind {
                            7 => {
                                self.avc.sps = true;
                                self.avc.new_idr = true;
                            }
                            8 => {
                                self.avc.pps = true;
                                self.avc.new_idr = true;
                                if !self.avc.sps {
                                    inject = Some(Injection::Sps);
                                }
                            }
                            5 => {
                                let first = *source
                                    .bytes
                                    .get(self.offset + 1)
                                    .filter(|_| self.length > 1)
                                    .ok_or(Error::Truncated)?
                                    & 128
                                    != 0;
                                if first {
                                    self.avc.new_idr = true;
                                }
                                if self.avc.new_idr && !self.avc.pps {
                                    inject = Some(if self.avc.sps {
                                        Injection::Pps
                                    } else {
                                        Injection::All
                                    });
                                }
                                self.avc.new_idr = false;
                            }
                            1 => {
                                self.avc = AvcState::default();
                            }
                            _ => {}
                        }
                    }
                    self.nal = Some(PendingNal {
                        range: self.offset..end,
                        kind,
                    });
                    self.nals += 1;
                    if self.nals == 1 {
                        let injected =
                            if inject.is_some() || (codec == Codec::Hevc && self.has_irap) {
                                self.config().config_nal_count()
                            } else {
                                0
                            };
                        if codec == Codec::Hevc && self.look_nals > 0 {
                            // Lookahead counted every packet NAL; an IRAP packet
                            // injects the whole configuration exactly once.
                            self.segment_hint = Some(self.look_nals + injected);
                        } else if end == source.bytes.len() {
                            self.segment_hint = Some(1 + injected);
                        }
                    }
                    if let Some(i) = inject {
                        self.injection = Some((i, 0));
                        self.phase = Phase::Inject;
                    } else {
                        self.phase = Phase::Nal;
                    }
                }
                Phase::Inject => {
                    let (which, index) = self
                        .injection
                        .ok_or(Error::Invalid("missing injection plan"))?;
                    if !work.try_charge(1) {
                        return Ok(NalPacketProgress::Yield);
                    }
                    let next = self.config().config_nal(index).map(|n| {
                        let selected = match (n.kind, which) {
                            (None, _) => true,
                            (Some(kind), Injection::All) => {
                                kind != AvcConfigArrayKind::SequenceExtension
                            }
                            (Some(kind), Injection::Sps) => kind == AvcConfigArrayKind::Sequence,
                            (Some(kind), Injection::Pps) => kind == AvcConfigArrayKind::Picture,
                        };
                        (
                            selected,
                            S::configuration(n.owner, index, n.range, self.total),
                        )
                    });
                    if let Some((selected, segment)) = next {
                        if selected {
                            self.append(segment)?;
                        }
                        self.injection = Some((which, index + 1));
                    } else {
                        if matches!(which, Injection::Sps) {
                            self.avc.sps = true;
                        }
                        self.injection = None;
                        self.injected_irap = true;
                        self.phase = Phase::Nal;
                    }
                }
                Phase::Nal => {
                    if !work.try_charge(1) {
                        return Ok(NalPacketProgress::Yield);
                    }
                    let nal = self
                        .nal
                        .as_ref()
                        .ok_or(Error::Invalid("missing source NAL"))?;
                    let prefix = if self.config().codec_kind() == Codec::Hevc
                        || matches!(nal.kind, 7 | 8)
                        || self.total == 0
                    {
                        4
                    } else {
                        3
                    };
                    let range = nal.range.clone();
                    let end = range.end;
                    let source = &self
                        .source
                        .as_ref()
                        .ok_or(Error::Invalid("missing packet"))?
                        .bytes;
                    let segment = S::packet(source, range, prefix, self.total)?;
                    self.append(segment)?;
                    self.offset = end;
                    self.length = 0;
                    self.prefix_read = 0;
                    self.nal = None;
                    self.phase = Phase::Scan;
                }
                Phase::Reserve => {
                    if !work.try_charge(1) {
                        return Ok(NalPacketProgress::Yield);
                    }
                    // A passthrough packet already has its immutable payload
                    // owner. Keep that owner and its charge instead of copying.
                    if self.config().length_width().is_none() {
                        self.phase = Phase::Freeze;
                        continue;
                    }
                    let minimum = self
                        .total
                        .checked_add(Bytes::owner_overhead_bytes())
                        .and_then(|v| v.checked_add(Budget::control_storage_bytes()))
                        .ok_or(Error::Overflow)?;
                    if minimum > self.session.pool.limit() {
                        return Err(Error::Limit {
                            requested: minimum,
                            maximum: self.session.pool.limit(),
                        });
                    }
                    self.output
                        .as_mut()
                        .ok_or(Error::Invalid("missing packet buffer"))?
                        .try_reserve_exact(self.total)?;
                    self.phase = Phase::Copy;
                }
                Phase::Copy => {
                    // One unit per pass exactly as the outer loop runs it: the
                    // cancellation check precedes every unit after the first, and
                    // every yield, charge and error is unchanged. Only the phase
                    // dispatch between units is skipped.
                    let parts = self
                        .segments
                        .as_ref()
                        .ok_or(Error::Invalid("missing packet plan"))?;
                    let mut first = true;
                    loop {
                        if !first && cancel.load(Ordering::Relaxed) {
                            return Err(Error::Cancelled);
                        }
                        first = false;
                        let Some(p) = parts.as_slice().get(self.copy_segment) else {
                            self.phase = Phase::Freeze;
                            break;
                        };
                        let prefix = usize::from(p.prefix_bytes());
                        let n = prefix.checked_add(p.len()).ok_or(Error::Overflow)?;
                        if self.copied == n {
                            if !work.try_charge(1) {
                                return Ok(NalPacketProgress::Yield);
                            }
                            self.copy_segment += 1;
                            self.copied = 0;
                            continue;
                        }
                        let count = if self.copied < prefix {
                            prefix - self.copied
                        } else {
                            (n - self.copied).min(4096)
                        }
                        .min(usize::try_from(work.remaining()).unwrap_or(usize::MAX));
                        if count == 0 {
                            return Ok(NalPacketProgress::Yield);
                        }
                        let b = self
                            .output
                            .as_mut()
                            .ok_or(Error::Invalid("missing packet buffer"))?;
                        if self.copied < prefix {
                            let start = 4 - prefix + self.copied;
                            b.try_extend_from_slice(&[0, 0, 0, 1][start..start + count])?;
                        } else {
                            let start = self.copied - prefix;
                            let source = &self
                                .source
                                .as_ref()
                                .ok_or(Error::Invalid("missing packet source"))?
                                .bytes;
                            let configuration = self
                                .update
                                .as_ref()
                                .unwrap_or(&self.session.configuration)
                                .raw();
                            let payload = p.payload(source, configuration)?;
                            b.try_extend_from_slice(&payload[start..start + count])?;
                        }
                        work.take(count as u64);
                        self.copied += count;
                    }
                }
                Phase::Freeze => {
                    if !work.try_charge(1) {
                        return Ok(NalPacketProgress::Yield);
                    }
                    let b = self
                        .output
                        .take()
                        .ok_or(Error::Invalid("missing packet buffer"))?;
                    let bytes = if self.config().length_width().is_none() {
                        drop(b);
                        self.source
                            .as_ref()
                            .ok_or(Error::Invalid("missing packet source"))?
                            .bytes
                            .clone()
                    } else {
                        match b.freeze() {
                            Ok(v) => v,
                            Err(e) => {
                                self.output = Some(e.buffer);
                                return Err(e.error);
                            }
                        }
                    };
                    let configuration = self.config().raw().clone();
                    let configuration_identity = self.config().identity();
                    let configuration_source_refusal = self.config().refusal();
                    let forward_new_extradata = self.config().codec_kind() == Codec::Hevc
                        || self
                            .source
                            .as_ref()
                            .and_then(|s| s.new_extradata.as_ref())
                            .is_some_and(Bytes::is_empty);
                    if let Some(c) = self.update.take() {
                        self.session.configuration = c;
                    }
                    self.session.avc = self.avc;
                    self.session.receiver_epoch = self.epoch;
                    self.phase = Phase::Complete;
                    *ready = Some(S::finish(PacketOutput {
                        bytes,
                        source: self
                            .source
                            .take()
                            .ok_or(Error::Invalid("missing packet source"))?,
                        configuration,
                        configuration_identity: configuration_identity.unwrap_or((
                            SourceLocation {
                                source: SourceId(0),
                                offset: 0,
                            },
                            0,
                        )),
                        has_configuration_identity: configuration_identity.is_some(),
                        configuration_source_refusal,
                        packet_source_refusal: self.packet_source_refusal,
                        segments: self
                            .segments
                            .take()
                            .ok_or(Error::Invalid("missing packet plan"))?,
                        receiver_epoch: self.epoch,
                        forward_new_extradata,
                    }));
                    return Ok(NalPacketProgress::Ready);
                }
                Phase::Complete => return Ok(NalPacketProgress::Complete),
            }
        }
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

// This identical test module is appended to both RED and candidate nal_packet.rs.
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
