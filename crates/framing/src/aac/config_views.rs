//! An AudioSpecificConfig read at two extents, the caller's bit length and every
//! byte of the buffer, and a decoder's view of a parsed configuration. Neither
//! extent is derived from a parse that succeeds, a file name, padding or how the
//! record was carried.
use crate::retryable;
use crate::{
    BuildStep,
    aac::{
        AacAscFacts, AacAscParser, AacAudioSpecificConfig, AacConfigLimits, AacExtensionSignal,
        AacSource,
    },
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, Result, WorkBudget};

/// Where the channel count of an [`AacDecoderChannelView`] comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AacDecoderChannelReason {
    /// The configuration signals it ([`AacAscFacts::signalled_render_channels`]).
    Signalled,
    /// The core is mono, SBR is signalled present and the record carries no signal
    /// for PS: the decoder outputs two channels.
    SbrMonoDecoderChoice,
}
/// The channels a decoder outputs for a configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacDecoderChannelView {
    /// Output channels.
    pub channels: u16,
    /// Where the count comes from.
    pub reason: AacDecoderChannelReason,
}
/// What a decoder accepts, for [`decoder_configuration_view`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacDecoderConfigPolicy {
    /// Whether a core sampling frequency coded as a 24-bit value
    /// (`samplingFrequencyIndex` 15) is accepted.
    pub allow_explicit_core_frequency: bool,
    /// Most output channels accepted. Zero is refused as [`Error::Invalid`].
    pub maximum_channels: u16,
}
/// Why a configuration is outside an [`AacDecoderConfigPolicy`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AacDecoderConfigRefusal {
    /// The core sampling frequency is coded as a 24-bit value and the policy does
    /// not accept that.
    ExplicitFrequency,
    /// The decoder would output more channels than the policy accepts.
    Channels {
        /// Channels the decoder would output ([`decoder_channel_view`]).
        declared: u16,
        /// The policy's `maximum_channels`.
        maximum: u16,
    },
}
/// Whether a configuration is within an [`AacDecoderConfigPolicy`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AacDecoderConfigDecision {
    /// It is; a decoder outputs these channels.
    Eligible(AacDecoderChannelView),
    /// It is not, for this reason.
    Refused(AacDecoderConfigRefusal),
}
/// Decides whether `config` is within `policy`: first its core frequency coding,
/// then the channel count of [`decoder_channel_view`].
///
/// The decision applies the caller's statement of what a decoder supports to the
/// parsed fields. It does not validate the record's syntax and does not promise
/// that a decoder opens or decodes the audio.
///
/// # Errors
///
/// When `policy.maximum_channels` is zero.
pub fn decoder_configuration_view(
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
    let channels = decoder_channel_view(f);
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
/// The channels a decoder outputs for `f`: two for a mono core with SBR signalled
/// present and no signal for PS, otherwise
/// [`AacAscFacts::signalled_render_channels`].
///
/// This is the decoder's choice, taken only when the caller asks for it; it changes
/// nothing the record signals. PS coded as absent is a signal, unlike PS not coded:
/// such a configuration stays mono.
pub fn decoder_channel_view(f: AacAscFacts) -> AacDecoderChannelView {
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

/// The result of parsing an AudioSpecificConfig over one extent.
#[derive(Debug)]
pub enum AacAscScopeOutcome {
    /// The extent holds a configuration the parser accepts.
    Parsed(AacAudioSpecificConfig),
    /// The parser refused the extent's content: it ends inside a field
    /// ([`Error::Truncated`]), breaks the syntax ([`Error::Invalid`]) or uses what
    /// the parser does not support ([`Error::Unsupported`]).
    Refused(Error),
}
impl AacAscScopeOutcome {
    /// The configuration; `None` for a refused extent.
    pub const fn parsed(&self) -> Option<&AacAudioSpecificConfig> {
        match self {
            Self::Parsed(o) => Some(o),
            Self::Refused(_) => None,
        }
    }
    /// The refusal; `None` for a parsed extent.
    pub const fn error(&self) -> Option<Error> {
        match self {
            Self::Parsed(_) => None,
            Self::Refused(e) => Some(*e),
        }
    }
}
/// One record parsed by [`AacAscScopeParser`] over its declared bit length and over
/// every byte, with the record.
#[derive(Debug)]
pub struct AacAscScopeViews {
    raw: Bytes,
    source: AacSource,
    declared_bits: usize,
    declared: Option<AacAscScopeOutcome>,
    physical: AacAscScopeOutcome,
}
impl AacAscScopeViews {
    /// Every byte of the record as the parser was given it.
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    /// Where the record starts in its source.
    pub const fn source(&self) -> AacSource {
        self.source
    }
    /// The bit length the caller declared.
    pub const fn declared_bits(&self) -> usize {
        self.declared_bits
    }
    /// The bits in the record's bytes: eight per byte.
    pub fn physical_bits(&self) -> usize {
        self.raw.len() * 8
    }
    /// The parse of the first [`declared_bits`](Self::declared_bits) bits. It is the
    /// same outcome as [`physical`](Self::physical) when the two extents are equal.
    pub fn declared(&self) -> &AacAscScopeOutcome {
        self.declared.as_ref().unwrap_or(&self.physical)
    }
    /// The parse of every byte of the record.
    pub fn physical(&self) -> &AacAscScopeOutcome {
        &self.physical
    }
    /// Whether the declared bit length covers every byte, so that the record was
    /// parsed once.
    pub fn same_extent(&self) -> bool {
        self.declared_bits == self.physical_bits()
    }
}
/// Parses one AudioSpecificConfig record over two extents: the bit length the caller
/// declares and every byte of the buffer. A record whose declared length covers
/// every byte is parsed once.
///
/// A parse the record's content refuses is an [`AacAscScopeOutcome::Refused`] of
/// that extent, not a failure of the parser.
#[derive(Debug)]
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
    /// A parser of `raw`, the record that starts at `source`, whose declared length
    /// is `bits` bits.
    ///
    /// # Errors
    ///
    /// When `bits` exceeds the bits in `raw`, and as [`AacAscParser::new`].
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
    /// Parses for as long as `work` lasts, the declared extent first.
    ///
    /// [`BuildStep::Yield`] asks for another call with more work.
    /// [`BuildStep::Ready`] carries both outcomes, once; later calls return
    /// [`BuildStep::Complete`].
    ///
    /// # Errors
    ///
    /// When `cancel` is set, or a program config element exceeds the limits or what
    /// the pool can ever hold. The parser then returns the same error from every
    /// later call. [`Error::Backpressure`] and [`Error::Allocation`] leave it as it
    /// was, to be stepped again.
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
            && !retryable(e)
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
                    Err(e @ Error::Backpressure(triergon_core::Backpressure { requested, .. })) => {
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
