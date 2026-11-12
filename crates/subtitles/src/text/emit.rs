//! Charged immutable cue materialization, preserving input and exact spans.
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

#[derive(Debug)]
pub(crate) struct Emitter {
    plan: Option<Plan>,
    cursor: Option<Cursor>,
    builder: Option<BudgetVec<u8>>,
    reserved: bool,
    pending: Option<Scalar>,
    encoded: bool,
    decoded: Option<Bytes>,
}

impl Emitter {
    pub(super) fn new(plan: Plan, pool: &Budget) -> Self {
        let transcode = matches!(
            plan.span.encoding,
            Encoding::Utf16Be | Encoding::Utf16Le | Encoding::Multibyte(_)
        );
        let cursor = transcode.then(|| Cursor::new(plan.span.decode.clone(), plan.span.encoding));
        let builder = transcode.then(|| BudgetVec::new(pool));
        Self {
            plan: Some(plan),
            cursor,
            builder,
            reserved: false,
            pending: None,
            encoded: false,
            decoded: None,
        }
    }

    pub(super) fn poll(
        &mut self,
        input: &Input,
        limits: Limits,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Poll<Cue>> {
        cancelled(cancel)?;
        let plan = self
            .plan
            .as_ref()
            .ok_or(Error::Invalid("cue already emitted"))?;
        let encoding = plan.span.encoding;
        let length = plan.span.decoded_len;
        if encoding != Encoding::Opaque && length > limits.max_decoded_bytes {
            return Err(Error::Limit {
                requested: length,
                maximum: limits.max_decoded_bytes,
            });
        }
        if encoding == Encoding::Utf8 {
            if self.decoded.is_none() {
                if !work.try_charge(1) {
                    return Ok(Poll::Yield);
                }
                if plan.span.text.len() != length {
                    return Err(Error::Invalid("UTF-8 span length disagrees with count"));
                }
                self.decoded = Some(input.payload.slice(plan.span.text.clone())?);
            }
        } else if matches!(
            encoding,
            Encoding::Utf16Be | Encoding::Utf16Le | Encoding::Multibyte(_)
        ) {
            if !self.reserved {
                if !work.try_charge(1) {
                    return Ok(Poll::Yield);
                }
                self.builder
                    .as_mut()
                    .ok_or(Error::Invalid("missing UTF-8 builder"))?
                    .try_reserve_exact(length)?;
                self.reserved = true;
            }
            while !self.encoded {
                cancelled(cancel)?;
                if let Some(scalar) = self.pending {
                    if !work.try_charge(1) {
                        return Ok(Poll::Yield);
                    }
                    let mut encoded = [0_u8; 4];
                    let encoded = scalar.value.encode_utf8(&mut encoded).as_bytes();
                    let builder = self
                        .builder
                        .as_mut()
                        .ok_or(Error::Invalid("missing UTF-8 builder"))?;
                    let after = builder
                        .len()
                        .checked_add(encoded.len())
                        .ok_or(Error::Overflow)?;
                    if after > length {
                        return Err(Error::Invalid("decoded text exceeded exact count"));
                    }
                    builder.try_extend_from_slice(encoded)?;
                    self.pending = None;
                    continue;
                }
                match self
                    .cursor
                    .as_mut()
                    .ok_or(Error::Invalid("missing Unicode cursor"))?
                    .poll(input.payload.as_ref(), work, cancel)?
                {
                    Poll::Yield => return Ok(Poll::Yield),
                    Poll::Ready(scalar) => self.pending = Some(scalar),
                    Poll::End => {
                        if self
                            .builder
                            .as_ref()
                            .ok_or(Error::Invalid("missing UTF-8 builder"))?
                            .len()
                            != length
                        {
                            return Err(Error::Invalid("decoded text fell short of exact count"));
                        }
                        self.encoded = true;
                    }
                }
            }
            if self.decoded.is_none() {
                if !work.try_charge(1) {
                    return Ok(Poll::Yield);
                }
                let builder = self
                    .builder
                    .take()
                    .ok_or(Error::Invalid("missing UTF-8 freeze builder"))?;
                match builder.freeze() {
                    Ok(bytes) => self.decoded = Some(bytes),
                    Err(rejection) => {
                        self.builder = Some(rejection.buffer);
                        return Err(rejection.error);
                    }
                }
            }
        } else if encoding != Encoding::Opaque {
            return Err(Error::Invalid("unresolved cue encoding"));
        }
        cancelled(cancel)?;
        if !work.try_charge(1) {
            return Ok(Poll::Yield);
        }
        let plan = self
            .plan
            .take()
            .ok_or(Error::Invalid("cue already emitted"))?;
        Ok(Poll::Ready(Cue {
            input: input.clone(),
            raw_range: plan.span.raw,
            text_range: plan.span.text,
            decoded_utf8: self.decoded.take(),
            effective_encoding: encoding,
            identifier: plan.fields.identifier,
            settings: plan.fields.settings,
            comments: plan.fields.comments,
            start: plan.times.start,
            end: plan.times.end,
            duration: plan.times.duration,
            modifiers: plan.modifiers,
        }))
    }
}
