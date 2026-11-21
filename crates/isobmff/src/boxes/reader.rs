//! [`BoxReader`], which walks the root boxes of a source through the caller's
//! [`InputCursor`]: it announces each header, then reads the box into memory or skips it,
//! as the caller chooses.
use crate::boxes::{BoxHeader, BoxView, HeaderRead};
use std::io::{Read, Seek};
use std::sync::atomic::AtomicBool;
use triergon_core::{
    BudgetVec, Error, InputCursor, InputToken, Result, SourceId, Step, WorkBudget, blocking::Feed,
};

/// What a [`BoxReader`] poll reports.
#[derive(Debug)]
pub enum BoxEvent {
    /// A root box header was read. Call [`BoxReader::read_metadata`] or
    /// [`BoxReader::skip`] before polling again.
    Header(BoxHeader),
    /// The box [`BoxReader::read_metadata`] asked for, complete.
    Metadata(BoxView),
    /// The announced box was skipped. For a size-zero box that ran to the end of the
    /// source, the header now states its size.
    Skipped(BoxHeader),
}

/// How a [`BoxReader`] treats an `mdat` that declares more bytes than the source holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MediaDataPolicy {
    /// `Error::Truncated`, as for any box past the known end of the source.
    #[default]
    Strict,
    /// Report a terminal media box's declared extent even if it crosses the
    /// known end of the source. The caller must bound reads to the available
    /// bytes and stop scanning at the physical end.
    PreserveTerminalDeclaration,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Header,
    Announced,
    Metadata { target: usize },
    Skipping,
    Skipped,
}

/// A resumable walk over the root boxes of one source, from the input cursor's position
/// when it was built. It is bound to that cursor's input transaction, so a poll after
/// anything else moved the cursor is `Error::Invalid`: build a new reader after a reset or
/// a source continuation. A failure other than backpressure is kept for later polls.
#[derive(Debug)]
pub struct BoxReader {
    source: SourceId,
    token: InputToken,
    position: u64,
    start: u64,
    prefix: [u8; 16],
    filled: usize,
    needed: usize,
    header: Option<BoxHeader>,
    phase: Phase,
    metadata_limit: usize,
    metadata: Option<BudgetVec<u8>>,
    failed: Option<Error>,
    media_data_policy: MediaDataPolicy,
}
impl BoxReader {
    /// A [`MediaDataPolicy::Strict`] reader that reads boxes of at most `metadata_limit`
    /// bytes into memory; `Error::Invalid` for a limit below 8.
    pub fn new(input: &InputCursor, metadata_limit: usize) -> Result<Self> {
        Self::with_media_data_policy(input, metadata_limit, MediaDataPolicy::Strict)
    }

    /// [`new`](Self::new) with a chosen [`MediaDataPolicy`].
    pub fn with_media_data_policy(
        input: &InputCursor,
        metadata_limit: usize,
        media_data_policy: MediaDataPolicy,
    ) -> Result<Self> {
        if metadata_limit < 8 {
            return Err(Error::Invalid("metadata policy smaller than box header"));
        }
        Ok(Self {
            source: input.source(),
            token: input.token(),
            position: input.position(),
            start: input.position(),
            prefix: [0; 16],
            filled: 0,
            needed: 8,
            header: None,
            phase: Phase::Header,
            metadata_limit,
            metadata: None,
            failed: None,
            media_data_policy,
        })
    }
    /// A [`MediaDataPolicy::PreserveTerminalDeclaration`] reader for the recovery view.
    /// Metadata boxes must still be complete.
    pub(crate) fn new_recovery(
        input: &InputCursor,
        metadata_limit: usize,
        _profile: crate::table::recovery::RunRecoveryProfile,
    ) -> Result<Self> {
        Self::with_media_data_policy(
            input,
            metadata_limit,
            MediaDataPolicy::PreserveTerminalDeclaration,
        )
    }
    /// The source the reader is bound to.
    pub fn source(&self) -> SourceId {
        self.source
    }
    /// Whether the box just announced is an `mdat` whose stored size is zero, even when
    /// its extent was worked out from the known source length.
    pub fn announced_open_mdat(&self) -> bool {
        self.phase == Phase::Announced && self.prefix[..8] == *b"\0\0\0\0mdat"
    }
    fn binding(&self, input: &InputCursor) -> Result<()> {
        // The token changes with every request, reset and source continuation
        // of its cursor, so an unchanged token also means an unchanged source.
        if self.token != input.token() || self.position != input.position() {
            return Err(Error::Invalid(
                "input changed outside box reader; reset format syntax state",
            ));
        }
        Ok(())
    }
    fn next_header(&mut self, input: &InputCursor) {
        self.start = input.position();
        self.position = input.position();
        self.filled = 0;
        self.needed = 8;
        self.header = None;
        self.phase = Phase::Header;
    }
    /// Reads the announced box into memory; later polls read it and report
    /// [`BoxEvent::Metadata`]. `Error::Unsupported` for an `mdat` or a box of unknown size,
    /// `Error::Limit` above the reader's metadata limit.
    pub fn read_metadata(&mut self, input: &InputCursor) -> Result<()> {
        self.binding(input)?;
        if self.phase != Phase::Announced {
            return Err(Error::Invalid("no announced box"));
        }
        let header = self.header.ok_or(Error::Invalid("missing box header"))?;
        if header.kind == *b"mdat" {
            return Err(Error::Unsupported("mdat is not retained metadata"));
        }
        let length = usize::try_from(
            header
                .size
                .ok_or(Error::Unsupported("metadata final bound unknown"))?,
        )
        .map_err(|_| Error::Overflow)?;
        if length > self.metadata_limit {
            return Err(Error::Limit {
                requested: length,
                maximum: self.metadata_limit,
            });
        }
        let mut buffer = BudgetVec::new(input.budget());
        buffer.try_reserve_exact(length)?;
        buffer.try_extend_from_slice(&self.prefix[..self.filled])?;
        self.metadata = Some(buffer);
        self.phase = Phase::Metadata { target: length };
        Ok(())
    }
    /// Skips the announced box; the next poll reports [`BoxEvent::Skipped`]. With random
    /// access the cursor requests the box's end at once, continuing from the current window
    /// where the box ends inside it; otherwise the polls read through the box.
    pub fn skip(&mut self, input: &mut InputCursor) -> Result<()> {
        self.binding(input)?;
        if self.phase != Phase::Announced {
            return Err(Error::Invalid("no announced box"));
        }
        let header = self.header.ok_or(Error::Invalid("missing header"))?;
        if let Some(end) = header.end()? {
            if input.position() == end {
                self.phase = Phase::Skipped;
                return Ok(());
            }
            if input.capabilities().random_access {
                // A box that ends inside the current window continues from it.
                input.request_range_reusing_window(end)?;
                self.token = input.token();
                self.position = input.position();
                self.phase = Phase::Skipped;
                return Ok(());
            }
        }
        self.phase = Phase::Skipping;
        Ok(())
    }
    /// Polls, serving the input it asks for from `feed`, until the next box event;
    /// `None` at the end of the source. Each poll gets `work` units. A window needs one
    /// poll, so with one-byte windows this loop runs once per byte; it is kept out of
    /// line so that its state stays in registers.
    #[inline(never)]
    pub fn next_event<S: Read + Seek>(
        &mut self,
        input: &mut InputCursor,
        feed: &mut Feed<S>,
        work: u64,
        cancel: &AtomicBool,
    ) -> Result<Option<BoxEvent>> {
        // Supplying a response changes neither the cursor's token nor its
        // position, and a failed poll returns at once, so the checks that
        // open every poll hold for each one after the first.
        self.check(input)?;
        loop {
            let step = self.poll_inner(input, &mut WorkBudget::new(work), cancel);
            match self.record(step)? {
                Step::Ready(event) => return Ok(Some(event)),
                Step::NeedInput(request) => feed.supply(input, request)?,
                Step::Yield => {}
                Step::End(_) => return Ok(None),
            }
        }
    }
    /// One bounded step towards the next [`BoxEvent`], or [`Step::End`] where the source
    /// ends between boxes. `Error::Truncated` where it ends inside a box, `Error::Invalid`
    /// after a header that was neither read nor skipped.
    #[inline(always)]
    pub fn poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<BoxEvent>> {
        self.check(input)?;
        let step = self.poll_inner(input, work, cancel);
        self.record(step)
    }
    /// The input is the one this reader last saw, and no poll has failed.
    #[inline(always)]
    fn check(&self, input: &InputCursor) -> Result<()> {
        self.binding(input)?;
        self.failed.map_or(Ok(()), Err)
    }
    /// Keeps a poll's failure, other than backpressure, for every later poll.
    #[inline(always)]
    fn record(&mut self, step: Result<Step<BoxEvent>>) -> Result<Step<BoxEvent>> {
        if let Err(error) = step
            && !matches!(error, Error::Backpressure { .. })
        {
            self.failed = Some(error);
        }
        step
    }
    #[inline(always)]
    fn poll_inner(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<BoxEvent>> {
        loop {
            match self.phase {
                Phase::Header => {
                    if self.filled < self.needed {
                        match input.read_into(
                            &mut self.prefix[self.filled..self.needed],
                            work,
                            cancel,
                        )? {
                            Step::Ready(count) => {
                                self.filled += count;
                                self.position = input.position();
                                if self.filled < self.needed
                                    && let Some(request) = input.pending_request(cancel)?
                                {
                                    return Ok(Step::NeedInput(request));
                                }
                            }
                            Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                            Step::Yield => return Ok(Step::Yield),
                            Step::End(location) => {
                                if self.filled == 0 {
                                    return Ok(Step::End(location));
                                }
                                return Err(Error::Truncated);
                            }
                        }
                        continue;
                    }
                    // Reads check input status; publishing a complete prefix
                    // can proceed without reading.
                    input.read_into(&mut [], work, cancel)?;
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let declared_mdat = self.media_data_policy
                        == MediaDataPolicy::PreserveTerminalDeclaration
                        && input.capabilities().length.is_some()
                        && self.prefix.get(4..8) == Some(b"mdat".as_slice())
                        && self.prefix[..4] != [0; 4];
                    let bound = if declared_mdat {
                        None
                    } else {
                        input.capabilities().length
                    };
                    match BoxHeader::parse(&self.prefix[..self.filled], self.start, bound, true)? {
                        HeaderRead::Need(count) => {
                            if count > self.prefix.len() {
                                return Err(Error::Unsupported(
                                    "box header beyond acquired subset",
                                ));
                            }
                            self.needed = count;
                        }
                        HeaderRead::Ready(header) => {
                            self.header = Some(header);
                            self.phase = Phase::Announced;
                            return Ok(Step::Ready(BoxEvent::Header(header)));
                        }
                    }
                }
                Phase::Announced => {
                    input.read_into(&mut [], work, cancel)?;
                    return Err(Error::Invalid("choose metadata or skip after header"));
                }
                Phase::Metadata { target } => {
                    let buffer = self
                        .metadata
                        .as_mut()
                        .ok_or(Error::Invalid("missing metadata buffer"))?;
                    let remaining = target - buffer.len();
                    if remaining == 0 {
                        input.read_into(&mut [], work, cancel)?;
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        let buffer = self
                            .metadata
                            .take()
                            .ok_or(Error::Invalid("missing metadata buffer"))?;
                        let bytes = match buffer.freeze() {
                            Ok(bytes) => bytes,
                            Err(rejected) => {
                                self.metadata = Some(rejected.value);
                                return Err(rejected.error);
                            }
                        };
                        let raw = BoxView::new(bytes, self.start)?;
                        self.next_header(input);
                        return Ok(Step::Ready(BoxEvent::Metadata(raw)));
                    }
                    match input.read_append(buffer, remaining, work, cancel)? {
                        Step::Ready(count) => {
                            self.position = input.position();
                            if count < remaining
                                && let Some(request) = input.pending_request(cancel)?
                            {
                                return Ok(Step::NeedInput(request));
                            }
                        }
                        Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(_) => return Err(Error::Truncated),
                    }
                }
                Phase::Skipping => {
                    let mut header = self.header.ok_or(Error::Invalid("missing header"))?;
                    let end = header.end()?;
                    if end == Some(input.position()) {
                        self.phase = Phase::Skipped;
                        continue;
                    }
                    let maximum = match end {
                        Some(end) => usize::try_from(
                            end.checked_sub(input.position())
                                .ok_or(Error::Invalid("skip past box"))?,
                        )
                        .unwrap_or(usize::MAX),
                        None => usize::MAX,
                    };
                    match input.read_span(maximum, work, cancel)? {
                        Step::Ready(_) => {
                            self.position = input.position();
                        }
                        Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(location) => {
                            if end.is_some() {
                                return Err(Error::Truncated);
                            }
                            header.size = Some(
                                location
                                    .offset
                                    .checked_sub(header.start)
                                    .ok_or(Error::Overflow)?,
                            );
                            self.header = Some(header);
                            self.phase = Phase::Skipped;
                        }
                    }
                }
                Phase::Skipped => {
                    input.read_into(&mut [], work, cancel)?;
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let header = self
                        .header
                        .ok_or(Error::Invalid("missing skipped header"))?;
                    self.next_header(input);
                    return Ok(Step::Ready(BoxEvent::Skipped(header)));
                }
            }
        }
    }
}
