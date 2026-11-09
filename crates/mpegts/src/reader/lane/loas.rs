//! Byte-aligned AudioSyncStream envelopes (ISO/IEC 14496-3, §1.7.2).
use crate::reader::LoasFrame;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Error, InputCursor, InputRequest, InputWindow, Result, SourceCapabilities,
    SourceId, Step, WorkBudget,
};

const HEADER_BYTES: usize = 3;

pub(super) struct Reader {
    input: InputCursor,
    maximum: usize,
    header: [u8; HEADER_BYTES],
    used: usize,
    frame_bytes: usize,
    buffer: BudgetVec<u8>,
}

impl Reader {
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
            frame_bytes: 0,
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

    fn envelope_bytes(header: &[u8], maximum: usize) -> Result<usize> {
        let prefix = u16::from_be_bytes([header[0], header[1]]);
        if prefix >> 5 != 0x2b7 {
            return Err(Error::Invalid("LOAS syncword"));
        }
        let body = (usize::from(prefix & 31) << 8) | usize::from(header[2]);
        if body == 0 {
            return Err(Error::Invalid("empty LOAS multiplex element"));
        }
        let size = HEADER_BYTES + body;
        if size > maximum {
            return Err(crate::limit(size, maximum));
        }
        Ok(size)
    }

    pub(super) fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<LoasFrame>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let buffered = self.input.buffered();
        if self.used == 0
            && buffered.len() >= HEADER_BYTES
            && work.remaining() >= HEADER_BYTES as u64
        {
            let size = Self::envelope_bytes(buffered, self.maximum)?;
            if size <= buffered.len() && size as u64 <= work.remaining() {
                let offset = self.input.position();
                let Step::Ready(bytes) = self.input.read_span(size, work, cancel)? else {
                    unreachable!("the complete LOAS frame is buffered");
                };
                return Ok(Step::Ready(LoasFrame { bytes, offset }));
            }
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
        if self.frame_bytes == 0 {
            self.frame_bytes = Self::envelope_bytes(&self.header, self.maximum)?;
        }
        if self.buffer.is_empty() {
            self.buffer.try_reserve_exact(self.frame_bytes)?;
            self.buffer.try_extend_from_slice(&self.header)?;
        }
        while self.buffer.len() < self.frame_bytes {
            let remaining = self.frame_bytes - self.buffer.len();
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
        let offset = self.input.position() - self.frame_bytes as u64;
        self.used = 0;
        self.frame_bytes = 0;
        Ok(Step::Ready(LoasFrame { bytes, offset }))
    }
}
