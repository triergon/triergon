//! Bounded framing for byte-aligned audio with a fixed header width.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputWindow, Result,
    SourceCapabilities, SourceId, Step, WorkBudget,
};

pub(super) struct Frame<Header> {
    pub(super) bytes: Bytes,
    pub(super) offset: u64,
    pub(super) header: Header,
}

pub(super) struct Reader<const HEADER_BYTES: usize, Header> {
    input: InputCursor,
    maximum: usize,
    header: [u8; HEADER_BYTES],
    used: usize,
    frame: Option<(Header, usize)>,
    buffer: BudgetVec<u8>,
}

impl<const HEADER_BYTES: usize, Header: Copy> Reader<HEADER_BYTES, Header> {
    pub(super) fn new(
        pool: &Budget,
        source: SourceId,
        caps: SourceCapabilities,
        maximum: usize,
        window: usize,
    ) -> Result<Self> {
        Ok(Self {
            input: InputCursor::new(pool, source, caps, window)?,
            maximum,
            header: [0; HEADER_BYTES],
            used: 0,
            frame: None,
            buffer: BudgetVec::new(pool),
        })
    }

    pub(super) fn provide(&mut self, window: InputWindow) -> Result<()> {
        self.input.provide_owned(window)
    }

    pub(super) fn input_wait_buffer_capacity(&self) -> usize {
        debug_assert!(self.input.buffered_owner().is_none());
        self.buffer.capacity()
    }

    pub(super) fn finish(&mut self, request: InputRequest) -> Result<()> {
        self.input.finish(request.source, request.token)
    }

    pub(super) fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        parse: impl Fn(&[u8; HEADER_BYTES]) -> Result<(Header, usize)>,
    ) -> Result<Step<Frame<Header>>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let buffered = self.input.buffered();
        if self.used == 0
            && work.remaining() >= HEADER_BYTES as u64
            && let Some(prefix) = buffered.first_chunk::<HEADER_BYTES>()
        {
            let (header, size) = parse(prefix)?;
            self.check_size(size)?;
            if size <= buffered.len() && size as u64 <= work.remaining() {
                let offset = self.input.position();
                let Step::Ready(bytes) = self.input.read_span(size, work, cancel)? else {
                    unreachable!("the complete audio frame is buffered");
                };
                return Ok(Step::Ready(Frame {
                    bytes,
                    offset,
                    header,
                }));
            }
            self.frame = Some((header, size));
        }
        while self.used < HEADER_BYTES {
            match self
                .input
                .read_into(&mut self.header[self.used..], work, cancel)?
            {
                Step::Ready(n) => self.used += n,
                Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                Step::Yield => return Ok(Step::Yield),
                Step::End(at) if self.used == 0 => return Ok(Step::End(at)),
                Step::End(_) => return Err(Error::Truncated),
                Step::Backpressure(never) => match never {},
            }
        }
        let (header, size) = if let Some(frame) = self.frame {
            frame
        } else {
            let frame = parse(&self.header)?;
            self.check_size(frame.1)?;
            self.frame = Some(frame);
            frame
        };
        if self.buffer.is_empty() {
            self.buffer.try_reserve_exact(size)?;
            self.buffer.try_extend_from_slice(&self.header)?;
        }
        while self.buffer.len() < size {
            let remaining = size - self.buffer.len();
            match self
                .input
                .read_append(&mut self.buffer, remaining, work, cancel)?
            {
                Step::Ready(_) => (),
                Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                Step::Yield => return Ok(Step::Yield),
                Step::End(_) => return Err(Error::Truncated),
                Step::Backpressure(never) => match never {},
            }
        }
        let buffer = BudgetVec::new(self.buffer.budget());
        let bytes = match std::mem::replace(&mut self.buffer, buffer).freeze() {
            Ok(bytes) => bytes,
            Err(error) => {
                self.buffer = error.buffer;
                return Err(error.error);
            }
        };
        let offset = self.input.position() - size as u64;
        self.used = 0;
        self.frame = None;
        Ok(Step::Ready(Frame {
            bytes,
            offset,
            header,
        }))
    }

    fn check_size(&self, size: usize) -> Result<()> {
        if size < HEADER_BYTES {
            return Err(Error::Invalid("audio frame smaller than its header"));
        }
        if size > self.maximum {
            return Err(crate::limit(size, self.maximum));
        }
        Ok(())
    }
}
