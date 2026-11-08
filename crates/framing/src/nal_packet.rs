//! Complete-packet, source-preserving projection of the reference public BSFs
//! named on [`crate::compat::nal`].
//! NAL bodies stay opaque; this does not establish access-unit/decoder validity.
mod phases;

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
            Self::ReceiverAvc(v) if v.fields().length_size_minus_one() == 2 => {
                NalConfigurationRefusal::AvcWidthThree
            }
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
    /// Declared HEVC array type, or None past the last array or for non-HEVC.
    pub(crate) fn hevc_array_type(&self, index: usize) -> Option<u8> {
        self.hevc_record()?
            .array(index)
            .map(|a| a.declared_nal_type())
    }
    /// Configuration NAL, or None past the last NAL or for passthrough.
    pub(crate) fn config_nal(&self, index: usize) -> Option<ConfigNal<'_>> {
        if let Some(c) = self.avc_record() {
            c.nal(index).map(|n| ConfigNal {
                kind: Some(n.array_kind()),
                owner: c.source_bytes(),
                range: n.source_range(),
            })
        } else {
            let c = self.hevc_record()?;
            c.nal(index).map(|n| ConfigNal {
                kind: None,
                owner: c.source_bytes(),
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

/// One configuration NAL as the packet and initialization writers read it.
pub(crate) struct ConfigNal<'a> {
    /// AVC array kind; HEVC records select every NAL.
    pub(crate) kind: Option<AvcConfigArrayKind>,
    /// The raw record, whose `range` addresses the NAL.
    pub(crate) owner: &'a Bytes,
    pub(crate) range: Range<usize>,
}

impl<'a> ConfigNal<'a> {
    /// The NAL payload. The parser validated its range against the same record.
    pub(crate) fn payload(&self) -> &'a [u8] {
        &self.owner[self.range.clone()]
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
        self.begin_packet(source, avc_update.map(NalPacketConfiguration::Avc), false)
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
        self.begin_packet(
            source,
            update.map(NalPacketConfiguration::ReceiverAvc),
            true,
        )
    }
    fn begin_packet<M>(
        &mut self,
        source: NalPacket<M>,
        avc_update: Option<NalPacketConfiguration>,
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
        let length_width = avc_update.as_ref().unwrap_or(&self.configuration).width();
        let phase = if changes {
            Phase::Verify
        } else if codec == Codec::Hevc {
            Phase::Arrays
        } else {
            Phase::Scan
        };
        Ok(NalPacketJob {
            session: self,
            source: Some(source),
            update: avc_update,
            codec,
            length_width,
            avc,
            epoch,
            phase,
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
            look_nals: 0,
            live_base: None,
            segment_hint: None,
            segments: None,
            output: None,
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
/// Small progress result for publication into an empty caller-owned slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NalPacketProgress {
    Yield,
    Ready,
    Complete,
}

/// An immutable source range. Its backing bytes are owned by the output,
/// rather than independently cloned into each descriptor.
#[derive(Clone, PartialEq, Eq)]
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
    fn owner<'a>(&self, packet: &'a Bytes, configuration: &'a Bytes) -> &'a Bytes {
        if (self.nal_and_prefix >> 24) as u8 & Self::CONFIGURATION_SOURCE == 0 {
            packet
        } else {
            configuration
        }
    }
    fn payload<'a>(&self, packet: &'a Bytes, configuration: &'a Bytes) -> Result<&'a [u8]> {
        self.owner(packet, configuration)
            .as_ref()
            .get(self.source_range())
            .ok_or(Error::Invalid("packet segment range"))
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
    segments: BudgetVec<NalPacketSegment>,
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
        part.owner(&self.source.bytes, &self.configuration)
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
        part.owner(&self.source.bytes, &self.configuration)
            .slice(part.source_range())
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
/// One cooperative packet framing job over the session it borrows.
pub struct NalPacketJob<'a, M = NalPacketFields> {
    session: &'a mut NalPacketSession,
    source: Option<NalPacket<M>>,
    update: Option<NalPacketConfiguration>,
    // Codec and width stay fixed while the job borrows the session. A valid
    // update changes only the width of this AVC packet and subsequent packets.
    codec: Codec,
    length_width: Option<LengthSize>,
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
    // Pending NAL bounds stay in offset/length until a successful append.
    nal: Option<u8>,
    injection: Option<(Injection, usize)>,
    injected_irap: bool,
    has_irap: bool,
    // Lookahead uses the scan cursor, then resets it before scanning.
    look_nals: usize,
    /// Job-invariant part of `minimum_live_bytes`: control storage, both
    /// descriptor lower bounds and the deduplicated raw owners. None of these
    /// change while segments are appended, so the sum is computed once.
    live_base: Option<usize>,
    /// Segment count known before the first append: width-framed HEVC after its
    /// lookahead, or a packet whose first NAL is also its last. Only sizes the
    /// first allocation; a refused reservation falls back to ordinary growth.
    segment_hint: Option<usize>,
    segments: Option<BudgetVec<NalPacketSegment>>,
    output: Option<BudgetVec<u8>>,
    total: usize,
    copy_segment: usize,
    copied: usize,
    failure: Option<Error>,
    receiver_failure: Option<NalPacketReceiverErrorKind>,
    observed_packet: bool,
    packet_source_refusal: Option<NalHeaderError>,
}
impl<M> NalPacketJob<'_, M> {
    fn config(&self) -> &NalPacketConfiguration {
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
            configuration: self.session.configuration.bytes().clone(),
            configuration_identity: self.session.configuration.source_identity(),
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
            None => *self.live_base.insert(self.fixed_live_bytes()?),
        };
        let copied_storage = if self.length_width.is_some() {
            Bytes::owner_overhead_bytes()
                .checked_add(output)
                .ok_or(Error::Overflow)?
        } else {
            0
        };
        segments
            .checked_mul(size_of::<NalPacketSegment>())
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
            Some(self.session.configuration.bytes()),
            self.update.as_ref().map(NalPacketConfiguration::bytes),
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
    fn append(&mut self, segment: NalPacketSegment) -> Result<()> {
        let parts = self.segments.as_ref().map_or(0, BudgetVec::len);
        if parts == self.session.limits.maximum_segments {
            return Err(Error::Limit {
                requested: parts.saturating_add(1),
                maximum: self.session.limits.maximum_segments,
            });
        }
        let count = parts.checked_add(1).ok_or(Error::Overflow)?;
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
            .get_or_insert_with(|| BudgetVec::new(&self.session.pool));
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
            assert_eq!(part.payload(&raw, &raw).unwrap(), &[0x41]);
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
