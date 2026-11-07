//! Explicit decoder-configuration policy over the complete source view.
//! This policy never makes an unknown array disappear or alters its bytes.
use crate::{BuildStep, HvcAnnexBOutput, HvcAnnexBWriter, HvcConfiguration};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HvcDecoderConfigRefusal {
    pub array_index: usize,
    pub declared_nal_type: u8,
}

pub enum HvcDecoderConfigOutput {
    Converted(HvcAnnexBOutput),
    Refused {
        configuration: HvcConfiguration,
        reason: HvcDecoderConfigRefusal,
    },
}
impl HvcDecoderConfigOutput {
    pub fn bytes(&self) -> &Bytes {
        match self {
            Self::Converted(o) => o.bytes(),
            Self::Refused { configuration, .. } => configuration.source_bytes(),
        }
    }
    pub fn configuration(&self) -> &HvcConfiguration {
        match self {
            Self::Converted(o) => o.configuration(),
            Self::Refused { configuration, .. } => configuration,
        }
    }
    pub const fn refusal(&self) -> Option<HvcDecoderConfigRefusal> {
        match self {
            Self::Converted(_) => None,
            Self::Refused { reason, .. } => Some(*reason),
        }
    }
}

/// Selected decoder-configuration conversion. Generic all-NAL conversion is
/// still HvcAnnexBWriter. A refusal retains the exact complete configuration.
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
            && !matches!(e, Error::Backpressure { .. } | Error::Allocation)
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
