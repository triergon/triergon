//! What one call to a resumable job of this crate did. The reader's own steps are
//! the core [`Step`]; a job has no source location to end at and answers with
//! [`ParseStep`].
use crate::{Error, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Backpressure, InputRequest, Step};

/// The outcome of one call to a resumable job: its next event, or why it stopped
/// without one. After any variant but `Event` and `Finished` the same call can
/// be repeated once the condition is resolved.
#[derive(Debug)]
pub(crate) enum ParseStep<T> {
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

impl<T> ParseStep<T> {
    /// This step, which carries no event, as a step of the reader. A job is stepped
    /// until its event and not again, so it has no end to report.
    #[inline(always)]
    pub(crate) fn waiting<U>(self) -> Result<Step<U, Backpressure>> {
        Ok(match self {
            Self::Yield => Step::Yield,
            Self::NeedInput(request) => Step::NeedInput(request),
            Self::Backpressure {
                requested,
                available,
            } => Step::Backpressure(Backpressure {
                requested,
                available,
            }),
            Self::Event(_) | Self::Finished => {
                return Err(Error::Invalid("unexpected job event"));
            }
        })
    }
    /// The step of a job over bytes it already holds, which neither asks for input
    /// nor ends.
    pub(crate) fn bounded(self) -> Result<Step<T, Backpressure>> {
        Ok(match self {
            Self::Event(value) => Step::Ready(value),
            Self::Yield => Step::Yield,
            Self::Backpressure {
                requested,
                available,
            } => Step::Backpressure(Backpressure {
                requested,
                available,
            }),
            Self::NeedInput(_) | Self::Finished => {
                return Err(Error::Invalid("unexpected job event"));
            }
        })
    }
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
