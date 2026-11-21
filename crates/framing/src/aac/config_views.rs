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
/// Fails when `policy.maximum_channels` is zero.
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
    source: AacSource,
    bits: usize,
    state: State,
}

/// Where a parse stands: the declared extent is parsed first, then every byte.
#[derive(Debug)]
enum State {
    Declared {
        raw: Bytes,
        declared: AacAscParser,
        physical: AacAscParser,
    },
    /// `declared` is `None` when the declared length covers every byte.
    Physical {
        raw: Bytes,
        declared: Option<AacAscScopeOutcome>,
        physical: AacAscParser,
    },
    /// Both outcomes, returned by the next unit of work.
    Parsed {
        raw: Bytes,
        declared: Option<AacAscScopeOutcome>,
        physical: AacAscScopeOutcome,
    },
    Complete,
    Failed(Error),
}

impl AacAscScopeParser {
    /// A parser of `raw`, the record that starts at `source`, whose declared length is
    /// `bits` bits. Fails when `bits` exceeds the bits in `raw`, and as
    /// [`AacAscParser::new`].
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
        let declared = if bits == physical_bits {
            None
        } else {
            Some(AacAscParser::with_bit_length(
                pool,
                source,
                raw.clone(),
                bits,
                limits,
            )?)
        };
        let physical = AacAscParser::new(pool, source, raw.clone(), limits)?;
        let state = match declared {
            Some(declared) => State::Declared {
                raw,
                declared,
                physical,
            },
            None => State::Physical {
                raw,
                declared: None,
                physical,
            },
        };
        Ok(Self {
            pool: pool.clone(),
            source,
            bits,
            state,
        })
    }
    /// The outcome of one step of an extent's parser, `None` until it has one. A
    /// refusal of the record's content is an outcome; other errors are returned.
    fn outcome(
        step: Result<BuildStep<AacAudioSpecificConfig>>,
    ) -> Result<Option<AacAscScopeOutcome>> {
        match step {
            Ok(BuildStep::Yield) => Ok(None),
            Ok(BuildStep::Ready(o)) => Ok(Some(AacAscScopeOutcome::Parsed(o))),
            Ok(BuildStep::Complete) => Err(Error::Invalid("missing ASC scope output")),
            Err(e)
                if matches!(
                    e,
                    Error::Invalid(_) | Error::Unsupported(_) | Error::Truncated
                ) =>
            {
                Ok(Some(AacAscScopeOutcome::Refused(e)))
            }
            Err(e) => Err(e),
        }
    }
    /// An error of an extent's parser in `state`: backpressure becomes a limit when
    /// the pool could never hold the record, both parses and the request together.
    fn refusal(&self, state: &State, error: Error) -> Error {
        let Error::Backpressure(triergon_core::Backpressure { requested, .. }) = error else {
            return error;
        };
        match Self::minimum_owned_storage(state, requested) {
            Ok(minimum) if minimum > self.pool.limit() => Error::Limit {
                requested: minimum,
                maximum: self.pool.limit(),
            },
            Ok(_) => error,
            Err(e) => e,
        }
    }
    fn minimum_owned_storage(state: &State, requested: usize) -> Result<usize> {
        let (raw, parsers, outcomes) = match state {
            State::Declared {
                raw,
                declared,
                physical,
            } => (Some(raw), [Some(declared), Some(physical)], [None, None]),
            State::Physical {
                raw,
                declared,
                physical,
            } => (Some(raw), [Some(physical), None], [declared.as_ref(), None]),
            State::Parsed {
                raw,
                declared,
                physical,
            } => (Some(raw), [None, None], [declared.as_ref(), Some(physical)]),
            State::Complete | State::Failed(_) => (None, [None, None], [None, None]),
        };
        let mut total = Budget::control_storage_bytes()
            .checked_add(raw.map_or(0, Bytes::retained_storage_bytes))
            .and_then(|v| v.checked_add(requested))
            .ok_or(Error::Overflow)?;
        for output in outcomes.into_iter().flatten() {
            if let AacAscScopeOutcome::Parsed(c) = output {
                total = total
                    .checked_add(c.descriptor_storage_bytes()?)
                    .ok_or(Error::Overflow)?;
            }
        }
        for parser in parsers.into_iter().flatten() {
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
    /// Fails when `cancel` is set, or a program config element exceeds the limits or
    /// what the pool can ever hold. The parser then returns the same error from every
    /// later call. [`Error::Backpressure`] and [`Error::Allocation`] leave it as it
    /// was, to be stepped again.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AacAscScopeViews>> {
        let state = std::mem::replace(&mut self.state, State::Complete);
        let (state, result) = self.advance(state, work, cancel);
        self.state = match result {
            Err(e) if !retryable(e) => State::Failed(e),
            _ => state,
        };
        result
    }
    /// Steps `state` for as long as `work` lasts; the state to keep, and the result.
    fn advance(
        &self,
        mut state: State,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> (State, Result<BuildStep<AacAscScopeViews>>) {
        loop {
            state = match state {
                State::Failed(e) => return (State::Failed(e), Err(e)),
                State::Complete => return (State::Complete, Ok(BuildStep::Complete)),
                state if cancel.load(Ordering::Relaxed) => return (state, Err(Error::Cancelled)),
                State::Declared {
                    raw,
                    mut declared,
                    physical,
                } => match Self::outcome(declared.step(work, cancel)) {
                    Ok(Some(outcome)) => State::Physical {
                        raw,
                        declared: Some(outcome),
                        physical,
                    },
                    Ok(None) => {
                        let state = State::Declared {
                            raw,
                            declared,
                            physical,
                        };
                        return (state, Ok(BuildStep::Yield));
                    }
                    Err(e) => {
                        let state = State::Declared {
                            raw,
                            declared,
                            physical,
                        };
                        let e = self.refusal(&state, e);
                        return (state, Err(e));
                    }
                },
                State::Physical {
                    raw,
                    declared,
                    mut physical,
                } => match Self::outcome(physical.step(work, cancel)) {
                    Ok(Some(outcome)) => State::Parsed {
                        raw,
                        declared,
                        physical: outcome,
                    },
                    Ok(None) => {
                        let state = State::Physical {
                            raw,
                            declared,
                            physical,
                        };
                        return (state, Ok(BuildStep::Yield));
                    }
                    Err(e) => {
                        let state = State::Physical {
                            raw,
                            declared,
                            physical,
                        };
                        let e = self.refusal(&state, e);
                        return (state, Err(e));
                    }
                },
                State::Parsed {
                    raw,
                    declared,
                    physical,
                } => {
                    if !work.try_charge(1) {
                        let state = State::Parsed {
                            raw,
                            declared,
                            physical,
                        };
                        return (state, Ok(BuildStep::Yield));
                    }
                    let views = AacAscScopeViews {
                        raw,
                        source: self.source,
                        declared_bits: self.bits,
                        declared,
                        physical,
                    };
                    return (State::Complete, Ok(BuildStep::Ready(views)));
                }
            };
        }
    }
}
