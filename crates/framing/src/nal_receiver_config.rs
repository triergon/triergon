//! The packet receiver's configuration acceptance ([`crate::compat::nal`]).
//! The ordinary ISO source parsers stay strict. In particular a three-byte
//! declaration remains a source refusal; its unchanged wire bytes can still drive
//! this separately typed receiver.
use crate::{
    AvcConfigArray, AvcConfigFields, AvcConfigLimits, AvcConfigNal, AvcConfigParser,
    AvcConfigSource, AvcConfiguration, BuildStep, ConfigProgress, HvcConfigArray, HvcConfigFields,
    HvcConfigLimits, HvcConfigNal, HvcConfigParser, HvcConfigSource, HvcConfiguration, LengthSize,
};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, Result, WorkBudget};

/// A malformed nonempty version-one AVC update header. Only a completed
/// short-header failure in the explicit update parser can create this rejection.
pub struct ReceiverAvcUpdateRejection {
    raw: Bytes,
    source: AvcConfigSource,
    error: Error,
    strict_source_refusal: Option<Error>,
}
impl ReceiverAvcUpdateRejection {
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    pub fn source(&self) -> AvcConfigSource {
        self.source
    }
    pub fn source_error(&self) -> Error {
        self.error
    }
    /// The ordinary source parser may refuse width three before this observed
    /// record walk reaches its later physical error. Preserve both facts.
    pub fn strict_source_refusal(&self) -> Option<Error> {
        self.strict_source_refusal
    }
    /// Observed public receive result; this is not an executed native call.
    /// Comparison-only.
    #[cfg(feature = "reference-trace")]
    pub fn native_receive_result(&self) -> i32 {
        -1_094_995_529
    }
}

pub struct ReceiverAvcConfiguration {
    inner: AvcConfiguration,
}
impl ReceiverAvcConfiguration {
    pub(crate) fn record(&self) -> &AvcConfiguration {
        &self.inner
    }
    pub fn arrays(
        &self,
    ) -> impl ExactSizeIterator<Item = AvcConfigArray<'_>> + DoubleEndedIterator + '_ {
        self.inner.arrays()
    }
    pub fn array(&self, index: usize) -> Option<AvcConfigArray<'_>> {
        self.inner.array(index)
    }
    pub fn trailing_bytes(&self) -> &[u8] {
        self.inner.trailing_bytes()
    }
    pub fn nal(&self, index: usize) -> Option<AvcConfigNal<'_>> {
        self.inner.nal(index)
    }
    pub fn nal_count(&self) -> usize {
        self.inner.nal_count()
    }
    pub fn array_count(&self) -> usize {
        self.inner.array_count()
    }

    pub fn source_bytes(&self) -> &Bytes {
        self.inner.source_bytes()
    }
    pub fn source(&self) -> AvcConfigSource {
        self.inner.source()
    }
    pub fn fields(&self) -> AvcConfigFields {
        self.inner.fields()
    }
    pub fn observed_length_size(&self) -> LengthSize {
        self.inner.sample_length_size()
    }
    pub fn nals(
        &self,
    ) -> impl ExactSizeIterator<Item = AvcConfigNal<'_>> + DoubleEndedIterator + '_ {
        self.inner.nals()
    }
    pub fn strict_source_refusal(&self) -> Option<Error> {
        (self.fields().length_size_minus_one() == 2)
            .then_some(Error::Invalid("AVC three-byte sample length"))
    }
}

pub struct ReceiverHevcConfiguration {
    inner: HvcConfiguration,
}
impl ReceiverHevcConfiguration {
    pub(crate) fn record(&self) -> &HvcConfiguration {
        &self.inner
    }
    pub fn array(&self, index: usize) -> Option<HvcConfigArray<'_>> {
        self.inner.array(index)
    }
    pub fn opaque_suffix(&self) -> &[u8] {
        self.inner.opaque_suffix()
    }
    pub fn syntax_issues(&self) -> u32 {
        self.inner.syntax_issues()
    }
    pub fn nal(&self, index: usize) -> Option<HvcConfigNal<'_>> {
        self.inner.nal(index)
    }
    pub fn nal_count(&self) -> usize {
        self.inner.nal_count()
    }
    pub fn array_count(&self) -> usize {
        self.inner.array_count()
    }

    pub fn source_bytes(&self) -> &Bytes {
        self.inner.source_bytes()
    }
    pub fn fields(&self) -> HvcConfigFields {
        self.inner.fields()
    }
    pub fn observed_length_size(&self) -> LengthSize {
        self.inner.length_size()
    }
    pub fn arrays(
        &self,
    ) -> impl ExactSizeIterator<Item = HvcConfigArray<'_>> + DoubleEndedIterator + '_ {
        self.inner.arrays()
    }
    pub fn nals(
        &self,
    ) -> impl ExactSizeIterator<Item = HvcConfigNal<'_>> + DoubleEndedIterator + '_ {
        self.inner.nals()
    }
    pub fn strict_source_refusal(&self) -> Option<Error> {
        (self.fields().length_size_minus_one() == 2)
            .then_some(Error::Unsupported("hvcC three-byte sample length"))
    }
}

pub struct ReceiverAvcConfigParser {
    inner: AvcConfigParser,
    raw: Bytes,
    source: AvcConfigSource,
    failure: Option<Error>,
    update_role: bool,
    rejection_taken: bool,
}
impl ReceiverAvcConfigParser {
    pub fn new(
        pool: &Budget,
        source: AvcConfigSource,
        raw: Bytes,
        limits: AvcConfigLimits,
    ) -> Result<Self> {
        let inner = AvcConfigParser::new_observed_width(pool, source, raw.clone(), limits)?;
        Ok(Self {
            inner,
            raw,
            source,
            failure: None,
            update_role: false,
            rejection_taken: false,
        })
    }
    /// Explicit nonempty packet-side-data operation, not initial configuration.
    /// Present empty side data remains the existing forwarded-empty policy.
    pub fn new_update(
        pool: &Budget,
        source: AvcConfigSource,
        raw: Bytes,
        limits: AvcConfigLimits,
    ) -> Result<Self> {
        let mut parser = Self::new(pool, source, raw, limits)?;
        parser.update_role = true;
        Ok(parser)
    }
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    pub fn strict_source_refusal(&self) -> Option<Error> {
        (self.raw.len() >= 6 && self.raw[0] == 1 && self.raw[4] & 3 == 2)
            .then_some(Error::Invalid("AVC three-byte sample length"))
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<ReceiverAvcConfiguration>> {
        let mut ready = None;
        match self.step_into(work, cancel, &mut ready)? {
            ConfigProgress::Ready => Ok(BuildStep::Ready(
                ready.ok_or(Error::Invalid("missing published configuration"))?,
            )),
            ConfigProgress::Yield => Ok(BuildStep::Yield),
            ConfigProgress::Complete => Ok(BuildStep::Complete),
        }
    }

    /// Publishes into an empty caller-provided slot. Occupied slots are rejected
    /// before any state, work or retained storage changes, including update rejections.
    /// Yield, completion and errors leave an initially empty slot empty.
    pub fn step_into(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        ready: &mut Option<ReceiverAvcConfiguration>,
    ) -> Result<ConfigProgress> {
        if ready.is_some() {
            return Err(Error::Invalid("configuration output sink occupied"));
        }
        let result =
            self.inner
                .step_into_with(work, cancel, ready, |inner| ReceiverAvcConfiguration {
                    inner,
                });
        if let Err(error) = result
            && !matches!(error, Error::Backpressure { .. } | Error::Allocation)
        {
            self.failure = Some(error);
        }
        result
    }
    pub fn take_update_rejection(&mut self) -> Option<ReceiverAvcUpdateRejection> {
        // A complete AVC record requires the six-byte prefix and a PPS count.
        // No native result is inferred for other strict syntax/version errors,
        // cancellation, resource refusals or failures after that header.
        if !self.update_role || self.rejection_taken || self.raw.len() >= 7 {
            return None;
        }
        let error = self.failure?;
        if error != Error::Truncated || self.raw.first().copied() != Some(1) {
            return None;
        }
        self.rejection_taken = true;
        Some(ReceiverAvcUpdateRejection {
            raw: self.raw.clone(),
            source: self.source,
            error,
            strict_source_refusal: self.strict_source_refusal(),
        })
    }
}

pub struct ReceiverHevcConfigParser {
    inner: HvcConfigParser,
}
impl ReceiverHevcConfigParser {
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
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<ReceiverHevcConfiguration>> {
        let mut ready = None;
        match self.step_into(work, cancel, &mut ready)? {
            ConfigProgress::Ready => Ok(BuildStep::Ready(
                ready.ok_or(Error::Invalid("missing published configuration"))?,
            )),
            ConfigProgress::Yield => Ok(BuildStep::Yield),
            ConfigProgress::Complete => Ok(BuildStep::Complete),
        }
    }

    /// Publishes into an empty caller-provided slot. Occupied slots are rejected
    /// before any state, work or retained storage changes. Yield, completion and errors
    /// leave an initially empty slot empty.
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
