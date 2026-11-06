//! Explicit caller-bit and full physical-byte ASC views. Neither extent is
//! inferred from native success, filename, padding or transport state.
use crate::{
    AacAscFacts, AacAscParser, AacAudioSpecificConfig, AacConfigLimits, AacExtensionSignal,
    AacSource, BuildStep,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AacDecoderChannelReason {
    Signalled,
    SbrMonoDecoderChoice,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacDecoderChannelView {
    pub channels: u16,
    pub reason: AacDecoderChannelReason,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacDecoderConfigPolicy {
    pub allow_explicit_core_frequency: bool,
    pub maximum_channels: u16,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AacDecoderConfigRefusal {
    ExplicitFrequency,
    Channels { declared: u16, maximum: u16 },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AacDecoderConfigDecision {
    Eligible(AacDecoderChannelView),
    Refused(AacDecoderConfigRefusal),
}
/// A caller-requested decoder capability policy, not source syntax validation
/// or a prediction that arbitrary compressed audio will open or decode.
pub fn aac_decoder_configuration_view(
    config: &AacAudioSpecificConfig,
    policy: AacDecoderConfigPolicy,
) -> Result<AacDecoderConfigDecision> {
    if policy.maximum_channels == 0 {
        return Err(Error::Invalid("zero AAC decoder channel policy"));
    }
    let f = config.facts();
    if !policy.allow_explicit_core_frequency && f.core_frequency.explicit_hz.is_some() {
        return Ok(AacDecoderConfigDecision::Refused(
            AacDecoderConfigRefusal::ExplicitFrequency,
        ));
    }
    let channels = aac_decoder_channel_view(f);
    if channels.channels > policy.maximum_channels {
        return Ok(AacDecoderConfigDecision::Refused(
            AacDecoderConfigRefusal::Channels {
                declared: channels.channels,
                maximum: policy.maximum_channels,
            },
        ));
    }
    Ok(AacDecoderConfigDecision::Eligible(channels))
}
/// An explicitly requested decoder choice, never a change to source/render
/// signalling. Unspecified PS with SBR is independent of explicit PS=false.
pub fn aac_decoder_channel_view(f: AacAscFacts) -> AacDecoderChannelView {
    if f.core_channels == 1 && f.sbr.is_present() && matches!(f.ps, AacExtensionSignal::Unsignalled)
    {
        AacDecoderChannelView {
            channels: 2,
            reason: AacDecoderChannelReason::SbrMonoDecoderChoice,
        }
    } else {
        AacDecoderChannelView {
            channels: f.signalled_render_channels(),
            reason: AacDecoderChannelReason::Signalled,
        }
    }
}

pub enum AacAscScopeOutcome {
    Parsed(AacAudioSpecificConfig),
    Refused(Error),
}
impl AacAscScopeOutcome {
    pub const fn parsed(&self) -> Option<&AacAudioSpecificConfig> {
        match self {
            Self::Parsed(o) => Some(o),
            Self::Refused(_) => None,
        }
    }
    pub const fn error(&self) -> Option<Error> {
        match self {
            Self::Parsed(_) => None,
            Self::Refused(e) => Some(*e),
        }
    }
}
pub struct AacAscScopeViews {
    raw: Bytes,
    source: AacSource,
    declared_bits: usize,
    declared: Option<AacAscScopeOutcome>,
    physical: AacAscScopeOutcome,
}
impl AacAscScopeViews {
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    pub const fn source(&self) -> AacSource {
        self.source
    }
    pub const fn declared_bits(&self) -> usize {
        self.declared_bits
    }
    pub fn physical_bits(&self) -> usize {
        self.raw.len() * 8
    }
    pub fn declared(&self) -> &AacAscScopeOutcome {
        self.declared.as_ref().unwrap_or(&self.physical)
    }
    pub fn physical(&self) -> &AacAscScopeOutcome {
        &self.physical
    }
    pub fn same_extent(&self) -> bool {
        self.declared_bits == self.physical_bits()
    }
}
pub struct AacAscScopeParser {
    pool: Budget,
    raw: Option<Bytes>,
    source: AacSource,
    bits: usize,
    declared: Option<AacAscScopeOutcome>,
    physical: Option<AacAscScopeOutcome>,
    declared_parser: Option<AacAscParser>,
    physical_parser: Option<AacAscParser>,
    completed: bool,
    failure: Option<Error>,
}
impl AacAscScopeParser {
    pub fn new(
        pool: &Budget,
        source: AacSource,
        raw: Bytes,
        bits: usize,
        limits: AacConfigLimits,
    ) -> Result<Self> {
        let physical_bits = raw.len().checked_mul(8).ok_or(Error::Overflow)?;
        if bits > physical_bits {
            return Err(Error::Invalid("AAC bit extent exceeds physical record"));
        }
        let mut out = Self {
            pool: pool.clone(),
            raw: Some(raw.clone()),
            source,
            bits,
            declared: None,
            physical: None,
            declared_parser: None,
            physical_parser: None,
            completed: false,
            failure: None,
        };
        if bits != physical_bits {
            out.declared_parser = Some(AacAscParser::with_bit_length(
                pool,
                source,
                raw.clone(),
                bits,
                limits,
            )?);
        }
        out.physical_parser = Some(AacAscParser::new(pool, source, raw, limits)?);
        Ok(out)
    }
    fn semantic(error: Error) -> bool {
        matches!(
            error,
            Error::Invalid(_) | Error::Unsupported(_) | Error::Truncated
        )
    }
    fn minimum_owned_storage(&self, requested: usize) -> Result<usize> {
        let mut total = Budget::control_storage_bytes()
            .checked_add(self.raw.as_ref().map_or(0, Bytes::retained_storage_bytes))
            .and_then(|v| v.checked_add(requested))
            .ok_or(Error::Overflow)?;
        for output in [&self.declared, &self.physical] {
            if let Some(AacAscScopeOutcome::Parsed(c)) = output {
                total = total
                    .checked_add(c.descriptor_storage_bytes()?)
                    .ok_or(Error::Overflow)?;
            }
        }
        for parser in [&self.declared_parser, &self.physical_parser]
            .into_iter()
            .flatten()
        {
            total = total
                .checked_add(parser.descriptor_storage_bytes()?)
                .ok_or(Error::Overflow)?;
        }
        Ok(total)
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AacAscScopeViews>> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let r = self.advance(work, cancel);
        if let Err(e) = r
            && !matches!(e, Error::Backpressure { .. } | Error::Allocation)
        {
            self.failure = Some(e);
            self.raw = None;
            self.declared = None;
            self.physical = None;
            self.declared_parser = None;
            self.physical_parser = None;
        }
        r
    }
    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AacAscScopeViews>> {
        loop {
            if self.completed {
                return Ok(BuildStep::Complete);
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let (slot, output) = if self.declared_parser.is_some() {
                (&mut self.declared_parser, &mut self.declared)
            } else {
                (&mut self.physical_parser, &mut self.physical)
            };
            if let Some(parser) = slot {
                match parser.step(work, cancel) {
                    Ok(BuildStep::Yield) => return Ok(BuildStep::Yield),
                    Ok(BuildStep::Ready(o)) => {
                        *output = Some(AacAscScopeOutcome::Parsed(o));
                        *slot = None;
                    }
                    Ok(BuildStep::Complete) => {
                        return Err(Error::Invalid("missing ASC scope output"));
                    }
                    Err(e) if Self::semantic(e) => {
                        *output = Some(AacAscScopeOutcome::Refused(e));
                        *slot = None;
                    }
                    Err(e @ Error::Backpressure { requested, .. }) => {
                        let minimum = self.minimum_owned_storage(requested)?;
                        if minimum > self.pool.limit() {
                            return Err(Error::Limit {
                                requested: minimum,
                                maximum: self.pool.limit(),
                            });
                        }
                        return Err(e);
                    }
                    Err(e) => return Err(e),
                }
            } else {
                if !work.try_charge(1) {
                    return Ok(BuildStep::Yield);
                }
                self.completed = true;
                return Ok(BuildStep::Ready(AacAscScopeViews {
                    raw: self
                        .raw
                        .take()
                        .ok_or(Error::Invalid("missing ASC source"))?,
                    source: self.source,
                    declared_bits: self.bits,
                    declared: self.declared.take(),
                    physical: self
                        .physical
                        .take()
                        .ok_or(Error::Invalid("missing physical ASC view"))?,
                }));
            }
        }
    }
}
