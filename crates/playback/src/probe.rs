//! Container probing: [`Probe`] selects Matroska, MP4, TS188 or M2TS192 from a bounded
//! prefix, and the selected input replays that prefix.
use crate::{
    Format, Result,
    source::{Source, SourceOwner, SourceReply},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::signature::{
    self, Container, Selection,
    TransportSync::{M2ts192, Ts188},
    TransportTie,
};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputWindow, SourceCapabilities,
    SourceId, Step, WorkBudget,
};
/// An input whose container format is known, with its source.
#[derive(Debug)]
pub struct SelectedInput<S: Source> {
    /// The input cursor, at offset zero.
    pub input: InputCursor,
    /// The source.
    pub source: SourceOwner<S>,
    /// The container format.
    pub format: Format,
}
/// One step of a [`Probe`].
#[derive(Debug)]
pub enum ProbeProgress {
    /// The work budget ran out: advance again.
    Yield,
    /// The source is pending on this request: advance again once it can reply.
    NeedInput(InputRequest),
    /// The format is known: call `finish`.
    Selected(Format),
}
/// Selects the container format of a source from its first bytes alone, within
/// work budgets. Three sync bytes one packet apart tell TS188 from M2TS192.
#[derive(Debug)]
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
    /// A probe of `source`; `window` is the input window of the selected input.
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
    /// Reads and checks the prefix within `w`. Fails with `Unsupported` when no
    /// signature matches and with `Invalid` when both TS packet sizes match. A failure
    /// other than backpressure closes the source and is final.
    pub fn advance(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<ProbeProgress> {
        if self.failed {
            return Err(Error::Closed);
        }
        let result = self.advance_inner(w, c);
        if result.is_err() && !matches!(&result, Err(Error::Backpressure { .. })) {
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
            return Err(Error::Cancelled);
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
                    });
                }
                self.cache
                    .try_push(CachedWindow {
                        offset: window.offset,
                        bytes: window.bytes().clone(),
                    })
                    .map_err(|e| e.error)?;
            }
            if reply.provide_to(input)? {
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
            return Err(Error::Unsupported(
                "source signatures do not select MKV, MP4 or TS",
            ));
        }
        // Four/eight initial bytes can select MKV/MP4 without reading a whole TS probe.
        let end = if self.used < 4 {
            4
        } else if self.used < 8 {
            8
        } else if self.used < CONTAINER_SIGNATURE_BYTES {
            CONTAINER_SIGNATURE_BYTES
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
            )),
        }
    }
    /// The selected input, replaying the probed prefix. Fails with `Closed` after a
    /// failure and with `Invalid` before selection.
    pub fn finish(mut self) -> Result<SelectedInput<ReplaySource<S>>> {
        if self.failed {
            return Err(Error::Closed);
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
/// Prefix bytes [`detect_container`] needs for a conclusive answer.
pub const CONTAINER_SIGNATURE_BYTES: usize = M2ts192.prefix_len();

/// What playback selects where the signatures leave a choice. An MP4 or QuickTime
/// file opens with `ftyp`, `styp`, `moov`, `free`, `wide` or `mdat`; a `moof`,
/// `sidx` or `skip` first, which `triergon`'s reader probe also accepts, is not a
/// file a player opens. A transport packet size is chosen on the whole
/// [`CONTAINER_SIGNATURE_BYTES`] and never for a prefix that fits both, where that
/// probe takes TS188 at its third sync byte.
pub const SELECTION: signature::Policy = signature::Policy {
    opening_boxes: &[b"ftyp", b"styp", b"moov", b"free", b"wide", b"mdat"],
    ogg: false,
    transport_tie: TransportTie::Refuse,
};

/// The container format a prefix starts with under [`SELECTION`]; `None` when it
/// matches none or is too short to say. Fails with `Invalid` for a prefix that fits
/// both transport packet sizes.
pub fn detect_container(prefix: &[u8]) -> Result<Option<Format>> {
    Ok(match signature::select(prefix, &SELECTION) {
        Selection::Container(Container::Matroska) => Some(Format::Matroska),
        Selection::Container(Container::Isobmff) => Some(Format::Mp4),
        Selection::Container(Container::Transport(Ts188)) => {
            Some(Format::Ts(triergon_mpegts::PacketFormat::Ts188))
        }
        Selection::Container(Container::Transport(M2ts192)) => {
            Some(Format::Ts(triergon_mpegts::PacketFormat::M2ts192))
        }
        Selection::Ambiguous => return Err(Error::Invalid("ambiguous transport signature")),
        Selection::Container(Container::Ogg) | Selection::Need(_) | Selection::Unrecognised => None,
    })
}

#[derive(Debug)]
struct CachedWindow {
    offset: u64,
    bytes: Bytes,
}
/// The source of a selected input: replays the windows the probe read, then
/// continues with the caller's source at the next offset, so a sequential
/// transport is neither reopened nor rewound.
#[derive(Debug)]
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
    ) -> Result<SourceReply> {
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
        self.inner.poll(r, w, c)
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

#[cfg(test)]
mod tests {
    use super::{Format, detect_container};

    #[test]
    fn playback_opening_boxes_and_transport_window() {
        for kind in [b"ftyp", b"styp", b"moov", b"mdat", b"free", b"wide"] {
            let bytes = [b"\0\0\0\x10".as_slice(), kind].concat();
            assert_eq!(detect_container(&bytes), Ok(Some(Format::Mp4)));
        }
        for kind in [b"moof", b"skip", b"sidx"] {
            let bytes = [b"\0\0\0\x10".as_slice(), kind].concat();
            assert_eq!(detect_container(&bytes), Ok(None));
        }
        let mut bytes = [0; 389];
        for at in [0, 188, 376] {
            bytes[at] = 0x47;
        }
        assert_eq!(detect_container(&bytes[..388]), Ok(None));
        assert_eq!(
            detect_container(&bytes),
            Ok(Some(Format::Ts(triergon_mpegts::PacketFormat::Ts188)))
        );
    }
}
