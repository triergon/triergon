//! The resumable reading of one input, phase by phase, from its envelope to
//! its cue.
use super::{
    Cue, Encoding, Format, Input, Limits, Modifier,
    emit::{Emitter, Fields, Plan, TextSpan, Times},
    sample,
    unicode::{Cursor, Poll, Probe, cancelled},
};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

/// What follows the text once it is validated.
#[derive(Clone, Copy, Debug)]
enum Next {
    /// tx3g: the modifier boxes after the text.
    Boxes,
    /// WebVTT in Matroska: the cue's block addition 1.
    Addition,
    /// The cue.
    Cue,
}

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "The phase is stepped in place inside its session; the cue's plan stays inline rather than in an allocation the pool does not charge."
)]
enum Phase {
    /// Nothing read yet.
    Init,
    /// tx3g: the text length.
    Length(sample::Length),
    /// The encoding selector at the start of the text.
    Probe(Probe, Next),
    /// The text, checked and its UTF-8 length counted.
    Validate(Cursor, Next),
    /// tx3g: the modifier boxes.
    Boxes(sample::Boxes),
    /// The WebVTT settings, identifier and comments.
    Addition(sample::Addition),
    /// The cue, built from what was read.
    Emit(Emitter),
    /// The cue was returned.
    Done,
}

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "Exclusive step outcomes transfer the owned event inline without a second allocation; guarded at 688 bytes."
)]
pub(crate) enum Action {
    Yield,
    Cue(Cue),
    Complete,
}

const _: () = assert!(size_of::<Action>() <= 688);

#[derive(Debug)]
pub(crate) struct Session {
    phase: Phase,
    iso_tx3g: bool,
    text_range: Range<usize>,
    decode_range: Range<usize>,
    encoding: Encoding,
    decoded_len: usize,
}

impl Session {
    pub(super) fn new(input: &Input, iso_tx3g: bool) -> Self {
        Self {
            phase: Phase::Init,
            iso_tx3g,
            text_range: 0..input.payload.len(),
            decode_range: 0..input.payload.len(),
            encoding: Encoding::Opaque,
            decoded_len: 0,
        }
    }

    /// The phase that builds the cue from the text read, the WebVTT `fields`
    /// and the tx3g `modifiers`.
    fn emit(
        &self,
        input: &Input,
        pool: &Budget,
        fields: Fields,
        modifiers: BudgetVec<Modifier>,
    ) -> Result<Phase> {
        let plan = Plan {
            span: TextSpan {
                raw: 0..input.payload.len(),
                text: self.text_range.clone(),
                decode: self.decode_range.clone(),
                encoding: self.encoding,
                decoded_len: self.decoded_len,
            },
            fields,
            times: Times::sample(input)?,
            modifiers,
        };
        Ok(Phase::Emit(Emitter::new(plan, pool)))
    }

    pub(super) fn step(
        &mut self,
        input: &Input,
        pool: &Budget,
        limits: Limits,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Action> {
        let payload = input.payload.as_ref();
        loop {
            cancelled(cancel)?;
            match &mut self.phase {
                Phase::Init => {
                    if !work.try_charge(1) {
                        return Ok(Action::Yield);
                    }
                    self.phase = match input.format {
                        Format::OpaqueAss => {
                            self.encoding = Encoding::Opaque;
                            self.emit(input, pool, Fields::default(), BudgetVec::new(pool))?
                        }
                        Format::Tx3g => Phase::Length(sample::Length::default()),
                        Format::Utf8Packet | Format::MatroskaWebVtt => {
                            if input.encoding != Encoding::Utf8 {
                                return Err(Error::Invalid("text format requires UTF-8"));
                            }
                            let next = match &input.addition {
                                Some(addition)
                                    if input.format == Format::MatroskaWebVtt
                                        && addition.id == 1 =>
                                {
                                    Next::Addition
                                }
                                _ => Next::Cue,
                            };
                            Phase::Probe(Probe::new(self.text_range.clone(), Encoding::Utf8), next)
                        }
                    };
                }
                Phase::Length(length) => {
                    let Some(range) = length.poll(payload, work, cancel)? else {
                        return Ok(Action::Yield);
                    };
                    self.text_range = range.clone();
                    self.phase = Phase::Probe(Probe::new(range, Encoding::Tx3gInBand), Next::Boxes);
                }
                Phase::Probe(probe, next) => {
                    let Some((encoding, range)) = probe.poll(payload, work, cancel)? else {
                        return Ok(Action::Yield);
                    };
                    self.encoding = encoding;
                    self.decode_range = range.clone();
                    self.phase = Phase::Validate(Cursor::new(range, encoding), *next);
                }
                Phase::Validate(validator, next) => match validator.poll(payload, work, cancel)? {
                    Poll::Yield => return Ok(Action::Yield),
                    Poll::Ready(_) => {}
                    Poll::End => {
                        self.decoded_len = validator.decoded_len();
                        self.phase = match (*next, &input.addition) {
                            (Next::Boxes, _) => Phase::Boxes(sample::Boxes::new(
                                pool,
                                self.iso_tx3g,
                                self.text_range.end,
                            )),
                            (Next::Addition, Some(addition)) => {
                                Phase::Addition(sample::Addition::new(&addition.bytes))
                            }
                            (Next::Addition | Next::Cue, _) => {
                                self.emit(input, pool, Fields::default(), BudgetVec::new(pool))?
                            }
                        };
                    }
                },
                Phase::Boxes(boxes) => {
                    if !boxes.poll(payload, limits, work, cancel)? {
                        return Ok(Action::Yield);
                    }
                    let modifiers = boxes.take_modifiers();
                    self.phase = self.emit(input, pool, Fields::default(), modifiers)?;
                }
                Phase::Addition(addition) => {
                    let Some(fields) = addition.poll(work, cancel)? else {
                        return Ok(Action::Yield);
                    };
                    self.phase = self.emit(input, pool, fields, BudgetVec::new(pool))?;
                }
                Phase::Emit(emitter) => {
                    let Some(cue) = emitter.poll(input, limits, work, cancel)? else {
                        return Ok(Action::Yield);
                    };
                    self.phase = Phase::Done;
                    return Ok(Action::Cue(cue));
                }
                Phase::Done => return Ok(Action::Complete),
            }
        }
    }
}
