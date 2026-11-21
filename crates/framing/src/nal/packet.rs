//! Complete length-prefixed samples framed as Annex B packets, as the FFmpeg
//! bitstream filters named on [`crate::compat::nal`] frame them, with every
//! output byte traceable to its source.
//! NAL bodies stay opaque; this does not establish access-unit/decoder validity.
mod phases;
use crate::retryable;
use phases::AvcBufferingLookahead;

use crate::inline_storage::InlineStorage;
use crate::nal::annexb::START_CODE;
use crate::nal::header::NalHeaderError;
use crate::{
    BuildStep,
    avc::{AvcConfigArrayKind, AvcConfiguration},
    hevc::HvcConfiguration,
    nal::{Codec, LengthSize},
};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, SourceLocation, WorkBudget};

/// The most bytes copied between two work and cancellation checkpoints.
const COPY_CHUNK: usize = 4096;

/// Resume copying one start code and its payload without crossing a work or
/// cancellation checkpoint. The cursor is relative to the start code.
#[inline(never)]
pub(crate) fn copy_annex_b_segment(
    prefix: usize,
    payload: &[u8],
    output: &mut BudgetVec<u8>,
    copied: &mut usize,
    work: &mut WorkBudget,
    cancel: &AtomicBool,
) -> Result<bool> {
    let end = payload.len().checked_add(prefix).ok_or(Error::Overflow)?;
    // A segment that fits one chunk and the remaining work is copied in one
    // pass, with the charge and cancellation check of the chunked copy.
    if *copied == 0 && end != 0 && end <= COPY_CHUNK && end as u64 <= work.remaining() {
        push_start_code(output, prefix)?;
        output.try_extend_from_slice(payload)?;
        work.take(end as u64);
        *copied = end;
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        return Ok(true);
    }
    while *copied < end {
        let remaining = if *copied < prefix {
            &START_CODE[4 - prefix + *copied..]
        } else {
            &payload[*copied - prefix..]
        };
        let count = (work.remaining().min(COPY_CHUNK as u64) as usize).min(remaining.len());
        if count == 0 {
            return Ok(false);
        }
        output.try_extend_from_slice(&remaining[..count])?;
        work.take(count as u64);
        *copied += count;
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
    }
    Ok(true)
}

/// Copy every entry, each a start-code length and a payload, in one pass when
/// the output fits one chunk and the remaining work. The pass charges what the
/// entry-by-entry copy charges: the output bytes and one unit for each of
/// `entries` walked, `output_len` and `entries` counting entries that copy
/// nothing. Returns false, without copying or charging, otherwise.
pub(crate) fn copy_annex_b_whole<'a>(
    output: &mut BudgetVec<u8>,
    output_len: usize,
    entries: usize,
    copies: impl IntoIterator<Item = Result<(usize, &'a [u8])>>,
    work: &mut WorkBudget,
    cancel: &AtomicBool,
) -> Result<bool> {
    let units = output_len.checked_add(entries).ok_or(Error::Overflow)? as u64;
    if output_len > COPY_CHUNK || units > work.remaining() {
        return Ok(false);
    }
    for copy in copies {
        let (prefix, payload) = copy?;
        push_start_code(output, prefix)?;
        output.try_extend_from_slice(payload)?;
    }
    work.take(units);
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    Ok(true)
}

/// Append the last `prefix` bytes of the four-byte start code. The three- and
/// four-byte codes are fixed-size copies, which compile to stores rather than
/// a call to copy a few bytes.
#[inline(always)]
fn push_start_code(output: &mut BudgetVec<u8>, prefix: usize) -> Result<()> {
    match prefix {
        4 => output.try_extend_from_slice(&START_CODE),
        3 => output.try_extend_from_slice(&START_CODE[1..]),
        _ => output.try_extend_from_slice(&START_CODE[4 - prefix..]),
    }
}

/// Start an output of exactly `bytes`, charged together with the header of the
/// shared owner it is published in.
#[inline(never)]
pub(crate) fn start_annex_b_output(pool: &Budget, bytes: usize) -> Result<BudgetVec<u8>> {
    let minimum = bytes
        .checked_add(Bytes::owner_overhead_bytes())
        .and_then(|v| v.checked_add(Budget::control_storage_bytes()))
        .ok_or(Error::Overflow)?;
    if minimum > pool.limit() {
        return Err(Error::Limit {
            requested: minimum,
            maximum: pool.limit(),
        });
    }
    BudgetVec::try_with_owner_capacity(pool, bytes)
}

/// Retain the charged bytes for retry if publishing the result fails.
#[inline(never)]
pub(crate) fn freeze_annex_b_output(
    output: &mut Option<BudgetVec<u8>>,
    absent: &'static str,
) -> Result<Bytes> {
    let buffer = output.take().ok_or(Error::Invalid(absent))?;
    match buffer.freeze() {
        Ok(bytes) => Ok(bytes),
        Err(error) => {
            *output = Some(error.value);
            Err(error.error)
        }
    }
}

/// The scalar fields of an FFmpeg `AVPacket`: the default metadata of a
/// [`NalPacket`]. A session reads none of them and carries them to its output as
/// given, so a value here says nothing about the clock it was taken from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NalPacketFields {
    /// Presentation timestamp (`pts`), in the caller's time base.
    pub pts: i64,
    /// Decoding timestamp (`dts`), in the caller's time base.
    pub dts: i64,
    /// Duration (`duration`), in the caller's time base.
    pub duration: i64,
    /// Byte position of the packet in its stream (`pos`).
    pub position: i64,
    /// The packet's flag bits (`flags`).
    pub flags: u32,
    /// Index of the packet's stream (`stream_index`).
    pub stream_index: i32,
}

/// One container sample given to a [`NalPacketSession`], with the caller's
/// metadata `M`.
#[derive(Clone, Debug)]
pub struct NalPacket<M = NalPacketFields> {
    /// The complete sample: NAL units, each after a length field of the
    /// configuration's width, or any bytes for a passthrough session. Charged
    /// to the pool of the session's configuration.
    pub bytes: Bytes,
    /// Where the sample starts in its source. Its offset plus the sample's
    /// length must fit `u64`.
    pub location: SourceLocation,
    /// The caller's generation of the source, carried to the output as given. It
    /// is independent of [`NalPacketSession::receiver_epoch`].
    pub generation: u64,
    /// The caller's metadata, carried to the output as given, timestamps known or
    /// unknown alike. [`NalPacketFields`] is the default type.
    pub fields: M,
    /// The packet's new-extradata side data (`AV_PKT_DATA_NEW_EXTRADATA`),
    /// exactly as received: `None` when the packet has none, `Some` of no bytes
    /// when it is present and empty.
    pub new_extradata: Option<Bytes>,
}

/// The configuration a [`NalPacketSession`] frames its packets with.
#[derive(Debug)]
pub enum NalPacketConfiguration {
    /// An AVC configuration record that [`AvcConfigParser`](crate::avc::AvcConfigParser)
    /// accepts.
    Avc(AvcConfiguration),
    /// An HEVC configuration record that [`HvcConfigParser`](crate::hevc::HvcConfigParser)
    /// accepts.
    Hevc(HvcConfiguration),
    /// An AVC record as FFmpeg's filter accepts it ([`crate::compat::nal`]). The
    /// error with which the strict parser refuses it stays readable.
    ReceiverAvc(crate::compat::nal::ReceiverAvcConfiguration),
    /// An HEVC record as FFmpeg's filter accepts it ([`crate::compat::nal`]).
    /// The error with which the strict parser refuses it stays readable.
    ReceiverHevc(crate::compat::nal::ReceiverHevcConfiguration),
    /// No record: every packet is published as it is and shares its bytes with
    /// the output. Neither `raw` nor a packet is checked to be Annex B.
    Passthrough {
        /// The coding standard of the stream.
        codec: Codec,
        /// The configuration bytes, kept as given and reported with every
        /// output.
        raw: Bytes,
    },
}
impl NalPacketConfiguration {
    /// The configuration's source bytes: the whole record, or the passthrough
    /// bytes.
    pub fn bytes(&self) -> &Bytes {
        self.record().bytes()
    }
    /// The coding standard of the stream.
    pub fn codec(&self) -> Codec {
        match self {
            Self::Avc(_) | Self::ReceiverAvc(_) => Codec::Avc,
            Self::Hevc(_) | Self::ReceiverHevc(_) => Codec::Hevc,
            Self::Passthrough { codec, .. } => *codec,
        }
    }
    /// The error with which the strict record parser refuses this record, when
    /// it was accepted through [`crate::compat::nal`]; `None` for a record that
    /// parser accepts and for passthrough.
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
    fn record(&self) -> ConfigurationRef<'_> {
        match self {
            Self::Avc(record) => ConfigurationRef::Avc(record),
            Self::Hevc(record) => ConfigurationRef::Hevc(record),
            Self::ReceiverAvc(record) => ConfigurationRef::Avc(record.record()),
            Self::ReceiverHevc(record) => ConfigurationRef::Hevc(record.record()),
            Self::Passthrough { raw, .. } => ConfigurationRef::Passthrough(raw),
        }
    }
    /// Where the caller says the record came from, and its generation there, as
    /// they were given to the record's parser. `None` for passthrough. It says
    /// nothing about the record's validity.
    pub fn source_identity(&self) -> Option<(SourceLocation, u64)> {
        self.record().source_identity()
    }
    /// Element-count lower bound for live descriptor storage.
    pub(crate) fn descriptor_lower_bound(&self) -> Result<usize> {
        match self.record() {
            ConfigurationRef::Avc(record) => record.descriptor_lower_bound(),
            ConfigurationRef::Hevc(record) => record.descriptor_lower_bound(),
            ConfigurationRef::Passthrough(_) => Ok(0),
        }
    }
    /// Sample length width; None for explicit passthrough.
    pub(crate) fn width(&self) -> Option<LengthSize> {
        match self.record() {
            ConfigurationRef::Avc(record) => Some(record.sample_length_size()),
            ConfigurationRef::Hevc(record) => Some(record.length_size()),
            ConfigurationRef::Passthrough(_) => None,
        }
    }
    /// Charge the array checks already established by the record parser.
    /// A refusal still consumes its own unit and is exposed only when reached.
    pub(crate) fn admit_hevc_arrays(
        &self,
        checked: &mut usize,
        work: &mut WorkBudget,
    ) -> std::result::Result<bool, u8> {
        let ConfigurationRef::Hevc(record) = self.record() else {
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
    /// Number of configuration NALs; zero for passthrough.
    pub(crate) fn config_nal_count(&self) -> usize {
        self.record().config_nal_count()
    }
    /// Configuration NAL, or None past the last NAL or for passthrough.
    pub(crate) fn config_nal(&self, index: usize) -> Option<ConfigNal<'_>> {
        self.record().config_nal(index)
    }
    /// The materialized configuration size and the sizing walk's work charge.
    pub(crate) fn annex_b_configuration_extent(&self) -> Option<(usize, u64)> {
        match self.record() {
            ConfigurationRef::Avc(record) => Some((
                record.annex_b_configuration_len(),
                record.nal_count() as u64 + 1,
            )),
            ConfigurationRef::Hevc(record) => Some((
                record.annex_b_configuration_len()?,
                record.nal_count() as u64 + 1,
            )),
            ConfigurationRef::Passthrough(raw) => Some((raw.len(), 2)),
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
}

// The writers read the same records for strict and receiver configurations.
enum ConfigurationRef<'a> {
    Avc(&'a AvcConfiguration),
    Hevc(&'a HvcConfiguration),
    Passthrough(&'a Bytes),
}
impl<'a> ConfigurationRef<'a> {
    fn bytes(&self) -> &'a Bytes {
        match self {
            Self::Avc(record) => record.source_bytes(),
            Self::Hevc(record) => record.source_bytes(),
            Self::Passthrough(bytes) => bytes,
        }
    }
    fn source_identity(self) -> Option<(SourceLocation, u64)> {
        let (source, offset, generation) = match self {
            Self::Avc(record) => {
                let source = record.source();
                (source.source, source.offset, source.generation)
            }
            Self::Hevc(record) => {
                let source = record.source();
                (source.source, source.offset, source.generation)
            }
            Self::Passthrough(_) => return None,
        };
        Some((SourceLocation { source, offset }, generation))
    }
    fn config_nal(self, index: usize) -> Option<ConfigNal<'a>> {
        match self {
            Self::Avc(record) => record.nal(index).map(|nal| ConfigNal {
                kind: Some(nal.array_kind()),
                storage: record.source_bytes(),
                range: nal.source_range(),
            }),
            Self::Hevc(record) => record.nal(index).map(|nal| ConfigNal {
                kind: None,
                storage: record.source_bytes(),
                range: nal.source_range(),
            }),
            Self::Passthrough(_) => None,
        }
    }
    /// Number of NALs in the record, an upper bound on one injection.
    fn config_nal_count(self) -> usize {
        match self {
            Self::Avc(record) => record.nal_count(),
            Self::Hevc(record) => record.nal_count(),
            Self::Passthrough(_) => 0,
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

/// Per-packet bounds of a [`NalPacketSession`]. A session admits packets,
/// configuration records and outputs of at most [`Self::MAXIMUM_SPAN`] bytes
/// whatever these limits allow.
#[derive(Clone, Copy, Debug)]
pub struct NalPacketLimits {
    /// The most bytes one input packet may hold.
    pub maximum_packet_bytes: usize,
    /// The most bytes one output may hold, start codes and inserted parameter
    /// sets included.
    pub maximum_output_bytes: usize,
    /// The most NAL units one input packet may hold. Must not be zero.
    pub maximum_nals: usize,
    /// The most segments one output may hold: the packet's NAL units and the
    /// parameter sets inserted before them. Must not be zero.
    pub maximum_segments: usize,
}
impl NalPacketLimits {
    /// The largest packet, configuration record or output a session frames:
    /// 4 GiB − 1, every size a 32-bit target can address. Segment offsets are
    /// stored in 32 bits, which keeps a [`NalPacketSegment`] at 16 bytes.
    pub const MAXIMUM_SPAN: usize = u32::MAX as usize;
}

fn check_span(bytes: usize) -> Result<()> {
    if bytes > NalPacketLimits::MAXIMUM_SPAN {
        return Err(Error::Limit {
            requested: bytes,
            maximum: NalPacketLimits::MAXIMUM_SPAN,
        });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct AvcState(u8);
impl AvcState {
    const SPS: u8 = 1;
    const PPS: u8 = 2;
    const NEW_IDR: u8 = 4;
    const BUFFERING_PERIOD: u8 = 8;

    fn has_any(self, bits: u8) -> bool {
        self.0 & bits != 0
    }

    fn set(&mut self, bits: u8) {
        self.0 |= bits;
    }

    fn clear(&mut self, bits: u8) {
        self.0 &= !bits;
    }
}
impl Default for AvcState {
    fn default() -> Self {
        Self(Self::NEW_IDR)
    }
}

/// Frames the length-prefixed samples of one AVC or HEVC stream as Annex B
/// packets, as FFmpeg's `h264_mp4toannexb` and `hevc_mp4toannexb` bitstream
/// filters do.
///
/// Every NAL unit of a packet gets a start code: `00 00 00 01` for an HEVC NAL
/// unit, an AVC parameter set and the first NAL unit of an output, `00 00 01`
/// for every other AVC NAL unit. The configuration's parameter sets are inserted
/// where the filters insert them. For AVC they go before the first slice of an
/// IDR picture, and the SPS before an in-band PPS, unless parameter sets the
/// stream carries in band already precede it; the session tracks those from
/// packet to packet. For HEVC the whole record goes before the first parameter
/// set or IRAP NAL unit of each packet that holds an IRAP NAL unit. The
/// nonempty `new_extradata` of an AVC packet replaces the configuration;
/// [`receiver_epoch`](Self::receiver_epoch) counts these updates and is
/// independent of the generations the caller gives its sources.
///
/// One packet is framed at a time, by the [`NalPacketJob`] that
/// [`packet`](Self::packet) or [`receiver_packet`](Self::receiver_packet)
/// returns. A job that fails or is cancelled publishes no partial packet and
/// leaves the in-band tracking as it was. An AVC update that the job has
/// verified is adopted even when the packet's payload is then refused as
/// invalid or truncated, as the filter adopts it; cancellation, limits and
/// backpressure do not adopt it.
///
/// The session holds a packet's update and segment descriptors while its job
/// runs. An update that is not adopted, and descriptor storage, are released
/// when the job publishes or fails, or, for a job dropped before it finishes,
/// when the next packet begins or with the session; [`flush`](Self::flush)
/// releases the update.
pub struct NalPacketSession {
    configuration: NalPacketConfiguration,
    codec: Codec,
    length_width: Option<LengthSize>,
    limits: NalPacketLimits,
    maximum_copy_storage: usize,
    avc: AvcState,
    receiver_epoch: u64,
    drained: bool,
    // State of the packet in flight that most packets leave empty. It lives
    // here rather than in the job, which every packet creates and moves.
    pending_update: Option<AvcUpdate>,
    avc_buffering: Option<AvcBufferingLookahead>,
    // The packet's segment descriptors, moved into its output on publication.
    segments: PacketSegmentStorage,
}
impl std::fmt::Debug for NalPacketSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NalPacketSession")
            .field("codec", &self.codec)
            .field("length_width", &self.length_width)
            .field("limits", &self.limits)
            .field("configuration_bytes", &self.configuration.bytes().len())
            .field("receiver_epoch", &self.receiver_epoch)
            .field("drained", &self.drained)
            .finish_non_exhaustive()
    }
}
impl NalPacketSession {
    /// A session for `configuration`, whose bytes are charged to `pool`; packets
    /// and outputs are charged to it too. Byte limits above
    /// [`NalPacketLimits::MAXIMUM_SPAN`] are lowered to it.
    ///
    /// Fails with [`Error::ForeignPool`] when `configuration` is charged to another
    /// pool; [`Error::Invalid`] when `maximum_nals` or `maximum_segments` is zero;
    /// [`Error::Limit`] when the configuration exceeds
    /// [`NalPacketLimits::MAXIMUM_SPAN`].
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
        check_span(configuration.bytes().len())?;
        let limits = NalPacketLimits {
            maximum_packet_bytes: limits
                .maximum_packet_bytes
                .min(NalPacketLimits::MAXIMUM_SPAN),
            maximum_output_bytes: limits
                .maximum_output_bytes
                .min(NalPacketLimits::MAXIMUM_SPAN),
            ..limits
        };
        // When the logical maximum overflows, keep the per-packet proof.
        // The pool's own charge means its available space is below usize::MAX.
        let maximum_copy_storage =
            PacketPlan::added_storage(limits.maximum_segments, limits.maximum_output_bytes, true)
                .unwrap_or(usize::MAX);
        Ok(Self {
            codec: configuration.codec(),
            length_width: configuration.width(),
            configuration,
            limits,
            maximum_copy_storage,
            avc: AvcState::default(),
            receiver_epoch: 0,
            drained: false,
            pending_update: None,
            avc_buffering: None,
            segments: empty_packet_segments(),
        })
    }
    /// The configuration in effect: the one given at construction, or the last
    /// AVC update adopted.
    pub fn configuration(&self) -> &NalPacketConfiguration {
        &self.configuration
    }
    fn install_avc(&mut self, update: AvcUpdate, length_width: Option<LengthSize>) {
        self.configuration = match update {
            AvcUpdate::Source(record) => NalPacketConfiguration::Avc(record),
            AvcUpdate::Receiver(record) => NalPacketConfiguration::ReceiverAvc(record),
        };
        // Only an AVC side-data update can reach this path. Its width was
        // selected from the parsed update when the packet began.
        self.length_width = length_width;
    }
    /// The number of AVC configuration updates adopted so far; zero until the
    /// first.
    pub fn receiver_epoch(&self) -> u64 {
        self.receiver_epoch
    }
    /// Marks the end of the stream: every later packet is refused until
    /// [`flush`](Self::flush). The session queues no output, so nothing is left
    /// to collect; a job borrows the session exclusively and has ended before
    /// this can be called.
    pub fn drain(&mut self) {
        self.drained = true;
    }
    /// Restarts the stream: clears the tracking of in-band parameter sets,
    /// releases an update that was not adopted and accepts packets again after
    /// [`drain`](Self::drain). The configuration and
    /// [`receiver_epoch`](Self::receiver_epoch) stay.
    pub fn flush(&mut self) {
        self.avc = AvcState::default();
        self.drained = false;
        self.pending_update = None;
    }
    /// Begins framing `source` and returns its job, which borrows the session
    /// until it is dropped.
    ///
    /// An AVC packet whose `new_extradata` is not empty replaces the
    /// configuration: `avc_update` is the record that
    /// [`AvcConfigParser`](crate::avc::AvcConfigParser) parsed from those bytes,
    /// and `None` for every other packet. The job compares the record's bytes
    /// with the side data, work-bounded, before it adopts the record. An HEVC
    /// session keeps side data on the packet and never adopts it.
    ///
    /// Fails with [`Error::Invalid`] after [`drain`](Self::drain), and when
    /// `avc_update` is given without nonempty AVC side data or is missing for it;
    /// [`Error::ForeignPool`] when the packet, its side data or the update is charged
    /// to another pool than the configuration; [`Error::Limit`] past
    /// `maximum_packet_bytes`, and for an update above
    /// [`NalPacketLimits::MAXIMUM_SPAN`]; [`Error::Overflow`] when the packet's end
    /// offset does not fit `u64`. The session stays as it was.
    pub fn packet<M>(
        &mut self,
        source: NalPacket<M>,
        avc_update: Option<AvcConfiguration>,
    ) -> Result<NalPacketJob<'_, M>> {
        self.begin_packet(source, avc_update.map(AvcUpdate::Source), false)
    }
    /// [`packet`](Self::packet) with the filter's acceptance
    /// ([`crate::compat::nal`]): a NAL unit whose header `packet` refuses is
    /// framed, and the first such refusal is kept on the output or rejection.
    /// An AVC IDR NAL unit of one byte is framed too. Before an AVC
    /// buffering-period SEI that no in-band parameter set precedes, the SPS and
    /// PPS are inserted: those the rest of the packet carries before its next
    /// slice, else the configuration's. An update is a
    /// [`ReceiverAvcConfiguration`](crate::compat::nal::ReceiverAvcConfiguration);
    /// the error with which the strict parser refuses a record stays on the
    /// configuration and on every output and rejection.
    ///
    /// Fails as [`packet`](Self::packet) does.
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
        if !source
            .bytes
            .budget()
            .same_pool(self.configuration.bytes().budget())
            || source
                .new_extradata
                .as_ref()
                .is_some_and(|extra| !extra.belongs_to(source.bytes.budget()))
            || avc_update
                .as_ref()
                .is_some_and(|update| !update.bytes().belongs_to(source.bytes.budget()))
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
        let codec = self.codec;
        let changes =
            codec == Codec::Avc && source.new_extradata.as_ref().is_some_and(|v| !v.is_empty());
        if changes != avc_update.is_some() {
            return Err(Error::Invalid("AVC update and literal side data disagree"));
        }
        if let Some(update) = &avc_update {
            check_span(update.bytes().len())?;
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
            None => self.length_width,
        };
        let phase = if changes {
            PlanPhase::Verify
        } else if codec == Codec::Hevc {
            PlanPhase::Arrays
        } else {
            PlanPhase::Scan
        };
        self.pending_update = avc_update;
        self.avc_buffering = None;
        // Descriptors a dropped job left behind; publication and failure
        // leave an empty inline table.
        if matches!(self.segments, PacketSegmentStorage::Heap(_)) {
            self.segments = empty_packet_segments();
        } else {
            self.segments.clear_inline();
        }
        Ok(NalPacketJob {
            source: Some(source),
            state: PacketState {
                session: self,
                codec,
                length_width,
                avc,
                epoch,
                update_verified: false,
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

/// Where the bytes of a [`NalPacketSegment`] come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NalPacketOrigin {
    /// A NAL unit of the input packet.
    Packet {
        /// Its byte range in the packet, without its length field. The whole
        /// packet for a passthrough output.
        source_range: Range<usize>,
    },
    /// A parameter set inserted from the configuration record.
    Configuration {
        /// Its index among the record's NAL units, as
        /// [`AvcConfiguration::nal`] and [`HvcConfiguration::nal`] count them.
        nal_index: usize,
        /// Its byte range in the record, without its length field.
        source_range: Range<usize>,
    },
}
/// Progress of [`NalPacketJob::step_into`], which publishes into a
/// caller-provided slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NalPacketProgress {
    /// The work budget is spent and the slot stays empty; call again with more.
    Yield,
    /// The output was placed in the slot.
    Ready,
    /// The output was already published; the slot stays empty.
    Complete,
}

/// One NAL unit of a [`NalPacketOutput`]: the source range it is copied from
/// and where it lies in the output.
///
/// An immutable source range. Its backing bytes are owned by the output,
/// rather than independently cloned into each descriptor.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct NalPacketSegment {
    // Offsets and lengths fit 32 bits: a session admits packets, records and
    // outputs of at most `NalPacketLimits::MAXIMUM_SPAN` bytes.
    source_start: u32,
    payload_length: u32,
    // The high byte carries the source and prefix tag. The low 24 bits carry
    // a configuration NAL index: avcC has at most 31+255+255 NALs; hvcC has at
    // most 255 arrays of 65535 NALs. Packet descriptors use index 0.
    nal_and_prefix: u32,
    output_offset: u32,
}
// Report the public range and offset fields instead of their packed storage.
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for NalPacketSegment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let nal_index = (self.nal_and_prefix & Self::NAL_INDEX_MASK) as usize;
        let prefix_and_source = (self.nal_and_prefix >> 24) as u8;
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
    const NAL_INDEX_MASK: u32 = 0x00ff_ffff;
    // Callers pass ranges and offsets within a packet, a configuration record
    // or an output that the session bounded by `NalPacketLimits::MAXIMUM_SPAN`.
    fn packet(range: Range<usize>, prefix: u8, offset: usize) -> Self {
        debug_assert!(
            range.end <= NalPacketLimits::MAXIMUM_SPAN && offset <= NalPacketLimits::MAXIMUM_SPAN
        );
        Self {
            source_start: range.start as u32,
            payload_length: range.len() as u32,
            nal_and_prefix: u32::from(prefix) << 24,
            output_offset: offset as u32,
        }
    }
    fn configuration(index: usize, range: Range<usize>, offset: usize) -> Self {
        // Called only for an entry in the parsed avcC/hvcC descriptor slice.
        // Their literal count fields bound every index below 255*65535 < 2^24.
        debug_assert!(index < usize::from(u8::MAX) * usize::from(u16::MAX));
        debug_assert!(u16::try_from(range.len()).is_ok());
        debug_assert!(
            range.end <= NalPacketLimits::MAXIMUM_SPAN && offset <= NalPacketLimits::MAXIMUM_SPAN
        );
        Self {
            source_start: range.start as u32,
            payload_length: range.len() as u32,
            nal_and_prefix: (index as u32) | (u32::from(Self::CONFIGURATION_SOURCE | 4) << 24),
            output_offset: offset as u32,
        }
    }
    /// The whole packet, unchanged, at offset zero.
    fn passthrough(length: usize) -> Self {
        Self::packet(0..length, 0, 0)
    }
    /// Whether the NAL unit comes from the packet or from the configuration
    /// record, and its byte range there.
    pub fn origin(&self) -> NalPacketOrigin {
        if self.is_configuration() {
            NalPacketOrigin::Configuration {
                nal_index: (self.nal_and_prefix & Self::NAL_INDEX_MASK) as usize,
                source_range: self.source_range(),
            }
        } else {
            NalPacketOrigin::Packet {
                source_range: self.source_range(),
            }
        }
    }
    /// Length of the start code written before the NAL unit: 3 or 4 bytes, and
    /// 0 for a passthrough output.
    pub fn prefix_bytes(&self) -> u8 {
        (self.nal_and_prefix >> 24) as u8 & !Self::CONFIGURATION_SOURCE
    }
    /// Offset of the segment's start code in the output's bytes; the NAL unit
    /// follows it.
    pub fn output_offset(&self) -> usize {
        self.output_offset as usize
    }
    fn len(&self) -> usize {
        self.payload_length as usize
    }
    fn framed_len(&self) -> usize {
        // The length is bounded by a byte slice, at most isize::MAX. The
        // inserted prefix is at most four bytes and zero for passthrough.
        self.len() + usize::from(self.prefix_bytes())
    }
    fn source_range(&self) -> Range<usize> {
        let start = self.source_start as usize;
        start..start + self.len()
    }
    fn is_configuration(&self) -> bool {
        (self.nal_and_prefix >> 24) as u8 & Self::CONFIGURATION_SOURCE != 0
    }
    fn backing<'a>(&self, packet: &'a Bytes, configuration: &'a Bytes) -> &'a Bytes {
        if self.is_configuration() {
            configuration
        } else {
            packet
        }
    }
    /// The start-code length and payload this segment copies into the output.
    fn copy<'a>(&self, packet: &'a Bytes, configuration: &'a Bytes) -> Result<(usize, &'a [u8])> {
        let payload = self
            .backing(packet, configuration)
            .get(self.source_range())
            .ok_or(Error::Invalid("packet segment range"))?;
        Ok((usize::from(self.prefix_bytes()), payload))
    }
}

const INLINE_PACKET_SEGMENTS: usize = 7;

type PacketSegmentStorage = InlineStorage<NalPacketSegment, INLINE_PACKET_SEGMENTS>;

fn empty_packet_segments() -> PacketSegmentStorage {
    PacketSegmentStorage::new(|_| NalPacketSegment::packet(0..0, 0, 0))
}

/// One framed packet. It owns its Annex B bytes, the input packet with its side
/// data, and the source bytes of the configuration it was framed with; all stay
/// charged for as long as the output lives, also after its job and session are
/// dropped. Borrowed segment bytes cannot outlive that output:
/// ```compile_fail
/// use triergon_framing::nal::NalPacketOutput;
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
impl<M> std::fmt::Debug for NalPacketOutput<M> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NalPacketOutput")
            .field("location", &self.source.location)
            .field("source_bytes", &self.source.bytes.len())
            .field("output_bytes", &self.bytes.len())
            .field("segments", &self.segments.as_slice())
            .field("receiver_epoch", &self.receiver_epoch)
            .field("forward_new_extradata", &self.forward_new_extradata)
            .field(
                "configuration_source_refusal",
                &self.configuration_source_refusal(),
            )
            .field("packet_source_refusal", &self.packet_source_refusal())
            .finish_non_exhaustive()
    }
}

/// Why the filter rejects a packet as invalid data. Limits, backpressure,
/// cancellation and the NAL header refusals of [`NalPacketSession::packet`] are
/// errors of this implementation, not of the filter, and have no kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NalPacketReceiverErrorKind {
    /// The packet ends inside a NAL length field.
    IncompleteLengthPrefix,
    /// A length field declares more bytes than the packet has left.
    IncompleteNalPayload,
    /// An AVC packet of no bytes.
    EmptyAvcPacket,
    /// An HEVC NAL length field of zero.
    EmptyHevcNal,
}

/// A packet that the filter rejects as invalid data, taken from its failed job
/// with [`NalPacketJob::take_receiver_rejection`]. It holds the input packet
/// with its metadata, the job's error and the session's configuration. Holding
/// them does not make the packet or the configuration valid.
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
impl<M> std::fmt::Debug for NalPacketRejection<M> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NalPacketRejection")
            .field("kind", &self.kind)
            .field("source_error", &self.error)
            .field("location", &self.source.location)
            .field("source_bytes", &self.source.bytes.len())
            .field("receiver_epoch", &self.receiver_epoch)
            .field(
                "configuration_source_refusal",
                &self.configuration_source_refusal(),
            )
            .field("packet_source_refusal", &self.packet_source_refusal())
            .finish_non_exhaustive()
    }
}
impl<M> NalPacketRejection<M> {
    /// Why the filter rejects the packet.
    pub fn kind(&self) -> NalPacketReceiverErrorKind {
        self.kind
    }
    /// The error the job returned: [`Error::Truncated`] for the incomplete
    /// kinds, [`Error::Invalid`] for the empty ones.
    pub fn source_error(&self) -> Error {
        self.error
    }
    /// FFmpeg's return code for this packet: `AVERROR_INVALIDDATA`. It is a
    /// constant for comparison, compiled with the `reference-trace` feature;
    /// FFmpeg is not called.
    #[cfg(feature = "reference-trace")]
    pub fn native_receive_result(&self) -> i32 {
        -1_094_995_529
    }
    /// The input packet, with its metadata and side data as given.
    pub fn source(&self) -> &NalPacket<M> {
        &self.source
    }
    /// The source bytes of the session's configuration after the failure: the
    /// packet's AVC update when the job had verified it.
    pub fn configuration_bytes(&self) -> &Bytes {
        &self.configuration
    }
    /// Where the caller says that configuration's record came from, and its
    /// generation there; `None` for passthrough.
    pub fn configuration_source_identity(&self) -> Option<(SourceLocation, u64)> {
        self.configuration_identity
    }
    /// The error with which the strict record parser refuses that
    /// configuration; `None` when it accepts the record.
    pub fn configuration_source_refusal(&self) -> Option<Error> {
        self.configuration_source_refusal.error()
    }
    /// The first NAL header refusal that
    /// [`NalPacketSession::receiver_packet`] framed past before the failure:
    /// the error [`NalPacketSession::packet`] returns for this packet. `None`
    /// when it accepts every header up to the failure.
    pub fn packet_source_refusal(&self) -> Option<Error> {
        self.packet_source_refusal.map(NalHeaderError::error)
    }
    /// The session's [`receiver_epoch`](NalPacketSession::receiver_epoch) after
    /// the failure; it counts the packet's update when the job had verified it.
    pub fn receiver_epoch(&self) -> u64 {
        self.receiver_epoch
    }
}
impl<M> NalPacketOutput<M> {
    /// The Annex B packet: the start code and NAL unit of each segment, in
    /// order. A passthrough output shares the input packet's bytes.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
    /// The input packet, with its metadata and side data as given.
    pub fn source(&self) -> &NalPacket<M> {
        &self.source
    }
    /// The source bytes of the configuration the packet was framed with: the
    /// packet's own AVC update when it carried one.
    pub fn configuration_bytes(&self) -> &Bytes {
        &self.configuration
    }
    /// Where the caller says that configuration's record came from, and its
    /// generation there; `None` for passthrough.
    pub fn configuration_source_identity(&self) -> Option<(SourceLocation, u64)> {
        self.has_configuration_identity
            .then_some(self.configuration_identity)
    }
    /// The output's NAL units in order, each with the source range it is copied
    /// from.
    pub fn segments(&self) -> &[NalPacketSegment] {
        self.segments.as_slice()
    }
    /// The error with which the strict record parser refuses that
    /// configuration; `None` when it accepts the record.
    pub fn configuration_source_refusal(&self) -> Option<Error> {
        self.configuration_source_refusal.error()
    }
    /// The first NAL header refusal that
    /// [`NalPacketSession::receiver_packet`] framed past: the error
    /// [`NalPacketSession::packet`] returns for this packet. `None` when it
    /// accepts every header.
    pub fn packet_source_refusal(&self) -> Option<Error> {
        self.packet_source_refusal.map(NalHeaderError::error)
    }
    /// The session's [`receiver_epoch`](NalPacketSession::receiver_epoch) with
    /// this packet, whose own update is counted.
    pub fn receiver_epoch(&self) -> u64 {
        self.receiver_epoch
    }
    /// Whether the filter leaves the packet's `new_extradata` side data on the
    /// packet it outputs: always for HEVC; for AVC only when it is present and
    /// empty, since a nonempty record is consumed as the configuration update.
    pub fn forward_new_extradata(&self) -> bool {
        self.forward_new_extradata
    }
    /// Borrow a segment payload through the output's retained packet/configuration
    /// owner. No Bytes clone or allocation occurs. `None` past the last segment.
    pub fn segment_bytes(&self, index: usize) -> Option<&[u8]> {
        let part = self.segments.as_slice().get(index)?;
        part.backing(&self.source.bytes, &self.configuration)
            .as_ref()
            .get(part.source_range())
    }
    /// One segment's payload as its own handle on the retained packet or configuration
    /// owner. No bytes are copied. Fails with [`Error::Invalid`] past the last segment.
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
// The effective update is the session's pending update.
struct PacketState<'a> {
    session: &'a mut NalPacketSession,
    codec: Codec,
    length_width: Option<LengthSize>,
    avc: AvcState,
    epoch: u64,
    update_verified: bool,
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
    look_nals: usize,
    storage_bound: PacketStorageBound,
    // A hint sizes a later spill without reserving while descriptors fit inline.
    segment_hint: Option<usize>,
}
enum PacketStorageBound {
    Unchecked,
    LimitsFit,
    Fixed(usize),
}
impl PacketPlan {
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
            look_nals: 0,
            storage_bound: PacketStorageBound::Unchecked,
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
///
/// [`step`](Self::step) or [`step_into`](Self::step_into) advances it until it
/// publishes its one [`NalPacketOutput`].
pub struct NalPacketJob<'a, M = NalPacketFields> {
    source: Option<NalPacket<M>>,
    state: PacketState<'a>,
    phase: PacketPhase,
}
impl<M> std::fmt::Debug for NalPacketJob<'_, M> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let source = self.source.as_ref();
        formatter
            .debug_struct("NalPacketJob")
            .field("codec", &self.state.codec)
            .field("location", &source.map(|packet| packet.location))
            .field("source_bytes", &source.map(|packet| packet.bytes.len()))
            .field("copying", &matches!(self.phase, PacketPhase::Emit(_)))
            .field("failure", &self.state.failure)
            .finish_non_exhaustive()
    }
}

// While planning, the source stays borrowed and every retained buffer uses
// its pool. Packet metadata is needed only when publishing the result.
#[derive(Clone, Copy)]
struct PacketInput<'a> {
    bytes: &'a Bytes,
    new_extradata: Option<&'a Bytes>,
}

impl PacketState<'_> {
    fn config(&self) -> ConfigurationRef<'_> {
        match &self.session.pending_update {
            Some(update) => ConfigurationRef::Avc(update.record()),
            None => self.session.configuration.record(),
        }
    }
    fn fixed_live_bytes(&self, input: PacketInput<'_>) -> Result<usize> {
        let mut total = Budget::control_storage_bytes()
            .checked_add(self.session.configuration.descriptor_lower_bound()?)
            .ok_or(Error::Overflow)?;
        if let Some(c) = &self.session.pending_update {
            total = total
                .checked_add(c.record().descriptor_lower_bound()?)
                .ok_or(Error::Overflow)?;
        }
        let owners = [
            Some(self.session.configuration.bytes()),
            self.session.pending_update.as_ref().map(AvcUpdate::bytes),
            Some(input.bytes),
            input.new_extradata,
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

    fn establish_storage_bound(
        &mut self,
        state: &PacketState<'_>,
        input: PacketInput<'_>,
    ) -> Result<()> {
        let maximum = if state.length_width.is_some() {
            state.session.maximum_copy_storage
        } else {
            PacketSegmentStorage::required_heap_bytes(state.session.limits.maximum_segments)
                .unwrap_or(usize::MAX)
        };
        // The job's retained source and configuration remain charged to this
        // pool. Its fixed storage is therefore at most the current live charge.
        // If both logical ceilings fit in the remaining capacity, they bound
        // every future append. Actual reservations still check current pressure.
        self.storage_bound = if maximum <= input.bytes.budget().available() {
            PacketStorageBound::LimitsFit
        } else {
            PacketStorageBound::Fixed(state.fixed_live_bytes(input)?)
        };
        Ok(())
    }

    // Known storage capacity lower bound, deduplicating raw caller aliases. A
    // larger allocator replacement peak may still require retryable pressure.
    // All terms are non-negative, so summing the job-invariant terms first
    // overflows exactly when a left-to-right sum of all terms would.
    #[cold]
    #[inline(never)]
    fn check_storage_bound(
        &mut self,
        state: &PacketState<'_>,
        input: PacketInput<'_>,
        segments: usize,
        output: usize,
    ) -> Result<()> {
        if matches!(self.storage_bound, PacketStorageBound::Unchecked) {
            self.establish_storage_bound(state, input)?;
        }
        let PacketStorageBound::Fixed(live_base) = self.storage_bound else {
            return Ok(());
        };
        let minimum = live_base
            .checked_add(Self::added_storage(
                segments,
                output,
                state.length_width.is_some(),
            )?)
            .ok_or(Error::Overflow)?;
        let limit = input.bytes.budget().limit();
        if minimum > limit {
            return Err(Error::Limit {
                requested: minimum,
                maximum: limit,
            });
        }
        Ok(())
    }
    // Share storage accounting across the packet and configuration segments.
    #[inline(never)]
    fn append(
        &mut self,
        state: &mut PacketState<'_>,
        input: PacketInput<'_>,
        segment: NalPacketSegment,
    ) -> Result<()> {
        let parts = state.session.segments.len();
        let maximum_parts = state.session.limits.maximum_segments;
        if parts >= maximum_parts {
            return Err(Error::Limit {
                requested: parts.saturating_add(1),
                maximum: maximum_parts,
            });
        }
        let count = parts + 1;
        let total = state
            .total
            .checked_add(segment.framed_len())
            .ok_or(Error::Overflow)?;
        if total > state.session.limits.maximum_output_bytes {
            return Err(Error::Limit {
                requested: total,
                maximum: state.session.limits.maximum_output_bytes,
            });
        }
        if !matches!(self.storage_bound, PacketStorageBound::LimitsFit) {
            self.check_storage_bound(state, input, count, total)?;
        }
        state.session.segments.try_push_spilling(
            || {
                let pool = input.bytes.budget();
                // Spare slots must leave room for output at the pool's limit.
                // Other reservations can be released before publication.
                let capacity = match self.storage_bound {
                    PacketStorageBound::Fixed(live_base) => {
                        let output = total.saturating_add(Bytes::owner_overhead_bytes());
                        let room = pool
                            .limit()
                            .saturating_sub(live_base)
                            .saturating_sub(output);
                        (room / size_of::<NalPacketSegment>()).min(maximum_parts)
                    }
                    _ => maximum_parts,
                };
                (pool, capacity)
            },
            self.segment_hint,
            segment,
        )?;
        state.total = total;
        Ok(())
    }
}

impl<M> NalPacketJob<'_, M> {
    /// The input packet, until it moves into the output or into a rejection;
    /// `None` after that.
    pub fn source(&self) -> Option<&NalPacket<M>> {
        self.source.as_ref()
    }
    /// After the job failed on a packet that the filter rejects as invalid data
    /// ([`NalPacketReceiverErrorKind`]): the packet and the failure, handed out
    /// once. `None` for every other failure, before a failure and on later
    /// calls. The job keeps returning its error.
    pub fn take_receiver_rejection(&mut self) -> Option<NalPacketRejection<M>> {
        let state = &self.state;
        let kind = state.receiver_failure?;
        let error = state.failure?;
        let source = self.source.take()?;
        Some(NalPacketRejection {
            kind,
            error,
            source,
            configuration: state.session.configuration.bytes().clone(),
            configuration_identity: state.session.configuration.source_identity(),
            configuration_source_refusal: state.session.configuration.refusal(),
            packet_source_refusal: state.packet_source_refusal,
            receiver_epoch: state.session.receiver_epoch,
        })
    }
    /// The same state engine as `step_into`, returning the output directly. Fails as
    /// [`step_into`](Self::step_into) does, without the occupied slot.
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
    ///
    /// Fails with [`Error::Invalid`] for an occupied slot. For the packet:
    /// [`Error::Truncated`] when it ends inside a length field or a NAL unit;
    /// [`Error::Invalid`] for an empty AVC packet, an HEVC NAL length of zero and
    /// update side data that differs from its parsed record; [`Error::Unsupported`] for
    /// an HEVC record that declares an array of a NAL unit type other than 32 to 34, 39
    /// and 40; [`Error::Limit`] past a limit of the session or of the pool;
    /// [`Error::Cancelled`] when `cancel` is set. A job of [`NalPacketSession::packet`]
    /// also fails with the error of [`NalHeader::parse`](crate::nal::NalHeader::parse)
    /// for a NAL header it refuses, and with [`Error::Truncated`] for an AVC IDR NAL
    /// unit of one byte. [`Error::Backpressure`] and [`Error::Allocation`] are
    /// retryable: the job keeps its progress. Every other error is final and returned
    /// again by each later call.
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
            && !retryable(e)
        {
            self.fail(e);
        }
        result
    }
    #[cold]
    #[inline(never)]
    fn fail(&mut self, e: Error) {
        // The filters apply a valid side-data configuration before they parse the
        // payload, so the new configuration survives a malformed packet; in-band
        // parameter sets of a failed packet are never taken.
        if matches!(e, Error::Invalid(_) | Error::Truncated)
            && self.state.update_verified
            && let Some(configuration) = self.state.session.pending_update.take()
        {
            self.state
                .session
                .install_avc(configuration, self.state.length_width);
            self.state.session.receiver_epoch = self.state.epoch;
            self.state.session.avc = AvcState::default();
        }
        // A failed job never adopts its update later.
        self.state.session.pending_update = None;
        self.state.failure = Some(e);
        if let PacketPhase::Emit(emission) = &mut self.phase {
            emission.output = None;
        }
        self.state.session.segments = empty_packet_segments();
    }
}

// It exercises the existing private constructor, not a new test-only API.
#[cfg(test)]
mod packed_plan_boundary_guards {
    use super::*;

    const SPAN: usize = NalPacketLimits::MAXIMUM_SPAN;

    #[test]
    fn maximum_wire_length_preserves_source_and_output_offsets_at_the_span() {
        let length = u32::MAX as usize;
        let range = SPAN - length..SPAN;
        let part = NalPacketSegment::packet(range.clone(), 4, SPAN);
        assert_eq!(
            part.origin(),
            NalPacketOrigin::Packet {
                source_range: range
            }
        );
        assert_eq!(part.len(), length);
        assert_eq!(part.output_offset(), SPAN);
        assert_eq!(part.prefix_bytes(), 4);
        // A synthetic wire-range descriptor may exceed a real 32-bit slice.
        let length = length.min(isize::MAX as usize);
        assert_eq!(
            NalPacketSegment::packet(0..length, 4, 0).framed_len(),
            length + 4
        );
    }

    #[test]
    fn passthrough_covers_a_packet_of_the_full_span() {
        let part = NalPacketSegment::passthrough(SPAN);
        assert_eq!(
            part.origin(),
            NalPacketOrigin::Packet {
                source_range: 0..SPAN
            }
        );
        assert_eq!(part.len(), SPAN);
        assert_eq!(part.framed_len(), SPAN);
        assert_eq!(part.output_offset(), 0);
        assert_eq!(part.prefix_bytes(), 0);
        assert_eq!(
            format!("{part:?}"),
            format!(
                "NalPacketSegment {{ source_range: 0..{SPAN}, nal_index: 0, prefix_and_source: 0, output_offset: 0 }}"
            )
        );
    }

    #[test]
    fn every_configuration_index_boundary_preserves_source_ranges() {
        // A hvcC has at most 255 arrays with 65535 NALs each. The last possible
        // index is checked without allocating a multi-gigabyte record.
        for index in [0, 31, 255, 65_535, 255 * 65_535 - 1] {
            let range = SPAN - 1..SPAN;
            let part = NalPacketSegment::configuration(index, range.clone(), SPAN);
            assert_eq!(
                part.origin(),
                NalPacketOrigin::Configuration {
                    nal_index: index,
                    source_range: range,
                }
            );
            assert_eq!(part.prefix_bytes(), 4);
            assert_eq!(part.output_offset(), SPAN);
            assert_eq!(
                format!("{part:?}"),
                format!(
                    "NalPacketSegment {{ source_range: {}..{SPAN}, nal_index: {index}, prefix_and_source: 132, output_offset: {SPAN} }}",
                    SPAN - 1,
                )
            );
        }
    }

    #[test]
    fn packet_tags_keep_all_prefix_values_and_offsets() {
        let pool = Budget::new(4096).unwrap();
        let raw = Bytes::copy_from_slice(&pool, &[0x41]).unwrap();
        for prefix in [0, 3, 4] {
            let part = NalPacketSegment::packet(0..1, prefix, SPAN);
            assert_eq!(
                part.origin(),
                NalPacketOrigin::Packet { source_range: 0..1 }
            );
            assert_eq!(part.prefix_bytes(), prefix);
            assert_eq!(part.output_offset(), SPAN);
            assert_eq!(&part.backing(&raw, &raw)[part.source_range()], &[0x41]);
            assert_eq!(
                format!("{part:?}"),
                format!(
                    "NalPacketSegment {{ source_range: 0..1, nal_index: 0, prefix_and_source: {prefix}, output_offset: {SPAN} }}",
                )
            );
        }
    }

    #[test]
    fn spans_above_the_maximum_are_limits() {
        assert_eq!(check_span(SPAN), Ok(()));
        if let Some(above) = SPAN.checked_add(1) {
            assert_eq!(
                check_span(above),
                Err(Error::Limit {
                    requested: above,
                    maximum: SPAN,
                })
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
            let mut parts = empty_packet_segments();
            for index in 0..INLINE_PACKET_SEGMENTS {
                parts
                    .try_push_spilling(|| (&pool, usize::MAX), Some(hint), segment(index))
                    .unwrap();
                assert!(matches!(parts, PacketSegmentStorage::Inline { .. }));
                assert_eq!(pool.used(), base);
                assert_eq!(pool.peak(), base);
            }
            parts
                .try_push_spilling(
                    || (&pool, usize::MAX),
                    Some(hint),
                    segment(INLINE_PACKET_SEGMENTS),
                )
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
        let mut parts = empty_packet_segments();
        assert!(parts.as_slice().is_empty());
        for index in 0..INLINE_PACKET_SEGMENTS {
            parts
                .try_push_spilling(|| (&pool, usize::MAX), None, segment(index))
                .unwrap();
            assert_eq!(pool.used(), base);
            assert_eq!(parts.len(), index + 1);
        }
        for index in INLINE_PACKET_SEGMENTS..9 {
            parts
                .try_push_spilling(|| (&pool, usize::MAX), None, segment(index))
                .unwrap();
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
        let mut parts = empty_packet_segments();
        assert!(matches!(parts, PacketSegmentStorage::Inline { .. }));
        assert!(parts.as_slice().is_empty());
        assert_eq!(pool.used(), charge);
        for index in 0..INLINE_PACKET_SEGMENTS {
            parts
                .try_push_spilling(|| (&pool, usize::MAX), Some(9), segment(index))
                .unwrap();
            assert_eq!(parts.len(), index + 1);
            assert_eq!(parts.as_slice()[index], segment(index));
            assert_eq!(pool.used(), charge);
        }
        let before = parts.as_slice().to_vec();
        for _ in 0..2 {
            assert!(matches!(
                parts.try_push_spilling(
                    || (&pool, usize::MAX),
                    Some(9),
                    segment(INLINE_PACKET_SEGMENTS)
                ),
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
            .try_push_spilling(
                || (&pool, usize::MAX),
                Some(9),
                segment(INLINE_PACKET_SEGMENTS),
            )
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
            let mut parts = empty_packet_segments();
            for index in 0..actual {
                parts
                    .try_push_spilling(|| (&pool, usize::MAX), Some(hint), segment(index))
                    .unwrap();
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
