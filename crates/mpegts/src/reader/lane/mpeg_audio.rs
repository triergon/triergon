//! MPEG audio frames, including bounded free-format spacing discovery.
use super::aligned_audio;
use crate::reader::MpegAudioFrame;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputWindow, Result,
    SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_framing::mpeg_audio::{MpegAudioHeader, MpegLayer, MpegVersion};

const HEADER: usize = 4;

pub(super) struct Reader {
    aligned: aligned_audio::Reader<HEADER, MpegAudioHeader>,
    free: Option<FreeFormat>,
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
            aligned: aligned_audio::Reader::new(pool, source, caps, maximum, window)?,
            free: None,
        })
    }

    pub(super) fn input_wait_buffer_capacity(&self) -> Result<usize> {
        self.aligned
            .input_wait_buffer_capacity()
            .checked_add(self.free.as_ref().map_or(0, |free| free.buffer.capacity()))
            .ok_or(Error::Overflow)
    }

    pub(super) fn provide(&mut self, window: InputWindow) -> Result<()> {
        self.aligned.provide(window)
    }

    pub(super) fn resume_at(&mut self, offset: u64, window: usize) -> Result<()> {
        if self.free.is_some() {
            return Err(Error::SourceMismatch);
        }
        self.aligned.resume_at(offset, window)
    }

    pub(super) fn finish(&mut self, request: InputRequest) -> Result<()> {
        self.aligned.finish(request)
    }

    #[expect(
        clippy::expect_used,
        reason = "error returns here measured +0.04% on MPEG-1 layer 1 and 2 workloads"
    )]
    pub(super) fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<MpegAudioFrame>> {
        if let Some(free) = &mut self.free {
            if let Some(base) = free.base
                && free.buffer.is_empty()
            {
                let first = free.first;
                return Ok(map_frame(self.aligned.step(work, cancel, |prefix| {
                    let header = matching_header(prefix, first)
                        .ok_or(Error::Invalid("free-format MPEG audio header changed"))?;
                    Ok((header, free_size(header, base)?))
                })?));
            }
        } else {
            match self.aligned.step(work, cancel, read_length)? {
                Step::Ready(frame) => {
                    if frame.header.bitrate_kbps != 0 {
                        return Ok(map_frame(Step::Ready(frame)));
                    }
                    let buffer = BudgetVec::new(self.aligned.input().budget());
                    self.free = Some(FreeFormat {
                        buffer,
                        prefix: Some(frame.bytes),
                        first: frame.header,
                        offset: frame.offset,
                        scan: minimum(&frame.header),
                        candidate: None,
                        base: None,
                        paid: 0,
                    });
                }
                Step::Yield => return Ok(Step::Yield),
                Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                Step::End(location) => return Ok(Step::End(location)),
                Step::Backpressure(never) => match never {},
            }
        }
        let maximum = self.aligned.maximum();
        self.free
            .as_mut()
            .expect("free-format reader initialized")
            .step(self.aligned.input(), maximum, work, cancel)
    }
}

fn map_frame(step: Step<aligned_audio::Frame<MpegAudioHeader>>) -> Step<MpegAudioFrame> {
    match step {
        Step::Ready(frame) => Step::Ready(MpegAudioFrame {
            bytes: frame.bytes,
            offset: frame.offset,
            header: frame.header,
        }),
        Step::Yield => Step::Yield,
        Step::NeedInput(request) => Step::NeedInput(request),
        Step::End(location) => Step::End(location),
        Step::Backpressure(never) => match never {},
    }
}

fn read_length(bytes: &[u8; HEADER]) -> Result<(MpegAudioHeader, usize)> {
    let header = MpegAudioHeader::parse(u32::from_be_bytes(*bytes))
        .ok_or(Error::Invalid("MPEG audio frame header"))?;
    if header.version == MpegVersion::Mpeg25 {
        return Err(Error::Unsupported("MPEG-2.5 audio in MPEG-TS"));
    }
    // In free format the prefix is read first. No frame leaves this lane until
    // two consecutive spacings establish the size.
    Ok((header, header.frame_bytes().unwrap_or(HEADER)))
}

fn minimum(header: &MpegAudioHeader) -> usize {
    HEADER
        + usize::from(header.crc) * 2
        + if header.layer == MpegLayer::III {
            header.side_info_bytes()
        } else {
            0
        }
}

fn padding(header: &MpegAudioHeader) -> usize {
    usize::from(header.padding) * if header.layer == MpegLayer::I { 4 } else { 1 }
}

fn matching_header(bytes: &[u8], first: MpegAudioHeader) -> Option<MpegAudioHeader> {
    let header = MpegAudioHeader::from_bytes(bytes)?;
    (header.bitrate_kbps == 0 && first.same_stream(&header)).then_some(header)
}

fn free_size(header: MpegAudioHeader, base: usize) -> Result<usize> {
    let size = base.checked_add(padding(&header)).ok_or(Error::Overflow)?;
    if size < minimum(&header) {
        return Err(Error::Invalid("free-format MPEG audio frame extent"));
    }
    Ok(size)
}

struct Spacing {
    base: usize,
    third_header: usize,
}

struct FreeFormat {
    buffer: BudgetVec<u8>,
    prefix: Option<Bytes>,
    first: MpegAudioHeader,
    offset: u64,
    scan: usize,
    candidate: Option<Spacing>,
    base: Option<usize>,
    paid: usize,
}

impl FreeFormat {
    fn header(&self, at: usize) -> Option<MpegAudioHeader> {
        matching_header(self.buffer.as_slice().get(at..)?, self.first)
    }

    fn fill(
        &mut self,
        len: usize,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<()>> {
        while self.buffer.len() < len {
            let remaining = len - self.buffer.len();
            match input.read_append(&mut self.buffer, remaining, work, cancel)? {
                Step::Ready(_) => (),
                Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                Step::Yield => return Ok(Step::Yield),
                Step::End(location) => return Ok(Step::End(location)),
                Step::Backpressure(never) => match never {},
            }
        }
        Ok(Step::Ready(()))
    }

    fn spacing(&self, next: MpegAudioHeader, maximum: usize) -> Option<Spacing> {
        let base = self.scan.checked_sub(padding(&self.first))?;
        if base < HEADER || (self.first.layer == MpegLayer::I && !base.is_multiple_of(4)) {
            return None;
        }
        let size = base.checked_add(padding(&next))?;
        if size < minimum(&next) || size > maximum {
            return None;
        }
        Some(Spacing {
            base,
            third_header: self.scan.checked_add(size)?,
        })
    }

    fn discover(
        &mut self,
        input: &mut InputCursor,
        maximum: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<()>> {
        // ISO/IEC 11172-3 and 13818-3 audio headers supply one padding slot
        // per frame. Two matching gaps establish the unpadded length.
        while self.scan <= maximum {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if let Some(Spacing { base, third_header }) = self.candidate {
                let end = third_header.checked_add(HEADER).ok_or(Error::Overflow)?;
                match self.fill(end, input, work, cancel)? {
                    Step::Ready(()) => {
                        if self.header(third_header).is_some_and(|header| {
                            free_size(header, base).is_ok_and(|size| size <= maximum)
                        }) {
                            self.base = Some(base);
                            return Ok(Step::Ready(()));
                        }
                    }
                    Step::End(_) => (),
                    wait => return Ok(wait),
                }
                self.candidate = None;
                self.scan = self.scan.checked_add(1).ok_or(Error::Overflow)?;
                continue;
            }
            let end = self.scan.checked_add(HEADER).ok_or(Error::Overflow)?;
            match self.fill(end, input, work, cancel)? {
                Step::Ready(()) => (),
                Step::End(_) => return Err(Error::Truncated),
                wait => return Ok(wait),
            }
            // Search spans for sync bytes before testing complete headers.
            // Take lookahead only from a window already held by the cursor;
            // an earlier candidate must never wait on a new input window.
            let limit = maximum.checked_add(HEADER).ok_or(Error::Overflow)?;
            let extra = if self.buffer.len() == end {
                input
                    .buffered()
                    .len()
                    .min(limit - end)
                    .min(512)
                    .min(usize::try_from(work.remaining() / 2).unwrap_or(usize::MAX))
            } else {
                0
            };
            if extra != 0 {
                match input.read_append(&mut self.buffer, extra, work, cancel)? {
                    Step::Ready(_) => (),
                    _ => unreachable!("buffered lookahead within the work grant"),
                }
            }
            let last = (self.buffer.len() - HEADER).min(maximum);
            let search =
                (last - self.scan + 1).min(usize::try_from(work.remaining()).unwrap_or(usize::MAX));
            if search == 0 {
                return Ok(Step::Yield);
            }
            let sync = self.buffer.as_slice()[self.scan..self.scan + search]
                .iter()
                .position(|&byte| byte == 0xff);
            work.take(sync.map_or(search, |offset| offset + 1) as u64);
            self.scan = self
                .scan
                .checked_add(sync.unwrap_or(search))
                .ok_or(Error::Overflow)?;
            if sync.is_none() {
                continue;
            }
            self.candidate = self
                .header(self.scan)
                .and_then(|h| self.spacing(h, maximum));
            if self.candidate.is_none() {
                self.scan = self.scan.checked_add(1).ok_or(Error::Overflow)?;
            }
        }
        Err(crate::limit(self.scan, maximum))
    }

    #[expect(
        clippy::expect_used,
        reason = "error returns here measured +0.04% on MPEG-1 layer 1 and 2 workloads"
    )]
    fn step(
        &mut self,
        input: &mut InputCursor,
        maximum: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<MpegAudioFrame>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if let Some(prefix) = &self.prefix {
            self.buffer.try_extend_from_slice(prefix.as_ref())?;
            self.prefix = None;
        }
        if self.base.is_none() {
            match self.discover(input, maximum, work, cancel)? {
                Step::Ready(()) => (),
                Step::Yield => return Ok(Step::Yield),
                Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                Step::Backpressure(never) => match never {},
                Step::End(_) => return Err(Error::Truncated),
            }
        }
        match self.fill(HEADER, input, work, cancel)? {
            Step::Ready(()) => (),
            Step::Yield => return Ok(Step::Yield),
            Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
            Step::End(location) if self.buffer.is_empty() => return Ok(Step::End(location)),
            Step::End(_) => return Err(Error::Truncated),
            Step::Backpressure(never) => match never {},
        }
        let header = self
            .header(0)
            .ok_or(Error::Invalid("free-format MPEG audio header changed"))?;
        let size = free_size(header, self.base.expect("confirmed free-format spacing"))?;
        if size > maximum {
            return Err(crate::limit(size, maximum));
        }
        match self.fill(size, input, work, cancel)? {
            Step::Ready(()) => (),
            Step::Yield => return Ok(Step::Yield),
            Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
            Step::End(_) => return Err(Error::Truncated),
            Step::Backpressure(never) => match never {},
        }
        let buffered = self.buffer.len();
        // Charge both publication and movement of the lookahead. The grant
        // survives yields and allocation retries.
        self.paid += work.take((buffered - self.paid) as u64) as usize;
        if self.paid < buffered {
            return Ok(Step::Yield);
        }
        let offset = self.offset;
        let next = offset.checked_add(size as u64).ok_or(Error::Overflow)?;
        let bytes = Bytes::copy_from_slice(self.buffer.budget(), &self.buffer.as_slice()[..size])?;
        self.buffer.as_mut_slice().copy_within(size.., 0);
        self.buffer.truncate(buffered - size);
        if self.buffer.is_empty() {
            self.buffer = BudgetVec::new(self.buffer.budget());
        }
        self.offset = next;
        self.paid = 0;
        Ok(Step::Ready(MpegAudioFrame {
            bytes,
            offset,
            header,
        }))
    }
}
