//! One finite metadata range on the reader's EXISTING InputCursor. No owned I/O.
use crate::matroska::typed::syntax::Required;
use crate::{
    Error, Result, ebml,
    incremental::{ElementHeader, ParseStep},
    storage::SharedBytes,
};
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, BudgetVec, Bytes, InputCursor, InputToken, SourceId, Step, WorkBudget,
};

#[derive(Debug)]
pub(crate) struct RangeLeaf {
    pub header: ElementHeader,
    pub data: SharedBytes,
}
#[derive(Debug)]
enum Phase {
    Header,
    Reserve,
    Body,
    Freeze,
    Done,
}
#[derive(Debug)]
pub(crate) struct RangeLeafJob {
    pool: Budget,
    source: SourceId,
    token: InputToken,
    expected: u64,
    parent_end: Option<u64>,
    limit: usize,
    bytes: [u8; 12],
    have: usize,
    target: usize,
    id_width: usize,
    header: ElementHeader,
    phase: Phase,
    size: usize,
    buffer: Option<BudgetVec<u8>>,
    span: Option<Bytes>,
    copied: usize,
    result: Option<SharedBytes>,
    failed: bool,
    header_only: bool,
    read_left: Option<u64>,
}
impl RangeLeafJob {
    pub(crate) fn needs_header_input(&self) -> bool {
        matches!(self.phase, Phase::Header) && self.have < self.target
    }

    pub(crate) fn begin(
        input: &mut InputCursor,
        offset: u64,
        expected: u64,
        parent_end: Option<u64>,
        limit: usize,
    ) -> Result<Self> {
        let request = input.request_range(offset)?;
        Ok(Self {
            pool: input.budget().clone(),
            source: request.source,
            token: request.token,
            expected,
            parent_end,
            limit,
            bytes: [0; 12],
            have: 0,
            target: 1,
            id_width: 0,
            header: ElementHeader {
                id: 0,
                size: None,
                start: offset,
                data: offset,
                generation: input.generation(),
            },
            phase: Phase::Header,
            size: 0,
            buffer: Some(BudgetVec::new(input.budget())),
            span: None,
            copied: 0,
            result: None,
            failed: false,
            header_only: false,
            read_left: None,
        })
    }
    pub(crate) fn begin_header_only(
        input: &mut InputCursor,
        offset: u64,
        expected: u64,
        parent_end: Option<u64>,
    ) -> Result<Self> {
        let mut job = Self::begin(input, offset, expected, parent_end, usize::MAX)?;
        job.header_only = true;
        Ok(job)
    }
    pub(crate) fn set_read_limit(&mut self, remaining: u64) {
        self.read_left = Some(remaining);
    }
    fn identity(&self, input: &InputCursor) -> Result<()> {
        if !self.pool.same_pool(input.budget()) {
            return Err(Error::ForeignPool);
        }
        if self.source != input.source() {
            return Err(Error::SourceMismatch);
        }
        let current = input.token();
        if current.generation() != self.token.generation() {
            return Err(Error::StaleGeneration {
                expected: self.token.generation(),
                received: current.generation(),
            });
        }
        if current.request() != self.token.request() {
            return Err(Error::StaleRequest {
                expected: self.token.request(),
                received: current.request(),
            });
        }
        Ok(())
    }
    pub(crate) fn step(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<RangeLeaf>> {
        self.identity(input)?;
        input.read_into(&mut [], work, cancel)?;
        if self.failed {
            return Err(Error::Invalid("range requires replacement after error"));
        }
        let result = self.advance(input, work, cancel);
        match result {
            Err(Error::Backpressure {
                requested,
                available,
            }) => Ok(ParseStep::Backpressure {
                requested,
                available,
            }),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
            other => other,
        }
    }
    fn advance(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<RangeLeaf>> {
        loop {
            input.read_into(&mut [], work, cancel)?;
            match self.phase {
                Phase::Header => {
                    if self.have < self.target {
                        let maximum = self.read_left.map_or(self.target - self.have, |left| {
                            (self.target - self.have)
                                .min(usize::try_from(left).unwrap_or(usize::MAX))
                        });
                        if maximum == 0 {
                            return Err(Error::Policy("operation read bytes"));
                        }
                        match input.read_into(
                            &mut self.bytes[self.have..self.have + maximum],
                            work,
                            cancel,
                        )? {
                            Step::Ready(count) => {
                                self.have += count;
                                if let Some(left) = &mut self.read_left {
                                    *left =
                                        left.checked_sub(count as u64).ok_or(Error::Overflow)?;
                                }
                            }
                            Step::Yield => return Ok(ParseStep::Yield),
                            Step::NeedInput(mut request) => {
                                request.maximum = request.maximum.min(maximum);
                                return Ok(ParseStep::NeedInput(request));
                            }
                            Step::End(_) => return Err(Error::Truncated),
                        }
                        if self.have < self.target {
                            continue;
                        }
                    }
                    if self.id_width == 0 {
                        if self.bytes[0] == 0 {
                            return Err(Error::Invalid("zero VINT marker"));
                        }
                        self.id_width = self.bytes[0].leading_zeros() as usize + 1;
                        if self.id_width > 4 {
                            return Err(Error::Invalid("element ID width"));
                        }
                        self.target = self.id_width + 1;
                        continue;
                    }
                    if self.target == self.id_width + 1 {
                        let first = self.bytes[self.id_width];
                        if first == 0 {
                            return Err(Error::Invalid("zero VINT marker"));
                        }
                        let target = self.id_width + first.leading_zeros() as usize + 1;
                        if target != self.target {
                            self.target = target;
                            continue;
                        }
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let (id, _, _) = ebml::vint(&self.bytes[..self.id_width], true)?;
                    let (size, _, unknown) =
                        ebml::vint(&self.bytes[self.id_width..self.target], false)?;
                    if id != self.expected {
                        return Err(Error::Invalid("SeekID disagrees with target"));
                    }
                    if self.header_only {
                        if self.parent_end.is_some_and(|end| input.position() > end) {
                            return Err(Error::Invalid("element exceeds parent"));
                        }
                        if !unknown
                            && self.parent_end.is_some_and(|end| {
                                input
                                    .position()
                                    .checked_add(size)
                                    .is_none_or(|value| value > end)
                            })
                        {
                            return Err(Error::Invalid("element exceeds parent"));
                        }
                        self.header.id = id;
                        self.header.size = (!unknown).then_some(size);
                        self.header.data = input.position();
                        self.result = Some(SharedBytes::default());
                        self.phase = Phase::Done;
                        continue;
                    }
                    if unknown {
                        return Err(Error::Invalid("unknown metadata size"));
                    }
                    let end = input.position().checked_add(size).ok_or(Error::Overflow)?;
                    if self.parent_end.is_some_and(|parent| end > parent) {
                        return Err(Error::Invalid("element exceeds parent"));
                    }
                    self.size = usize::try_from(size).map_err(|_| Error::Overflow)?;
                    if self.size > self.limit {
                        return Err(Error::Policy("element bytes"));
                    }
                    self.header.id = id;
                    self.header.size = Some(size);
                    self.header.data = input.position();
                    self.phase = Phase::Reserve;
                }
                Phase::Reserve => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    self.buffer
                        .as_mut()
                        .required("missing range buffer")?
                        .try_reserve_exact(self.size)?;
                    self.phase = Phase::Body;
                }
                Phase::Body => {
                    if let Some(span) = &self.span {
                        let count = work.take((span.len() - self.copied).min(8192) as u64) as usize;
                        if count == 0 {
                            return Ok(ParseStep::Yield);
                        }
                        self.buffer
                            .as_mut()
                            .required("missing range buffer")?
                            .try_extend_from_slice(&span[self.copied..self.copied + count])?;
                        self.copied += count;
                        if self.copied == span.len() {
                            self.span = None;
                            self.copied = 0;
                        }
                        continue;
                    }
                    let have = self.buffer.as_ref().required("missing range buffer")?.len();
                    if have == self.size {
                        self.phase = Phase::Freeze;
                        continue;
                    }
                    let maximum = self.read_left.map_or(self.size - have, |left| {
                        (self.size - have).min(usize::try_from(left).unwrap_or(usize::MAX))
                    });
                    if maximum == 0 {
                        return Err(Error::Policy("operation read bytes"));
                    }
                    match input.read_span(maximum, work, cancel)? {
                        Step::Ready(span) => {
                            if let Some(left) = &mut self.read_left {
                                *left =
                                    left.checked_sub(span.len() as u64).ok_or(Error::Overflow)?;
                            }
                            self.span = Some(span);
                        }
                        Step::Yield => return Ok(ParseStep::Yield),
                        Step::NeedInput(mut request) => {
                            request.maximum = request.maximum.min(maximum);
                            return Ok(ParseStep::NeedInput(request));
                        }
                        Step::End(_) => return Err(Error::Truncated),
                    }
                }
                Phase::Freeze => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let buffer = self.buffer.take().required("missing range buffer")?;
                    match buffer.freeze() {
                        Ok(bytes) => {
                            self.result = Some(SharedBytes::from_core(bytes));
                            self.phase = Phase::Done;
                        }
                        Err(error) => {
                            self.buffer = Some(error.buffer);
                            return Err(error.error);
                        }
                    }
                }
                Phase::Done => {
                    return Ok(ParseStep::Event(RangeLeaf {
                        header: self.header,
                        data: self
                            .result
                            .as_ref()
                            .required("missing range result")?
                            .clone(),
                    }));
                }
            }
        }
    }
}

super::usage::storage!(RangeLeafJob, |s| [s.buffer, s.span, s.result]);
