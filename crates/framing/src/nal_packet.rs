//! Complete-packet, source-preserving projection of the reference public BSFs
//! named on [`crate::compat::nal`].
//! NAL bodies stay opaque; this does not establish access-unit/decoder validity.
mod phases;
use phases::AvcBufferingLookahead;

use crate::header::NalHeaderError;
use crate::{AvcConfigArrayKind, AvcConfiguration, BuildStep, Codec, HvcConfiguration, LengthSize};
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
    /// Receiver acceptance ([`crate::compat::nal`]); any strict ISO refusal remains attached.
    ReceiverAvc(crate::compat::nal::ReceiverAvcConfiguration),
    ReceiverHevc(crate::compat::nal::ReceiverHevcConfiguration),
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
            Self::ReceiverAvc(v) => v.source_bytes(),
            Self::ReceiverHevc(v) => v.source_bytes(),
            Self::Passthrough { raw, .. } => raw,
        }
    }
    pub fn codec(&self) -> Codec {
        match self {
            Self::Avc(_) | Self::ReceiverAvc(_) => Codec::Avc,
            Self::Hevc(_) | Self::ReceiverHevc(_) => Codec::Hevc,
            Self::Passthrough { codec, .. } => *codec,
        }
    }
    pub fn strict_source_refusal(&self) -> Option<Error> {
        self.refusal().error()
    }
    pub(crate) fn refusal(&self) -> NalConfigurationRefusal {
        match self {
            Self::ReceiverAvc(v) => v.refusal(),
            Self::ReceiverHevc(v) if v.fields().length_size_minus_one() == 2 => {
                NalConfigurationRefusal::HevcWidthThree
            }
            _ => NalConfigurationRefusal::None,
        }
    }
    fn avc_record(&self) -> Option<&AvcConfiguration> {
        match self {
            Self::Avc(v) => Some(v),
            Self::ReceiverAvc(v) => Some(v.record()),
            _ => None,
        }
    }
    fn hevc_record(&self) -> Option<&HvcConfiguration> {
        match self {
            Self::Hevc(v) => Some(v),
            Self::ReceiverHevc(v) => Some(v.record()),
            _ => None,
        }
    }
    /// Literal source identity, independently of a record-validity conclusion.
    pub fn source_identity(&self) -> Option<(SourceLocation, u64)> {
        let (source, offset, generation) = if let Some(c) = self.avc_record() {
            let s = c.source();
            (s.source, s.offset, s.generation)
        } else {
            let s = self.hevc_record()?.source();
            (s.source, s.offset, s.generation)
        };
        Some((SourceLocation { source, offset }, generation))
    }
    /// Element-count lower bound for live descriptor storage.
    pub(crate) fn descriptor_lower_bound(&self) -> Result<usize> {
        if let Some(c) = self.avc_record() {
            c.descriptor_lower_bound()
        } else if let Some(c) = self.hevc_record() {
            c.descriptor_lower_bound()
        } else {
            Ok(0)
        }
    }
    /// Sample length width; None for explicit passthrough.
    pub(crate) fn width(&self) -> Option<LengthSize> {
        self.avc_record()
            .map(AvcConfiguration::sample_length_size)
            .or_else(|| self.hevc_record().map(HvcConfiguration::length_size))
    }
    /// Charge the array checks already established by the record parser.
    /// A refusal still consumes its own unit and is exposed only when reached.
    pub(crate) fn admit_hevc_arrays(
        &self,
        checked: &mut usize,
        work: &mut WorkBudget,
    ) -> std::result::Result<bool, u8> {
        let Some(record) = self.hevc_record() else {
            return Ok(true);
        };
        let (limit, refused) = record.first_unsupported_array().map_or_else(
            || (record.array_count(), None),
            |(index, kind)| (usize::from(index), Some(kind)),
        );
        *checked += work.take((limit - *checked) as u64) as usize;
        if *checked < limit {
            return Ok(false);
        }
        match refused {
            Some(kind) if work.try_charge(1) => Err(kind),
            Some(_) => Ok(false),
            None => Ok(true),
        }
    }
    /// Configuration NAL, or None past the last NAL or for passthrough.
    pub(crate) fn config_nal(&self, index: usize) -> Option<ConfigNal<'_>> {
        if let Some(c) = self.avc_record() {
            c.nal(index).map(|n| ConfigNal {
                kind: Some(n.array_kind()),
                storage: c.source_bytes(),
                range: n.source_range(),
            })
        } else {
            let c = self.hevc_record()?;
            c.nal(index).map(|n| ConfigNal {
                kind: None,
                storage: c.source_bytes(),
                range: n.source_range(),
            })
        }
    }
    /// Number of configuration NALs; an upper bound on one injection.
    pub(crate) fn config_nal_count(&self) -> usize {
        self.avc_record()
            .map(AvcConfiguration::nal_count)
            .or_else(|| self.hevc_record().map(HvcConfiguration::nal_count))
            .unwrap_or(0)
    }
    /// The materialized configuration size and the sizing walk's work charge.
    pub(crate) fn annex_b_configuration_extent(&self) -> Option<(usize, u64)> {
        if let Some(record) = self.avc_record() {
            Some((
                record.annex_b_configuration_len(),
                record.nal_count() as u64 + 1,
            ))
        } else if let Some(record) = self.hevc_record() {
            Some((
                record.annex_b_configuration_len()?,
                record.nal_count() as u64 + 1,
            ))
        } else {
            Some((self.bytes().len(), 2))
        }
    }
}

/// These are the only strict-source configuration refusals carried by the
/// observed receiver. Raw configuration bytes/identity remain separate owners.
#[derive(Clone, Copy)]
pub(crate) enum NalConfigurationRefusal {
    None,
    AvcWidthThree,
    AvcExtensionTruncated,
    HevcWidthThree,
}
impl NalConfigurationRefusal {
    pub(crate) fn error(self) -> Option<Error> {
        match self {
            Self::None => None,
            Self::AvcWidthThree => Some(Error::Invalid("AVC three-byte sample length")),
            Self::AvcExtensionTruncated => Some(Error::Truncated),
            Self::HevcWidthThree => Some(Error::Unsupported("hvcC three-byte sample length")),
        }
    }
}

// Packet side data can replace only an AVC record. Keep its source/receiver
// distinction without reserving space for HEVC or passthrough configurations.
enum AvcUpdate {
    Source(AvcConfiguration),
    Receiver(crate::compat::nal::ReceiverAvcConfiguration),
}
impl AvcUpdate {
    fn record(&self) -> &AvcConfiguration {
        match self {
            Self::Source(record) => record,
            Self::Receiver(record) => record.record(),
        }
    }
    fn bytes(&self) -> &Bytes {
        self.record().source_bytes()
    }
    fn refusal(&self) -> NalConfigurationRefusal {
        match self {
            Self::Receiver(record) => record.refusal(),
            Self::Source(_) => NalConfigurationRefusal::None,
        }
    }
    fn install(self, configuration: &mut NalPacketConfiguration) {
        match self {
            Self::Source(record) => *configuration = NalPacketConfiguration::Avc(record),
            Self::Receiver(record) => {
                *configuration = NalPacketConfiguration::ReceiverAvc(record);
            }
        }
    }
}

// Borrow the active configuration without widening an in-flight AVC update.
enum ConfigurationRef<'a> {
    Current(&'a NalPacketConfiguration),
    Update(&'a AvcUpdate),
}
impl<'a> ConfigurationRef<'a> {
    fn bytes(self) -> &'a Bytes {
        match self {
            Self::Current(configuration) => configuration.bytes(),
            Self::Update(update) => update.bytes(),
        }
    }
    fn source_identity(self) -> Option<(SourceLocation, u64)> {
        match self {
            Self::Current(configuration) => configuration.source_identity(),
            Self::Update(update) => {
                let source = update.record().source();
                Some((
                    SourceLocation {
                        source: source.source,
                        offset: source.offset,
                    },
                    source.generation,
                ))
            }
        }
    }
    fn refusal(self) -> NalConfigurationRefusal {
        match self {
            Self::Current(configuration) => configuration.refusal(),
            Self::Update(update) => update.refusal(),
        }
    }
    fn config_nal(self, index: usize) -> Option<ConfigNal<'a>> {
        match self {
            Self::Current(configuration) => configuration.config_nal(index),
            Self::Update(update) => {
                let record = update.record();
                record.nal(index).map(|nal| ConfigNal {
                    kind: Some(nal.array_kind()),
                    storage: record.source_bytes(),
                    range: nal.source_range(),
                })
            }
        }
    }
    fn config_nal_count(self) -> usize {
        match self {
            Self::Current(configuration) => configuration.config_nal_count(),
            Self::Update(update) => update.record().nal_count(),
        }
    }
}

/// One configuration NAL as the packet and initialization writers read it.
pub(crate) struct ConfigNal<'a> {
    /// AVC array kind; HEVC records select every NAL.
    pub(crate) kind: Option<AvcConfigArrayKind>,
    /// The raw record, whose `range` addresses the NAL.
    pub(crate) storage: &'a Bytes,
    pub(crate) range: Range<usize>,
}

impl<'a> ConfigNal<'a> {
    /// The NAL payload. The parser validated its range against the same record.
    pub(crate) fn payload(&self) -> &'a [u8] {
        &self.storage[self.range.clone()]
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
    injected_for_buffering_period: bool,
}
impl Default for AvcState {
    fn default() -> Self {
        Self {
            sps: false,
            pps: false,
            new_idr: true,
            injected_for_buffering_period: false,
        }
    }
}

/// Source and native configuration lifetimes are separate. Failed/cancelled
/// jobs never publish partial packets or commit partial in-band state. A valid
/// AVC side-data update survives a later physical payload error, as an explicit
/// receiver fact; cancellation/resource refusal does not adopt that update.
pub struct NalPacketSession {
    pool: Budget,
    configuration: NalPacketConfiguration,
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
        if !configuration.bytes().belongs_to(pool) {
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
    pub fn configuration(&self) -> &NalPacketConfiguration {
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
    /// A nonempty AVC side-data record is parsed by the existing cooperative
    /// AvcConfigParser before this call. Exact raw equality is then work-bounded
    /// inside the job. HEVC retains but does not adopt the side-data record.
    pub fn packet<M>(
        &mut self,
        source: NalPacket<M>,
        avc_update: Option<AvcConfiguration>,
    ) -> Result<NalPacketJob<'_, M>> {
        self.begin_packet(source, avc_update.map(AvcUpdate::Source), false)
    }
    /// Frames a packet with the receiver's acceptance ([`crate::compat::nal`]):
    /// a NAL header the strict path refuses is framed and its refusal recorded.
    /// The source record error stays in the receiver configuration and in every
    /// output/rejection.
    pub fn receiver_packet<M>(
        &mut self,
        source: NalPacket<M>,
        update: Option<crate::compat::nal::ReceiverAvcConfiguration>,
    ) -> Result<NalPacketJob<'_, M>> {
        self.begin_packet(source, update.map(AvcUpdate::Receiver), true)
    }
    fn begin_packet<M>(
        &mut self,
        source: NalPacket<M>,
        avc_update: Option<AvcUpdate>,
        observed_packet: bool,
    ) -> Result<NalPacketJob<'_, M>> {
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
                .is_some_and(|v| !v.bytes().belongs_to(&self.pool))
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
        let codec = self.configuration.codec();
        let changes =
            codec == Codec::Avc && source.new_extradata.as_ref().is_some_and(|v| !v.is_empty());
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
        let length_width = match &avc_update {
            Some(update) => Some(update.record().sample_length_size()),
            None => self.configuration.width(),
        };
        let phase = if changes {
            PlanPhase::Verify
        } else if codec == Codec::Hevc {
            PlanPhase::Arrays
        } else {
            PlanPhase::Scan
        };
        Ok(NalPacketJob {
            state: PacketState {
                session: self,
                source: Some(source),
                update: avc_update,
                codec,
                length_width,
                avc,
                epoch,
                update_verified: false,
                segments: PacketSegmentStorage::new(),
                total: 0,
                failure: None,
                receiver_failure: None,
                observed_packet,
                packet_source_refusal: None,
            },
            phase: PacketPhase::Prepare(PacketPlan::new(phase)),
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
/// Small progress result for publication into an empty caller-owned slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NalPacketProgress {
    Yield,
    Ready,
    Complete,
}

/// An immutable source range. Its backing bytes are owned by the output,
/// rather than independently cloned into each descriptor.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct NalPacketSegment {
    source_start: usize,
    // Length-framed packet NALs have at most four length bytes; configuration
    // NALs have two. Passthrough keeps its unbounded length in the offset word.
    payload_length: u32,
    // The high byte carries the existing source/prefix tag. The low24 bits
    // carry a configuration NAL index: avcC has at most31+255+255 NALs;
    // hvcC has at most255 arrays of65535 NALs. Packet descriptors use index0.
    // Source ranges and output offsets remain full usize values.
    nal_and_prefix: u32,
    output_offset_or_length: usize,
}
// Report the unchanged public range and offset fields instead of their packed storage.
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for NalPacketSegment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Preserve the existing public diagnostic fields after private packing.
        let nal_index = (self.nal_and_prefix & Self::NAL_INDEX_MASK) as usize;
        let prefix_and_source = (self.nal_and_prefix >> 24) as u8 & !Self::PASSTHROUGH;
        formatter
            .debug_struct("NalPacketSegment")
            .field("source_range", &self.source_range())
            .field("nal_index", &nal_index)
            .field("prefix_and_source", &prefix_and_source)
            .field("output_offset", &self.output_offset())
            .finish()
    }
}
impl NalPacketSegment {
    const CONFIGURATION_SOURCE: u8 = 0x80;
    const PASSTHROUGH: u8 = 0x40;
    const NAL_INDEX_MASK: u32 = 0x00ff_ffff;
    fn packet(range: Range<usize>, prefix: u8, offset: usize) -> Self {
        debug_assert!(u32::try_from(range.len()).is_ok());
        Self {
            source_start: range.start,
            payload_length: range.len() as u32,
            nal_and_prefix: u32::from(prefix) << 24,
            output_offset_or_length: offset,
        }
    }
    fn configuration(index: usize, range: Range<usize>, offset: usize) -> Self {
        // Called only for an entry in the parsed avcC/hvcC descriptor slice.
        // Their literal count fields bound every index below255*65535<2^24;
        // this does not narrow source ranges, output lengths or packet offsets.
        debug_assert!(index < usize::from(u8::MAX) * usize::from(u16::MAX));
        debug_assert!(u16::try_from(range.len()).is_ok());
        Self {
            source_start: range.start,
            payload_length: range.len() as u32,
            nal_and_prefix: (index as u32) | (u32::from(Self::CONFIGURATION_SOURCE | 4) << 24),
            output_offset_or_length: offset,
        }
    }
    fn passthrough(length: usize) -> Self {
        // Passthrough publishes exactly one whole-packet segment at offset zero.
        Self {
            source_start: 0,
            payload_length: 0,
            nal_and_prefix: u32::from(Self::PASSTHROUGH) << 24,
            output_offset_or_length: length,
        }
    }
    pub fn origin(&self) -> NalPacketOrigin {
        if (self.nal_and_prefix >> 24) as u8 & Self::CONFIGURATION_SOURCE == 0 {
            NalPacketOrigin::Packet {
                source_range: self.source_range(),
            }
        } else {
            NalPacketOrigin::Configuration {
                nal_index: (self.nal_and_prefix & Self::NAL_INDEX_MASK) as usize,
                source_range: self.source_range(),
            }
        }
    }
    pub fn prefix_bytes(&self) -> u8 {
        (self.nal_and_prefix >> 24) as u8 & !(Self::CONFIGURATION_SOURCE | Self::PASSTHROUGH)
    }
    pub fn output_offset(&self) -> usize {
        if self.is_passthrough() {
            0
        } else {
            self.output_offset_or_length
        }
    }
    fn len(&self) -> usize {
        if self.is_passthrough() {
            self.output_offset_or_length
        } else {
            self.payload_length as usize
        }
    }
    fn source_range(&self) -> Range<usize> {
        self.source_start..self.source_start + self.len()
    }
    fn is_passthrough(&self) -> bool {
        (self.nal_and_prefix >> 24) as u8 & Self::PASSTHROUGH != 0
    }
    fn backing<'a>(&self, packet: &'a Bytes, configuration: &'a Bytes) -> &'a Bytes {
        if (self.nal_and_prefix >> 24) as u8 & Self::CONFIGURATION_SOURCE == 0 {
            packet
        } else {
            configuration
        }
    }
    fn payload<'a>(&self, backing: &'a Bytes) -> Result<&'a [u8]> {
        backing
            .as_ref()
            .get(self.source_range())
            .ok_or(Error::Invalid("packet segment range"))
    }
}

const INLINE_PACKET_SEGMENTS: usize = 6;

// Fixed descriptor metadata travels with the job/output. Only a heap table
// allocates; its complete capacity stays charged through output retention.
enum PacketSegmentStorage {
    Inline {
        data: [NalPacketSegment; INLINE_PACKET_SEGMENTS],
        len: u8,
    },
    Heap(BudgetVec<NalPacketSegment>),
}
impl PacketSegmentStorage {
    fn new() -> Self {
        Self::Inline {
            data: std::array::from_fn(|_| NalPacketSegment::packet(0..0, 0, 0)),
            len: 0,
        }
    }
    fn len(&self) -> usize {
        match self {
            Self::Inline { len, .. } => usize::from(*len),
            Self::Heap(parts) => parts.len(),
        }
    }
    fn as_slice(&self) -> &[NalPacketSegment] {
        match self {
            Self::Inline { data, len } => &data[..usize::from(*len)],
            Self::Heap(parts) => parts.as_slice(),
        }
    }
    fn required_heap_bytes(count: usize) -> Result<usize> {
        if count <= INLINE_PACKET_SEGMENTS {
            Ok(0)
        } else {
            count
                .checked_mul(size_of::<NalPacketSegment>())
                .ok_or(Error::Overflow)
        }
    }
    fn try_push(
        &mut self,
        pool: &Budget,
        planned: Option<usize>,
        segment: NalPacketSegment,
    ) -> Result<()> {
        match self {
            Self::Inline { data, len } if usize::from(*len) < INLINE_PACKET_SEGMENTS => {
                data[usize::from(*len)] = segment;
                *len += 1;
            }
            Self::Inline { data, .. } => {
                *self = Self::Heap(Self::spill(pool, planned, data, segment)?);
            }
            Self::Heap(parts) => parts.try_push(segment).map_err(|e| e.error)?,
        }
        Ok(())
    }
    #[cold]
    #[inline(never)]
    fn spill(
        pool: &Budget,
        planned: Option<usize>,
        data: &[NalPacketSegment; INLINE_PACKET_SEGMENTS],
        segment: NalPacketSegment,
    ) -> Result<BudgetVec<NalPacketSegment>> {
        let mut parts = BudgetVec::new(pool);
        if let Some(planned) = planned {
            // As with the old first allocation, a refused hint falls back to
            // ordinary growth. The inline entries remain intact on any error.
            let _ = parts.try_reserve_exact(planned);
        }
        let needed = INLINE_PACKET_SEGMENTS + 1;
        if parts.capacity() < needed {
            // Try the first heap capacity, then the exact size if necessary.
            parts
                .try_reserve_exact(needed.next_power_of_two())
                .or_else(|error| {
                    if matches!(
                        error,
                        Error::Backpressure { .. } | Error::Limit { .. } | Error::Overflow
                    ) {
                        parts.try_reserve_exact(needed)
                    } else {
                        Err(error)
                    }
                })?;
        }
        parts.try_extend_from_slice(data)?;
        parts.try_push(segment).map_err(|e| e.error)?;
        Ok(parts)
    }
}

/// Complete output. Payload, source, side data and raw effective configuration
/// remain owned and charged after producer teardown. Borrowed segment bytes
/// cannot outlive that output:
/// ```compile_fail
/// use triergon_framing::NalPacketOutput;
/// fn invalid(output: NalPacketOutput) -> &'static [u8] {
///     output.segment_bytes(0).unwrap()
/// }
/// ```
pub struct NalPacketOutput<M = NalPacketFields> {
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
    segments: PacketSegmentStorage,
    receiver_epoch: u64,
    forward_new_extradata: bool,
}

/// A physical complete-packet rejection observed at the reference receive
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
    /// The reference filter's `AVERROR_INVALIDDATA`, not an executed native call.
    /// Comparison-only.
    #[cfg(feature = "reference-trace")]
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
        &self.bytes
    }
    pub fn source(&self) -> &NalPacket<M> {
        &self.source
    }
    pub fn configuration_bytes(&self) -> &Bytes {
        &self.configuration
    }
    pub fn configuration_source_identity(&self) -> Option<(SourceLocation, u64)> {
        self.has_configuration_identity
            .then_some(self.configuration_identity)
    }
    pub fn segments(&self) -> &[NalPacketSegment] {
        self.segments.as_slice()
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
    pub fn forward_new_extradata(&self) -> bool {
        self.forward_new_extradata
    }
    /// Borrow a segment payload through the output's retained packet/configuration
    /// owner. No Bytes clone or allocation occurs.
    pub fn segment_bytes(&self, index: usize) -> Option<&[u8]> {
        let part = self.segments.as_slice().get(index)?;
        part.backing(&self.source.bytes, &self.configuration)
            .as_ref()
            .get(part.source_range())
    }
    /// One segment's payload as its own handle on the retained packet or
    /// configuration owner. No bytes are copied.
    pub fn retain_segment_bytes(&self, index: usize) -> Result<Bytes> {
        let part = self
            .segments
            .as_slice()
            .get(index)
            .ok_or(Error::Invalid("packet segment index"))?;
        part.backing(&self.source.bytes, &self.configuration)
            .slice(part.source_range())
    }
}

#[derive(Clone, Copy)]
enum PlanPhase {
    Verify,
    Arrays,
    Lookahead,
    AvcBuffering,
    AvcBufferingInject,
    Scan,
    Inject,
    Nal,
    Reserve,
}
#[derive(Clone, Copy)]
enum EmissionPhase {
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

// Source, effective update and descriptors coexist through packet copying and
// publication. Only the preparation scratch can share space with the buffer.
struct PacketState<'a, M> {
    session: &'a mut NalPacketSession,
    source: Option<NalPacket<M>>,
    update: Option<AvcUpdate>,
    codec: Codec,
    length_width: Option<LengthSize>,
    avc: AvcState,
    epoch: u64,
    update_verified: bool,
    segments: PacketSegmentStorage,
    total: usize,
    failure: Option<Error>,
    receiver_failure: Option<NalPacketReceiverErrorKind>,
    observed_packet: bool,
    packet_source_refusal: Option<NalHeaderError>,
}
struct PacketPlan {
    phase: PlanPhase,
    // Verification, array admission, lookahead and scanning run in order.
    // The cursor is reset before its next role; source offsets stay full width.
    offset: usize,
    length: usize,
    prefix_read: usize,
    nals: usize,
    nal: Option<u8>,
    injection: Option<(Injection, usize)>,
    injected_irap: bool,
    has_irap: bool,
    avc_buffering: Option<AvcBufferingLookahead>,
    look_nals: usize,
    // No fixed storage can be smaller than the budget control allocation.
    // The two smaller values represent an unchecked and a proven bound.
    live_base: usize,
    // A hint sizes a later spill without reserving while descriptors fit inline.
    segment_hint: Option<usize>,
}
impl PacketPlan {
    const UNCHECKED_STORAGE: usize = 0;
    const LIMITS_FIT: usize = 1;

    fn new(phase: PlanPhase) -> Self {
        Self {
            phase,
            offset: 0,
            length: 0,
            prefix_read: 0,
            nals: 0,
            nal: None,
            injection: None,
            injected_irap: false,
            has_irap: false,
            avc_buffering: None,
            look_nals: 0,
            live_base: Self::UNCHECKED_STORAGE,
            segment_hint: None,
        }
    }
}
struct PacketEmission {
    phase: EmissionPhase,
    output: Option<BudgetVec<u8>>,
    copy_segment: usize,
    copied: usize,
}
impl PacketEmission {
    fn new() -> Self {
        Self {
            phase: EmissionPhase::Reserve,
            output: None,
            copy_segment: 0,
            copied: 0,
        }
    }
}
enum PacketPhase {
    Prepare(PacketPlan),
    Emit(PacketEmission),
}

/// One cooperative packet framing job over the session it borrows.
pub struct NalPacketJob<'a, M = NalPacketFields> {
    state: PacketState<'a, M>,
    phase: PacketPhase,
}
impl<M> PacketState<'_, M> {
    fn config(&self) -> ConfigurationRef<'_> {
        match &self.update {
            Some(update) => ConfigurationRef::Update(update),
            None => ConfigurationRef::Current(&self.session.configuration),
        }
    }
    fn source(&self) -> Option<&NalPacket<M>> {
        self.source.as_ref()
    }
    /// Move the original failed source into an observed receiver-error view.
    /// Only the physical error sites below can enable this transition.
    /// The job's source error remains sticky after the one-time move.
    fn take_receiver_rejection(&mut self) -> Option<NalPacketRejection<M>> {
        let kind = self.receiver_failure?;
        let error = self.failure?;
        let source = self.source.take()?;
        Some(NalPacketRejection {
            kind,
            error,
            source,
            configuration: self.session.configuration.bytes().clone(),
            configuration_identity: self.session.configuration.source_identity(),
            configuration_source_refusal: self.session.configuration.refusal(),
            packet_source_refusal: self.packet_source_refusal,
            receiver_epoch: self.session.receiver_epoch,
        })
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
                .checked_add(c.record().descriptor_lower_bound()?)
                .ok_or(Error::Overflow)?;
        }
        let owners = [
            Some(self.session.configuration.bytes()),
            self.update.as_ref().map(AvcUpdate::bytes),
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
}

impl PacketPlan {
    fn added_storage(count: usize, bytes: usize, copying: bool) -> Result<usize> {
        let output = if copying {
            Bytes::owner_overhead_bytes()
                .checked_add(bytes)
                .ok_or(Error::Overflow)?
        } else {
            0
        };
        PacketSegmentStorage::required_heap_bytes(count)?
            .checked_add(output)
            .ok_or(Error::Overflow)
    }

    fn establish_storage_bound<M>(&mut self, state: &PacketState<'_, M>) -> Result<()> {
        let limits = state.session.limits;
        let maximum = Self::added_storage(
            limits.maximum_segments,
            limits.maximum_output_bytes,
            state.length_width.is_some(),
        );
        // The job's retained source and configuration remain charged to this
        // pool. Its fixed storage is therefore at most the current live charge.
        // If both logical ceilings fit in the remaining capacity, they bound
        // every future append. Actual reservations still check current pressure.
        self.live_base = if maximum.is_ok_and(|bytes| bytes <= state.session.pool.available()) {
            Self::LIMITS_FIT
        } else {
            state.fixed_live_bytes()?
        };
        Ok(())
    }

    // Known storage capacity lower bound, deduplicating raw caller aliases. A
    // larger allocator replacement peak may still require retryable pressure.
    // All terms are non-negative, so summing the job-invariant terms first
    // overflows exactly when the original left-to-right sum overflowed.
    #[cold]
    #[inline(never)]
    fn check_storage_bound<M>(
        &mut self,
        state: &PacketState<'_, M>,
        segments: usize,
        output: usize,
    ) -> Result<()> {
        if self.live_base == Self::UNCHECKED_STORAGE {
            self.establish_storage_bound(state)?;
        }
        if self.live_base == Self::LIMITS_FIT {
            return Ok(());
        }
        let minimum = self
            .live_base
            .checked_add(Self::added_storage(
                segments,
                output,
                state.length_width.is_some(),
            )?)
            .ok_or(Error::Overflow)?;
        if minimum > state.session.pool.limit() {
            return Err(Error::Limit {
                requested: minimum,
                maximum: state.session.pool.limit(),
            });
        }
        Ok(())
    }
    fn append<M>(
        &mut self,
        state: &mut PacketState<'_, M>,
        segment: NalPacketSegment,
    ) -> Result<()> {
        let parts = state.segments.len();
        if parts == state.session.limits.maximum_segments {
            return Err(Error::Limit {
                requested: parts.saturating_add(1),
                maximum: state.session.limits.maximum_segments,
            });
        }
        let count = parts.checked_add(1).ok_or(Error::Overflow)?;
        let total = state
            .total
            .checked_add(usize::from(segment.prefix_bytes()))
            .and_then(|v| v.checked_add(segment.len()))
            .ok_or(Error::Overflow)?;
        if total > state.session.limits.maximum_output_bytes {
            return Err(Error::Limit {
                requested: total,
                maximum: state.session.limits.maximum_output_bytes,
            });
        }
        if self.live_base != Self::LIMITS_FIT {
            self.check_storage_bound(state, count, total)?;
        }
        state
            .segments
            .try_push(&state.session.pool, self.segment_hint, segment)?;
        state.total = total;
        Ok(())
    }
}

impl<M> NalPacketJob<'_, M> {
    pub fn source(&self) -> Option<&NalPacket<M>> {
        self.state.source()
    }
    /// Move the original failed source into an observed receiver-error view.
    /// The source error remains sticky after the one-time move.
    pub fn take_receiver_rejection(&mut self) -> Option<NalPacketRejection<M>> {
        self.state.take_receiver_rejection()
    }
    /// The same state engine as `step_into`, returning the output directly.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<NalPacketOutput<M>>> {
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
    #[inline(never)]
    pub fn step_into(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        ready: &mut Option<NalPacketOutput<M>>,
    ) -> Result<NalPacketProgress> {
        if ready.is_some() {
            return Err(Error::Invalid("packet output sink occupied"));
        }
        if let Some(e) = self.state.failure {
            return Err(e);
        }
        let result = self.advance_into(work, cancel, ready);
        if let Err(e) = result
            && !matches!(e, Error::Backpressure { .. } | Error::Allocation)
        {
            self.fail(e);
        }
        result
    }
    #[cold]
    #[inline(never)]
    fn fail(&mut self, e: Error) {
        // A valid side-data configuration precedes payload parsing in
        // the pinned receiver. Its change survives a physical packet
        // error, while partial in-band availability never commits.
        if matches!(e, Error::Invalid(_) | Error::Truncated)
            && self.state.update_verified
            && let Some(configuration) = self.state.update.take()
        {
            configuration.install(&mut self.state.session.configuration);
            self.state.session.receiver_epoch = self.state.epoch;
            self.state.session.avc = AvcState::default();
        }
        self.state.failure = Some(e);
        if let PacketPhase::Emit(emission) = &mut self.phase {
            emission.output = None;
        }
        self.state.segments = PacketSegmentStorage::new();
    }
}

// It exercises the existing private constructor, not a new test-only API.
#[cfg(test)]
mod packed_plan_boundary_guards {
    use super::*;

    #[test]
    fn maximum_wire_length_preserves_full_source_and_output_offsets() {
        let length = u32::MAX as usize;
        let range = usize::MAX - length..usize::MAX;
        let part = NalPacketSegment::packet(range.clone(), 4, usize::MAX);
        assert_eq!(
            part.origin(),
            NalPacketOrigin::Packet {
                source_range: range
            }
        );
        assert_eq!(part.len(), length);
        assert_eq!(part.output_offset(), usize::MAX);
        assert_eq!(part.prefix_bytes(), 4);
    }

    #[test]
    fn passthrough_length_is_not_limited_to_the_wire_length_field() {
        let part = NalPacketSegment::passthrough(usize::MAX);
        assert_eq!(
            part.origin(),
            NalPacketOrigin::Packet {
                source_range: 0..usize::MAX
            }
        );
        assert_eq!(part.len(), usize::MAX);
        assert_eq!(part.output_offset(), 0);
        assert_eq!(part.prefix_bytes(), 0);
        assert_eq!(
            format!("{part:?}"),
            format!(
                "NalPacketSegment {{ source_range: 0..{}, nal_index: 0, prefix_and_source: 0, output_offset: 0 }}",
                usize::MAX
            )
        );
    }

    #[test]
    fn every_configuration_index_boundary_preserves_full_source_ranges() {
        // A hvcC has at most255 arrays with65535 NALs each. The last possible
        // index is checked without allocating a multi-gigabyte record.
        for index in [0, 31, 255, 65_535, 255 * 65_535 - 1] {
            let range = usize::MAX - 1..usize::MAX;
            let part = NalPacketSegment::configuration(index, range.clone(), usize::MAX);
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
                    "NalPacketSegment {{ source_range: {}..{}, nal_index: {index}, prefix_and_source: 132, output_offset: {} }}",
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
            let part = NalPacketSegment::packet(0..1, prefix, usize::MAX);
            assert_eq!(
                part.origin(),
                NalPacketOrigin::Packet { source_range: 0..1 }
            );
            assert_eq!(part.prefix_bytes(), prefix);
            assert_eq!(part.output_offset(), usize::MAX);
            assert_eq!(part.payload(part.backing(&raw, &raw)).unwrap(), &[0x41]);
            assert_eq!(
                format!("{part:?}"),
                format!(
                    "NalPacketSegment {{ source_range: 0..1, nal_index: 0, prefix_and_source: {prefix}, output_offset: {} }}",
                    usize::MAX,
                )
            );
        }
    }
}

#[cfg(test)]
mod inline_packet_segments {
    use super::*;

    fn segment(index: usize) -> NalPacketSegment {
        NalPacketSegment::packet(index..index + 1, 4, index * 5)
    }

    #[test]
    fn large_hints_keep_inline_appends_uncharged_then_size_the_spill() {
        for hint in [INLINE_PACKET_SEGMENTS + 1, 9] {
            let pool = Budget::new(4096).unwrap();
            let base = pool.used();
            let mut parts = PacketSegmentStorage::new();
            for index in 0..INLINE_PACKET_SEGMENTS {
                parts.try_push(&pool, Some(hint), segment(index)).unwrap();
                assert!(matches!(parts, PacketSegmentStorage::Inline { .. }));
                assert_eq!(pool.used(), base);
                assert_eq!(pool.peak(), base);
            }
            parts
                .try_push(&pool, Some(hint), segment(INLINE_PACKET_SEGMENTS))
                .unwrap();
            let PacketSegmentStorage::Heap(heap) = &parts else {
                panic!("descriptor did not spill");
            };
            assert_eq!(heap.capacity(), hint);
            assert_eq!(pool.used(), base + hint * size_of::<NalPacketSegment>());
            for (index, part) in parts.as_slice().iter().enumerate() {
                assert_eq!(*part, segment(index));
            }
            drop(parts);
            assert_eq!(pool.used(), base);
        }
    }

    #[test]
    fn initial_descriptors_are_inline_and_spill_keeps_every_entry() {
        let pool = Budget::new(4096).unwrap();
        let base = pool.used();
        let mut parts = PacketSegmentStorage::new();
        assert!(parts.as_slice().is_empty());
        for index in 0..INLINE_PACKET_SEGMENTS {
            parts.try_push(&pool, None, segment(index)).unwrap();
            assert_eq!(pool.used(), base);
            assert_eq!(parts.len(), index + 1);
        }
        for index in INLINE_PACKET_SEGMENTS..9 {
            parts.try_push(&pool, None, segment(index)).unwrap();
            let PacketSegmentStorage::Heap(heap) = &parts else {
                panic!("descriptor did not spill");
            };
            assert_eq!(
                pool.used(),
                base + heap.capacity() * size_of::<NalPacketSegment>()
            );
            assert_eq!(parts.len(), index + 1);
            for (index, part) in parts.as_slice().iter().enumerate() {
                assert_eq!(*part, segment(index));
            }
        }
        drop(parts);
        assert_eq!(pool.used(), base);
    }

    #[test]
    fn refused_spill_keeps_inline_progress_and_resumes_with_exact_capacity() {
        let pool = Budget::new(4096).unwrap();
        let blocker = pool
            .reserve(pool.available() - INLINE_PACKET_SEGMENTS * size_of::<NalPacketSegment>())
            .unwrap();
        let charge = pool.used();
        let mut parts = PacketSegmentStorage::new();
        assert!(matches!(parts, PacketSegmentStorage::Inline { .. }));
        assert!(parts.as_slice().is_empty());
        assert_eq!(pool.used(), charge);
        for index in 0..INLINE_PACKET_SEGMENTS {
            parts.try_push(&pool, Some(9), segment(index)).unwrap();
            assert_eq!(parts.len(), index + 1);
            assert_eq!(parts.as_slice()[index], segment(index));
            assert_eq!(pool.used(), charge);
        }
        let before = parts.as_slice().to_vec();
        for _ in 0..2 {
            assert!(matches!(
                parts.try_push(&pool, Some(9), segment(INLINE_PACKET_SEGMENTS)),
                Err(Error::Backpressure { .. })
            ));
            assert!(matches!(parts, PacketSegmentStorage::Inline { .. }));
            assert_eq!(parts.as_slice(), before);
            assert_eq!(pool.used(), charge);
        }
        drop(blocker);
        let needed = INLINE_PACKET_SEGMENTS + 1;
        let blocker = pool
            .reserve(pool.available() - needed * size_of::<NalPacketSegment>())
            .unwrap();
        parts
            .try_push(&pool, Some(9), segment(INLINE_PACKET_SEGMENTS))
            .unwrap();
        let PacketSegmentStorage::Heap(heap) = &parts else {
            panic!("retry did not spill");
        };
        assert_eq!(heap.capacity(), needed);
        for (index, part) in parts.as_slice().iter().enumerate() {
            assert_eq!(*part, segment(index));
        }
        drop(blocker);
        drop(parts);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }

    #[test]
    fn lower_bound_excludes_inline_metadata_and_checks_heap_overflow() {
        for count in 0..=INLINE_PACKET_SEGMENTS {
            assert_eq!(PacketSegmentStorage::required_heap_bytes(count), Ok(0));
        }
        assert_eq!(
            PacketSegmentStorage::required_heap_bytes(INLINE_PACKET_SEGMENTS + 1),
            Ok((INLINE_PACKET_SEGMENTS + 1) * size_of::<NalPacketSegment>())
        );
        assert_eq!(
            PacketSegmentStorage::required_heap_bytes(usize::MAX),
            Err(Error::Overflow)
        );
    }

    #[test]
    fn upper_bounds_preserve_inline_progress_across_the_spill() {
        for actual in [
            3,
            INLINE_PACKET_SEGMENTS - 1,
            INLINE_PACKET_SEGMENTS,
            INLINE_PACKET_SEGMENTS + 1,
        ] {
            let pool = Budget::new(4096).unwrap();
            let base = pool.used();
            let hint = 9;
            let mut parts = PacketSegmentStorage::new();
            for index in 0..actual {
                parts.try_push(&pool, Some(hint), segment(index)).unwrap();
                assert_eq!(parts.len(), index + 1);
                assert_eq!(parts.as_slice()[index], segment(index));
                if index < INLINE_PACKET_SEGMENTS {
                    assert!(matches!(parts, PacketSegmentStorage::Inline { .. }));
                    assert_eq!(pool.used(), base);
                } else {
                    let PacketSegmentStorage::Heap(heap) = &parts else {
                        panic!("descriptor did not spill");
                    };
                    assert_eq!(heap.capacity(), hint);
                    assert_eq!(pool.used(), base + hint * size_of::<NalPacketSegment>());
                }
            }
            drop(parts);
            assert_eq!(pool.used(), base);
        }
    }
}
