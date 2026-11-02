//! Top-level box framing over the parent's concrete InputCursor. No duplicate
//! input windows, request tokens, generation, cancellation or allocator framework.
use crate::{BoxHeader, BoxView, HeaderRead};
use std::sync::atomic::AtomicBool;
use triergon_core::{
    BudgetVec, Error, InputCursor, InputToken, Result, SourceId, Step, WorkBudget,
};

#[derive(Debug)]
pub enum BoxEvent {
    Header(BoxHeader),
    Metadata(BoxView),
    Skipped(BoxHeader),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Header,
    Announced,
    Metadata,
    Skipping,
    Skipped,
}

/// Syntax state bound to one concrete input transaction. Internal range skips
/// update the core token but preserve generation. Create a new reader after a
/// logical reset or continue_source; decoder/configuration epochs remain external.
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
}
impl BoxReader {
    pub fn new(input: &InputCursor, metadata_limit: usize) -> Result<Self> {
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
        })
    }
    pub fn source(&self) -> SourceId {
        self.source
    }
    fn binding(&self, input: &InputCursor) -> Result<()> {
        if self.source != input.source()
            || self.token != input.token()
            || self.position != input.position()
        {
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
        self.phase = Phase::Metadata;
        Ok(())
    }
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
                input.request_range(end)?;
                self.token = input.token();
                self.position = input.position();
                self.phase = Phase::Skipped;
                return Ok(());
            }
        }
        self.phase = Phase::Skipping;
        Ok(())
    }
    pub fn poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<BoxEvent>> {
        self.binding(input)?;
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(input, work, cancel);
        if let Err(error) = result {
            if !matches!(error, Error::Backpressure { .. }) {
                self.failed = Some(error);
            }
        }
        result
    }
    fn poll_inner(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<BoxEvent>> {
        // Delegates even already-buffered cancellation/failure checks to core.
        input.read_into(&mut [], work, cancel)?;
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
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    match BoxHeader::parse(
                        &self.prefix[..self.filled],
                        self.start,
                        input.capabilities().length,
                        true,
                    )? {
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
                    return Err(Error::Invalid("choose metadata or skip after header"));
                }
                Phase::Metadata => {
                    let header = self.header.ok_or(Error::Invalid("missing header"))?;
                    let target =
                        usize::try_from(header.size.ok_or(Error::Invalid("metadata bound"))?)
                            .map_err(|_| Error::Overflow)?;
                    let filled = self
                        .metadata
                        .as_ref()
                        .ok_or(Error::Invalid("missing metadata buffer"))?
                        .len();
                    if filled == target {
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
                                self.metadata = Some(rejected.buffer);
                                return Err(rejected.error);
                            }
                        };
                        let raw = BoxView::new(bytes, header.start)?;
                        self.next_header(input);
                        return Ok(Step::Ready(BoxEvent::Metadata(raw)));
                    }
                    let buffer = self
                        .metadata
                        .as_mut()
                        .ok_or(Error::Invalid("missing metadata buffer"))?;
                    match input.read_append(buffer, target - filled, work, cancel)? {
                        Step::Ready(_) => {
                            self.position = input.position();
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
