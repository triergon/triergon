pub use crate::raw_video::{
    PlaybackInput, PlaybackInputKind, PlaybackProbe, PlaybackProbeStep, ProbeReplay,
    RawVideoProbeLimits,
};
use crate::{
    Format, Result,
    input::{Source, SourceReply},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputWindow, SourceCapabilities,
    SourceFailure, SourceId, Step, WorkBudget,
};
/// The injected source remains the sole transport owner. Dropping a partially
/// opened format aborts/quiesces and closes exactly once.
pub struct SourceOwner<S: Source> {
    source: Option<S>,
    aborted: bool,
}
impl<S: Source> SourceOwner<S> {
    pub fn new(source: S) -> Self {
        Self {
            source: Some(source),
            aborted: false,
        }
    }
    pub(crate) fn map<T: Source>(mut self, map: impl FnOnce(S) -> T) -> Result<SourceOwner<T>> {
        if self.aborted {
            return Err(crate::Error::Closed);
        }
        let source = self.source.take().ok_or(crate::Error::Closed)?;
        Ok(SourceOwner::new(map(source)))
    }
    pub(crate) fn source_mut(&mut self) -> Option<&mut S> {
        self.source.as_mut()
    }
    pub fn invalidate(&mut self) {
        if let Some(s) = self.source.as_mut() {
            s.invalidate();
        }
    }
    pub fn poll(
        &mut self,
        r: &InputRequest,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<SourceReply> {
        if c.load(Ordering::Relaxed) {
            return Err(Error::Cancelled.into());
        }
        if w.remaining() == 0 {
            return Ok(SourceReply::Pending);
        }
        Ok(self
            .source
            .as_mut()
            .ok_or(Error::Invalid("source closed"))?
            .poll(r, w, c)?)
    }
    pub fn abort(&mut self) {
        if !self.aborted {
            if let Some(s) = self.source.as_mut() {
                s.abort();
            }
            self.aborted = true;
        }
    }
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
pub fn provide(input: &mut InputCursor, reply: &SourceReply) -> Result<bool> {
    match reply {
        SourceReply::Pending => Ok(false),
        SourceReply::Data(w) => {
            input.provide(w)?;
            Ok(!w.bytes().is_empty())
        }
        SourceReply::End { source, token } => {
            input.finish(*source, token.clone())?;
            Ok(true)
        }
        SourceReply::Failed {
            source,
            token,
            failure,
        } => {
            input.fail(*source, token.clone(), *failure)?;
            Err(match failure {
                SourceFailure::Transport => Error::Transport,
                SourceFailure::Cancelled => Error::Cancelled,
            }
            .into())
        }
    }
}
pub struct SelectedInput<S: Source> {
    pub input: InputCursor,
    pub source: SourceOwner<S>,
    pub format: Format,
}
pub enum ProbeProgress {
    Yield,
    NeedInput(InputRequest),
    Selected(Format),
}
/// Original bounded signature selection. Filenames and caller expected output
/// have no role. Three physical sync bytes distinguish TS188 and M2TS192.
pub struct Probe<S: Source> {
    input: Option<InputCursor>,
    source: Option<SourceOwner<S>>,
    prefix: [u8; 580],
    used: usize,
    request: Option<InputRequest>,
    pending_reply: Option<SourceReply>,
    chosen: Option<Format>,
    failed: bool,
    window: usize,
    cache: BudgetVec<CachedWindow>,
}
impl<S: Source> Probe<S> {
    pub fn new(
        pool: &Budget,
        id: SourceId,
        caps: SourceCapabilities,
        window: usize,
        source: S,
    ) -> Result<Self> {
        let owner = SourceOwner::new(source);
        let input = InputCursor::new(pool, id, caps, window)?;
        Ok(Self {
            input: Some(input),
            source: Some(owner),
            prefix: [0; 580],
            used: 0,
            request: None,
            pending_reply: None,
            chosen: None,
            failed: false,
            window,
            cache: BudgetVec::new(pool),
        })
    }
    pub fn advance(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<ProbeProgress> {
        if self.failed {
            return Err(crate::Error::Closed);
        }
        let result = self.advance_inner(w, c);
        if result.is_err()
            && !matches!(&result, Err(crate::Error::Core(Error::Backpressure { .. })))
        {
            self.failed = true;
            if let Some(source) = self.source.as_mut() {
                source.close();
            }
        }
        result
    }
    fn advance_inner(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<ProbeProgress> {
        if c.load(Ordering::Relaxed) {
            self.source
                .as_mut()
                .ok_or(Error::Invalid("closed probe"))?
                .close();
            return Err(Error::Cancelled.into());
        }
        if let Some(f) = self.chosen {
            return Ok(ProbeProgress::Selected(f));
        }
        if w.remaining() == 0 {
            return Ok(ProbeProgress::Yield);
        }
        let input = self
            .input
            .as_mut()
            .ok_or(Error::Invalid("finished probe"))?;
        if let Some(r) = self.request.clone() {
            // A transport reply has already consumed the producer operation.
            // Keep its exact owner/request across retryable cache pressure.
            if self.pending_reply.is_none() {
                self.pending_reply = Some(
                    self.source
                        .as_mut()
                        .ok_or(Error::Invalid("closed probe"))?
                        .poll(&r, w, c)?,
                );
            }
            let reply = self
                .pending_reply
                .as_ref()
                .ok_or(Error::Invalid("missing probe reply"))?;
            if let SourceReply::Data(window) = reply
                && !window.bytes().is_empty()
            {
                if self.cache.len() >= 581 {
                    return Err(Error::Limit {
                        requested: self.cache.len() + 1,
                        maximum: 581,
                    }
                    .into());
                }
                self.cache
                    .try_push(CachedWindow {
                        offset: window.offset,
                        bytes: window.bytes().clone(),
                    })
                    .map_err(|e| e.error)?;
            }
            if provide(input, reply)? {
                self.request = None;
            }
            self.pending_reply = None;
            return Ok(ProbeProgress::Yield);
        }
        if let Some(f) = detect_container(&self.prefix[..self.used])? {
            self.chosen = Some(f);
            return Ok(ProbeProgress::Selected(f));
        }
        if self.used == self.prefix.len() {
            return Err(
                Error::Unsupported("source signatures do not select MKV, MP4 or TS").into(),
            );
        }
        // Four/eight initial bytes can select MKV/MP4 without reading a whole TS probe.
        let end = if self.used < 4 {
            4
        } else if self.used < 8 {
            8
        } else {
            self.prefix.len()
        };
        match input.read_into(&mut self.prefix[self.used..end], w, c)? {
            Step::Ready(n) => {
                self.used += n;
                Ok(ProbeProgress::Yield)
            }
            Step::NeedInput(r) => {
                self.request = Some(r.clone());
                Ok(ProbeProgress::NeedInput(r))
            }
            Step::Yield => Ok(ProbeProgress::Yield),
            Step::End(_) => Err(Error::Unsupported(
                "short or unknown source signature; explicit format required",
            )
            .into()),
        }
    }
    pub fn finish(mut self) -> Result<SelectedInput<ReplaySource<S>>> {
        if self.failed {
            return Err(crate::Error::Closed);
        }
        let format = self.chosen.ok_or(Error::Invalid("format not selected"))?;
        let old = self.input.take().ok_or(Error::Invalid("probe consumed"))?;
        let input = InputCursor::new(old.budget(), old.source(), old.capabilities(), self.window)?;
        let mut source = self
            .source
            .take()
            .ok_or(Error::Invalid("probe source consumed"))?;
        source.invalidate();
        let source = SourceOwner::new(ReplaySource {
            source: input.source(),
            inner: source,
            cache: std::mem::replace(&mut self.cache, BudgetVec::new(input.budget())),
        });
        Ok(SelectedInput {
            input,
            source,
            format,
        })
    }
}
pub(crate) const CONTAINER_SIGNATURE_BYTES: usize = 389;

pub(crate) fn detect_container(p: &[u8]) -> Result<Option<Format>> {
    if p.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return Ok(Some(Format::Matroska));
    }
    if p.len() >= 8
        && [*b"ftyp", *b"styp", *b"moov", *b"free", *b"wide", *b"mdat"]
            .contains(&p[4..8].try_into().map_err(|_| Error::Truncated)?)
    {
        return Ok(Some(Format::Mp4));
    }
    if p.len() < CONTAINER_SIGNATURE_BYTES {
        return Ok(None);
    }
    let ts = p.len() > 376 && [0, 188, 376].into_iter().all(|i| p[i] == 0x47);
    let m2 = p.len() > 388 && [4, 196, 388].into_iter().all(|i| p[i] == 0x47);
    match (ts, m2) {
        (true, false) => Ok(Some(Format::Ts(triergon_mpegts::PacketFormat::Ts188))),
        (false, true) => Ok(Some(Format::Ts(triergon_mpegts::PacketFormat::M2ts192))),
        (true, true) => Err(Error::Invalid("ambiguous transport signature").into()),
        _ => Ok(None),
    }
}

struct CachedWindow {
    offset: u64,
    bytes: Bytes,
}
/// A probe replays only actual provided prefix owners. Sequential transports do
/// not need to reopen or seek; the next upstream range follows the cached bytes.
pub struct ReplaySource<S: Source> {
    source: SourceId,
    inner: SourceOwner<S>,
    cache: BudgetVec<CachedWindow>,
}
impl<S: Source> Source for ReplaySource<S> {
    fn poll(
        &mut self,
        r: &InputRequest,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> triergon_core::Result<SourceReply> {
        if c.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if r.source != self.source {
            return Err(Error::SourceMismatch);
        }
        if w.remaining() == 0 {
            return Ok(SourceReply::Pending);
        }
        let entries = self.cache.as_slice();
        let at = entries.partition_point(|v| v.offset <= r.offset);
        if let Some(cached) = at.checked_sub(1).and_then(|i| entries.get(i)) {
            let end = cached
                .offset
                .checked_add(cached.bytes.len() as u64)
                .ok_or(Error::Overflow)?;
            if r.offset < end {
                if !w.try_charge(1) {
                    return Ok(SourceReply::Pending);
                }
                let offset =
                    usize::try_from(r.offset - cached.offset).map_err(|_| Error::Overflow)?;
                let n = (cached.bytes.len() - offset).min(r.maximum);
                return Ok(SourceReply::Data(InputWindow::new(
                    r.source,
                    r.token.clone(),
                    r.offset,
                    cached.bytes.slice(offset..offset + n)?,
                )?));
            }
        }
        self.inner.poll(r, w, c).map_err(|e| match e {
            crate::Error::Core(c) => c,
            _ => Error::Invalid("probe upstream error"),
        })
    }
    fn invalidate(&mut self) {
        self.inner.invalidate();
    }
    fn abort(&mut self) {
        self.inner.abort();
    }
    fn close(&mut self) {
        self.inner.close();
    }
}
