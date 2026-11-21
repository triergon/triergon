//! The NAL units of an `hvcC` record that a decoder is configured with, as Annex B.
//! A record with an array of another type is refused rather than written without
//! it, and no NAL unit's bytes are changed.
use crate::retryable;
use crate::{
    BuildStep,
    hevc::{HvcAnnexBOutput, HvcAnnexBWriter, HvcConfiguration},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, Result, WorkBudget};

/// The first array of an `hvcC` record that a decoder configuration does not
/// carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HvcDecoderConfigRefusal {
    /// Index of the array in the record.
    pub array_index: usize,
    /// The `NAL_unit_type` the array declares: none of 32 to 34, 39 and 40.
    pub declared_nal_type: u8,
}

/// What [`HvcDecoderConfigWriter`] makes of a record.
#[derive(Debug)]
pub enum HvcDecoderConfigOutput {
    /// Every array declares a VPS, SPS, PPS or SEI type: the record's NAL units as
    /// Annex B.
    Converted(HvcAnnexBOutput),
    /// An array declares another type; no bytes are written.
    Refused {
        /// The record as it was given to the writer.
        configuration: HvcConfiguration,
        /// The first such array.
        reason: HvcDecoderConfigRefusal,
    },
}
impl HvcDecoderConfigOutput {
    /// The Annex B bytes of a converted record; the `hvcC` bytes of a refused one.
    pub fn bytes(&self) -> &Bytes {
        match self {
            Self::Converted(o) => o.bytes(),
            Self::Refused { configuration, .. } => configuration.source_bytes(),
        }
    }
    /// The record given to the writer, converted or refused.
    pub fn configuration(&self) -> &HvcConfiguration {
        match self {
            Self::Converted(o) => o.configuration(),
            Self::Refused { configuration, .. } => configuration,
        }
    }
    /// The array that refused the record; `None` when it was converted.
    pub const fn refusal(&self) -> Option<HvcDecoderConfigRefusal> {
        match self {
            Self::Converted(_) => None,
            Self::Refused { reason, .. } => Some(*reason),
        }
    }
}

/// Writes the NAL units of an `hvcC` record as Annex B when every array declares
/// a type a decoder configuration carries: VPS, SPS, PPS, prefix SEI or suffix
/// SEI (32 to 34, 39, 40). It refuses any other record, and the refusal retains
/// the exact complete configuration. [`HvcAnnexBWriter`] converts the NAL units
/// of every record, whatever its arrays declare.
#[derive(Debug)]
pub struct HvcDecoderConfigWriter {
    pool: Budget,
    input: Option<HvcConfiguration>,
    writer: Option<HvcAnnexBWriter>,
    maximum: usize,
    array: usize,
    complete: bool,
    failure: Option<Error>,
}
impl HvcDecoderConfigWriter {
    /// Starts the conversion of `input` into a buffer of at most `maximum` bytes
    /// charged to `pool`. Fails when the record belongs to another pool.
    pub fn new(pool: &Budget, input: HvcConfiguration, maximum: usize) -> Result<Self> {
        if !input.source_bytes().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        Ok(Self {
            pool: pool.clone(),
            input: Some(input),
            writer: None,
            maximum,
            array: 0,
            complete: false,
            failure: None,
        })
    }
    /// Advances by at most the units in `work` and returns the output once, as
    /// [`BuildStep`] describes. A refusal is an output, not an error. Checking an array
    /// costs one unit; the rest is the work of [`HvcAnnexBWriter::step`]. Fails as
    /// [`HvcAnnexBWriter::step`] does.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<HvcDecoderConfigOutput>> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = self.advance(work, cancel);
        if let Err(e) = result
            && !retryable(e)
        {
            self.failure = Some(e);
            self.input = None;
            self.writer = None;
        }
        result
    }
    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<HvcDecoderConfigOutput>> {
        loop {
            if self.complete {
                return Ok(BuildStep::Complete);
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if let Some(writer) = &mut self.writer {
                return match writer.step(work, cancel)? {
                    BuildStep::Yield => Ok(BuildStep::Yield),
                    BuildStep::Ready(o) => {
                        self.complete = true;
                        self.writer = None;
                        Ok(BuildStep::Ready(HvcDecoderConfigOutput::Converted(o)))
                    }
                    BuildStep::Complete => Err(Error::Invalid("missing decoder-config output")),
                };
            }
            if !work.try_charge(1) {
                return Ok(BuildStep::Yield);
            }
            let input = self
                .input
                .as_ref()
                .ok_or(Error::Invalid("missing decoder configuration"))?;
            if let Some(array) = input.array(self.array) {
                let kind = array.declared_nal_type();
                if !matches!(kind, 32 | 33 | 34 | 39 | 40) {
                    let reason = HvcDecoderConfigRefusal {
                        array_index: self.array,
                        declared_nal_type: kind,
                    };
                    let configuration = self
                        .input
                        .take()
                        .ok_or(Error::Invalid("missing decoder configuration"))?;
                    self.complete = true;
                    return Ok(BuildStep::Ready(HvcDecoderConfigOutput::Refused {
                        configuration,
                        reason,
                    }));
                }
                self.array += 1;
            } else {
                self.writer = Some(HvcAnnexBWriter::new(
                    &self.pool,
                    self.input
                        .take()
                        .ok_or(Error::Invalid("missing decoder configuration"))?,
                    self.maximum,
                )?);
            }
        }
    }
}
