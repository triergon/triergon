//! Independently observed, explicitly selected malformed MOV projection.
//! No canonical source table/configuration/clock is repaired. The initial-zero
//! description rule is bounded to a single catalogue entry and an immediate
//! valid second chunk row. Raw declarations and demux read coordinates differ.
use super::*;
use crate::{
    CursorStatus, IndexLimits, PacketLimits, PacketProjector, PacketView, RecoveryTrackHeader,
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy, Debug)]
pub struct DirectRecoveryProfile;
impl DirectRecoveryProfile {
    /// Exact original public reference allocates this AVIO buffer. Demux
    /// controls establish the common forward-EOF interval for ordinary/tiny
    /// inputs; this is not an ISO rule or a universal AVIO algorithm claim.
    pub(crate) const OBSERVED_FORWARD_READ_CAPACITY: u64 = 32768;
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InitialDescriptionDiagnostic {
    pub field_start: u64,
    pub field_end: u64,
    pub raw_description: u32,
    pub declared_first_chunk_samples: u32,
    pub observed_first_chunk_samples: u32,
    pub demux_catalogue_entry: u32,
    pub strict_error: Error,
}
#[derive(Clone, Copy, Debug)]
pub struct DirectContext {
    pub physical_length: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectAssociation {
    /// Raw source declaration corresponds to the demux coordinate; it may
    /// still contain an invalid clock/extent diagnosed independently.
    DeclarationCoordinate,
    /// Only the raw size/time row ordinal corresponds. This never associates
    /// the source frame/configuration/range with the derived demux read range.
    RawSizeAndClockOrdinalOnly,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectRead {
    Available,
    ObservedForwardEof,
    UnreviewedSeekRefusal,
}
#[derive(Clone, Debug)]
pub struct DirectOccurrence {
    pub declaration: Sample,
    pub demux: PacketView,
    pub source_timescale: u32,
    pub association: DirectAssociation,
    pub raw_chunk_description: u32,
    pub range_error: Option<Error>,
    pub read: DirectRead,
}
impl RunRecoveryBuilder {
    pub fn new_direct(
        raw: BoxView,
        budget: Budget,
        limits: Limits,
        _profile: DirectRecoveryProfile,
    ) -> Result<Self> {
        let mut b = Self::new(raw, budget, limits, RunRecoveryProfile)?;
        b.inner.recover_initial_description = true;
        Ok(b)
    }
}
impl RunRecoveryTable {
    pub fn initial_description_diagnostic(&self) -> Option<InitialDescriptionDiagnostic> {
        self.initial_description
    }
    pub fn direct_packet_cursor<'a>(
        &'a self,
        pool: &Budget,
        header: &RecoveryTrackHeader,
        media: &'a [Range<u64>],
        limits: PacketLimits,
        context: DirectContext,
        _profile: DirectRecoveryProfile,
    ) -> Result<DirectPacketCursor<'a>> {
        if !self.raw().bytes().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        let direct = self.initial_description.is_some()
            || (self.storage.sample_count() != 0
                && self.storage.offsets.get(0)? >= context.physical_length);
        if !direct {
            return Ok(self.packet_cursor(pool, header, media, limits)?.into());
        }
        if media.len() != 1
            || media[0].start > media[0].end
            || media[0].end < context.physical_length
            || self.descriptions().len() != 1
        {
            return Err(Error::Unsupported(
                "v2 direct observation requires one terminal media extent/catalogue entry",
            ));
        }
        if limits.max_tracks == 0 || media.len() > limits.max_media_extents {
            return Err(Error::Limit {
                requested: 1,
                maximum: limits.max_tracks.min(limits.max_media_extents),
            });
        }
        count(self.storage.sample_count(), limits.max_source_samples)?;
        if self.storage.ctts.as_ref().is_some_and(|t| t.count > 2) {
            return Err(Error::Unsupported(
                "v2 direct composition shift with more than two rows",
            ));
        }
        let base = header.demux_projection_base()?;
        Ok(DirectPacketCursor {
            legacy: None,
            direct: Some(DirectCursor {
                table: self,
                source: self.storage.cursor(),
                state: DirectState::default(),
                view: PacketProjector::new(base, self.storage.sync_entry_count()),
                source_timescale: header.source().timescale,
                media: &media[0],
                context,
                limits,
            }),
            ready: None,
            failed: None,
        })
    }
    pub fn direct_index_cursor<'a>(
        &'a self,
        pool: &Budget,
        header: &RecoveryTrackHeader,
        media: &'a [Range<u64>],
        limits: IndexLimits,
        context: DirectContext,
        profile: DirectRecoveryProfile,
    ) -> Result<DirectIndexCursor<'a>> {
        let direct = self.initial_description.is_some()
            || (self.storage.sample_count() != 0
                && self.storage.offsets.get(0)? >= context.physical_length);
        if !direct {
            return Ok(self.index_cursor(pool, header, media, limits)?.into());
        }
        Ok(DirectIndexCursor {
            engine: IndexEngine::Direct(self.direct_packet_cursor(
                pool,
                header,
                media,
                limits.packets,
                context,
                profile,
            )?),
            next: 0,
            distance: 0,
            limit: limits.max_entries,
            ready: None,
            failed: None,
        })
    }
}
fn count(n: u64, max: u64) -> Result<()> {
    if n > max {
        return Err(Error::Limit {
            requested: usize::try_from(n).map_err(|_| Error::Overflow)?,
            maximum: usize::try_from(max).map_err(|_| Error::Overflow)?,
        });
    }
    Ok(())
}
#[derive(Debug, Clone, Copy, Default)]
struct DirectState {
    ordinal: u64,
    chunk: u32,
    left: u32,
    row: u32,
    byte: u64,
}
#[derive(Debug)]
struct DirectCursor<'a> {
    table: &'a RunRecoveryTable,
    source: SampleCursor<'a>,
    state: DirectState,
    view: PacketProjector,
    source_timescale: u32,
    media: &'a Range<u64>,
    context: DirectContext,
    limits: PacketLimits,
}
impl DirectCursor<'_> {
    fn poll(&mut self, work: &mut WorkBudget) -> Result<Option<DirectOccurrence>> {
        let t = &self.table.storage;
        if self.state.ordinal == t.sample_count()
            || (self.state.left == 0 && self.state.chunk == t.offsets.table.count)
        {
            return Ok(None);
        }
        let mut source = self.source;
        let declaration = match source.poll(work)? {
            SampleStep::Yield => return Err(Error::Invalid("internal v2 yield sentinel")),
            SampleStep::End => return Ok(None),
            SampleStep::Sample(s) => s,
        };
        let mut next = self.state;
        if next.left == 0 {
            let number = next.chunk.checked_add(1).ok_or(Error::Overflow)?;
            if next.row + 1 < t.stsc.count && t.stsc.word(next.row + 1, 0)? == number {
                next.row += 1;
            }
            next.left = if next.row == 0 {
                self.table
                    .initial_description
                    .map_or(t.stsc.word(next.row, 4)?, |d| {
                        d.observed_first_chunk_samples
                    })
            } else {
                t.stsc.word(next.row, 4)?
            };
            next.byte = t.offsets.get(next.chunk)?;
            next.chunk = number;
        }
        count(
            next.ordinal.checked_add(1).ok_or(Error::Overflow)?,
            self.limits.max_occurrences,
        )?;
        let raw_description = t.stsc.word(next.row, 8)?;
        let mut projected = declaration.clone();
        projected.range = next.byte
            ..next
                .byte
                .checked_add(u64::from(t.sizes.get(next.ordinal)?))
                .ok_or(Error::Overflow)?;
        projected.description_index = raw_description;
        let demux = self.view.project_regular(&projected, 0)?;
        let range_error = projected.require_within(self.media.clone()).err();
        let read = if !projected.range.is_empty()
            && projected.range.start >= self.context.physical_length
        {
            let ceiling = self
                .media
                .start
                .checked_add(DirectRecoveryProfile::OBSERVED_FORWARD_READ_CAPACITY)
                .ok_or(Error::Overflow)?;
            if projected.range.start <= ceiling {
                DirectRead::ObservedForwardEof
            } else {
                DirectRead::UnreviewedSeekRefusal
            }
        } else {
            DirectRead::Available
        };
        let association = if self.table.initial_description.is_some() {
            DirectAssociation::RawSizeAndClockOrdinalOnly
        } else {
            DirectAssociation::DeclarationCoordinate
        };
        let mut declaration = declaration;
        if self.table.sync_diagnostic.is_some() {
            declaration.access = Access::Unknown;
        }
        let row = DirectOccurrence {
            declaration,
            demux,
            source_timescale: self.source_timescale,
            association,
            raw_chunk_description: raw_description,
            range_error,
            read,
        };
        next.byte = projected.range.end;
        next.left = next
            .left
            .checked_sub(1)
            .ok_or(Error::Invalid("v2 zero projected chunk count"))?;
        next.ordinal += 1;
        self.source = source;
        self.state = next;
        Ok(Some(row))
    }
}
#[derive(Debug)]
pub struct DirectPacketCursor<'a> {
    // Exactly one engine is initialized by these sealed constructors. Keep
    // state inline rather than introduce a profile-switch heap allocation.
    legacy: Option<RecoveryPacketCursor<'a>>,
    direct: Option<DirectCursor<'a>>,
    ready: Option<DirectOccurrence>,
    failed: Option<Error>,
}
impl<'a> From<RecoveryPacketCursor<'a>> for DirectPacketCursor<'a> {
    fn from(inner: RecoveryPacketCursor<'a>) -> Self {
        Self {
            legacy: Some(inner),
            direct: None,
            ready: None,
            failed: None,
        }
    }
}
impl DirectPacketCursor<'_> {
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        let r = self.poll_inner(work, cancel);
        if let Err(e) = r {
            self.failed = Some(e);
            self.ready = None;
        }
        r
    }
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.ready.is_some() {
            return Ok(CursorStatus::Ready);
        }
        if let Some(c) = &mut self.legacy {
            return match c.poll(work, cancel)? {
                CursorStatus::Ready => {
                    let r = c.take_occurrence()?;
                    self.ready = Some(DirectOccurrence {
                        raw_chunk_description: r.demux.source_description,
                        declaration: r.source,
                        demux: r.demux,
                        source_timescale: r.source_timescale,
                        association: DirectAssociation::DeclarationCoordinate,
                        range_error: None,
                        read: DirectRead::Available,
                    });
                    Ok(CursorStatus::Ready)
                }
                other => Ok(other),
            };
        }
        let c = self
            .direct
            .as_mut()
            .ok_or(Error::Invalid("missing v2 cursor engine"))?;

        if c.state.ordinal == c.table.storage.sample_count()
            || (c.state.left == 0 && c.state.chunk == c.table.storage.offsets.table.count)
        {
            return Ok(CursorStatus::End);
        }
        if work.remaining() == 0 {
            return Ok(CursorStatus::Yield);
        }
        match c.poll(work)? {
            Some(r) => {
                self.ready = Some(r);
                Ok(CursorStatus::Ready)
            }
            None => Ok(CursorStatus::End),
        }
    }
    pub fn take_occurrence(&mut self) -> Result<DirectOccurrence> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        self.ready
            .take()
            .ok_or(Error::Invalid("no ready v2 observation"))
    }
}
#[derive(Clone, Debug)]
pub struct DirectIndexEntry {
    pub ordinal: u64,
    pub position: i64,
    pub timestamp: i64,
    pub flags: i32,
    pub size: u32,
    pub min_distance: i32,
    pub declaration: Sample,
    pub association: DirectAssociation,
    pub range_error: Option<Error>,
}
#[derive(Debug)]
enum IndexEngine<'a> {
    Legacy(RecoveryIndexCursor<'a>),
    Direct(DirectPacketCursor<'a>),
}
#[derive(Debug)]
pub struct DirectIndexCursor<'a> {
    engine: IndexEngine<'a>,
    next: u64,
    distance: u64,
    limit: u64,
    ready: Option<DirectIndexEntry>,
    failed: Option<Error>,
}
impl<'a> From<RecoveryIndexCursor<'a>> for DirectIndexCursor<'a> {
    fn from(c: RecoveryIndexCursor<'a>) -> Self {
        Self {
            engine: IndexEngine::Legacy(c),
            next: 0,
            distance: 0,
            limit: u64::MAX,
            ready: None,
            failed: None,
        }
    }
}
impl DirectIndexCursor<'_> {
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        let r = self.poll_inner(work, cancel);
        if let Err(e) = r {
            self.failed = Some(e);
            self.ready = None;
        }
        r
    }
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.ready.is_some() {
            return Ok(CursorStatus::Ready);
        }
        match &mut self.engine {
            IndexEngine::Legacy(c) => match c.poll(work, cancel)? {
                CursorStatus::Ready => {
                    let e = c.take_entry()?;
                    self.ready = Some(DirectIndexEntry {
                        ordinal: e.ordinal,
                        position: e.position,
                        timestamp: e.timestamp,
                        flags: e.flags,
                        size: e.size,
                        min_distance: e.min_distance,
                        declaration: e.source,
                        association: DirectAssociation::DeclarationCoordinate,
                        range_error: None,
                    });
                    Ok(CursorStatus::Ready)
                }
                other => Ok(other),
            },
            IndexEngine::Direct(c) => match c.poll(work, cancel)? {
                CursorStatus::Ready => {
                    if !work.try_charge(1) {
                        return Ok(CursorStatus::Yield);
                    }
                    count(self.next.checked_add(1).ok_or(Error::Overflow)?, self.limit)?;
                    let r = c.take_occurrence()?;
                    let size = r
                        .demux
                        .source_range
                        .end
                        .checked_sub(r.demux.source_range.start)
                        .ok_or(Error::Overflow)?;
                    count(size, 0x1fff_ffff)?;
                    if r.demux.flags & !1 != 0 {
                        return Err(Error::Unsupported("v2 direct native index flags"));
                    }
                    let key = r.demux.flags & 1 != 0;
                    let distance = if key || self.next == 0 {
                        0
                    } else {
                        self.distance.checked_add(1).ok_or(Error::Overflow)?
                    };
                    self.ready = Some(DirectIndexEntry {
                        ordinal: self.next,
                        position: i64::try_from(r.demux.source_range.start)
                            .map_err(|_| Error::Unsupported("native index signed position"))?,
                        timestamp: r.demux.dts_field.signed_ticks(),
                        flags: i32::from(key),
                        size: u32::try_from(size).map_err(|_| Error::Overflow)?,
                        min_distance: i32::try_from(distance).map_err(|_| Error::Overflow)?,
                        declaration: r.declaration,
                        association: r.association,
                        range_error: r.range_error,
                    });
                    self.distance = distance;
                    self.next += 1;
                    Ok(CursorStatus::Ready)
                }
                other => Ok(other),
            },
        }
    }
    pub fn take_entry(&mut self) -> Result<DirectIndexEntry> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        self.ready.take().ok_or(Error::Invalid("no ready v2 index"))
    }
}
