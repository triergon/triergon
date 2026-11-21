//! The configuration of a packet session as Annex B extradata, as the FFmpeg
//! bitstream filters named on [`crate::compat::nal`] publish it when they are
//! initialised, in a bounded output.
use crate::nal::packet::{
    NalConfigurationRefusal, copy_annex_b_segment, copy_annex_b_whole, freeze_annex_b_output,
    start_annex_b_output,
};
use crate::retryable;
use crate::{BuildStep, avc::AvcConfigArrayKind, nal::NalPacketConfiguration};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

/// A session configuration as Annex B extradata, written by
/// [`NalPacketConfigWriter`].
pub struct NalPacketConfigOutput {
    bytes: Bytes,
    source: Bytes,
    source_refusal: NalConfigurationRefusal,
}
impl std::fmt::Debug for NalPacketConfigOutput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NalPacketConfigOutput")
            .field("output_bytes", &self.bytes.len())
            .field("source_bytes", &self.source.len())
            .field("strict_source_refusal", &self.strict_source_refusal())
            .finish_non_exhaustive()
    }
}
impl NalPacketConfigOutput {
    /// The Annex B extradata: every SPS and PPS of an AVC record, or every NAL
    /// unit of an HEVC record, in record order and each behind `00 00 00 01`.
    /// For a passthrough configuration, its bytes as they are.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
    /// The configuration's source bytes: the record the output was written
    /// from, or the passthrough bytes.
    pub fn source_bytes(&self) -> &Bytes {
        &self.source
    }
    /// The error with which the strict record parser refuses the record, when
    /// it was accepted through [`crate::compat::nal`]; `None` for a record that
    /// parser accepts.
    pub fn strict_source_refusal(&self) -> Option<Error> {
        self.source_refusal.error()
    }
}

/// The record and error of an HEVC configuration that declares an array of a
/// NAL unit type the filter does not write: any type but 32 to 34, 39 and 40.
/// No output exists for it.
pub struct NalPacketConfigRejection {
    source: Bytes,
    error: Error,
    declared_array_type: u8,
    source_refusal: NalConfigurationRefusal,
}
impl std::fmt::Debug for NalPacketConfigRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NalPacketConfigRejection")
            .field("source_bytes", &self.source.len())
            .field("source_error", &self.error)
            .field("declared_array_type", &self.declared_array_type)
            .field("strict_source_refusal", &self.strict_source_refusal())
            .finish_non_exhaustive()
    }
}
impl NalPacketConfigRejection {
    /// The whole record as it was given.
    pub fn source_bytes(&self) -> &Bytes {
        &self.source
    }
    /// The error [`NalPacketConfigWriter::step`] returned for the array.
    pub fn source_error(&self) -> Error {
        self.error
    }
    /// The NAL unit type the first such array declares, 0 to 63.
    pub fn declared_array_type(&self) -> u8 {
        self.declared_array_type
    }
    /// The error with which the strict record parser refuses the record, when
    /// it was accepted through [`crate::compat::nal`]; `None` for a record that
    /// parser accepts.
    pub fn strict_source_refusal(&self) -> Option<Error> {
        self.source_refusal.error()
    }
    /// FFmpeg's return code for initialising `hevc_mp4toannexb` with this
    /// record: `AVERROR_INVALIDDATA`. It is a constant for comparison, compiled
    /// with the `reference-trace` feature; FFmpeg is not called.
    #[cfg(feature = "reference-trace")]
    pub fn native_initialize_result(&self) -> i32 {
        -1_094_995_529
    }
}
#[derive(Clone, Copy)]
enum Phase {
    /// Admit the HEVC decoder arrays.
    Arrays,
    /// Size the output, NAL by NAL or from the record's validated extent.
    Size,
    /// Charge and allocate the output.
    Reserve,
    /// Copy the start codes and NAL payloads.
    Copy,
    /// Publish the output.
    Publish,
    /// Published: every later step reports completion.
    Complete,
}

/// Writes the configuration of a [`NalPacketSession`](crate::nal::NalPacketSession)
/// as Annex B extradata, as FFmpeg's `h264_mp4toannexb` and `hevc_mp4toannexb`
/// bitstream filters publish it when they are initialised.
pub struct NalPacketConfigWriter<'a> {
    config: &'a NalPacketConfiguration,
    length_prefixed: bool,
    output: Option<BudgetVec<u8>>,
    maximum: usize,
    total: usize,
    array: usize,
    index: usize,
    copied: usize,
    phase: Phase,
    failure: Option<Error>,
    rejected_array: Option<u8>,
    rejection_taken: bool,
}
impl std::fmt::Debug for NalPacketConfigWriter<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NalPacketConfigWriter")
            .field("codec", &self.config.codec())
            .field("source_bytes", &self.config.bytes().len())
            .field("maximum_output_bytes", &self.maximum)
            .field("failure", &self.failure)
            .field("rejected_array_type", &self.rejected_array)
            .finish_non_exhaustive()
    }
}
impl<'a> NalPacketConfigWriter<'a> {
    /// A writer for `configuration`, whose bytes are charged to `pool`; the output is
    /// charged to it too and holds at most `maximum_output_bytes`. Fails with
    /// [`Error::ForeignPool`] when `configuration` is charged to another pool.
    pub fn new(
        pool: &Budget,
        configuration: &'a NalPacketConfiguration,
        maximum_output_bytes: usize,
    ) -> Result<Self> {
        if !configuration.bytes().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        Ok(Self {
            config: configuration,
            length_prefixed: configuration.width().is_some(),
            output: None,
            maximum: maximum_output_bytes,
            total: 0,
            array: 0,
            index: 0,
            copied: 0,
            phase: Phase::Arrays,
            failure: None,
            rejected_array: None,
            rejection_taken: false,
        })
    }
    // Sizing and copying use the same checked view of the config record.
    #[inline(never)]
    fn nal(
        config: &NalPacketConfiguration,
        length_prefixed: bool,
        index: usize,
    ) -> Option<(bool, &[u8], usize)> {
        if length_prefixed {
            config.config_nal(index).map(|n| {
                (
                    n.kind != Some(AvcConfigArrayKind::SequenceExtension),
                    n.payload(),
                    4,
                )
            })
        } else {
            (index == 0).then_some((true, config.bytes().as_ref(), 0))
        }
    }
    /// After [`step`](Self::step) refused an HEVC record for the type of one of
    /// its arrays: the record and the error, once. `None` for every other
    /// failure, before a failure and on later calls.
    pub fn take_receiver_rejection(&mut self) -> Option<NalPacketConfigRejection> {
        if self.rejection_taken {
            return None;
        }
        let declared_array_type = self.rejected_array?;
        let error = self.failure?;
        self.rejection_taken = true;
        Some(NalPacketConfigRejection {
            source: self.config.bytes().clone(),
            error,
            declared_array_type,
            source_refusal: self.config.refusal(),
        })
    }
    /// Sizes the extradata, then copies it. Work is charged per array checked,
    /// NAL unit sized and byte copied.
    ///
    /// Fails with [`Error::Unsupported`] for an HEVC record that declares an array of a
    /// NAL unit type other than 32 to 34, 39 and 40, which
    /// [`take_receiver_rejection`](Self::take_receiver_rejection) then reports;
    /// [`Error::Limit`] when the extradata exceeds `maximum_output_bytes` or the pool's
    /// limit; [`Error::Cancelled`] when `cancel` is set. [`Error::Backpressure`] and
    /// [`Error::Allocation`] are retryable: the writer keeps its progress. Every other
    /// error is final and returned again by each later call.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<NalPacketConfigOutput>> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let r = self.advance(work, cancel);
        if let Err(e) = r
            && !retryable(e)
        {
            self.failure = Some(e);
            self.output = None;
        }
        r
    }
    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<NalPacketConfigOutput>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match self.phase {
                Phase::Arrays => match self.config.admit_hevc_arrays(&mut self.array, work) {
                    Ok(true) => self.phase = Phase::Size,
                    Ok(false) => return Ok(BuildStep::Yield),
                    Err(declared) => {
                        self.rejected_array = Some(declared);
                        return Err(Error::Unsupported("pinned HEVC decoder array type"));
                    }
                },
                Phase::Size => {
                    // A successful full sizing pass can use the validated extent.
                    // Keep the incremental walk for short work budgets and to
                    // report the first NAL that crosses the output limit.
                    if self.index == 0
                        && let Some((len, charge)) = self.config.annex_b_configuration_extent()
                        && len <= self.maximum
                        && work.try_charge(charge)
                    {
                        self.total = len;
                        self.phase = Phase::Reserve;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(BuildStep::Yield);
                    }
                    if let Some((selected, n, prefix)) =
                        Self::nal(self.config, self.length_prefixed, self.index)
                    {
                        if selected {
                            let total = self
                                .total
                                .checked_add(prefix)
                                .and_then(|v| v.checked_add(n.len()))
                                .ok_or(Error::Overflow)?;
                            if total > self.maximum {
                                return Err(Error::Limit {
                                    requested: total,
                                    maximum: self.maximum,
                                });
                            }
                            self.total = total;
                        }
                        self.index += 1;
                    } else {
                        self.phase = Phase::Reserve;
                        self.index = 0;
                    }
                }
                Phase::Reserve => {
                    if !work.try_charge(1) {
                        return Ok(BuildStep::Yield);
                    }
                    self.output = Some(start_annex_b_output(
                        self.config.bytes().budget(),
                        self.total,
                    )?);
                    self.phase = Phase::Copy;
                }
                Phase::Copy => {
                    let output = self
                        .output
                        .as_mut()
                        .ok_or(Error::Invalid("missing config output"))?;
                    let (config, length_prefixed) = (self.config, self.length_prefixed);
                    if self.index == 0 && self.copied == 0 {
                        let entries = if length_prefixed {
                            config.config_nal_count()
                        } else {
                            1
                        };
                        let copies = (0..)
                            .map_while(|index| Self::nal(config, length_prefixed, index))
                            .filter(|&(selected, ..)| selected)
                            .map(|(_, payload, prefix)| Ok((prefix, payload)));
                        if copy_annex_b_whole(output, self.total, entries, copies, work, cancel)? {
                            self.phase = Phase::Publish;
                            continue;
                        }
                    }
                    let Some((selected, b, prefix)) =
                        Self::nal(config, length_prefixed, self.index)
                    else {
                        self.phase = Phase::Publish;
                        continue;
                    };
                    if selected
                        && !copy_annex_b_segment(prefix, b, output, &mut self.copied, work, cancel)?
                    {
                        return Ok(BuildStep::Yield);
                    }
                    if !work.try_charge(1) {
                        return Ok(BuildStep::Yield);
                    }
                    self.index += 1;
                    self.copied = 0;
                }
                Phase::Publish => {
                    if !work.try_charge(1) {
                        return Ok(BuildStep::Yield);
                    }
                    let bytes = freeze_annex_b_output(&mut self.output, "missing config output")?;
                    self.phase = Phase::Complete;
                    return Ok(BuildStep::Ready(NalPacketConfigOutput {
                        bytes,
                        source: self.config.bytes().clone(),
                        source_refusal: self.config.refusal(),
                    }));
                }
                Phase::Complete => return Ok(BuildStep::Complete),
            }
        }
    }
}
