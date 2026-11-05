//! Explicit TS entity boundaries; source I/O generations remain core-owned.
use crate::{PacketFormat, SourceEpochs};
use triergon_core::{Budget, Error, InputCursor, InputToken, Result, SourceLocation};

/// Witness of a real, fully drained source entity, not logical media EOF.
/// Only the composed source operation can create this value.
#[derive(Clone, Debug)]
pub struct MediaEntityEndV1 {
    pub(crate) location: SourceLocation,
    pub(crate) token: InputToken,
    pub(crate) epochs: SourceEpochs,
    pub(crate) program: u16,
    pub(crate) format: PacketFormat,
    pub(crate) budget: Budget,
}
impl MediaEntityEndV1 {
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
