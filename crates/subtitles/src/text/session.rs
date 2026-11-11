//! Resumable per-input operation; repeated loops perform actual parsing.
use super::{
    Cue, Encoding, Format, Input, Limits,
    emit::{Emitter, Fields, Plan, TextSpan, Times},
    sample,
    unicode::{Cursor, Poll, Probe, cancelled},
};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug)]
enum Phase {
    Init,
    Length,
    Probe,
    Validate,
    Boxes,
    Addition,
    Emit,
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
    text_range: Range<usize>,
    decode_range: Range<usize>,
    encoding: Encoding,
    decoded_len: usize,
    probe: Option<Probe>,
    validator: Option<Cursor>,
    tx3g: Option<sample::Tx3g>,
    addition: Option<sample::Addition>,
    fields: Fields,
    emitter: Option<Emitter>,
}

impl Session {
    pub(super) fn new(input: &Input, pool: &Budget, iso_tx3g: bool) -> Self {
        Self {
            phase: Phase::Init,
            text_range: 0..input.payload.len(),
            decode_range: 0..input.payload.len(),
            encoding: Encoding::Opaque,
            decoded_len: 0,
            probe: None,
            validator: None,
            tx3g: (input.format == Format::Tx3g).then(|| sample::Tx3g::new(pool, iso_tx3g)),
            addition: None,
            fields: Fields::default(),
            emitter: None,
        }
    }

    fn sample_plan(&mut self, input: &Input, pool: &Budget) -> Result<()> {
        let modifiers = if let Some(tx3g) = self.tx3g.as_mut() {
            tx3g.take_modifiers(pool)
        } else {
            BudgetVec::new(pool)
        };
        let plan = Plan {
            span: TextSpan {
                raw: 0..input.payload.len(),
                text: self.text_range.clone(),
                decode: self.decode_range.clone(),
                encoding: self.encoding,
                decoded_len: self.decoded_len,
            },
            fields: std::mem::take(&mut self.fields),
            times: Times::sample(input)?,
            modifiers,
        };
        self.emitter = Some(Emitter::new(plan, pool));
        self.phase = Phase::Emit;
        Ok(())
    }

    fn after_validation(&mut self, input: &Input, pool: &Budget) -> Result<()> {
        match input.format {
            Format::Tx3g => self.phase = Phase::Boxes,
            Format::MatroskaWebVtt => {
                if let Some(addition) = input.addition.as_ref().filter(|addition| addition.id == 1)
                {
                    self.addition = Some(sample::Addition::new(addition.bytes.len()));
                    self.phase = Phase::Addition;
                } else {
                    self.sample_plan(input, pool)?;
                }
            }
            Format::Utf8Packet => self.sample_plan(input, pool)?,
            Format::OpaqueAss => return Err(Error::Invalid("opaque text validation phase")),
        }
        Ok(())
    }

    pub(super) fn step(
        &mut self,
        input: &Input,
        pool: &Budget,
        limits: Limits,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Action> {
        loop {
            cancelled(cancel)?;
            match self.phase {
                Phase::Init => {
                    if !work.try_charge(1) {
                        return Ok(Action::Yield);
                    }
                    match input.format {
                        Format::OpaqueAss => {
                            self.encoding = Encoding::Opaque;
                            self.sample_plan(input, pool)?;
                        }
                        Format::Tx3g => self.phase = Phase::Length,
                        Format::Utf8Packet | Format::MatroskaWebVtt => {
                            if input.encoding != Encoding::Utf8 {
                                return Err(Error::Invalid("text format requires UTF-8"));
                            }
                            self.probe = Some(Probe::new(self.text_range.clone(), Encoding::Utf8));
                            self.phase = Phase::Probe;
                        }
                    }
                }
                Phase::Length => {
                    match self
                        .tx3g
                        .as_mut()
                        .ok_or(Error::Invalid("missing tx3g envelope"))?
                        .text(input.payload.as_ref(), work, cancel)?
                    {
                        Poll::Yield => return Ok(Action::Yield),
                        Poll::End => return Err(Error::Invalid("tx3g envelope ended")),
                        Poll::Ready(range) => {
                            self.text_range = range.clone();
                            self.probe = Some(Probe::new(range, Encoding::Tx3gInBand));
                            self.phase = Phase::Probe;
                        }
                    }
                }
                Phase::Probe => {
                    match self
                        .probe
                        .as_mut()
                        .ok_or(Error::Invalid("missing encoding probe"))?
                        .poll(input.payload.as_ref(), work, cancel)?
                    {
                        Poll::Yield => return Ok(Action::Yield),
                        Poll::End => return Err(Error::Invalid("encoding probe ended")),
                        Poll::Ready((encoding, range)) => {
                            self.encoding = encoding;
                            self.decode_range = range.clone();
                            self.validator = Some(Cursor::new(range, encoding));
                            self.probe = None;
                            self.phase = Phase::Validate;
                        }
                    }
                }
                Phase::Validate => {
                    let validator = self
                        .validator
                        .as_mut()
                        .ok_or(Error::Invalid("missing Unicode validator"))?;
                    match validator.poll(input.payload.as_ref(), work, cancel)? {
                        Poll::Yield => return Ok(Action::Yield),
                        Poll::Ready(_) => {}
                        Poll::End => {
                            self.decoded_len = validator.decoded_len();
                            self.validator = None;
                            self.after_validation(input, pool)?;
                        }
                    }
                }
                Phase::Boxes => {
                    match self
                        .tx3g
                        .as_mut()
                        .ok_or(Error::Invalid("missing tx3g modifiers"))?
                        .boxes(input.payload.as_ref(), limits, work, cancel)?
                    {
                        Poll::Yield => return Ok(Action::Yield),
                        Poll::End => return Err(Error::Invalid("tx3g modifier parser ended")),
                        Poll::Ready(()) => self.sample_plan(input, pool)?,
                    }
                }
                Phase::Addition => {
                    let bytes = &input
                        .addition
                        .as_ref()
                        .ok_or(Error::Invalid("missing WebVTT addition"))?
                        .bytes;
                    match self
                        .addition
                        .as_mut()
                        .ok_or(Error::Invalid("missing addition parser"))?
                        .poll(bytes.as_ref(), work, cancel)?
                    {
                        Poll::Yield => return Ok(Action::Yield),
                        Poll::End => return Err(Error::Invalid("WebVTT addition parser ended")),
                        Poll::Ready(fields) => {
                            self.fields = fields;
                            self.addition = None;
                            self.sample_plan(input, pool)?;
                        }
                    }
                }
                Phase::Emit => {
                    match self
                        .emitter
                        .as_mut()
                        .ok_or(Error::Invalid("missing cue emitter"))?
                        .poll(input, limits, work, cancel)?
                    {
                        Poll::Yield => return Ok(Action::Yield),
                        Poll::End => return Err(Error::Invalid("cue emitter ended")),
                        Poll::Ready(cue) => {
                            self.emitter = None;
                            self.phase = Phase::Done;
                            return Ok(Action::Cue(cue));
                        }
                    }
                }
                Phase::Done => return Ok(Action::Complete),
            }
        }
    }
}
