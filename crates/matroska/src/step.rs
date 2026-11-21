//! The step vocabulary every resumable job of this crate answers with.
use crate::element::ElementEvent;
use crate::{Error, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::InputRequest;

#[derive(Debug)]
pub enum ParseStep<T = ElementEvent> {
    Event(T),
    NeedInput(InputRequest),
    Yield,
    Backpressure { requested: usize, available: usize },
    Finished,
}

#[inline(always)]
pub(crate) fn pending<T, U>(step: ParseStep<T>) -> Result<ParseStep<U>> {
    Ok(match step {
        ParseStep::Yield => ParseStep::Yield,
        ParseStep::NeedInput(request) => ParseStep::NeedInput(request),
        ParseStep::Backpressure {
            requested,
            available,
        } => ParseStep::Backpressure {
            requested,
            available,
        },
        ParseStep::Finished => ParseStep::Finished,
        ParseStep::Event(_) => return Err(Error::Invalid("unexpected job event")),
    })
}

pub(crate) fn check(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// Required state has a success-only branch: no temporary Error is constructed
/// on Some. In particular, do not replace hot calls with eager ok_or(Error),
/// whose Io-containing error enum can generate cleanup/drop paths on success.
pub(crate) trait Required<T> {
    fn required(self, reason: &'static str) -> Result<T>;
}
impl<T> Required<T> for Option<T> {
    #[inline(always)]
    fn required(self, reason: &'static str) -> Result<T> {
        let Some(value) = self else {
            return Err(Error::Invalid(reason));
        };
        Ok(value)
    }
}
