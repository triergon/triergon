//! Explicit TS entity boundaries; source I/O generations remain core-owned.
use crate::{PacketFormat, SourceEpochs};
use triergon_core::{Budget, Error, InputCursor, InputToken, Result, SourceLocation};

/// An event issued by the transport parser for a particular input operation.
#[derive(Debug)]
pub struct ObservedTrace<T> {
    pub(crate) event: T,
    pub(crate) origin: TraceOrigin,
}
impl<T> ObservedTrace<T> {
    pub fn event(&self) -> &T {
        &self.event
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TraceOrigin {
    pub(crate) issuer: usize,
    pub(crate) token: InputToken,
    pub(crate) source: triergon_core::SourceId,
    pub(crate) epochs: SourceEpochs,
}

/// Witness of a real, fully drained source entity, not logical media EOF.
/// Only the composed source operation can create this value.
#[derive(Clone, Debug)]
pub struct MediaEntityEnd {
    pub(crate) location: SourceLocation,
    pub(crate) issuer: usize,
    pub(crate) token: InputToken,
    pub(crate) epochs: SourceEpochs,
    pub(crate) program: u16,
    pub(crate) format: PacketFormat,
    pub(crate) budget: Budget,
}
impl MediaEntityEnd {
    pub fn location(&self) -> SourceLocation {
        self.location
    }
    pub fn epochs(&self) -> SourceEpochs {
        self.epochs
    }
    /// A continuation must be the same core cursor's immediately next request,
    /// before any byte from the new entity has been consumed.
    pub fn validate_continuation(&self, input: &InputCursor) -> Result<()> {
        if !self.budget.same_pool(input.budget()) {
            return Err(Error::ForeignPool);
        }
        let token = input.token();
        if token.generation() != self.token.generation() {
            return Err(Error::StaleGeneration {
                expected: self.token.generation(),
                received: token.generation(),
            });
        }
        let expected = self.token.request().checked_add(1).ok_or(Error::Overflow)?;
        if token.request() != expected {
            return Err(Error::StaleRequest {
                expected,
                received: token.request(),
            });
        }
        if input.source() == self.location.source {
            return Err(Error::SourceMismatch);
        }
        if input.position() != 0 {
            return Err(Error::Invalid("media entity continuation after input"));
        }
        Ok(())
    }
}
