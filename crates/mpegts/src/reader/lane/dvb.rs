//! DVB subtitle PES envelopes (EN 300 743, §§6.2 and 7.2).
use super::{Piece, table_storage};
use crate::reader::model::{PacketData, Shared};
use crate::reader::{DvbSubtitleTailPolicy, DvbSubtitleUnit, PayloadSource};
use crate::{Pes, PesSpan, TraceEvent};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

const WRAPPER: [u8; 2] = [0x20, 0x00];
const SEGMENT_SYNC: u8 = 0x0f;
const END_MARKER: u8 = 0xff;

pub(super) struct Reader {
    policy: DvbSubtitleTailPolicy,
    pending: Option<Envelope>,
}

impl Reader {
    pub(super) fn new(policy: DvbSubtitleTailPolicy) -> Self {
        Self {
            policy,
            pending: None,
        }
    }

    pub(super) fn step(
        &mut self,
        pieces: &BudgetVec<Piece>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<DvbSubtitleUnit>> {
        let piece = pieces.as_slice().last().expect("pending DVB subtitle PES");
        let pes = piece.source.pes();
        if self.pending.is_none() {
            if !work.try_charge(1) {
                return Ok(None);
            }
            if pes.header.stream_id != 0xbd
                || pes.header.declared_packet_length == 0
                || !pes.header.data_alignment_indicator
                || pes.header.pts_raw33.is_none()
            {
                return Err(Error::Invalid("DVB subtitle PES header"));
            }
            self.pending = Some(Envelope::new());
        }
        let envelope = self.pending.as_mut().expect("DVB subtitle envelope");
        if !envelope.scan(pes, self.policy, work, cancel)? {
            return Ok(None);
        }
        let Some(bytes) = envelope.materialize(pes, pieces, work, cancel)? else {
            return Ok(None);
        };
        let envelope = self.pending.take().expect("materialized subtitle envelope");
        Ok(Some(DvbSubtitleUnit {
            bytes,
            range: piece.range.start + WRAPPER.len() as u64
                ..piece.range.start + envelope.complete_end as u64,
            pes_end: piece.range.end,
            segment_count: envelope.segments,
            truncated: envelope.truncated,
        }))
    }
}

#[derive(Default)]
struct Cursor {
    span: usize,
    offset: usize,
    position: usize,
}

impl Cursor {
    fn take<'a>(
        &mut self,
        pes: &'a Pes,
        count: usize,
        work: &mut WorkBudget,
    ) -> Result<Option<&'a [u8]>> {
        if work.remaining() == 0 {
            return Ok(None);
        }
        let span = pes.spans().get(self.span).ok_or(Error::Truncated)?.as_ref();
        let available = span.len() - self.offset;
        if available == 0 {
            return Err(Error::Invalid("empty DVB subtitle PES span"));
        }
        let count = work.take(count.min(available).min(4096) as u64) as usize;
        let start = self.offset;
        self.offset += count;
        self.position += count;
        if self.offset == span.len() {
            self.span += 1;
            self.offset = 0;
        }
        Ok(Some(&span[start..start + count]))
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Wrapper,
    Sync,
    Header,
    Body(usize),
    Done,
}

struct Envelope {
    phase: Phase,
    cursor: Cursor,
    header: [u8; 5],
    used: usize,
    segments: usize,
    complete_end: usize,
    truncated: bool,
    storage: Option<Storage>,
    buffer: Option<BudgetVec<u8>>,
}

impl Envelope {
    fn new() -> Self {
        Self {
            phase: Phase::Wrapper,
            cursor: Cursor::default(),
            header: [0; 5],
            used: 0,
            segments: 0,
            complete_end: WRAPPER.len(),
            truncated: false,
            storage: None,
            buffer: None,
        }
    }

    fn scan(
        &mut self,
        pes: &Pes,
        policy: DvbSubtitleTailPolicy,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match self.phase {
                phase @ (Phase::Wrapper | Phase::Header) => {
                    let target = if matches!(phase, Phase::Wrapper) {
                        WRAPPER.len()
                    } else {
                        self.header.len()
                    };
                    let Some(part) = self.cursor.take(pes, target - self.used, work)? else {
                        return Ok(false);
                    };
                    let end = self.used + part.len();
                    self.header[self.used..end].copy_from_slice(part);
                    self.used = end;
                    if end != target {
                        continue;
                    }
                    self.used = 0;
                    if matches!(phase, Phase::Wrapper) {
                        if self.header[..target] != WRAPPER {
                            return Err(Error::Invalid("DVB subtitle data field wrapper"));
                        }
                        self.phase = Phase::Sync;
                        continue;
                    }
                    let length = usize::from(u16::from_be_bytes([self.header[3], self.header[4]]));
                    let end = self
                        .cursor
                        .position
                        .checked_add(length)
                        .ok_or(Error::Overflow)?;
                    if end > pes.payload_len {
                        if self.segments == 0 || policy == DvbSubtitleTailPolicy::RejectTruncated {
                            return Err(Error::Truncated);
                        }
                        self.truncated = true;
                        self.phase = Phase::Done;
                    } else {
                        self.phase = Phase::Body(end);
                    }
                }
                Phase::Sync => {
                    let Some(part) = self.cursor.take(pes, 1, work)? else {
                        return Ok(false);
                    };
                    match part[0] {
                        SEGMENT_SYNC => self.phase = Phase::Header,
                        END_MARKER
                            if self.segments != 0 && self.cursor.position == pes.payload_len =>
                        {
                            self.phase = Phase::Done;
                        }
                        _ => return Err(Error::Invalid("DVB subtitle segment or end marker")),
                    }
                }
                Phase::Body(end) => {
                    if self.cursor.position == end {
                        self.segments += 1;
                        self.complete_end = end;
                        self.phase = Phase::Sync;
                    } else if self
                        .cursor
                        .take(pes, end - self.cursor.position, work)?
                        .is_none()
                    {
                        return Ok(false);
                    }
                }
                Phase::Done => return Ok(true),
            }
        }
    }

    fn materialize(
        &mut self,
        pes: &Pes,
        pieces: &BudgetVec<Piece>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Bytes>> {
        let first = pes.spans().first().ok_or(Error::Truncated)?;
        let contiguous = first.as_ref().len() >= self.complete_end;
        let size = self.complete_end - WRAPPER.len();
        let pool = pieces.budget();
        if self.storage.is_none() {
            let minimum = Budget::control_storage_bytes()
                .checked_add(table_storage(pieces)?)
                .and_then(|n| n.checked_add(table_storage(&pes.header_source_ranges).ok()?))
                .and_then(|n| n.checked_add(pes.spans().len().checked_mul(size_of::<PesSpan>())?))
                .and_then(|n| n.checked_add(pes.header.retained_storage()))
                .and_then(|n| n.checked_add(Shared::<TraceEvent>::allocation_bytes().ok()?))
                .and_then(|n| n.checked_add(Shared::<PacketData>::allocation_bytes().ok()?))
                .and_then(|n| n.checked_add(size_of::<PayloadSource>()))
                .and_then(|n| {
                    if contiguous {
                        Some(n)
                    } else {
                        n.checked_add(size)?
                            .checked_add(Bytes::owner_overhead_bytes())
                    }
                })
                .ok_or(Error::Overflow)?;
            self.storage = Some(Storage {
                minimum,
                span: 0,
                prior: 0,
            });
            self.cursor = Cursor::default();
        }
        let storage = self.storage.as_mut().expect("DVB subtitle storage check");
        if !storage.step(pes, work, cancel)? {
            return Ok(None);
        }
        if storage.minimum > pool.limit() {
            return Err(Error::Limit {
                requested: storage.minimum,
                maximum: pool.limit(),
            });
        }
        if contiguous {
            if !work.try_charge(1) {
                return Ok(None);
            }
            return Ok(Some(
                first
                    .payload_bytes()?
                    .slice(WRAPPER.len()..self.complete_end)?,
            ));
        }
        if self.buffer.is_none() {
            if !work.try_charge(1) {
                return Ok(None);
            }
            let mut bytes = BudgetVec::new(pool);
            bytes.try_reserve_exact(size)?;
            self.buffer = Some(bytes);
        }
        while self.cursor.position < self.complete_end {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let start = self.cursor.position;
            let Some(part) = self.cursor.take(pes, self.complete_end - start, work)? else {
                return Ok(None);
            };
            let skip = WRAPPER.len().saturating_sub(start).min(part.len());
            self.buffer
                .as_mut()
                .expect("DVB subtitle buffer")
                .try_extend_from_slice(&part[skip..])?;
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if !work.try_charge(1) {
            return Ok(None);
        }
        match self.buffer.take().expect("DVB subtitle buffer").freeze() {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) => {
                self.buffer = Some(error.buffer);
                Err(error.error)
            }
        }
    }
}

// Count each allocation pinned by this PES once, regardless of its slice count.
// Other pool reservations can still cause retryable backpressure.
struct Storage {
    minimum: usize,
    span: usize,
    prior: usize,
}

impl Storage {
    fn step(&mut self, pes: &Pes, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        while let Some(span) = pes.spans().get(self.span) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(false);
            }
            let bytes = span.backing_bytes();
            if self.prior == self.span {
                self.minimum = self
                    .minimum
                    .checked_add(bytes.retained_storage_bytes())
                    .ok_or(Error::Overflow)?;
            } else if !pes.spans()[self.prior]
                .backing_bytes()
                .shares_storage_with(bytes)
            {
                self.prior += 1;
                continue;
            }
            self.span += 1;
            self.prior = 0;
        }
        Ok(true)
    }
}
