//! Transport-backed finite and bounded-prefix native TS startup.
//! The finite profile needs a complete window and an actual positive-size EOF
//! reply. The M2TS prefix profile keeps subsequent Source reads open.
use crate::input::{Source, SourceReply};
use crate::{
    Engine, Format, Player, PlayerPolicy, Result, Selection, TsNativeSelection,
    io::{SelectedInput, SourceOwner},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    BudgetVec, Error, InputCursor, InputRequest, InputWindow, SourceFailure, WorkBudget,
};
use triergon_mpegts::{
    PacketFormat, ParseLimits,
    clock::SourceTimePolicy,
    compat::input::{InputBootstrap, InputContext, InputStep},
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupStep {
    Yield,
    NeedEof,
    Ready,
}
pub struct NativeStartup<S: Source> {
    input: Option<InputCursor>,
    source: Option<SourceOwner<S>>,
    format: PacketFormat,
    bootstrap: Option<InputBootstrap>,
    prefix: Option<PrefixRead>,
    eof_request: Option<InputRequest>,
    context: Option<InputContext>,
    failed: bool,
}
pub struct PreparedNative<S: Source> {
    input: InputCursor,
    source: SourceOwner<S>,
    format: PacketFormat,
    context: InputContext,
}
impl<S: Source> NativeStartup<S> {
    pub fn new(selected: SelectedInput<S>) -> Result<Self> {
        let Format::Ts(format) = selected.format else {
            return Err(Error::Invalid("native startup requires TS selection"));
        };
        if selected.input.position() != 0 {
            return Err(Error::Invalid("native startup not at initial cursor"));
        }
        Ok(Self {
            input: Some(selected.input),
            source: Some(selected.source),
            format,
            bootstrap: None,
            prefix: None,
            eof_request: None,
            context: None,
            failed: false,
        })
    }
    /// Read one bounded M2TS probe from a fresh selected Source. The cursor
    /// must admit an 8 KiB input window. Individual replies may be smaller.
    pub fn new_m2ts_prefix_v2(selected: SelectedInput<S>) -> Result<Self> {
        if selected.format != Format::Ts(PacketFormat::M2ts192)
            || !selected.input.buffered().is_empty()
            || selected.input.request().maximum < InputBootstrap::M2TS_PREFIX_BYTES
        {
            return Err(Error::Unsupported(
                "native M2TS prefix needs a fresh 8192-byte window",
            ));
        }
        let bytes = BudgetVec::new(selected.input.budget());
        let mut startup = Self::new(selected)?;
        startup.prefix = Some(PrefixRead {
            bytes,
            pending: None,
        });
        Ok(startup)
    }

    pub fn advance(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<StartupStep> {
        if self.failed {
            return Err(Error::Closed);
        }
        let result = self.inner(w, c);
        let retry = self.prefix.is_some()
            && matches!(&result, Err(Error::Backpressure { .. } | Error::Allocation));
        if result.is_err() && !retry {
            self.failed = true;
            if let Some(source) = self.source.as_mut() {
                source.close();
            }
        }
        result
    }
    fn inner(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<StartupStep> {
        if c.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if w.remaining() == 0 {
            return Ok(StartupStep::Yield);
        }
        if self.context.is_some() {
            return Ok(StartupStep::Ready);
        }
        let input = self.input.as_mut().ok_or(Error::Closed)?;
        let source = self.source.as_mut().ok_or(Error::Closed)?;
        if let Some(prefix) = self.prefix.as_mut() {
            if let Some(bootstrap) = prefix.poll(input, source, w, c)? {
                self.bootstrap = Some(bootstrap);
                self.prefix = None;
            }
            return Ok(StartupStep::Yield);
        }
        if self.bootstrap.is_none() {
            let request = input.request();
            match source.poll(&request, w, c)? {
                SourceReply::Pending => return Ok(StartupStep::Yield),
                SourceReply::Failed {
                    source,
                    token,
                    failure,
                } => {
                    input.fail(source, token, failure)?;
                    return Err(match failure {
                        SourceFailure::Cancelled => Error::Cancelled,
                        SourceFailure::Transport => Error::Transport,
                    });
                }
                SourceReply::End { source, token } => {
                    input.finish(source, token)?;
                    return Err(Error::Unsupported("empty native startup entity"));
                }
                SourceReply::Data(window) => {
                    if window.bytes().is_empty() {
                        return Ok(StartupStep::Yield);
                    }
                    if input
                        .capabilities()
                        .length
                        .is_some_and(|n| n != window.bytes().len() as u64)
                    {
                        return Err(Error::Unsupported(
                            "native finite startup requires one actual complete initial window; short reads stay source-only",
                        ));
                    }
                    input.provide(&window)?;
                    let bootstrap = InputBootstrap::new(input, &window, self.format)?;
                    let mut eof = request;
                    eof.offset = window.bytes().len() as u64;
                    eof.maximum = 1;
                    self.bootstrap = Some(bootstrap);
                    self.eof_request = Some(eof);
                    return Ok(StartupStep::NeedEof);
                }
            }
        }
        if let Some(request) = self.eof_request.as_ref() {
            match source.poll(request, w, c)? {
                SourceReply::Pending => return Ok(StartupStep::NeedEof),
                SourceReply::Failed {
                    source,
                    token,
                    failure,
                } => {
                    input.fail(source, token, failure)?;
                    return Err(match failure {
                        SourceFailure::Cancelled => Error::Cancelled,
                        SourceFailure::Transport => Error::Transport,
                    });
                }
                SourceReply::Data(window) => {
                    if window.source != input.source() {
                        return Err(Error::SourceMismatch);
                    }
                    if window.token != input.token() {
                        return Err(Error::Invalid("stale EOF-probe reply"));
                    }
                    if !window.bytes().belongs_to(input.budget()) {
                        return Err(Error::ForeignPool);
                    }
                    if window.offset != request.offset || window.bytes().len() > request.maximum {
                        return Err(Error::Invalid("EOF-probe range"));
                    }
                    if window.bytes().is_empty() {
                        return Ok(StartupStep::NeedEof);
                    }
                    return Err(Error::Unsupported(
                        "positive-size read found more entity data; finite startup unproven",
                    ));
                }
                SourceReply::End { source, token } => {
                    self.bootstrap
                        .as_mut()
                        .ok_or(Error::Closed)?
                        .observe_entity_eof(input, source, &token, request.offset)?;
                    self.eof_request = None;
                }
            }
            return Ok(StartupStep::Yield);
        }
        match self
            .bootstrap
            .as_mut()
            .ok_or(Error::Closed)?
            .poll(input, w, c)?
        {
            InputStep::NeedEntityEof => Ok(StartupStep::NeedEof),
            InputStep::Yield => Ok(StartupStep::Yield),
            InputStep::Ready(context) => {
                self.context = Some(context);
                Ok(StartupStep::Ready)
            }
        }
    }
    pub fn finish(mut self) -> Result<PreparedNative<S>> {
        if self.failed {
            return Err(Error::Closed);
        }
        Ok(PreparedNative {
            context: self
                .context
                .take()
                .ok_or(Error::Invalid("native startup not ready"))?,
            input: self.input.take().ok_or(Error::Closed)?,
            source: self.source.take().ok_or(Error::Closed)?,
            format: self.format,
        })
    }
}
struct PrefixRead {
    bytes: BudgetVec<u8>,
    pending: Option<InputWindow>,
}

impl PrefixRead {
    fn poll<S: Source>(
        &mut self,
        input: &mut InputCursor,
        source: &mut SourceOwner<S>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<InputBootstrap>> {
        let limit = InputBootstrap::M2TS_PREFIX_BYTES;
        if self.pending.is_none() && self.bytes.len() < limit {
            let mut request = input.request();
            request.offset = self.bytes.len() as u64;
            request.maximum = limit - self.bytes.len();
            match source.poll(&request, work, cancel)? {
                SourceReply::Pending => return Ok(None),
                SourceReply::Failed {
                    source,
                    token,
                    failure,
                } => {
                    input.fail(source, token, failure)?;
                    return Err(match failure {
                        SourceFailure::Cancelled => Error::Cancelled,
                        SourceFailure::Transport => Error::Transport,
                    });
                }
                SourceReply::End { source, token } => {
                    if source != request.source {
                        return Err(Error::SourceMismatch);
                    }
                    if token != request.token {
                        return Err(Error::Invalid("stale M2TS prefix End"));
                    }
                    return Err(if input.capabilities().length.is_some() {
                        Error::Truncated
                    } else {
                        Error::Unsupported("short source for native M2TS prefix")
                    });
                }
                SourceReply::Data(window) => {
                    if window.source != request.source {
                        return Err(Error::SourceMismatch);
                    }
                    if window.token != request.token
                        || window.offset != request.offset
                        || window.bytes().len() > request.maximum
                    {
                        return Err(Error::Invalid("native M2TS prefix reply range"));
                    }
                    if !window.bytes().belongs_to(input.budget()) {
                        return Err(Error::ForeignPool);
                    }
                    if window.bytes().is_empty() {
                        return Ok(None);
                    }
                    if window.bytes().len() == limit {
                        input.provide(&window)?;
                        return Ok(Some(InputBootstrap::new_m2ts_prefix(input, &window)?));
                    }
                    self.pending = Some(window);
                }
            }
        }
        if let Some(window) = self.pending.as_ref() {
            if work.remaining() == 0 {
                return Ok(None);
            }
            // Reserve once, before consuming any part of the pending reply.
            // On pressure the source operation is already complete and its
            // exact owner remains here for retry.
            self.bytes.try_reserve_exact(limit - self.bytes.len())?;
            let start = self.bytes.len() - window.offset as usize;
            let count = work.take((window.bytes().len() - start) as u64) as usize;
            self.bytes
                .try_extend_from_slice(&window.bytes().as_ref()[start..start + count])?;
            if start + count == window.bytes().len() {
                self.pending = None;
            }
        }
        if self.bytes.len() != limit {
            return Ok(None);
        }
        let builder = std::mem::replace(&mut self.bytes, BudgetVec::new(input.budget()));
        let bytes = match builder.freeze() {
            Ok(bytes) => bytes,
            Err(rejected) => {
                self.bytes = rejected.buffer;
                return Err(rejected.error);
            }
        };
        let window = InputWindow::new(input.source(), input.token(), 0, bytes)?;
        input.provide(&window)?;
        Ok(Some(InputBootstrap::new_m2ts_prefix(input, &window)?))
    }
}

impl<S: Source> PreparedNative<S> {
    pub fn context(&self) -> &InputContext {
        &self.context
    }
    pub fn into_player(
        self,
        program: u16,
        limits: ParseLimits,
        policy: SourceTimePolicy,
        native: TsNativeSelection,
        selection: Selection,
        player_policy: PlayerPolicy,
    ) -> Result<Player<S>> {
        let engine = Engine::ts_native_context(
            self.input,
            self.format,
            program,
            limits,
            policy,
            native,
            self.context,
        )?;
        Player::new(engine, self.source, selection, player_policy)
    }
}
