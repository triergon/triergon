//! The caller-owned source protocol a player or probe drives without blocking: a
//! [`Source`] answers an [`InputRequest`] at once with a [`SourceReply`], which
//! [`SourceReply::provide_to`] hands to whatever asked ([`Provide`]). A
//! [`SourceOwner`] aborts and closes a source exactly once, and [`ByteSource`] serves
//! immutable local bytes. HTTP, authentication and retries stay outside.
use crate::{
    Bytes, Error, InputCursor, InputRequest, InputToken, InputWindow, Result, SourceFailure,
    SourceId, WorkBudget,
};
use std::sync::atomic::{AtomicBool, Ordering};

/// A source of input that answers promptly: it performs no blocking wait inside
/// [`Self::poll`] and charges its own bounded work before any I/O. Neither a pending
/// nor an empty reply is the end of the source.
pub trait Source {
    /// Answers `request` within `work`, promptly: data, the end, a transport failure,
    /// or `Pending`.
    fn poll(
        &mut self,
        request: &InputRequest,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<SourceReply>;
    /// Invalidates an outstanding range operation, without closing the source.
    fn invalidate(&mut self);
    /// Wakes or cancels any producer and makes its ownership quiescent before it
    /// returns.
    fn abort(&mut self);
    /// Releases the source. It follows `abort`, and no call follows it.
    fn close(&mut self);
}

/// A source's answer to one input request.
#[derive(Debug)]
pub enum SourceReply {
    /// Bytes at the requested offset. An empty window is not the end.
    Data(InputWindow),
    /// The source ends at the requested offset.
    End {
        /// The source the request named.
        source: SourceId,
        /// The request's token.
        token: InputToken,
    },
    /// The transport failed.
    Failed {
        /// The source the request named.
        source: SourceId,
        /// The request's token.
        token: InputToken,
        /// How it failed.
        failure: SourceFailure,
    },
    /// No answer yet: poll again later.
    Pending,
}

/// What takes a source's replies: an [`InputCursor`], or a reader that answers its own
/// input requests.
pub trait Provide {
    /// Answers the last request with `window`.
    fn provide(&mut self, window: &InputWindow) -> Result<()>;
    /// Answers it with the end of the source.
    fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()>;
    /// Answers it with a failure; every later read returns that failure.
    fn fail(&mut self, source: SourceId, token: InputToken, failure: SourceFailure) -> Result<()>;
}

impl Provide for InputCursor {
    #[inline]
    fn provide(&mut self, window: &InputWindow) -> Result<()> {
        InputCursor::provide(self, window)
    }
    #[inline]
    fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        InputCursor::finish(self, source, token)
    }
    #[inline]
    fn fail(&mut self, source: SourceId, token: InputToken, failure: SourceFailure) -> Result<()> {
        InputCursor::fail(self, source, token, failure)
    }
}

impl SourceReply {
    /// Hands the reply to `input`, and returns whether it made progress: `false` for
    /// `Pending` or empty data. A failure reply fails with `Transport` or `Cancelled`.
    pub fn provide_to<P: Provide + ?Sized>(&self, input: &mut P) -> Result<bool> {
        match self {
            Self::Pending => Ok(false),
            Self::Data(window) => {
                input.provide(window)?;
                Ok(!window.bytes().is_empty())
            }
            Self::End { source, token } => {
                input.finish(*source, token.clone())?;
                Ok(true)
            }
            Self::Failed {
                source,
                token,
                failure,
            } => {
                input.fail(*source, token.clone(), *failure)?;
                Err((*failure).into())
            }
        }
    }
}

/// Immutable local bytes as a source, for startup assets and tests. Replies share
/// the bytes' existing pool charge.
#[derive(Debug)]
pub struct ByteSource {
    source: SourceId,
    bytes: Bytes,
    quantum: usize,
    closed: bool,
}

impl ByteSource {
    /// A source of `bytes` as `source`, replying with at most `quantum` bytes. Fails with
    /// `Invalid` when `quantum` is zero.
    pub fn new(source: SourceId, bytes: Bytes, quantum: usize) -> Result<Self> {
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
    ) -> Result<SourceReply> {
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

/// The one owner of a source: it aborts and closes the source once, at the latest
/// when it drops.
#[derive(Debug)]
pub struct SourceOwner<S: Source> {
    source: Option<S>,
    aborted: bool,
}

impl<S: Source> SourceOwner<S> {
    /// Takes ownership of `source`.
    pub fn new(source: S) -> Self {
        Self {
            source: Some(source),
            aborted: false,
        }
    }
    /// The same ownership over `map(source)`; `Closed` after an abort.
    pub fn map<T: Source>(mut self, map: impl FnOnce(S) -> T) -> Result<SourceOwner<T>> {
        if self.aborted {
            return Err(Error::Closed);
        }
        let source = self.source.take().ok_or(Error::Closed)?;
        Ok(SourceOwner::new(map(source)))
    }
    /// The source, until it is closed.
    pub fn source_mut(&mut self) -> Option<&mut S> {
        self.source.as_mut()
    }
    /// Invalidates an outstanding range operation of the source.
    pub fn invalidate(&mut self) {
        if let Some(s) = self.source.as_mut() {
            s.invalidate();
        }
    }
    /// Polls the source: `Pending` without work left, `Cancelled` when `c` is set, and
    /// `Invalid` after `close`.
    pub fn poll(
        &mut self,
        r: &InputRequest,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<SourceReply> {
        if c.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if w.remaining() == 0 {
            return Ok(SourceReply::Pending);
        }
        self.source
            .as_mut()
            .ok_or(Error::Invalid("source closed"))?
            .poll(r, w, c)
    }
    /// Aborts the source, once.
    pub fn abort(&mut self) {
        if !self.aborted {
            if let Some(s) = self.source.as_mut() {
                s.abort();
            }
            self.aborted = true;
        }
    }
    /// Aborts, then closes the source, once.
    pub fn close(&mut self) {
        self.abort();
        if let Some(mut s) = self.source.take() {
            s.close();
        }
    }
}

impl<S: Source> Drop for SourceOwner<S> {
    fn drop(&mut self) {
        self.close();
    }
}
