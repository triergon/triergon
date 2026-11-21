use crate::retryable;
use crate::{
    BuildStep,
    nal::{Codec, Framing, NalReader, NalUnit, ParameterSetKind, ReaderLimits},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputWindow, Result, SourceCapabilities, SourceId, Step,
    WorkBudget,
};

/// How the source bytes of a [`Configuration`] are encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigEncoding {
    /// Parameter sets behind Annex B start codes.
    AnnexBParameterSets,
    /// An `AVCDecoderConfigurationRecord` of ISO/IEC 14496-15: the payload of an
    /// `avcC` box.
    AvcDecoderConfigurationRecord,
    /// An `HEVCDecoderConfigurationRecord` of ISO/IEC 14496-15: the payload of an
    /// `hvcC` box.
    HevcDecoderConfigurationRecord,
}

/// How much of a configuration's source bytes was checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigInspection {
    /// Record bytes and externally declared sample framing only. No record
    /// syntax, `lengthSizeMinusOne`, arrays, profile, completeness or validity is
    /// inferred: the ISO/IEC 14496-15 record is not parsed.
    DeclaredOpaque,
    /// Annex-B boundaries and fixed NAL headers checked. Parameter-set contents
    /// and completeness/consistency are not parsed or certified.
    AnnexBFramed,
}

/// Bounds of a [`Configuration`] and of the reader that frames an Annex B one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigLimits {
    /// The most source bytes a configuration may hold. Must not be zero.
    pub maximum_source_bytes: usize,
    /// The most parameter sets kept. [`AnnexBConfigParser`] reserves their
    /// descriptors at its first step. Must not be zero.
    pub maximum_parameter_sets: usize,
    /// Bounds of the [`NalReader`] that frames an Annex B configuration; they
    /// are validated for a declared record too.
    pub reader: ReaderLimits,
}

impl ConfigLimits {
    fn validate(self) -> Result<()> {
        self.reader.validate()?;
        if self.maximum_source_bytes == 0 || self.maximum_parameter_sets == 0 {
            return Err(Error::Invalid("zero configuration policy"));
        }
        Ok(())
    }

    fn check_source(self, pool: &Budget, source: &Bytes) -> Result<()> {
        self.validate()?;
        if !source.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        if source.len() > self.maximum_source_bytes {
            return Err(Error::Limit {
                requested: source.len(),
                maximum: self.maximum_source_bytes,
            });
        }
        if source.is_empty() {
            return Err(Error::Invalid("empty configuration"));
        }
        Ok(())
    }
}

/// One parameter set of an Annex B [`Configuration`].
#[derive(Clone, Debug)]
pub struct ParameterSet {
    kind: ParameterSetKind,
    unit: NalUnit,
}

impl ParameterSet {
    /// The parameter set its `nal_unit_type` names.
    pub const fn kind(&self) -> ParameterSetKind {
        self.kind
    }
    /// The NAL unit, a span of the configuration's source bytes, with its
    /// offsets and start code there.
    pub fn unit(&self) -> &NalUnit {
        &self.unit
    }
}

/// Preserves the complete source configuration, its declared encoding and the
/// independently declared framing of media samples. Parameter-set order and
/// duplicates are preserved. This is not a decoder-ready/complete-set claim.
#[derive(Debug)]
pub struct Configuration {
    codec: Codec,
    encoding: ConfigEncoding,
    inspection: ConfigInspection,
    source: Bytes,
    sample_framing: Framing,
    parameters: BudgetVec<ParameterSet>,
}

impl Configuration {
    /// Holds an unparsed ISO record without guessing sample framing. The caller
    /// must supply the framing fact from a separately validated source. A wrong
    /// or conflicting declaration cannot be detected by this opaque constructor.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] when `source` is empty, a limit that must not be zero
    /// is, or `encoding` is not the record of `codec`; [`Error::ForeignPool`]
    /// when `source` is charged to another pool; [`Error::Limit`] when it exceeds
    /// `maximum_source_bytes`.
    pub fn declared_record(
        pool: &Budget,
        codec: Codec,
        encoding: ConfigEncoding,
        source: Bytes,
        sample_framing: Framing,
        limits: ConfigLimits,
    ) -> Result<Self> {
        limits.check_source(pool, &source)?;
        if !matches!(
            (codec, encoding),
            (Codec::Avc, ConfigEncoding::AvcDecoderConfigurationRecord)
                | (Codec::Hevc, ConfigEncoding::HevcDecoderConfigurationRecord)
        ) {
            return Err(Error::Invalid("configuration encoding/codec mismatch"));
        }
        Ok(Self {
            codec,
            encoding,
            inspection: ConfigInspection::DeclaredOpaque,
            source,
            sample_framing,
            parameters: BudgetVec::new(pool),
        })
    }

    /// The coding standard the caller declared.
    pub const fn codec(&self) -> Codec {
        self.codec
    }
    /// How [`source_bytes`](Self::source_bytes) is encoded.
    pub const fn encoding(&self) -> ConfigEncoding {
        self.encoding
    }
    /// How much of the source bytes was checked.
    pub const fn inspection(&self) -> ConfigInspection {
        self.inspection
    }
    /// The whole configuration as it was given, start codes and padding
    /// included.
    pub fn source_bytes(&self) -> &Bytes {
        &self.source
    }
    /// The framing of the media samples this configuration belongs to, as the
    /// caller declared it.
    pub const fn sample_framing(&self) -> Framing {
        self.sample_framing
    }
    /// The parameter sets in source order, duplicates included. Empty for a
    /// declared record.
    pub fn parameter_sets(&self) -> &[ParameterSet] {
        self.parameters.as_slice()
    }
}

/// Parses an already pool-owned, bounded Annex-B parameter-set configuration.
/// The entire original source remains available even when normalization drops
/// start codes/padding from the extracted NAL views. Feeding the private reader
/// uses source slices, without private I/O or a second input copy.
#[derive(Debug)]
pub struct AnnexBConfigParser {
    pool: Budget,
    source: Option<Bytes>,
    reader: Option<NalReader>,
    codec: Codec,
    sample_framing: Framing,
    limits: ConfigLimits,
    parameters: BudgetVec<ParameterSet>,
    pending: Option<NalUnit>,
    reserved: bool,
    complete: bool,
    failure: Option<Error>,
}

impl AnnexBConfigParser {
    /// A parser for `source`, the whole configuration, charged to `pool`.
    /// `source_id` names it in each parameter set's facts, and `sample_framing`
    /// is kept as declared.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] when `source` is empty or a limit that must not be
    /// zero is; [`Error::ForeignPool`] when `source` is charged to another pool;
    /// [`Error::Limit`] when it exceeds `maximum_source_bytes`.
    pub fn new(
        pool: &Budget,
        codec: Codec,
        source_id: SourceId,
        source: Bytes,
        sample_framing: Framing,
        limits: ConfigLimits,
    ) -> Result<Self> {
        limits.check_source(pool, &source)?;
        let length = u64::try_from(source.len()).map_err(|_| Error::Overflow)?;
        let reader = NalReader::new(
            pool,
            codec,
            Framing::AnnexB,
            source_id,
            SourceCapabilities {
                length: Some(length),
                random_access: false,
            },
            limits.reader,
        )?;
        Ok(Self {
            pool: pool.clone(),
            source: Some(source),
            reader: Some(reader),
            codec,
            sample_framing,
            limits,
            parameters: BudgetVec::new(pool),
            pending: None,
            reserved: false,
            complete: false,
            failure: None,
        })
    }

    fn stop(&mut self, error: Error) {
        self.failure = Some(error);
        self.pending = None;
        self.reader = None;
        self.source = None;
        self.parameters = BudgetVec::new(&self.pool);
    }

    /// Frames the source's NAL units and keeps each as a parameter set. Work is
    /// charged per source byte and per parameter set.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] for a NAL unit that is not a parameter set of the
    /// codec; [`Error::Invalid`] for a source that holds no parameter set;
    /// [`Error::Limit`] past `maximum_parameter_sets`; the errors of
    /// [`NalReader::step`] for the Annex B framing; [`Error::Cancelled`] when
    /// `cancel` is set. [`Error::Backpressure`] and [`Error::Allocation`] are
    /// retryable: the parser keeps its progress. Every other error is final and
    /// returned again by each later call.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<Configuration>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            self.stop(Error::Cancelled);
            return Err(Error::Cancelled);
        }
        let result = self.step_inner(work, cancel);
        if let Err(error) = result
            && !retryable(error)
        {
            self.stop(error);
        }
        result
    }

    fn step_inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<Configuration>> {
        if self.complete {
            return Ok(BuildStep::Complete);
        }
        if !self.reserved {
            if !work.try_charge(1) {
                return Ok(BuildStep::Yield);
            }
            // Preallocate empty slots, avoiding unbudgeted growth/copies of
            // retained parameter-set descriptors in later steps.
            self.parameters
                .try_reserve_exact(self.limits.maximum_parameter_sets)?;
            self.reserved = true;
        }
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.pending.is_some() {
                if !work.try_charge(1) {
                    return Ok(BuildStep::Yield);
                }
                let unit = self
                    .pending
                    .take()
                    .ok_or(Error::Invalid("missing parameter set"))?;
                let kind = unit
                    .facts()
                    .header
                    .parameter_set_kind()
                    .ok_or(Error::Unsupported(
                        "configuration NAL is outside the bounded parameter-set types",
                    ))?;
                if self.parameters.len() >= self.limits.maximum_parameter_sets {
                    return Err(Error::Limit {
                        requested: self
                            .parameters
                            .len()
                            .checked_add(1)
                            .ok_or(Error::Overflow)?,
                        maximum: self.limits.maximum_parameter_sets,
                    });
                }
                let source = self
                    .source
                    .as_ref()
                    .ok_or(Error::Invalid("missing configuration source"))?;
                let unit = unit.configuration_view(&self.pool, source)?;
                if let Err(rejected) = self.parameters.try_push(ParameterSet { kind, unit }) {
                    self.pending = Some(rejected.value.unit);
                    return Err(rejected.error);
                }
            }
            let reader = self
                .reader
                .as_mut()
                .ok_or(Error::Invalid("missing configuration reader"))?;
            match reader.step(work, cancel)? {
                Step::Ready(unit) => self.pending = Some(unit),
                Step::Yield => return Ok(BuildStep::Yield),
                Step::NeedInput(request) => {
                    let source = self
                        .source
                        .as_ref()
                        .ok_or(Error::Invalid("missing configuration source"))?;
                    let start = usize::try_from(request.offset).map_err(|_| Error::Overflow)?;
                    if start == source.len() {
                        reader.finish(request.source, request.token)?;
                    } else {
                        let remaining = source.len().checked_sub(start).ok_or(Error::Overflow)?;
                        let count = remaining.min(request.maximum);
                        if count == 0 {
                            return Err(Error::Invalid("zero configuration input request"));
                        }
                        let end = start.checked_add(count).ok_or(Error::Overflow)?;
                        let window = InputWindow::new(
                            request.source,
                            request.token,
                            request.offset,
                            source.slice(start..end)?,
                        )?;
                        reader.provide(&window)?;
                    }
                }
                Step::End(_) => {
                    if self.parameters.is_empty() {
                        return Err(Error::Invalid("configuration has no parameter sets"));
                    }
                    if !work.try_charge(1) {
                        return Ok(BuildStep::Yield);
                    }
                    let parameters =
                        std::mem::replace(&mut self.parameters, BudgetVec::new(&self.pool));
                    let source = self
                        .source
                        .take()
                        .ok_or(Error::Invalid("missing configuration source"))?;
                    self.reader = None;
                    self.complete = true;
                    return Ok(BuildStep::Ready(Configuration {
                        codec: self.codec,
                        encoding: ConfigEncoding::AnnexBParameterSets,
                        inspection: ConfigInspection::AnnexBFramed,
                        source,
                        sample_framing: self.sample_framing,
                        parameters,
                    }));
                }
            }
        }
    }
}
