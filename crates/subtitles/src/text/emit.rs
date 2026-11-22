//! A cue built from its input: the text decoded into charged storage of
//! exactly the counted size, or shared with the payload when it is UTF-8.
use super::{
    Cue, Encoding, Field, Input, Limits, Modifier,
    unicode::{Cursor, Poll, Scalar, cancelled},
};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, Time, WorkBudget};

#[derive(Debug, Default)]
pub(crate) struct Fields {
    pub identifier: Option<Field>,
    pub settings: Option<Field>,
    pub comments: Option<Field>,
}

#[derive(Debug)]
pub(crate) struct Times {
    pub start: Option<Time>,
    pub end: Option<Time>,
    pub duration: Option<Time>,
}

impl Times {
    pub(super) fn sample(input: &Input) -> Result<Self> {
        let start = input.timing.start();
        let duration = input.timing.duration();
        let end = match (start, duration) {
            (Some(start), Some(duration)) => Some(start.checked_add(duration)?),
            _ => None,
        };
        Ok(Self {
            start,
            end,
            duration,
        })
    }
}

#[derive(Debug)]
pub(crate) struct TextSpan {
    pub raw: Range<usize>,
    pub text: Range<usize>,
    pub decode: Range<usize>,
    pub encoding: Encoding,
    /// Counted during actual validation/framing; no allocation-size guess.
    pub decoded_len: usize,
}

#[derive(Debug)]
pub(crate) struct Plan {
    pub span: TextSpan,
    pub fields: Fields,
    pub times: Times,
    pub modifiers: BudgetVec<Modifier>,
}

/// The cue's character view, decoded as its encoding requires.
#[derive(Debug)]
enum Text {
    /// None: the encoding is opaque.
    Opaque,
    /// UTF-8: the payload's text range, once paid for.
    Shared,
    /// Another encoding, written as UTF-8 into storage of exactly the
    /// counted size.
    Transcode(Transcode),
    /// The view.
    Decoded(Bytes),
}

#[derive(Debug)]
struct Transcode {
    cursor: Cursor,
    output: BudgetVec<u8>,
    /// Whether the output's storage is reserved.
    reserved: bool,
    /// A decoded character not yet written.
    pending: Option<Scalar>,
    /// Whether every character is written.
    encoded: bool,
}

#[derive(Debug)]
pub(crate) struct Emitter {
    plan: Plan,
    text: Text,
}

impl Emitter {
    pub(super) fn new(plan: Plan, pool: &Budget) -> Self {
        let text = match plan.span.encoding {
            Encoding::Utf16Be | Encoding::Utf16Le | Encoding::Multibyte(_) => {
                Text::Transcode(Transcode {
                    cursor: Cursor::new(plan.span.decode.clone(), plan.span.encoding),
                    output: BudgetVec::new(pool),
                    reserved: false,
                    pending: None,
                    encoded: false,
                })
            }
            Encoding::Opaque => Text::Opaque,
            _ => Text::Shared,
        };
        Self { plan, text }
    }

    /// The cue; `None` when `work` runs out first. One unit for the view,
    /// one per character written and one per byte decoded, then one for the
    /// cue.
    pub(super) fn poll(
        &mut self,
        input: &Input,
        limits: Limits,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Cue>> {
        cancelled(cancel)?;
        let span = &self.plan.span;
        let length = span.decoded_len;
        if span.encoding != Encoding::Opaque && length > limits.max_decoded_bytes {
            return Err(Error::Limit {
                requested: length,
                maximum: limits.max_decoded_bytes,
            });
        }
        match &mut self.text {
            Text::Opaque | Text::Decoded(_) => {}
            Text::Shared => {
                if span.encoding != Encoding::Utf8 {
                    return Err(Error::Invalid("unresolved cue encoding"));
                }
                if !work.try_charge(1) {
                    return Ok(None);
                }
                if span.text.len() != length {
                    return Err(Error::Invalid("UTF-8 span length disagrees with count"));
                }
                self.text = Text::Decoded(input.payload.slice(span.text.clone())?);
            }
            Text::Transcode(transcode) => {
                let Some(decoded) = transcode.poll(input, length, work, cancel)? else {
                    return Ok(None);
                };
                self.text = Text::Decoded(decoded);
            }
        }
        cancelled(cancel)?;
        if !work.try_charge(1) {
            return Ok(None);
        }
        let decoded_utf8 = match std::mem::replace(&mut self.text, Text::Opaque) {
            Text::Decoded(bytes) => Some(bytes),
            _ => None,
        };
        let plan = &mut self.plan;
        Ok(Some(Cue {
            input: input.clone(),
            raw_range: plan.span.raw.clone(),
            text_range: plan.span.text.clone(),
            decoded_utf8,
            effective_encoding: plan.span.encoding,
            identifier: plan.fields.identifier.take(),
            settings: plan.fields.settings.take(),
            comments: plan.fields.comments.take(),
            start: plan.times.start,
            end: plan.times.end,
            duration: plan.times.duration,
            modifiers: {
                let empty = BudgetVec::new(plan.modifiers.budget());
                std::mem::replace(&mut plan.modifiers, empty)
            },
        }))
    }
}

impl Transcode {
    /// The text as UTF-8 of exactly `length` bytes; `None` when `work` runs
    /// out first. One unit to reserve the storage, one per character written
    /// and one to freeze it, besides the cursor's unit per byte.
    fn poll(
        &mut self,
        input: &Input,
        length: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Bytes>> {
        if !self.reserved {
            if !work.try_charge(1) {
                return Ok(None);
            }
            self.output.try_reserve_exact(length)?;
            self.reserved = true;
        }
        while !self.encoded {
            cancelled(cancel)?;
            if let Some(scalar) = self.pending {
                if !work.try_charge(1) {
                    return Ok(None);
                }
                let mut encoded = [0_u8; 4];
                let encoded = scalar.value.encode_utf8(&mut encoded).as_bytes();
                let after = self
                    .output
                    .len()
                    .checked_add(encoded.len())
                    .ok_or(Error::Overflow)?;
                if after > length {
                    return Err(Error::Invalid("decoded text exceeded exact count"));
                }
                self.output.try_extend_from_slice(encoded)?;
                self.pending = None;
                continue;
            }
            match self.cursor.poll(input.payload.as_ref(), work, cancel)? {
                Poll::Yield => return Ok(None),
                Poll::Ready(scalar) => self.pending = Some(scalar),
                Poll::End => {
                    if self.output.len() != length {
                        return Err(Error::Invalid("decoded text fell short of exact count"));
                    }
                    self.encoded = true;
                }
            }
        }
        if !work.try_charge(1) {
            return Ok(None);
        }
        let empty = BudgetVec::new(self.output.budget());
        match std::mem::replace(&mut self.output, empty).freeze() {
            Ok(bytes) => Ok(Some(bytes)),
            Err(rejection) => {
                self.output = rejection.value;
                Err(rejection.error)
            }
        }
    }
}
