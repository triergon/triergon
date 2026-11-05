//! Explicit observed d32 framing views. The ordinary ISO source parsers stay
//! strict. In particular a three-byte declaration remains a source refusal;
//! its unchanged wire bytes can still drive this separately typed receiver.
use crate::{
    AvcConfigArrayRef, AvcConfigFields, AvcConfigLimits, AvcConfigNal, AvcConfigNalRef,
    AvcConfigParser, AvcConfigSource, AvcConfigViewParser, AvcConfiguration, AvcConfigurationView,
    BuildStep, HvcConfigArray, HvcConfigArrayRef, HvcConfigFields, HvcConfigLimits, HvcConfigNal,
    HvcConfigNalRef, HvcConfigParser, HvcConfigSource, HvcConfigViewParser, HvcConfiguration,
    HvcConfigurationView, LengthSize,
};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, Result, WorkBudget};

pub struct NalObservedAvcConfiguration {
    inner: AvcConfiguration,
}
impl NalObservedAvcConfiguration {
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
    pub fn nals(&self) -> &[AvcConfigNal] {
        self.inner.nals()
    }
    pub fn strict_source_refusal(&self) -> Option<Error> {
        (self.fields().length_size_minus_one() == 2)
            .then_some(Error::Invalid("AVC three-byte sample length"))
    }
    pub(crate) fn record(&self) -> &AvcConfiguration {
        &self.inner
    }
}

pub struct NalObservedHevcConfiguration {
    inner: HvcConfiguration,
}
impl NalObservedHevcConfiguration {
    pub fn source_bytes(&self) -> &Bytes {
        self.inner.source_bytes()
    }
    pub fn source(&self) -> HvcConfigSource {
        self.inner.source()
    }
    pub fn fields(&self) -> HvcConfigFields {
        self.inner.fields()
    }
    pub fn observed_length_size(&self) -> LengthSize {
        self.inner.length_size()
    }
    pub fn arrays(&self) -> &[HvcConfigArray] {
        self.inner.arrays()
    }
    pub fn nals(&self) -> &[HvcConfigNal] {
        self.inner.nals()
    }
    pub fn strict_source_refusal(&self) -> Option<Error> {
        (self.fields().length_size_minus_one() == 2)
            .then_some(Error::Unsupported("hvcC three-byte sample length"))
    }
    pub(crate) fn record(&self) -> &HvcConfiguration {
        &self.inner
    }
}

/// A malformed nonempty version-one AVC update header. Only a completed
/// short-header failure in the explicit update parser can create this receipt.
pub struct NalAvcUpdateRejection {
    raw: Bytes,
    source: AvcConfigSource,
    error: Error,
    strict_source_refusal: Option<Error>,
}
impl NalAvcUpdateRejection {
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
    pub fn native_receive_result(&self) -> i32 {
        -1_094_995_529
    }
}

pub struct NalObservedAvcConfigParser {
    inner: AvcConfigParser,
    raw: Bytes,
    source: AvcConfigSource,
    failure: Option<Error>,
    update_role: bool,
    rejection_taken: bool,
}
impl NalObservedAvcConfigParser {
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
    ) -> Result<BuildStep<NalObservedAvcConfiguration>> {
        let result = self.inner.step(work, cancel);
        match result {
            Ok(BuildStep::Ready(inner)) => {
                Ok(BuildStep::Ready(NalObservedAvcConfiguration { inner }))
            }
            Ok(BuildStep::Yield) => Ok(BuildStep::Yield),
            Ok(BuildStep::Complete) => Ok(BuildStep::Complete),
            Err(error) => {
                if !matches!(error, Error::Backpressure { .. } | Error::Allocation) {
                    self.failure = Some(error);
                }
                Err(error)
            }
        }
    }
    pub fn take_update_rejection(&mut self) -> Option<NalAvcUpdateRejection> {
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
        Some(NalAvcUpdateRejection {
            raw: self.raw.clone(),
            source: self.source,
            error,
            strict_source_refusal: self.strict_source_refusal(),
        })
    }
}

pub struct NalObservedHevcConfigParser {
    inner: HvcConfigParser,
}
impl NalObservedHevcConfigParser {
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
    ) -> Result<BuildStep<NalObservedHevcConfiguration>> {
        match self.inner.step(work, cancel)? {
            BuildStep::Ready(inner) => Ok(BuildStep::Ready(NalObservedHevcConfiguration { inner })),
            BuildStep::Yield => Ok(BuildStep::Yield),
            BuildStep::Complete => Ok(BuildStep::Complete),
        }
    }
}

// Additive observed compact views. The AVC wrapper retains the same fixed
// per-record raw handle required for source queries and short-update receipts;
// it creates no eager NAL/array aliases and leaves the existing wrappers intact.
pub struct NalObservedAvcConfigurationView {
    inner: AvcConfigurationView,
}
impl NalObservedAvcConfigurationView {
    pub(crate) fn view(&self) -> &AvcConfigurationView {
        &self.inner
    }
    pub fn arrays(
        &self,
    ) -> impl ExactSizeIterator<Item = AvcConfigArrayRef<'_>> + DoubleEndedIterator + '_ {
        self.inner.arrays()
    }
    pub fn array(&self, index: usize) -> Option<AvcConfigArrayRef<'_>> {
        self.inner.array(index)
    }
    pub fn trailing_bytes(&self) -> &[u8] {
        self.inner.trailing_bytes()
    }
    pub fn retain_trailing_bytes(&self) -> Result<Bytes> {
        self.inner.retain_trailing_bytes()
    }
    pub fn nal(&self, index: usize) -> Option<AvcConfigNalRef<'_>> {
        self.inner.nal(index)
    }
    pub fn nal_count(&self) -> usize {
        self.inner.nal_count()
    }
    pub fn array_count(&self) -> usize {
        self.inner.array_count()
    }
    pub fn understood_end(&self) -> usize {
        self.inner.understood_end()
    }
    pub fn descriptor_storage_bytes(&self) -> Result<usize> {
        self.inner.descriptor_storage_bytes()
    }
    pub fn retained_storage_bytes(&self) -> Result<usize> {
        self.inner.retained_storage_bytes()
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
    ) -> impl ExactSizeIterator<Item = AvcConfigNalRef<'_>> + DoubleEndedIterator + '_ {
        self.inner.nals()
    }
    pub fn strict_source_refusal(&self) -> Option<Error> {
        (self.fields().length_size_minus_one() == 2)
            .then_some(Error::Invalid("AVC three-byte sample length"))
    }
}

pub struct NalObservedHevcConfigurationView {
    inner: HvcConfigurationView,
}
impl NalObservedHevcConfigurationView {
    pub(crate) fn view(&self) -> &HvcConfigurationView {
        &self.inner
    }
    pub fn array(&self, index: usize) -> Option<HvcConfigArrayRef<'_>> {
        self.inner.array(index)
    }
    pub fn opaque_suffix(&self) -> &[u8] {
        self.inner.opaque_suffix()
    }
    pub fn retain_opaque_suffix(&self) -> Result<Bytes> {
        self.inner.retain_opaque_suffix()
    }
    pub fn syntax_issues(&self) -> u32 {
        self.inner.syntax_issues()
    }
    pub fn nal(&self, index: usize) -> Option<HvcConfigNalRef<'_>> {
        self.inner.nal(index)
    }
    pub fn nal_count(&self) -> usize {
        self.inner.nal_count()
    }
    pub fn array_count(&self) -> usize {
        self.inner.array_count()
    }
    pub fn understood_end(&self) -> usize {
        self.inner.understood_end()
    }
    pub fn descriptor_storage_bytes(&self) -> Result<usize> {
        self.inner.descriptor_storage_bytes()
    }
    pub fn retained_storage_bytes(&self) -> Result<usize> {
        self.inner.retained_storage_bytes()
    }

    pub fn source_bytes(&self) -> &Bytes {
        self.inner.source_bytes()
    }
    pub fn source(&self) -> HvcConfigSource {
        self.inner.source()
    }
    pub fn fields(&self) -> HvcConfigFields {
        self.inner.fields()
    }
    pub fn observed_length_size(&self) -> LengthSize {
        self.inner.length_size()
    }
    pub fn arrays(
        &self,
    ) -> impl ExactSizeIterator<Item = HvcConfigArrayRef<'_>> + DoubleEndedIterator + '_ {
        self.inner.arrays()
    }
    pub fn nals(
        &self,
    ) -> impl ExactSizeIterator<Item = HvcConfigNalRef<'_>> + DoubleEndedIterator + '_ {
        self.inner.nals()
    }
    pub fn strict_source_refusal(&self) -> Option<Error> {
        (self.fields().length_size_minus_one() == 2)
            .then_some(Error::Unsupported("hvcC three-byte sample length"))
    }
}

pub struct NalObservedAvcConfigViewParser {
    inner: AvcConfigViewParser,
    raw: Bytes,
    source: AvcConfigSource,
    failure: Option<Error>,
    update_role: bool,
    rejection_taken: bool,
}
impl NalObservedAvcConfigViewParser {
    pub fn new(
        pool: &Budget,
        source: AvcConfigSource,
        raw: Bytes,
        limits: AvcConfigLimits,
    ) -> Result<Self> {
        let inner = AvcConfigViewParser::new_observed_width(pool, source, raw.clone(), limits)?;
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
    ) -> Result<BuildStep<NalObservedAvcConfigurationView>> {
        let result = self.inner.step(work, cancel);
        match result {
            Ok(BuildStep::Ready(inner)) => {
                Ok(BuildStep::Ready(NalObservedAvcConfigurationView { inner }))
            }
            Ok(BuildStep::Yield) => Ok(BuildStep::Yield),
            Ok(BuildStep::Complete) => Ok(BuildStep::Complete),
            Err(error) => {
                if !matches!(error, Error::Backpressure { .. } | Error::Allocation) {
                    self.failure = Some(error);
                }
                Err(error)
            }
        }
    }
    pub fn take_update_rejection(&mut self) -> Option<NalAvcUpdateRejection> {
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
        Some(NalAvcUpdateRejection {
            raw: self.raw.clone(),
            source: self.source,
            error,
            strict_source_refusal: self.strict_source_refusal(),
        })
    }
}

pub struct NalObservedHevcConfigViewParser {
    inner: HvcConfigViewParser,
}
impl NalObservedHevcConfigViewParser {
    pub fn new(
        pool: &Budget,
        source: HvcConfigSource,
        raw: Bytes,
        limits: HvcConfigLimits,
    ) -> Result<Self> {
        Ok(Self {
            inner: HvcConfigViewParser::new_observed_width(pool, source, raw, limits)?,
        })
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<NalObservedHevcConfigurationView>> {
        match self.inner.step(work, cancel)? {
            BuildStep::Ready(inner) => {
                Ok(BuildStep::Ready(NalObservedHevcConfigurationView { inner }))
            }
            BuildStep::Yield => Ok(BuildStep::Yield),
            BuildStep::Complete => Ok(BuildStep::Complete),
        }
    }
}
