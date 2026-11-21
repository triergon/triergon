//! The step vocabulary every resumable job of this crate answers with.
use crate::element::ElementEvent;
use crate::{Error, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::InputRequest;

/// The outcome of one call to a resumable job: its next event, or why it stopped
/// without one. After any variant but `Event` and `Finished` the same call can
/// be repeated once the condition is resolved.
#[derive(Debug)]
pub enum ParseStep<T = ElementEvent> {
    /// The job's next result.
    Event(T),
    /// The job needs the bytes this request names before it can continue.
    NeedInput(InputRequest),
    /// The work budget ran out; call again with a fresh budget.
    Yield,
    /// Storage the job needs does not fit the pool until retained output is released.
    Backpressure {
        /// Bytes the job tried to reserve.
        requested: usize,
        /// Bytes free in the pool when the reservation was refused.
        available: usize,
    },
    /// The job reached the end of its input range and has no further events.
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

/// Turns missing required state into `Error::Invalid(reason)`. `required` keeps a
/// success-only branch, so hot callers build no error value on `Some`; callers
/// keep it instead of `ok_or`.
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
