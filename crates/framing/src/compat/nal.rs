//! The NAL packet receiver's configuration acceptance.
//!
//! [`NalPacketSession`](crate::nal::NalPacketSession) frames length-prefixed AVC
//! and HEVC samples as FFmpeg's `h264_mp4toannexb` and `hevc_mp4toannexb`
//! bitstream filters do. The record parsers of the format modules
//! ([`AvcConfigParser`], [`HvcConfigParser`]) refuse a three-byte NAL length
//! (`lengthSizeMinusOne = 2`); the filters accept it. The parsers here accept what
//! the receiver accepts and keep the strict refusal readable through
//! `strict_source_refusal()`.
//!
//! Pass their output as
//! [`NalPacketConfiguration::ReceiverAvc`](crate::nal::NalPacketConfiguration::ReceiverAvc)
//! or [`ReceiverHevc`](crate::nal::NalPacketConfiguration::ReceiverHevc), and AVC
//! side-data updates to
//! [`NalPacketSession::receiver_packet`](crate::nal::NalPacketSession::receiver_packet).
//! That path also frames NAL units whose header the strict path refuses and records
//! the refusal on the output or rejection.
//!
//! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.
use crate::avc::config::profile_extension_required;
use crate::nal::packet::NalConfigurationRefusal;
use crate::{
    BuildStep, ConfigProgress,
    avc::{
        AvcConfigArray, AvcConfigFields, AvcConfigLimits, AvcConfigNal, AvcConfigParser,
        AvcConfigSource, AvcConfiguration,
    },
    hevc::{HvcConfigFields, HvcConfigLimits, HvcConfigParser, HvcConfigSource, HvcConfiguration},
    nal::LengthSize,
};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, Result, WorkBudget};

/// An AVC configuration update that the filter rejects together with its packet:
/// a record of `configurationVersion` 1 that is shorter than seven bytes and so
/// ends before its PPS count. Only a parser made with
/// [`ReceiverAvcConfigParser::new_update`] creates it, after it failed with
/// [`Error::Truncated`].
#[derive(Debug)]
pub struct ReceiverAvcUpdateRejection {
    raw: Bytes,
    source: AvcConfigSource,
    error: Error,
    strict_source_refusal: Option<Error>,
}
impl ReceiverAvcUpdateRejection {
    /// The update record as it was given.
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    /// Where the caller says the record came from.
    pub fn source(&self) -> AvcConfigSource {
        self.source
    }
    /// The parser's error: [`Error::Truncated`].
    pub fn source_error(&self) -> Error {
        self.error
    }
    /// The error with which [`AvcConfigParser`] refuses the record before it
    /// reaches the truncation: [`Error::Invalid`] for a three-byte NAL length in
    /// a record of six bytes. `None` when it stops at the truncation too.
    pub fn strict_source_refusal(&self) -> Option<Error> {
        self.strict_source_refusal
    }
    /// FFmpeg's return code for the packet that carries this update:
    /// `AVERROR_INVALIDDATA`. It is a constant for comparison, compiled with the
    /// `reference-trace` feature; FFmpeg is not called.
    #[cfg(feature = "reference-trace")]
    pub fn native_receive_result(&self) -> i32 {
        -1_094_995_529
    }
}

/// An AVC configuration record as FFmpeg's `h264_mp4toannexb` filter accepts it.
///
/// Its SPS and PPS arrays are complete. Unlike an [`AvcConfiguration`], it may
/// declare a three-byte NAL length, and a record whose `AVCProfileIndication`
/// is 100, 110, 122 or 144 may end inside its four-byte extension header, or
/// directly after one that declares SPS extension entries.
/// [`strict_source_refusal`](Self::strict_source_refusal) tells such a record
/// from one that [`AvcConfigParser`] accepts.
pub struct ReceiverAvcConfiguration {
    inner: AvcConfiguration,
    refusal: NalConfigurationRefusal,
}
impl std::fmt::Debug for ReceiverAvcConfiguration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReceiverAvcConfiguration")
            .field("record", &self.inner)
            .field("strict_source_refusal", &self.strict_source_refusal())
            .finish_non_exhaustive()
    }
}
impl ReceiverAvcConfiguration {
    fn from_record(inner: AvcConfiguration) -> Self {
        let fields = inner.fields();
        let refusal = if fields.length_size_minus_one() == 2 {
            NalConfigurationRefusal::AvcWidthThree
        } else if profile_extension_required(fields.profile_indication())
            && fields.raw_extension().is_none_or(|header| {
                let parsed = inner
                    .arrays()
                    .next_back()
                    .map_or(0, |array| array.nal_indices().len());
                usize::from(header[3]) != parsed
            })
        {
            NalConfigurationRefusal::AvcExtensionTruncated
        } else {
            NalConfigurationRefusal::None
        };
        Self { inner, refusal }
    }

    pub(crate) fn record(&self) -> &AvcConfiguration {
        &self.inner
    }
    pub(crate) const fn refusal(&self) -> NalConfigurationRefusal {
        self.refusal
    }
    /// The array at `index`; `None` past the last.
    pub fn array(&self, index: usize) -> Option<AvcConfigArray<'_>> {
        self.inner.array(index)
    }
    /// The record's bytes after its last array. The zero to three bytes of an
    /// extension header that is cut short are among them.
    pub fn trailing_bytes(&self) -> &[u8] {
        self.inner.trailing_bytes()
    }
    /// NAL units in all arrays together.
    pub fn nal_count(&self) -> usize {
        self.inner.nal_count()
    }

    /// The whole record as it was given.
    pub fn source_bytes(&self) -> &Bytes {
        self.inner.source_bytes()
    }
    /// Where the caller says the record came from.
    pub fn source(&self) -> AvcConfigSource {
        self.inner.source()
    }
    /// The record's fixed fields.
    pub fn fields(&self) -> AvcConfigFields {
        self.inner.fields()
    }
    /// Width of the length field before each NAL unit of a sample,
    /// `lengthSizeMinusOne + 1`: 1 to 4 bytes, three among them.
    pub fn observed_length_size(&self) -> LengthSize {
        self.inner.sample_length_size()
    }
    /// The NAL units of all arrays in record order.
    pub fn nals(
        &self,
    ) -> impl ExactSizeIterator<Item = AvcConfigNal<'_>> + DoubleEndedIterator + '_ {
        self.inner.nals()
    }
    /// The error with which [`AvcConfigParser`] refuses this record:
    /// [`Error::Invalid`] for a three-byte NAL length, otherwise
    /// [`Error::Truncated`] for an extension that is cut short. `None` when it
    /// accepts the record.
    pub fn strict_source_refusal(&self) -> Option<Error> {
        self.refusal.error()
    }
}

/// An HEVC configuration record as FFmpeg's `hevc_mp4toannexb` filter accepts
/// it: what an [`HvcConfiguration`] holds, and also a record that declares a
/// three-byte NAL length.
#[derive(Debug)]
pub struct ReceiverHevcConfiguration {
    inner: HvcConfiguration,
}
impl ReceiverHevcConfiguration {
    pub(crate) fn record(&self) -> &HvcConfiguration {
        &self.inner
    }
    /// The record's bytes after its last array.
    pub fn opaque_suffix(&self) -> &[u8] {
        self.inner.opaque_suffix()
    }
    /// The bit set of [`HvcConfiguration::syntax_issues`]; bit 5 marks the
    /// three-byte NAL length.
    pub fn syntax_issues(&self) -> u32 {
        self.inner.syntax_issues()
    }
    /// NAL units in all arrays together.
    pub fn nal_count(&self) -> usize {
        self.inner.nal_count()
    }

    /// The whole record as it was given.
    pub fn source_bytes(&self) -> &Bytes {
        self.inner.source_bytes()
    }
    /// The fields of the record's 23-byte fixed part.
    pub fn fields(&self) -> HvcConfigFields {
        self.inner.fields()
    }
    /// Width of the length field before each NAL unit of a sample,
    /// `lengthSizeMinusOne + 1`: 1 to 4 bytes, three among them.
    pub fn observed_length_size(&self) -> LengthSize {
        self.inner.length_size()
    }
    /// [`Error::Unsupported`], with which [`HvcConfigParser`] refuses a
    /// three-byte NAL length; `None` for a record it accepts.
    pub fn strict_source_refusal(&self) -> Option<Error> {
        (self.fields().length_size_minus_one() == 2)
            .then_some(Error::Unsupported("hvcC three-byte sample length"))
    }
}

/// Parses an AVC configuration record with the acceptance of FFmpeg's
/// `h264_mp4toannexb` filter: a session's initial configuration
/// ([`new`](Self::new)) or the `new_extradata` side data of a packet
/// ([`new_update`](Self::new_update)).
pub struct ReceiverAvcConfigParser {
    inner: AvcConfigParser,
    raw: Bytes,
    refusal: NalConfigurationRefusal,
    update_role: bool,
    rejection_taken: bool,
}
impl std::fmt::Debug for ReceiverAvcConfigParser {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReceiverAvcConfigParser")
            .field("parser", &self.inner)
            .field("update", &self.update_role)
            .field("strict_source_refusal", &self.strict_source_refusal())
            .finish_non_exhaustive()
    }
}
impl ReceiverAvcConfigParser {
    /// A parser for `raw`, the record of a session's initial configuration (the payload
    /// of an `avcC` box), charged to `pool`. Fails with [`Error::Invalid`] when a byte
    /// limit is zero; [`Error::ForeignPool`] when `raw` is charged to another pool;
    /// [`Error::Limit`] when it exceeds `maximum_record_bytes`; [`Error::Truncated`]
    /// when it is empty.
    pub fn new(
        pool: &Budget,
        source: AvcConfigSource,
        raw: Bytes,
        limits: AvcConfigLimits,
    ) -> Result<Self> {
        Self::with_role(pool, source, raw, limits, false)
    }

    // Initial configuration and updates have the same construction checks.
    #[inline(never)]
    fn with_role(
        pool: &Budget,
        source: AvcConfigSource,
        raw: Bytes,
        limits: AvcConfigLimits,
        update_role: bool,
    ) -> Result<Self> {
        let inner = AvcConfigParser::new_receiver(pool, source, raw.clone(), limits)?;
        let refusal = if raw.len() >= 6 && raw[0] == 1 && raw[4] & 3 == 2 {
            NalConfigurationRefusal::AvcWidthThree
        } else {
            NalConfigurationRefusal::None
        };
        Ok(Self {
            inner,
            raw,
            refusal,
            update_role,
            rejection_taken: false,
        })
    }
    /// A parser for `raw`, the nonempty `new_extradata` side data of a packet,
    /// which replaces the session's configuration. Side data that is present
    /// and empty is no update: the session forwards it and takes no record.
    /// Only a parser made here reports
    /// [`take_update_rejection`](Self::take_update_rejection).
    ///
    /// Fails as [`new`](Self::new) does.
    pub fn new_update(
        pool: &Budget,
        source: AvcConfigSource,
        raw: Bytes,
        limits: AvcConfigLimits,
    ) -> Result<Self> {
        Self::with_role(pool, source, raw, limits, true)
    }
    /// The record as it was given.
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    /// The error with which [`AvcConfigParser`] refuses the record, as far as
    /// it is established: a three-byte NAL length from construction on, an
    /// extension that is cut short once the record is published.
    pub fn strict_source_refusal(&self) -> Option<Error> {
        self.refusal.error()
    }
    /// Parses the record. One work unit reads at most the six fixed bytes, or
    /// the length and header of one NAL unit.
    ///
    /// Fails with [`Error::Unsupported`] for a `configurationVersion` other than 1;
    /// [`Error::Truncated`] for a record that ends inside its fixed bytes, its SPS and
    /// PPS arrays or an SPS extension entry; [`Error::Invalid`] for a NAL length of
    /// zero and a refused NAL header; [`Error::Limit`] past `maximum_nals` or
    /// `maximum_nal_bytes`; [`Error::Cancelled`] when `cancel` is set.
    /// [`Error::Backpressure`] and [`Error::Allocation`] are retryable: the parser
    /// keeps its progress. Every other error is final and returned again by each later
    /// call.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<ReceiverAvcConfiguration>> {
        let mut ready = None;
        self.step_into(work, cancel, &mut ready)?
            .into_build_step(ready)
    }

    /// Publishes into an empty caller-provided slot. Occupied slots are rejected before
    /// any state, work or retained storage changes, including update rejections. Yield,
    /// completion and errors leave an initially empty slot empty. Fails with
    /// [`Error::Invalid`] for an occupied slot, after which the parser can be stepped
    /// again; otherwise as [`step`](Self::step).
    pub fn step_into(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        ready: &mut Option<ReceiverAvcConfiguration>,
    ) -> Result<ConfigProgress> {
        self.inner.step_into_with(work, cancel, ready, |inner| {
            let record = ReceiverAvcConfiguration::from_record(inner);
            self.refusal = record.refusal();
            record
        })
    }
    /// After an update parser failed with [`Error::Truncated`] on a record of
    /// `configurationVersion` 1 that is shorter than seven bytes: the record
    /// and the error, once. `None` for a parser made with [`new`](Self::new),
    /// for every other failure, before a failure and on later calls.
    pub fn take_update_rejection(&mut self) -> Option<ReceiverAvcUpdateRejection> {
        // A complete AVC record needs its six-byte prefix and a PPS count. Only a
        // record that ends before its PPS count is rejected with its packet; every
        // other failure is the parser's own.
        if !self.update_role || self.rejection_taken || self.raw.len() >= 7 {
            return None;
        }
        let error = self.inner.failure()?;
        if error != Error::Truncated || self.raw.first().copied() != Some(1) {
            return None;
        }
        self.rejection_taken = true;
        Some(ReceiverAvcUpdateRejection {
            raw: self.raw.clone(),
            source: self.inner.source(),
            error,
            strict_source_refusal: self.strict_source_refusal(),
        })
    }
}

/// Parses an HEVC configuration record with the acceptance of FFmpeg's
/// `hevc_mp4toannexb` filter: as [`HvcConfigParser`] does, and also with a
/// three-byte NAL length.
#[derive(Debug)]
pub struct ReceiverHevcConfigParser {
    inner: HvcConfigParser,
}
impl ReceiverHevcConfigParser {
    /// A parser for `raw`, the record of a session's configuration (the payload of an
    /// `hvcC` box), charged to `pool`. Fails with [`Error::Invalid`] when a byte limit
    /// is zero; [`Error::ForeignPool`] when `raw` is charged to another pool;
    /// [`Error::Limit`] when it exceeds `maximum_record_bytes`; [`Error::Truncated`]
    /// when it is empty.
    pub fn new(
        pool: &Budget,
        source: HvcConfigSource,
        raw: Bytes,
        limits: HvcConfigLimits,
    ) -> Result<Self> {
        Ok(Self {
            inner: HvcConfigParser::new_observed_width(pool, source, raw, limits)?,
        })
    }
    /// Parses the record. One work unit reads at most the 23 fixed bytes, one
    /// array header, or the length and header of one NAL unit.
    ///
    /// Fails with [`Error::Unsupported`] for a `configurationVersion` other than 1;
    /// [`Error::Truncated`] for a record that ends inside its fixed bytes, an array
    /// header or a NAL unit; [`Error::Invalid`] for a NAL length of zero and a refused
    /// NAL header; [`Error::Limit`] past `maximum_arrays`, `maximum_nals` or
    /// `maximum_nal_bytes`; [`Error::Cancelled`] when `cancel` is set.
    /// [`Error::Backpressure`] and [`Error::Allocation`] are retryable: the parser
    /// keeps its progress. Every other error is final and returned again by each later
    /// call.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<ReceiverHevcConfiguration>> {
        let mut ready = None;
        self.step_into(work, cancel, &mut ready)?
            .into_build_step(ready)
    }

    /// Publishes into an empty caller-provided slot. Occupied slots are rejected before
    /// any state, work or retained storage changes. Yield, completion and errors leave
    /// an initially empty slot empty. Fails with [`Error::Invalid`] for an occupied
    /// slot, after which the parser can be stepped again; otherwise as
    /// [`step`](Self::step).
    pub fn step_into(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        ready: &mut Option<ReceiverHevcConfiguration>,
    ) -> Result<ConfigProgress> {
        self.inner
            .step_into_with(work, cancel, ready, |inner| ReceiverHevcConfiguration {
                inner,
            })
    }
}
