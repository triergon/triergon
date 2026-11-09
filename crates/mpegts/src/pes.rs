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
    Budget, BudgetVec, Bytes, Error, PushError, Result, SourceId, SourceLocation, Time, WorkBudget,
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
    pub(crate) fn table_storage(&self) -> Result<usize> {
        crate::table_storage(&self.spans)?
            .checked_add(crate::table_storage(&self.header_source_ranges)?)
            .ok_or(Error::Overflow)
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
    // A final span can take the backing before this input slot is drained.
    bytes: Option<Bytes>,
    pusi: bool,
    started: bool,
    position: usize,
    entity: SourceId,
    offset: u64,
    chunk: u64,
}

impl Payload {
    fn bytes(&self) -> &Bytes {
        self.bytes.as_ref().expect("PES payload not consumed")
    }

    fn remaining(&self) -> usize {
        self.bytes
            .as_ref()
            .map_or(0, |bytes| bytes.len() - self.position)
    }
}

#[derive(Debug)]
enum SpanGrowth {
    Credit(usize),
    // The allocation was fully paid before the audit began.
    Audit(SpanGrowthAudit),
    // A refused header-range replacement, including the pending backing.
    HeaderBlocked(usize),
}

// A refused span growth pins the source bytes and both table allocations.
// Count whole backing allocations once, even when a window supplies many spans.
#[derive(Debug)]
struct SpanGrowthAudit {
    minimum: usize,
    span: usize,
    prior: usize,
}

impl SpanGrowthAudit {
    fn new(spans: &BudgetVec<PesSpan>, headers: &BudgetVec<SourceRange>) -> Result<Self> {
        let tables = spans
            .capacity()
            .checked_add(spans.len())
            .and_then(|n| n.checked_add(1))
            .and_then(|n| n.checked_mul(size_of::<PesSpan>()))
            .ok_or(Error::Overflow)?;
        let headers = headers
            .capacity()
            .checked_mul(size_of::<SourceRange>())
            .ok_or(Error::Overflow)?;
        let minimum = Budget::control_storage_bytes()
            .checked_add(tables)
            .and_then(|n| n.checked_add(headers))
            .ok_or(Error::Overflow)?;
        Ok(Self {
            minimum,
            span: 0,
            prior: 0,
        })
    }

    fn step(
        &mut self,
        spans: &[PesSpan],
        pending: &Bytes,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        // Treat the pending payload as the final span for deduplication.
        while self.span <= spans.len() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(false);
            }
            let bytes = spans.get(self.span).map_or(pending, PesSpan::backing_bytes);
            if self.prior == self.span {
                self.minimum = self
                    .minimum
                    .checked_add(bytes.retained_storage_bytes())
                    .ok_or(Error::Overflow)?;
            } else if !bytes.shares_storage_with(spans[self.prior].backing_bytes()) {
                self.prior += 1;
                continue;
            }
            self.span += 1;
            self.prior = 0;
        }
        Ok(true)
    }
}

#[derive(Debug)]
pub struct PesAssembler {
    pid: u16,
    video: bool,
    unbounded_adts: bool,
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
    span_growth: SpanGrowth,
    validated_spans: usize,
    validated_chunk: Option<u64>,
}

pub(crate) const UNBOUNDED_NON_VIDEO: Error = Error::Invalid("unbounded non-video PES");

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
            unbounded_adts: false,
            header_ranges: BudgetVec::new(&budget),
            spans: BudgetVec::new(&budget),
            span_growth: SpanGrowth::Credit(0),
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

    pub(crate) fn allow_unbounded_adts(&mut self) {
        self.unbounded_adts = true;
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
            bytes: Some(bytes),
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
    pub(crate) fn pending_header_location(&self) -> Option<SourceLocation> {
        self.header_ranges
            .as_slice()
            .first()
            .map(|range| SourceLocation {
                source: range.entity,
                offset: range.offset,
            })
    }

    fn validating_body(&self) -> bool {
        self.payload.is_none()
            && !self.ended
            && self.fault.is_none()
            && self.active
            && self.header.is_some()
    }

    /// Capacity before a finite PES completes and needs ordinary publication.
    pub(crate) fn body_run_capacity(&self, carrier_payload: usize) -> usize {
        if !self.validating_body() {
            return 0;
        }
        let limit = self.total.map_or(self.limits.pes_bytes, |end| end - 1);
        (limit.saturating_sub(self.seen) / carrier_payload)
            .min(self.limits.source_spans - self.validated_spans)
            .min(usize::try_from(u64::MAX - self.next_chunk).unwrap_or(usize::MAX))
    }

    pub(crate) fn record_body_carriers(&mut self, carriers: usize, payload: usize) {
        self.next_chunk += carriers as u64;
        self.validated_chunk = Some(self.next_chunk - 1);
        self.validated_spans += carriers;
        self.payload_len += payload;
        self.seen += payload;
    }

    /// Validate one ordinary body carrier. A successful result records whether
    /// this finishes the finite PES. Other cases use the incremental parser.
    pub(crate) fn validate_body_carrier(
        &mut self,
        count: usize,
        work: &mut WorkBudget,
    ) -> Result<Option<bool>> {
        if !self.validating_body()
            || self.next_chunk == u64::MAX
            || self.validated_spans == self.limits.source_spans
            || count > self.total.unwrap_or(self.limits.pes_bytes) - self.seen
            || !work.try_charge(count as u64 + 1)
        {
            return Ok(None);
        }
        self.record_body_carriers(1, count);
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
        self.span_growth = SpanGrowth::Credit(0);
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
    fn complete_timed_header(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        let pending = self
            .payload
            .as_ref()
            .ok_or(Error::Invalid("PES payload missing"))?;
        let bytes = &pending.bytes().as_ref()[pending.position..];
        let Some((header, total)) = parse_timed_header(bytes, self.limits.pes_bytes, work) else {
            return Ok(false);
        };
        let id = header.stream_id;
        if !(id == 0xbd || (0xc0..=0xef).contains(&id))
            || (total.is_none() && !(self.video && (0xe0..=0xef).contains(&id)))
        {
            return Ok(false);
        }
        let used = header.as_bytes().len();
        let extent = SourceRange {
            entity: pending.entity,
            offset: pending
                .offset
                .checked_add(pending.position as u64)
                .ok_or(Error::Overflow)?,
            bytes: used,
        };
        let merge = prepare_range(&mut self.header_ranges, extent, self.limits.source_spans)?;
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        commit_range(&mut self.header_ranges, extent, merge)?;
        work.take(used as u64 + 3);
        self.header_used = used;
        self.header_target = used;
        self.seen = used;
        self.total = total;
        self.header = Some(header);
        self.payload
            .as_mut()
            .ok_or(Error::Invalid("PES payload missing"))?
            .position += used;
        Ok(true)
    }

    pub(crate) fn table_storage(&self) -> Result<usize> {
        crate::table_storage(&self.spans)?
            .checked_add(crate::table_storage(&self.header_ranges)?)
            .ok_or(Error::Overflow)
    }

    pub(crate) fn span_backing(&self, index: usize) -> Option<&Bytes> {
        self.spans.as_slice().get(index).map(PesSpan::backing_bytes)
    }

    pub(crate) fn pending_backing(&self) -> Option<&Bytes> {
        self.payload.as_ref()?.bytes.as_ref()
    }

    pub(crate) fn blocked_growth_floor(&self) -> Option<usize> {
        match &self.span_growth {
            SpanGrowth::Audit(audit) if audit.span > self.spans.len() => Some(audit.minimum),
            SpanGrowth::HeaderBlocked(minimum) => Some(*minimum),
            _ => None,
        }
    }

    #[cold]
    fn record_blocked_header_growth(&mut self) -> Result<()> {
        if matches!(self.span_growth, SpanGrowth::HeaderBlocked(_)) {
            return Ok(());
        }
        let replacement = self
            .header_ranges
            .len()
            .checked_add(1)
            .and_then(|len| len.checked_mul(size_of::<SourceRange>()))
            .ok_or(Error::Overflow)?;
        // The header keeps previous physical extents and copies their bytes
        // inline. The pending payload is the only assembler backing at this point.
        debug_assert!(self.spans.is_empty());
        let pending = self.pending_backing().expect("pending PES header");
        let minimum = Budget::control_storage_bytes()
            .checked_add(self.table_storage()?)
            .and_then(|bytes| bytes.checked_add(replacement))
            .and_then(|bytes| bytes.checked_add(pending.retained_storage_bytes()))
            .ok_or(Error::Overflow)?;
        if minimum > self.budget.limit() {
            return Err(limit(minimum, self.budget.limit()));
        }
        self.span_growth = SpanGrowth::HeaderBlocked(minimum);
        Ok(())
    }

    fn reserve_adts_span(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        let spans = self.spans.len();
        if spans < self.spans.capacity() {
            return Ok(true);
        }
        if let SpanGrowth::Credit(credit) = &mut self.span_growth {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            // Pay for the allocation and each span moved before growing the table.
            // The pending payload and table length are stable across retries.
            let cost = spans + 1;
            *credit += work.take((cost - *credit) as u64) as usize;
            if *credit != cost {
                return Ok(false);
            }
        }
        loop {
            if let SpanGrowth::Audit(audit) = &mut self.span_growth {
                let pending = self.payload.as_ref().expect("pending PES span");
                if !audit.step(self.spans.as_slice(), pending.bytes(), work, cancel)? {
                    return Ok(false);
                }
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                if audit.minimum > self.budget.limit() {
                    return Err(limit(audit.minimum, self.budget.limit()));
                }
                if work.remaining() == 0 {
                    return Ok(false);
                }
            } else if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match self.spans.try_reserve_exact(1) {
                Ok(()) => {
                    self.span_growth = SpanGrowth::Credit(0);
                    if cancel.load(Ordering::Relaxed) {
                        return Err(Error::Cancelled);
                    }
                    // The grant can end at the reservation. Commit the new span
                    // only when the caller can also charge its first byte.
                    return Ok(work.remaining() != 0);
                }
                Err(Error::Backpressure { .. })
                    if matches!(self.span_growth, SpanGrowth::Credit(_)) =>
                {
                    self.span_growth =
                        SpanGrowth::Audit(SpanGrowthAudit::new(&self.spans, &self.header_ranges)?);
                }
                Err(error) => return Err(error),
            }
        }
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
                    .is_some_and(|payload| payload.remaining() != 0)
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
                            // H.222.0 §2.4.3.7 reserves this syntax for TS video.
                            let video = self.video && (0xe0..=0xef).contains(&id);
                            let recovered_adts = self.unbounded_adts && (0xc0..=0xdf).contains(&id);
                            if !video && !recovered_adts {
                                return Err(UNBOUNDED_NON_VIDEO);
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
            let Some(pending) = self.payload.as_ref() else {
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
            let available = pending.remaining();
            if available == 0 {
                self.payload = None;
                continue;
            }
            if !self.active {
                let count = work.take(available as u64) as usize;
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
                if !RETAIN && self.header_used == 0 && self.complete_timed_header(work, cancel)? {
                    continue;
                }
                let pending = self
                    .payload
                    .as_ref()
                    .ok_or(Error::Invalid("PES payload missing"))?;
                let grant = work.remaining();
                if grant == 0 {
                    return Ok(AssemblyStep::Yield);
                }
                let retry = matches!(self.span_growth, SpanGrowth::HeaderBlocked(_));
                // On a refused retry, leave the grant for the transport's
                // retained-state audit. Charge the header bytes on success.
                let prepaid = u64::from(!retry);
                work.take(prepaid);
                if self.seen == self.limits.pes_bytes {
                    return Err(limit(self.seen.saturating_add(1), self.limits.pes_bytes));
                }
                let count = (self.header_target - self.header_used)
                    .min(self.limits.pes_bytes - self.seen)
                    .min(available)
                    .min(grant.min(HEADER_MAX as u64) as usize);
                let extent = SourceRange {
                    entity: pending.entity,
                    offset: pending
                        .offset
                        .checked_add(pending.position as u64)
                        .ok_or(Error::Overflow)?,
                    bytes: count,
                };
                let merge = match prepare_range(
                    &mut self.header_ranges,
                    extent,
                    self.limits.source_spans,
                ) {
                    Err(error @ Error::Backpressure { .. }) => {
                        self.record_blocked_header_growth()?;
                        return Err(error);
                    }
                    result => result?,
                };
                if retry {
                    self.span_growth = SpanGrowth::Credit(0);
                }
                work.take(count as u64 - prepaid);
                commit_range(&mut self.header_ranges, extent, merge)?;
                let pending = self
                    .payload
                    .as_mut()
                    .ok_or(Error::Invalid("PES payload missing"))?;
                let bytes = &pending.bytes().as_ref()[pending.position..pending.position + count];
                self.raw_header[self.header_used..self.header_used + count].copy_from_slice(bytes);
                self.header_used += count;
                self.seen += count;
                pending.position += count;
                continue;
            }
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
            let chunk = pending.chunk;
            let (merge, spans) = if RETAIN {
                (
                    self.spans.as_slice().last().is_some_and(|last| {
                        last.chunk == chunk && last.range.end == pending.position
                    }),
                    self.spans.len(),
                )
            } else {
                (self.validated_chunk == Some(chunk), self.validated_spans)
            };
            if !merge {
                if spans == self.limits.source_spans {
                    return Err(limit(spans.saturating_add(1), self.limits.source_spans));
                }
                if RETAIN {
                    let rejected_adts = self.unbounded_adts
                        && self.header.as_ref().is_some_and(|header| {
                            header.declared_packet_length == 0
                                && (0xc0..=0xdf).contains(&header.stream_id)
                        });
                    if rejected_adts {
                        if !self.reserve_adts_span(work, cancel)? {
                            return Ok(AssemblyStep::Yield);
                        }
                    } else {
                        self.spans.try_reserve_exact(1)?;
                    }
                }
            }
            let count = work.take(maximum as u64) as usize;
            if !RETAIN {
                if !merge {
                    self.validated_spans += 1;
                    self.validated_chunk = Some(chunk);
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
                let pending = self.payload.as_mut().expect("pending PES span");
                let physical = pending
                    .offset
                    .checked_add(pending.position as u64)
                    .ok_or(Error::Overflow)?;
                let bytes = if count == available {
                    pending.bytes.take().expect("pending PES span")
                } else {
                    pending.bytes().clone()
                };
                if let Err(rejected) = self.spans.try_push(PesSpan {
                    bytes,
                    range: pending.position..pending.position + count,
                    source: SourceRange {
                        entity: pending.entity,
                        offset: physical,
                        bytes: count,
                    },
                    chunk: pending.chunk,
                }) {
                    if count == available {
                        pending.bytes = Some(rejected.value.bytes);
                    }
                    return Err(rejected.error);
                }
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

pub(crate) fn timestamp(bytes: &[u8], prefix: u8) -> Result<u64> {
    if bytes.len() != 5
        || bytes[0] >> 4 != prefix
        || bytes[0] & 1 == 0
        || bytes[2] & 1 == 0
        || bytes[4] & 1 == 0
    {
        return Err(Error::Invalid("PES timestamp markers"));
    }
    Ok(timestamp_bits(bytes))
}
/// The caller independently checks the five-byte field's applicability.
pub(crate) fn timestamp_bits(bytes: &[u8]) -> u64 {
    (u64::from(bytes[0] & 0x0e) << 29)
        | (u64::from(bytes[1]) << 22)
        | (u64::from(bytes[2] & 0xfe) << 14)
        | (u64::from(bytes[3]) << 7)
        | u64::from(bytes[4] >> 1)
}

/// Parse a complete timestamp-only header without stepping its three stages.
/// Refusal leaves the caller free to use the ordinary parser and diagnostics.
pub(crate) fn parse_timed_header(
    bytes: &[u8],
    maximum: usize,
    work: &WorkBudget,
) -> Option<(PesHeader, Option<usize>)> {
    let prefix = bytes.first_chunk::<9>()?;
    if prefix[..3] != [0, 0, 1]
        || prefix[6] & 0xf0 != 0x80
        || !matches!((prefix[7], prefix[8]), (0x80, 5) | (0xc0, 10))
    {
        return None;
    }
    let used = 9 + usize::from(prefix[8]);
    if used > maximum || work.remaining() < used as u64 + 3 {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([prefix[4], prefix[5]]));
    let declared = (length != 0).then_some(length + 6);
    if declared.is_some_and(|total| total < used || total > maximum) {
        return None;
    }
    let bytes = bytes.get(..used)?;
    let timing = prefix[7] >> 6;
    let pts_raw33 = Some(timestamp(&bytes[9..14], timing).ok()?);
    let dts_raw33 = if timing == 3 {
        Some(timestamp(&bytes[14..19], 1).ok()?)
    } else {
        None
    };
    let mut raw = [0; HEADER_MAX];
    raw[..used].copy_from_slice(bytes);
    Some((
        PesHeader {
            stream_id: prefix[3],
            declared_packet_length: u16::from_be_bytes([prefix[4], prefix[5]]),
            pts_raw33,
            dts_raw33,
            data_alignment_indicator: prefix[6] & 4 != 0,
            priority: prefix[6] & 8 != 0,
            raw,
            used,
        },
        declared,
    ))
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
        if ext & 0x0e != 14 {
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

#[cfg(test)]
#[path = "pes/adts_span_budget.rs"]
mod adts_span_budget;

#[cfg(test)]
mod timed_header_tests {
    use super::*;

    fn stamp(raw: u64, prefix: u8) -> [u8; 5] {
        [
            (prefix << 4) | u8::try_from((raw >> 29) & 0x0e).unwrap() | 1,
            ((raw >> 22) & 255).try_into().unwrap(),
            u8::try_from((raw >> 14) & 0xfe).unwrap() | 1,
            ((raw >> 7) & 255).try_into().unwrap(),
            u8::try_from((raw << 1) & 0xfe).unwrap() | 1,
        ]
    }

    fn header(pts: u64, dts: Option<u64>, control: u8) -> ([u8; HEADER_MAX], usize) {
        let mut raw = [0; HEADER_MAX];
        let (timing, used) = if dts.is_some() { (3, 19) } else { (2, 14) };
        raw[..9].copy_from_slice(&[
            0,
            0,
            1,
            0xe0,
            0,
            0,
            control,
            timing << 6,
            u8::try_from(used - 9).unwrap(),
        ]);
        raw[9..14].copy_from_slice(&stamp(pts, timing));
        if let Some(dts) = dts {
            raw[14..19].copy_from_slice(&stamp(dts, 1));
        }
        (raw, used)
    }

    #[test]
    fn complete_timestamps_match_general_headers_and_keep_admission_bounds() {
        for pts in [0, 1, 0x0001_0000_0011, (1 << 33) - 1] {
            for dts in [None, Some(0), Some((1 << 33) - 1)] {
                for control in 0x80..=0x8f {
                    let (mut raw, used) = header(pts, dts, control);
                    for length in [0, u16::try_from(used - 6).unwrap(), u16::MAX] {
                        raw[4..6].copy_from_slice(&length.to_be_bytes());
                        let work = WorkBudget::new(used as u64 + 3);
                        let (fast, total) =
                            parse_timed_header(&raw[..used], 1 << 20, &work).unwrap();
                        let slow = parse_header(&raw, used).unwrap();
                        assert_eq!(format!("{fast:?}"), format!("{slow:?}"));
                        assert_eq!(fast.pts_raw33, Some(pts));
                        assert_eq!(fast.dts_raw33, dts);
                        assert_eq!(total, (length != 0).then_some(usize::from(length) + 6));
                        assert_eq!(work.spent(), 0);
                        let too_small = if length == 0 {
                            used - 1
                        } else {
                            usize::from(length) + 5
                        };
                        assert!(parse_timed_header(&raw[..used], too_small, &work).is_none());
                        for units in [0, used as u64 + 2] {
                            assert!(
                                parse_timed_header(&raw[..used], 1 << 20, &WorkBudget::new(units))
                                    .is_none()
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn invalid_and_incomplete_timestamps_fall_back_to_the_general_parser() {
        for dts in [None, Some((1 << 33) - 1)] {
            let (raw, used) = header(0x0001_0000_0011, dts, 0x84);
            let work = WorkBudget::new(4096);
            for length in 0..used {
                assert!(parse_timed_header(&raw[..length], 1 << 20, &work).is_none());
            }
            for at in std::iter::once(6).chain(9..used) {
                for bit in 0..8 {
                    let mut changed = raw;
                    changed[at] ^= 1 << bit;
                    let fast = parse_timed_header(&changed[..used], 1 << 20, &work);
                    match parse_header(&changed, used) {
                        Ok(slow) => {
                            assert_eq!(format!("{:?}", fast.unwrap().0), format!("{slow:?}"));
                        }
                        Err(_) => assert!(fast.is_none()),
                    }
                }
            }
            for at in [0, 1, 2, 7, 8] {
                let mut changed = raw;
                changed[at] ^= 1;
                assert!(parse_timed_header(&changed[..used], 1 << 20, &work).is_none());
            }
            let mut short = raw;
            short[4..6].copy_from_slice(&u16::try_from(used - 7).unwrap().to_be_bytes());
            assert!(parse_timed_header(&short[..used], 1 << 20, &work).is_none());
        }
        let (mut raw, used) = header(17, None, 0x84);
        raw[8] += 1;
        raw[used] = 0xff;
        assert!(parse_header(&raw, used + 1).is_ok());
        assert!(parse_timed_header(&raw[..used + 1], 1 << 20, &WorkBudget::new(4096)).is_none());
    }
}
