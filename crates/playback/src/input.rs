//! Caller-owned source protocol and immutable local bytes. HTTP/auth/retry stay outside.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Bytes, Error, InputRequest, InputToken, InputWindow, SourceFailure, SourceId, WorkBudget,
};
/// A source must return promptly. It performs no blocking wait inside poll.
/// Source implementations charge their own bounded poll work before I/O.
/// Any external producer must be quiesced by abort before close releases its state.
/// Neither pending nor an empty response signifies EOF. HTTP policy stays outside.
pub trait Source {
    fn poll(
        &mut self,
        request: &InputRequest,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<SourceReply>;
    /// Invalidate an outstanding range operation, without closing the source.
    fn invalidate(&mut self);
    /// Wake/cancel any producer, and synchronously make its ownership quiescent.
    fn abort(&mut self);
    fn close(&mut self);
}
#[derive(Debug)]
pub enum SourceReply {
    Data(InputWindow),
    End {
        source: SourceId,
        token: InputToken,
    },
    Failed {
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    },
    Pending,
}

/// Actual bounded local bytes for startup assets and deterministic functional tests.
/// The immutable caller input retains its existing pool charge; ranges share it.
pub struct ByteSource {
    source: SourceId,
    bytes: Bytes,
    quantum: usize,
    closed: bool,
}
impl ByteSource {
    pub fn new(source: SourceId, bytes: Bytes, quantum: usize) -> triergon_core::Result<Self> {
        if quantum == 0 {
            return Err(Error::Invalid("zero read quantum"));
        }
        Ok(Self {
            source,
            bytes,
            quantum,
            closed: false,
        })
    }
}
impl Source for ByteSource {
    fn poll(
        &mut self,
        r: &InputRequest,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<SourceReply> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.closed {
            return Err(Error::Invalid("closed source"));
        }
        if r.source != self.source {
            return Err(Error::SourceMismatch);
        }
        if !work.try_charge(1) {
            return Ok(SourceReply::Pending);
        }
        let at = usize::try_from(r.offset).map_err(|_| Error::Overflow)?;
        if at > self.bytes.len() {
            return Err(Error::Invalid("range beyond source"));
        }
        if at == self.bytes.len() {
            return Ok(SourceReply::End {
                source: r.source,
                token: r.token.clone(),
            });
        }
        let count = (self.bytes.len() - at).min(self.quantum).min(r.maximum);
        let bytes = self.bytes.slice(at..at + count)?;
        Ok(SourceReply::Data(InputWindow::new(
            r.source,
            r.token.clone(),
            r.offset,
            bytes,
        )?))
    }
    fn invalidate(&mut self) {}
    fn abort(&mut self) {
        self.closed = true;
    }
    fn close(&mut self) {
        self.closed = true;
    }
}
