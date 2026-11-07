//! Charged payload storage with complete logical carrier ranges.

use super::{DemuxSpan, SourceContext};
use crate::SourceRange;
use crate::carrier::{PLAIN_PAYLOAD_BYTES, TS_PAYLOAD_START, TS188_BYTES};
use triergon_core::{Budget, BudgetVec, BudgetVecIntoIter, BytesView};

/// One logical payload span. The byte range borrows its packet or cursor;
/// independent retention uses `bytes.into_bytes()`.
#[derive(Clone, Copy, Debug)]
pub struct DemuxSpanView<'a> {
    pub bytes: BytesView<'a>,
    pub source: SourceRange,
    pub transport: SourceRange,
    pub context: SourceContext,
}

impl AsRef<[u8]> for DemuxSpanView<'_> {
    fn as_ref(&self) -> &[u8] {
        self.bytes.as_ref()
    }
}

fn record_count(span: &DemuxSpan, grouped: bool) -> usize {
    if grouped
        && span.bytes.len() >= 2 * TS188_BYTES
        && span.bytes.len() == span.transport.bytes
        && span.bytes.len().is_multiple_of(TS188_BYTES)
    {
        span.bytes.len() / TS188_BYTES
    } else {
        1
    }
}

#[inline]
fn byte_view(span: &DemuxSpan, index: usize, run: bool) -> BytesView<'_> {
    let range = if run {
        let at = index * TS188_BYTES;
        at + TS_PAYLOAD_START..at + TS188_BYTES
    } else {
        0..span.bytes.len()
    };
    span.bytes.view(range).expect(if run {
        "admitted complete carrier"
    } else {
        "complete stored span"
    })
}

fn view(span: &DemuxSpan, index: usize, grouped: bool) -> DemuxSpanView<'_> {
    let run = record_count(span, grouped) != 1;
    let bytes = byte_view(span, index, run);
    if !run {
        return DemuxSpanView {
            bytes,
            source: span.source,
            transport: span.transport,
            context: span.context,
        };
    }
    let at = index * TS188_BYTES;
    DemuxSpanView {
        bytes,
        source: SourceRange {
            offset: span.source.offset + at as u64,
            bytes: PLAIN_PAYLOAD_BYTES,
            ..span.source
        },
        transport: SourceRange {
            offset: span.transport.offset + at as u64,
            bytes: TS188_BYTES,
            ..span.transport
        },
        context: SourceContext {
            source_event_ordinal: span.context.source_event_ordinal + 2 * index as u64,
            ..span.context
        },
    }
}

/// The same logical carrier spans with one backing handle per stored run.
/// Capacity remains charged for every logical `DemuxSpan`, including slots
/// represented by a run, so grouping does not change admission headroom.
#[derive(Debug)]
pub struct CompactPayload {
    records: BudgetVec<DemuxSpan>,
    logical: usize,
    grouped: bool,
}

impl CompactPayload {
    #[cfg(test)]
    pub(crate) fn retained_handles(&self) -> usize {
        self.records.len()
    }
    pub(super) fn new(records: BudgetVec<DemuxSpan>, logical: usize, grouped: bool) -> Self {
        Self {
            records,
            logical,
            grouped,
        }
    }
    /// Adopt ordinary independently owned spans without copying or cloning.
    pub fn from_spans(records: BudgetVec<DemuxSpan>) -> Self {
        let logical = records.len();
        Self::new(records, logical, false)
    }
    /// Number of logical spans, including those represented by a stored run.
    pub fn len(&self) -> usize {
        self.logical
    }
    /// Whether the payload contains no logical spans.
    pub fn is_empty(&self) -> bool {
        self.logical == 0
    }
    /// Charged span slots, including slots represented by a run.
    pub fn capacity(&self) -> usize {
        self.records.capacity()
    }
    /// Allocation pool for the span arena. Individual backing pools remain
    /// available through each view for ordered validation.
    pub fn budget(&self) -> &Budget {
        self.records.budget()
    }
    /// Visit every logical span in source order without adding shared handles.
    pub fn spans(&self) -> DemuxSpanViews<'_> {
        DemuxSpanViews {
            records: self.records.as_slice().iter(),
            current: None,
            first_index: 0,
            remaining: self.logical,
            grouped: self.grouped,
        }
    }
    /// Move storage into a cursor that retires individual logical ranges.
    pub fn into_cursor(self) -> PayloadCursor {
        PayloadCursor {
            records: self.records.into_iter(),
            current: None,
            index: 0,
            current_count: 0,
            active: false,
            remaining: self.logical,
            tail_retired: 0,
            head_retired: 0,
            grouped: self.grouped,
        }
    }
    pub(super) fn into_owned_spans(self) -> triergon_core::Result<BudgetVec<DemuxSpan>> {
        if self.grouped || self.records.len() != self.logical {
            return Err(triergon_core::Error::Invalid(
                "PES payload polling representation",
            ));
        }
        Ok(self.records)
    }
}

/// Borrowed traversal of every logical source/transport/context record.
#[derive(Debug)]
pub struct DemuxSpanViews<'a> {
    records: std::slice::Iter<'a, DemuxSpan>,
    current: Option<(&'a DemuxSpan, usize)>,
    first_index: usize,
    remaining: usize,
    grouped: bool,
}
impl<'a> Iterator for DemuxSpanViews<'a> {
    type Item = DemuxSpanView<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        loop {
            if let Some((span, index)) = &mut self.current
                && *index < record_count(span, self.grouped)
            {
                let output = view(span, *index, self.grouped);
                *index += 1;
                self.remaining -= 1;
                return Some(output);
            }
            self.current = Some((self.records.next()?, std::mem::take(&mut self.first_index)));
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}
impl ExactSizeIterator for DemuxSpanViews<'_> {}

/// A consuming payload cursor. Retiring the last range of a run releases its
/// backing handle at that range's ordinary retirement boundary.
#[derive(Debug)]
pub struct PayloadCursor {
    records: BudgetVecIntoIter<DemuxSpan>,
    current: Option<DemuxSpan>,
    index: usize,
    current_count: usize,
    active: bool,
    remaining: usize,
    tail_retired: usize,
    head_retired: usize,
    grouped: bool,
}
impl PayloadCursor {
    /// Unadmitted ranges, excluding the active range and successful retirements.
    pub fn remaining(&self) -> usize {
        self.remaining
    }
    /// Borrow the active range until it is explicitly retired.
    pub fn current(&self) -> Option<DemuxSpanView<'_>> {
        self.active.then(|| {
            view(
                self.current.as_ref().expect("active payload range"),
                self.index,
                self.grouped,
            )
        })
    }
    /// Borrow the active byte range without constructing its source facts.
    /// The range and backing are identical to `current().map(|span| span.bytes)`.
    #[inline]
    pub fn current_bytes(&self) -> Option<BytesView<'_>> {
        self.active.then(|| {
            let span = self.current.as_ref().expect("active payload range");
            byte_view(span, self.index, self.current_count != 1)
        })
    }
    /// Admit the next logical range. The caller validates its pool and full
    /// extent before consuming bytes, as with an ordinary span iterator.
    pub fn next_span(&mut self) -> Option<DemuxSpanView<'_>> {
        self.admit_next()?;
        self.current()
    }
    /// Admit the next logical range with the same state transitions as
    /// `next_span`, returning only its borrowed bytes. Its full facts remain
    /// available through `current` until `retire_current`.
    #[inline]
    pub fn next_bytes(&mut self) -> Option<BytesView<'_>> {
        self.admit_next()?;
        self.current_bytes()
    }

    fn admit_next(&mut self) -> Option<()> {
        if self.active {
            return Some(());
        }
        if self.remaining == 0 {
            return None;
        }
        self.index += std::mem::take(&mut self.tail_retired);
        if self
            .current
            .as_ref()
            .is_some_and(|_| self.index == self.current_count)
        {
            self.current = None;
            self.current_count = 0;
        }
        if self.current.is_none() {
            self.current = self.records.next();
            self.current_count = self
                .current
                .as_ref()
                .map_or(0, |span| record_count(span, self.grouped));
            self.index = std::mem::take(&mut self.head_retired);
        }
        self.current.as_ref()?;
        self.remaining -= 1;
        self.active = true;
        Some(())
    }
    /// Release the active logical range. Other ranges keep its backing alive.
    /// Calling this without an active range has no effect.
    pub fn retire_current(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        self.index += 1 + std::mem::take(&mut self.tail_retired);
        if self
            .current
            .as_ref()
            .is_some_and(|_| self.index == self.current_count)
        {
            self.current = None;
            self.index = 0;
            self.current_count = 0;
        }
    }
    /// Borrow only ranges not yet admitted or successfully retired.
    pub fn remaining_spans(&self) -> DemuxSpanViews<'_> {
        DemuxSpanViews {
            records: self.records.as_slice().iter(),
            current: self.current.as_ref().and_then(|span| {
                let index = self.index + usize::from(self.active) + self.tail_retired;
                (index < self.current_count).then_some((span, index))
            }),
            first_index: self.head_retired,
            remaining: self.remaining,
            grouped: self.grouped,
        }
    }
    /// Retire pending ranges while keeping the active range alive until its
    /// caller commits a replacement. Successfully retired ranges are not visited
    /// again. A callback error leaves its range pending for a later retry.
    pub fn drain_remaining(
        &mut self,
        mut retire: impl FnMut() -> triergon_core::Result<()>,
    ) -> triergon_core::Result<()> {
        self.drain_inner(&mut retire)
    }

    fn drain_inner(
        &mut self,
        retire: &mut impl FnMut() -> triergon_core::Result<()>,
    ) -> triergon_core::Result<()> {
        if self.remaining == 0 {
            return Ok(());
        }
        if self.current.is_some() {
            let start = self.index + usize::from(self.active) + self.tail_retired;
            for _ in start..self.current_count {
                retire()?;
                self.remaining -= 1;
                self.tail_retired += 1;
            }
        }
        if !self.active {
            self.current = None;
            self.index = 0;
            self.current_count = 0;
            self.tail_retired = 0;
        }
        while let Some(span) = self.records.as_slice().first() {
            for _ in self.head_retired..record_count(span, self.grouped) {
                retire()?;
                self.remaining -= 1;
                self.head_retired += 1;
            }
            drop(self.records.next());
            self.head_retired = 0;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SourceEpochs, SourceRange};
    use triergon_core::{Bytes, SourceId};

    fn grouped_cursor(pool: &Budget, counts: &[usize]) -> PayloadCursor {
        let mut records = BudgetVec::new(pool);
        let total = counts.iter().sum();
        records.try_reserve_exact(total).unwrap();
        let mut at = 0;
        for &count in counts {
            records
                .try_push(DemuxSpan {
                    bytes: Bytes::copy_from_slice(pool, &vec![0; count * TS188_BYTES]).unwrap(),
                    source: SourceRange {
                        entity: SourceId(1),
                        offset: (at * TS188_BYTES + 4) as u64,
                        bytes: count * PLAIN_PAYLOAD_BYTES,
                    },
                    transport: SourceRange {
                        entity: SourceId(1),
                        offset: (at * TS188_BYTES) as u64,
                        bytes: count * TS188_BYTES,
                    },
                    context: SourceContext {
                        source_event_ordinal: 2 * at as u64,
                        epochs: SourceEpochs {
                            configuration: 0,
                            clock: 0,
                        },
                        byte_run_epoch: None,
                    },
                })
                .unwrap();
            at += count;
        }
        CompactPayload::new(records, total, true).into_cursor()
    }

    #[test]
    fn repeated_drain_keeps_the_active_range_and_never_repeats_callbacks() {
        let pool = Budget::new(8192).unwrap();
        let mut cursor = grouped_cursor(&pool, &[3]);
        let active = cursor.next_span().unwrap();
        let storage = active.bytes.backing_bytes().retained_storage_bytes();
        let charged = pool.used();
        let mut calls = 0;
        cursor
            .drain_remaining(|| {
                calls += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(calls, 2);
        cursor
            .drain_remaining(|| panic!("completed drain must not revisit ranges"))
            .unwrap();
        assert_eq!(cursor.remaining(), 0);
        let mut remaining = cursor.remaining_spans();
        assert_eq!(remaining.len(), 0);
        assert_eq!(remaining.size_hint(), (0, Some(0)));
        assert!(remaining.next().is_none());
        assert!(remaining.next().is_none());
        assert_eq!(remaining.count(), 0);
        assert_eq!(cursor.current().unwrap().context.source_event_ordinal, 0);
        assert_eq!(pool.used(), charged);
        cursor.retire_current();
        assert_eq!(cursor.remaining_spans().len(), 0);
        assert_eq!(cursor.remaining_spans().count(), 0);
        assert!(cursor.remaining_spans().next().is_none());
        assert!(cursor.next_span().is_none());
        assert_eq!(pool.used(), charged - storage);
        cursor
            .drain_remaining(|| panic!("empty cursor must not retire twice"))
            .unwrap();
        drop(cursor);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }

    #[test]
    fn partial_drain_retries_only_pending_ranges_or_resumes_ordinary_traversal() {
        for active in [false, true] {
            let pending = 5 - usize::from(active);
            for fail_after in 0..pending {
                for retry_drain in [false, true] {
                    let pool = Budget::new(8192).unwrap();
                    let mut cursor = grouped_cursor(&pool, &[3, 2]);
                    if active {
                        cursor.next_span().unwrap();
                    }
                    let mut retired = 0;
                    assert_eq!(
                        cursor.drain_remaining(|| {
                            if retired == fail_after {
                                return Err(triergon_core::Error::Cancelled);
                            }
                            retired += 1;
                            Ok(())
                        }),
                        Err(triergon_core::Error::Cancelled)
                    );
                    assert_eq!(cursor.remaining(), pending - retired);
                    let expected: Vec<_> = (usize::from(active) + retired..5)
                        .map(|index| 2 * index as u64)
                        .collect();
                    assert_eq!(
                        cursor
                            .remaining_spans()
                            .map(|span| span.context.source_event_ordinal)
                            .collect::<Vec<_>>(),
                        expected
                    );
                    assert_eq!(cursor.current().is_some(), active);
                    if retry_drain {
                        cursor
                            .drain_remaining(|| {
                                retired += 1;
                                Ok(())
                            })
                            .unwrap();
                        assert_eq!(retired, pending);
                        cursor
                            .drain_remaining(|| panic!("successful retirement must not repeat"))
                            .unwrap();
                        assert!(cursor.remaining_spans().next().is_none());
                        cursor.retire_current();
                    } else {
                        cursor.retire_current();
                        let mut actual = Vec::new();
                        while let Some(span) = cursor.next_span() {
                            actual.push(span.context.source_event_ordinal);
                            cursor.retire_current();
                        }
                        assert_eq!(actual, expected);
                    }
                    assert!(cursor.next_span().is_none());
                    drop(cursor);
                    assert_eq!(pool.used(), Budget::control_storage_bytes());
                }
            }
        }
    }

    #[test]
    fn ordinary_public_spans_cannot_select_group_decoding() {
        let pool = Budget::new(8192).unwrap();
        let bytes = Bytes::copy_from_slice(&pool, &[0x55; 2 * TS188_BYTES]).unwrap();
        let source = SourceRange {
            entity: SourceId(1),
            offset: 0,
            bytes: bytes.len(),
        };
        let mut records = BudgetVec::new(&pool);
        records
            .try_push(DemuxSpan {
                bytes,
                source,
                transport: source,
                context: SourceContext {
                    source_event_ordinal: 3,
                    epochs: SourceEpochs {
                        configuration: 0,
                        clock: 0,
                    },
                    byte_run_epoch: None,
                },
            })
            .unwrap();
        let payload = CompactPayload::from_spans(records);
        assert_eq!(payload.len(), 1);
        let views: Vec<_> = payload.spans().collect();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].as_ref(), &[0x55; 2 * TS188_BYTES]);
        assert_eq!(views[0].source, source);
        let mut cursor = payload.into_cursor();
        assert_eq!(cursor.next_span().unwrap().source, source);
        assert_eq!(cursor.current().unwrap().bytes.len(), 2 * TS188_BYTES);
        assert_eq!(cursor.current_bytes().unwrap().len(), 2 * TS188_BYTES);
        assert_eq!(cursor.next_bytes().unwrap().len(), 2 * TS188_BYTES);
        cursor.retire_current();
        assert!(cursor.next_span().is_none());
    }

    #[test]
    fn byte_projection_preserves_full_facts_and_mixed_admission_boundaries() {
        let pool = Budget::new(8192).unwrap();
        let mut cursor = grouped_cursor(&pool, &[3, 2]);
        assert!(cursor.current_bytes().is_none());
        for index in 0..5 {
            let (data, backing) = {
                let bytes = if index % 2 == 0 {
                    cursor.next_bytes().unwrap()
                } else {
                    cursor.next_span().unwrap().bytes
                };
                assert_eq!(bytes.as_ref(), &[0; PLAIN_PAYLOAD_BYTES]);
                assert!(!bytes.backing_bytes().has_shared_storage());
                (
                    bytes.as_ref().as_ptr(),
                    std::ptr::from_ref(bytes.backing_bytes()),
                )
            };
            let span = cursor.current().unwrap();
            assert_eq!(span.as_ref(), &[0; PLAIN_PAYLOAD_BYTES]);
            assert_eq!(data, span.bytes.as_ref().as_ptr());
            assert_eq!(backing, std::ptr::from_ref(span.bytes.backing_bytes()));
            assert_eq!(span.source.offset, (index * TS188_BYTES + 4) as u64);
            assert_eq!(span.source.bytes, PLAIN_PAYLOAD_BYTES);
            assert_eq!(span.transport.offset, (index * TS188_BYTES) as u64);
            assert_eq!(span.transport.bytes, TS188_BYTES);
            assert_eq!(span.context.source_event_ordinal, 2 * index as u64);
            assert_eq!(cursor.current_bytes().unwrap().as_ref(), span.as_ref());
            assert_eq!(cursor.remaining(), 4 - index);
            assert_eq!(cursor.next_bytes().unwrap().len(), PLAIN_PAYLOAD_BYTES);
            assert_eq!(cursor.remaining(), 4 - index);
            cursor.retire_current();
            assert!(cursor.current_bytes().is_none());
        }
        assert!(cursor.next_bytes().is_none());
        assert!(cursor.next_span().is_none());
        drop(cursor);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }

    #[test]
    fn grouped_ranges_keep_every_carrier_fact_and_backing_charge() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<CompactPayload>();
        send_sync::<PayloadCursor>();
        send_sync::<DemuxSpanView<'static>>();
        let pool = Budget::new(8192).unwrap();
        let mut wire = vec![0; 3 * TS188_BYTES];
        for (index, carrier) in wire.chunks_exact_mut(TS188_BYTES).enumerate() {
            carrier.fill(index as u8 + 7);
            carrier[..4].copy_from_slice(&[0x47, 1, 0, 0x10 | index as u8]);
        }
        let bytes = Bytes::copy_from_slice(&pool, &wire).unwrap();
        let source_charge = bytes.retained_storage_bytes();
        let source = SourceId(4);
        let context = SourceContext {
            source_event_ordinal: 8,
            epochs: SourceEpochs {
                configuration: 2,
                clock: 3,
            },
            byte_run_epoch: None,
        };
        let mut records = BudgetVec::new(&pool);
        records.try_reserve_exact(4).unwrap();
        records
            .try_push(DemuxSpan {
                bytes,
                source: SourceRange {
                    entity: source,
                    offset: 104,
                    bytes: 3 * PLAIN_PAYLOAD_BYTES,
                },
                transport: SourceRange {
                    entity: source,
                    offset: 100,
                    bytes: wire.len(),
                },
                context,
            })
            .unwrap();
        let payload = CompactPayload::new(records, 3, true);
        assert_eq!(payload.len(), 3);
        assert_eq!(payload.capacity(), 4);
        for (index, span) in payload.spans().enumerate() {
            assert_eq!(
                span.as_ref(),
                &wire[index * TS188_BYTES + 4..(index + 1) * TS188_BYTES]
            );
            assert_eq!(
                span.source,
                SourceRange {
                    entity: source,
                    offset: 104 + (index * TS188_BYTES) as u64,
                    bytes: PLAIN_PAYLOAD_BYTES
                }
            );
            assert_eq!(
                span.transport,
                SourceRange {
                    entity: source,
                    offset: 100 + (index * TS188_BYTES) as u64,
                    bytes: TS188_BYTES
                }
            );
            assert_eq!(
                span.context,
                SourceContext {
                    source_event_ordinal: 8 + 2 * index as u64,
                    ..context
                }
            );
            assert!(!span.bytes.backing_bytes().has_shared_storage());
        }
        let mut cursor = payload.into_cursor();
        let charged = pool.used();
        for index in 0..3 {
            let span = cursor.next_span().unwrap();
            assert_eq!(span.context.source_event_ordinal, 8 + 2 * index);
            assert_eq!(cursor.remaining_spans().len(), (2 - index) as usize);
            assert_eq!(pool.used(), charged);
            cursor.retire_current();
            assert_eq!(
                pool.used(),
                charged - if index == 2 { source_charge } else { 0 }
            );
        }
        assert!(cursor.next_span().is_none());
        drop(cursor);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}
