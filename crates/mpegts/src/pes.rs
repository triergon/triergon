//! H.222.0 2.4.3.6-8: bounded PES assembly with original timestamp/header facts.
//! This module deliberately does not group codec access units or infer DTS.
use crate::{
    AssemblyStep, ParseLimits, SourceEpochs, SourceRange, checked_pid, commit_range, limit,
    prepare_range,
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, PushError, Result, SourceId, Time, WorkBudget,
};

pub(crate) const HEADER_MAX: usize = 264; // 9 fixed bytes plus 8-bit optional-header length

#[derive(Clone, Debug)]
pub struct PesHeader {
    pub stream_id: u8,
    pub declared_packet_length: u16,
    pub pts_raw33: Option<u64>,
    pub dts_raw33: Option<u64>,
    pub data_alignment_indicator: bool,
    pub priority: bool,
    raw: [u8; HEADER_MAX],
    used: usize,
}
impl PesHeader {
    pub fn as_bytes(&self) -> &[u8] {
        &self.raw[..self.used]
    }
    /// Raw period-zero view only. Clock extension/epoch selection remains explicit.
    pub fn raw_pts_time(&self) -> Result<Option<Time>> {
        self.pts_raw33
            .map(|ticks| Time::new(i128::from(ticks), 90_000))
            .transpose()
    }
    pub fn raw_dts_time(&self) -> Result<Option<Time>> {
        self.dts_raw33
            .map(|ticks| Time::new(i128::from(ticks), 90_000))
            .transpose()
    }
}

#[derive(Debug)]
pub struct PesSpan {
    bytes: Bytes,
    range: Range<usize>,
    source: SourceRange,
    chunk: u64,
}
impl AsRef<[u8]> for PesSpan {
    fn as_ref(&self) -> &[u8] {
        &self.bytes.as_ref()[self.range.clone()]
    }
}
impl PesSpan {
    /// An exact charged alias of this elementary payload span.
    pub fn payload_bytes(&self) -> Result<Bytes> {
        self.bytes.slice(self.range.clone())
    }

    pub fn source_range(&self) -> SourceRange {
        self.source
    }
    pub fn backing_bytes(&self) -> &Bytes {
        &self.bytes
    }
}

#[derive(Debug)]
pub struct Pes {
    pub pid: u16,
    pub header: PesHeader,
    pub epochs: SourceEpochs,
    pub elementary_offset: u64,
    /// Identifies an uninterrupted elementary byte run; unrelated to I/O or time.
    pub byte_run_epoch: u64,
    pub payload_len: usize,
    pub header_source_ranges: BudgetVec<SourceRange>,
    spans: BudgetVec<PesSpan>,
}
impl Pes {
    pub fn spans(&self) -> &[PesSpan] {
        self.spans.as_slice()
    }
    pub fn payload(&self) -> impl Iterator<Item = &[u8]> {
        self.spans.as_slice().iter().map(AsRef::as_ref)
    }
    pub fn source_ranges(&self) -> impl Iterator<Item = SourceRange> + '_ {
        self.spans.as_slice().iter().map(|span| span.source)
    }
}

#[derive(Debug)]
struct Payload {
    bytes: Bytes,
    pusi: bool,
    started: bool,
    position: usize,
    entity: SourceId,
    offset: u64,
    chunk: u64,
}

#[derive(Debug)]
pub struct PesAssembler {
    pid: u16,
    video: bool,
    budget: Budget,
    limits: ParseLimits,
    epochs: SourceEpochs,
    payload: Option<Payload>,
    next_chunk: u64,
    active: bool,
    ended: bool,
    fault: Option<Error>,
    raw_header: [u8; HEADER_MAX],
    header_used: usize,
    header_target: usize,
    header: Option<PesHeader>,
    total: Option<usize>,
    seen: usize,
    payload_len: usize,
    elementary_offset: u64,
    byte_run_epoch: u64,
    header_ranges: BudgetVec<SourceRange>,
    spans: BudgetVec<PesSpan>,
    validated_spans: usize,
    validated_chunk: Option<u64>,
}

impl PesAssembler {
    pub fn new(
        pid: u16,
        video: bool,
        budget: Budget,
        limits: ParseLimits,
        epochs: SourceEpochs,
    ) -> Result<Self> {
        checked_pid(pid)?;
        limits.validate()?;
        Ok(Self {
            pid,
            video,
            header_ranges: BudgetVec::new(&budget),
            spans: BudgetVec::new(&budget),
            budget,
            limits,
            epochs,
            payload: None,
            next_chunk: 0,
            active: false,
            ended: false,
            fault: None,
            raw_header: [0; HEADER_MAX],
            header_used: 0,
            header_target: 6,
            header: None,
            total: None,
            seen: 0,
            payload_len: 0,
            elementary_offset: 0,
            byte_run_epoch: 0,
            validated_spans: 0,
            validated_chunk: None,
        })
    }
    /// One TS payload at a time. Drain poll before supplying another. A rejected
    /// input is returned intact; no bytes have been consumed from that input.
    pub fn push(
        &mut self,
        bytes: Bytes,
        pusi: bool,
        entity: SourceId,
        offset: u64,
    ) -> std::result::Result<(), PushError<Bytes>> {
        let failure = if self.payload.is_some() {
            Some(Error::Invalid("PES payload not drained"))
        } else if self.ended {
            Some(Error::Invalid("PES after EOF"))
        } else if self.fault.is_some() {
            self.fault
        } else if !bytes.belongs_to(&self.budget) {
            Some(Error::Invalid("foreign PES byte pool"))
        } else if offset.checked_add(bytes.len() as u64).is_none() || self.next_chunk == u64::MAX {
            Some(Error::Overflow)
        } else {
            None
        };
        if let Some(error) = failure {
            return Err(PushError {
                error,
                value: bytes,
            });
        }
        let chunk = self.next_chunk;
        self.next_chunk += 1;
        self.payload = Some(Payload {
            bytes,
            pusi,
            started: false,
            position: 0,
            entity,
            offset,
            chunk,
        });
        Ok(())
    }
    /// Definitive logical EOF only. An entity/segment end may retain a partial PES.
    pub fn end_of_source(&mut self) {
        self.ended = true;
    }
    pub fn partial_header(&self) -> &[u8] {
        &self.raw_header[..self.header_used]
    }

    /// Validate one ordinary body carrier. A successful result records whether
    /// this finishes the finite PES. Other cases use the incremental parser.
    pub(crate) fn validate_body_carrier(
        &mut self,
        count: usize,
        work: &mut WorkBudget,
    ) -> Result<Option<bool>> {
        if self.payload.is_some()
            || self.ended
            || self.fault.is_some()
            || !self.active
            || self.header.is_none()
            || self.next_chunk == u64::MAX
            || self.validated_spans == self.limits.source_spans
            || count > self.total.unwrap_or(self.limits.pes_bytes) - self.seen
            || !work.try_charge(count as u64 + 1)
        {
            return Ok(None);
        }
        self.validated_chunk = Some(self.next_chunk);
        self.next_chunk += 1;
        self.validated_spans += 1;
        self.payload_len += count;
        self.seen += count;
        let completed = self.total == Some(self.seen);
        if completed {
            self.emit::<false>()?;
        }
        Ok(Some(completed))
    }

    /// Explicit continuity/configuration/clock recovery. Caller retains source
    /// bytes if it intends to replay the current PUSI after this transition.
    pub fn discard(&mut self, epochs: SourceEpochs) {
        let next_run = self.byte_run_epoch.checked_add(1);
        self.payload = None;
        self.active = false;
        self.ended = false;
        self.fault = None;
        if let Some(next) = next_run {
            self.byte_run_epoch = next;
        } else {
            self.fault = Some(Error::Overflow);
        }
        self.header_used = 0;
        self.header_target = 6;
        self.header = None;
        self.total = None;
        self.seen = 0;
        self.payload_len = 0;
        self.elementary_offset = 0;
        self.epochs = epochs;
        self.header_ranges = BudgetVec::new(&self.budget);
        self.spans = BudgetVec::new(&self.budget);
        self.validated_spans = 0;
        self.validated_chunk = None;
    }
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<Pes>> {
        Ok(match self.poll_source::<true>(work, cancel)? {
            AssemblyStep::Output(Some(pes)) => AssemblyStep::Output(pes),
            AssemblyStep::Output(None) => return Err(Error::Invalid("missing source PES")),
            AssemblyStep::Yield => AssemblyStep::Yield,
            AssemblyStep::NeedPayload => AssemblyStep::NeedPayload,
            AssemblyStep::Finished => AssemblyStep::Finished,
        })
    }
    pub(crate) fn poll_source<const RETAIN: bool>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<Option<Pes>>> {
        if let Some(error) = self.fault {
            return Err(error);
        }
        let result = self.poll_inner::<RETAIN>(work, cancel);
        if let Err(error) = result.as_ref()
            && !matches!(
                error,
                Error::Backpressure { .. } | Error::Allocation | Error::Cancelled
            )
        {
            self.fault = Some(*error);
        }
        result
    }
    fn emit<const RETAIN: bool>(&mut self) -> Result<AssemblyStep<Option<Pes>>> {
        let next = self
            .elementary_offset
            .checked_add(self.payload_len as u64)
            .ok_or(Error::Overflow)?;
        let header = self.header.take().ok_or(Error::Truncated)?;
        let result = RETAIN.then(|| Pes {
            pid: self.pid,
            header,
            epochs: self.epochs,
            elementary_offset: self.elementary_offset,
            byte_run_epoch: self.byte_run_epoch,
            payload_len: self.payload_len,
            header_source_ranges: std::mem::replace(
                &mut self.header_ranges,
                BudgetVec::new(&self.budget),
            ),
            spans: std::mem::replace(&mut self.spans, BudgetVec::new(&self.budget)),
        });
        if !RETAIN {
            self.header_ranges.clear();
            self.validated_spans = 0;
            self.validated_chunk = None;
        }
        self.elementary_offset = next;
        self.active = false;
        self.header_used = 0;
        self.header_target = 6;
        self.total = None;
        self.seen = 0;
        self.payload_len = 0;
        Ok(AssemblyStep::Output(result))
    }
    fn poll_inner<const RETAIN: bool>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<Option<Pes>>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.active && self.header.is_some() && self.total == Some(self.seen) {
                if self
                    .payload
                    .as_ref()
                    .is_some_and(|payload| payload.position < payload.bytes.len())
                {
                    return Err(Error::Invalid("bytes beyond finite PES"));
                }
                // A completed finite PES consumes its current payload. Release the
                // input slot before publishing, so the caller can supply the next.
                self.payload = None;
                return self.emit::<RETAIN>();
            }
            if self.active && self.header.is_none() && self.header_used == self.header_target {
                if !work.try_charge(1) {
                    return Ok(AssemblyStep::Yield);
                }
                match self.header_used {
                    6 => {
                        if self.raw_header[..3] != [0, 0, 1] {
                            return Err(Error::Invalid("PES start code"));
                        }
                        let id = self.raw_header[3];
                        if !(id == 0xbd || (0xc0..=0xef).contains(&id)) {
                            return Err(Error::Unsupported("PES stream-id syntax"));
                        }
                        let length = usize::from(u16::from_be_bytes([
                            self.raw_header[4],
                            self.raw_header[5],
                        ]));
                        if length == 0 {
                            if !self.video || !(0xe0..=0xef).contains(&id) {
                                return Err(Error::Invalid("unbounded non-video PES"));
                            }
                        } else {
                            let total = 6 + length;
                            if total > self.limits.pes_bytes {
                                return Err(limit(total, self.limits.pes_bytes));
                            }
                            if total < 9 {
                                return Err(Error::Invalid("PES optional-header length"));
                            }
                            self.total = Some(total);
                        }
                        self.header_target = 9;
                    }
                    9 => {
                        self.header_target = 9 + usize::from(self.raw_header[8]);
                        if self.total.is_some_and(|total| self.header_target > total) {
                            return Err(Error::Invalid("PES header exceeds length"));
                        }
                        if self.header_target > self.limits.pes_bytes {
                            return Err(limit(self.header_target, self.limits.pes_bytes));
                        }
                        if self.header_target == 9 {
                            self.header = Some(parse_header(&self.raw_header, 9)?);
                        }
                    }
                    _ => self.header = Some(parse_header(&self.raw_header, self.header_used)?),
                }
                continue;
            }
            let pending = match self.payload.as_ref() {
                Some(payload) => payload,
                None => {
                    return if self.ended {
                        if !self.active {
                            Ok(AssemblyStep::Finished)
                        } else if self.total.is_none() && self.header.is_some() {
                            self.emit::<RETAIN>()
                        } else {
                            Err(Error::Truncated)
                        }
                    } else {
                        Ok(AssemblyStep::NeedPayload)
                    };
                }
            };
            if !pending.started {
                if pending.pusi {
                    if self.active {
                        if self.total.is_none() && self.header.is_some() {
                            return self.emit::<RETAIN>();
                        }
                        return Err(Error::Truncated);
                    }
                    self.active = true;
                }
                self.payload
                    .as_mut()
                    .ok_or(Error::Invalid("PES payload missing"))?
                    .started = true;
                continue;
            }
            if pending.position == pending.bytes.len() {
                self.payload = None;
                continue;
            }
            if !self.active {
                let remaining = pending.bytes.len() - pending.position;
                let count = work.take(remaining as u64) as usize;
                if count == 0 {
                    return Ok(AssemblyStep::Yield);
                }
                self.payload
                    .as_mut()
                    .ok_or(Error::Invalid("PES payload missing"))?
                    .position += count;
                continue;
            }
            if self.header.is_none() {
                if !work.try_charge(1) {
                    return Ok(AssemblyStep::Yield);
                }
                if self.seen == self.limits.pes_bytes {
                    return Err(limit(self.seen.saturating_add(1), self.limits.pes_bytes));
                }
                let count = (self.header_target - self.header_used)
                    .min(self.limits.pes_bytes - self.seen)
                    .min(pending.bytes.len() - pending.position)
                    .min(work.remaining().min(HEADER_MAX as u64) as usize + 1);
                let bytes = &pending.bytes.as_ref()[pending.position..pending.position + count];
                let extent = SourceRange {
                    entity: pending.entity,
                    offset: pending
                        .offset
                        .checked_add(pending.position as u64)
                        .ok_or(Error::Overflow)?,
                    bytes: count,
                };
                let merge =
                    prepare_range(&mut self.header_ranges, extent, self.limits.source_spans)?;
                commit_range(&mut self.header_ranges, extent, merge)?;
                self.raw_header[self.header_used..self.header_used + count].copy_from_slice(bytes);
                work.take(count as u64 - 1);
                self.header_used += count;
                self.seen += count;
                self.payload
                    .as_mut()
                    .ok_or(Error::Invalid("PES payload missing"))?
                    .position += count;
                continue;
            }
            let available = pending.bytes.len() - pending.position;
            let remaining = self
                .total
                .map_or(self.limits.pes_bytes - self.seen, |total| total - self.seen);
            if remaining == 0 {
                return Err(limit(self.seen.saturating_add(1), self.limits.pes_bytes));
            }
            let maximum = available.min(remaining).min(4096);
            if work.remaining() == 0 {
                return Ok(AssemblyStep::Yield);
            }
            // Validation-only feeds count exactly the same spans. Each chunk
            // is contiguous and consumed monotonically, so no owners are needed.
            let (merge, spans) = if RETAIN {
                (
                    self.spans.as_slice().last().is_some_and(|last| {
                        last.chunk == pending.chunk && last.range.end == pending.position
                    }),
                    self.spans.len(),
                )
            } else {
                (
                    self.validated_chunk == Some(pending.chunk),
                    self.validated_spans,
                )
            };
            if !merge {
                if spans == self.limits.source_spans {
                    return Err(limit(spans.saturating_add(1), self.limits.source_spans));
                }
                if RETAIN {
                    self.spans.try_reserve_exact(1)?;
                }
            }
            let count = work.take(maximum as u64) as usize;
            if !RETAIN {
                if !merge {
                    self.validated_spans += 1;
                    self.validated_chunk = Some(pending.chunk);
                }
            } else if merge {
                let span = self
                    .spans
                    .as_mut_slice()
                    .last_mut()
                    .ok_or(Error::Invalid("missing PES span"))?;
                span.range.end += count;
                span.source.bytes += count;
            } else {
                let physical = pending
                    .offset
                    .checked_add(pending.position as u64)
                    .ok_or(Error::Overflow)?;
                self.spans
                    .try_push(PesSpan {
                        bytes: pending.bytes.clone(),
                        range: pending.position..pending.position + count,
                        source: SourceRange {
                            entity: pending.entity,
                            offset: physical,
                            bytes: count,
                        },
                        chunk: pending.chunk,
                    })
                    .map_err(|error| error.error)?;
            }
            self.payload_len += count;
            self.seen += count;
            self.payload
                .as_mut()
                .ok_or(Error::Invalid("PES payload missing"))?
                .position += count;
        }
    }
}

fn take<'a>(bytes: &'a [u8], position: &mut usize, count: usize) -> Result<&'a [u8]> {
    let end = position.checked_add(count).ok_or(Error::Overflow)?;
    let value = bytes
        .get(*position..end)
        .ok_or(Error::Invalid("PES optional-field length"))?;
    *position = end;
    Ok(value)
}

fn timestamp(bytes: &[u8], prefix: u8) -> Result<u64> {
    if bytes.len() != 5
        || bytes[0] >> 4 != prefix
        || bytes[0] & 1 == 0
        || bytes[2] & 1 == 0
        || bytes[4] & 1 == 0
    {
        return Err(Error::Invalid("PES timestamp markers"));
    }
    Ok((u64::from(bytes[0] & 14) << 29)
        | (u64::from(bytes[1]) << 22)
        | (u64::from(bytes[2] & 254) << 14)
        | (u64::from(bytes[3]) << 7)
        | u64::from(bytes[4] >> 1))
}

pub(crate) fn parse_header(raw: &[u8; HEADER_MAX], used: usize) -> Result<PesHeader> {
    let bytes = &raw[..used];
    if used < 9 || bytes[6] & 0xc0 != 0x80 || used != 9 + usize::from(bytes[8]) {
        return Err(Error::Invalid("PES optional-header prefix"));
    }
    if bytes[6] & 0x30 != 0 {
        return Err(Error::Unsupported("scrambled PES"));
    }
    let flags = bytes[7];
    let timing = flags >> 6;
    if timing == 1 {
        return Err(Error::Invalid("forbidden PES time flags"));
    }
    let mut position = 9;
    let pts_raw33 = if timing >= 2 {
        Some(timestamp(take(bytes, &mut position, 5)?, timing)?)
    } else {
        None
    };
    let dts_raw33 = if timing == 3 {
        Some(timestamp(take(bytes, &mut position, 5)?, 1)?)
    } else {
        None
    };
    if flags & 32 != 0 {
        let field = take(bytes, &mut position, 6)?;
        let extension = (u16::from(field[4] & 3) << 7) | u16::from(field[5] >> 1);
        if field[0] & 4 == 0
            || field[2] & 4 == 0
            || field[4] & 4 == 0
            || field[5] & 1 == 0
            || extension > 299
        {
            return Err(Error::Invalid("ESCR markers/extension"));
        }
    }
    if flags & 16 != 0 {
        let field = take(bytes, &mut position, 3)?;
        if field[0] & 128 == 0 || field[2] & 1 == 0 {
            return Err(Error::Invalid("ES rate markers"));
        }
    }
    if flags & 8 != 0 {
        take(bytes, &mut position, 1)?;
    }
    if flags & 4 != 0 && take(bytes, &mut position, 1)?[0] & 128 == 0 {
        return Err(Error::Invalid("copy-info marker"));
    }
    if flags & 2 != 0 {
        take(bytes, &mut position, 2)?;
    }
    if flags & 1 != 0 {
        let ext = take(bytes, &mut position, 1)?[0];
        if ext & 14 != 14 {
            return Err(Error::Invalid("PES extension reserved bits"));
        }
        if ext & 128 != 0 {
            take(bytes, &mut position, 16)?;
        }
        if ext & 64 != 0 {
            let size = usize::from(take(bytes, &mut position, 1)?[0]);
            take(bytes, &mut position, size)?;
        }
        if ext & 32 != 0 {
            let field = take(bytes, &mut position, 2)?;
            if field[0] & 128 == 0 || field[1] & 128 == 0 {
                return Err(Error::Invalid("PES sequence markers"));
            }
        }
        if ext & 16 != 0 && take(bytes, &mut position, 2)?[0] & 0xc0 != 0x40 {
            return Err(Error::Invalid("P-STD prefix"));
        }
        if ext & 1 != 0 {
            let length = take(bytes, &mut position, 1)?[0];
            if length & 128 == 0 {
                return Err(Error::Invalid("PES extension-2 marker"));
            }
            let field = take(bytes, &mut position, usize::from(length & 127))?;
            if let Some(first) = field.first() {
                if first & 128 != 0 && first & 1 == 0 {
                    if field.len() < 6 {
                        return Err(Error::Invalid("PES TREF length"));
                    }
                    // Reserved prefix is retained; validate the three marker bits.
                    let tref = &field[1..6];
                    if tref[0] & 1 == 0 || tref[2] & 1 == 0 || tref[4] & 1 == 0 {
                        return Err(Error::Invalid("PES TREF markers"));
                    }
                }
            } else {
                return Err(Error::Invalid("empty PES extension-2"));
            }
        }
    }
    if bytes[position..].iter().any(|byte| *byte != 255) {
        return Err(Error::Invalid("PES header stuffing"));
    }
    Ok(PesHeader {
        stream_id: bytes[3],
        declared_packet_length: u16::from_be_bytes([bytes[4], bytes[5]]),
        pts_raw33,
        dts_raw33,
        data_alignment_indicator: bytes[6] & 4 != 0,
        priority: bytes[6] & 8 != 0,
        raw: *raw,
        used,
    })
}
