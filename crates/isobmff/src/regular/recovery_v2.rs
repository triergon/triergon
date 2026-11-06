//! Independently observed, explicitly pinned malformed MOV projection.
//! No canonical source table/configuration/clock is repaired. The initial-zero
//! description rule is bounded to a single catalogue entry and an immediate
//! valid second chunk row. Raw declarations and native read coordinates differ.
use super::*;
use crate::{
    Mov9CursorStatus, Mov9NativeIndexLimits, Mov9PacketLimits, Mov9PacketView,
    Mov9RecoveryTrackHeader, Mov9View,
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy, Debug)]
pub struct PinnedMovD32RegularRecoveryV2;
impl PinnedMovD32RegularRecoveryV2 {
    /// Exact original public reference allocates this AVIO buffer. Native
    /// controls establish the common forward-EOF interval for ordinary/tiny
    /// inputs; this is not an ISO rule or a universal AVIO algorithm claim.
    pub const OBSERVED_FORWARD_READ_CAPACITY: u64 = 32768;
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mov9InitialDescriptionDiagnostic {
    pub field_start: u64,
    pub field_end: u64,
    pub raw_description: u32,
    pub declared_first_chunk_samples: u32,
    pub observed_first_chunk_samples: u32,
    pub native_catalogue_entry: u32,
    pub strict_error: Error,
}
#[derive(Clone, Copy, Debug)]
pub struct Mov9RecoveryV2Context {
    pub physical_length: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mov9RecoveryV2Association {
    /// Raw source declaration corresponds to the native coordinate; it may
    /// still contain an invalid clock/extent diagnosed independently.
    DeclarationCoordinate,
    /// Only the raw size/time row ordinal corresponds. This never associates
    /// the source frame/configuration/range with the derived native read range.
    RawSizeAndClockOrdinalOnly,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mov9RecoveryV2Read {
    Available,
    ObservedForwardEof,
    UnreviewedSeekRefusal,
}
#[derive(Clone, Debug)]
pub struct Mov9RecoveryV2Occurrence {
    pub declaration: Sample,
    pub native: Mov9PacketView,
    pub source_timescale: u32,
    pub association: Mov9RecoveryV2Association,
    pub raw_chunk_description: u32,
    pub range_error: Option<Error>,
    pub read: Mov9RecoveryV2Read,
}
impl Mov9RunRecoveryBuilder {
    pub fn new_v2(
        raw: BoxView,
        budget: Budget,
        limits: Limits,
        _profile: PinnedMovD32RegularRecoveryV2,
    ) -> Result<Self> {
        let mut b = Self::new(raw, budget, limits, PinnedMovD32RegularRecoveryV1)?;
        b.inner.recover_initial_description = true;
        Ok(b)
    }
}
impl Mov9RunRecoveryTable {
    pub fn initial_description_diagnostic(&self) -> Option<Mov9InitialDescriptionDiagnostic> {
        self.initial_description
    }
    pub fn packet_cursor_v2<'a>(
        &'a self,
        pool: &Budget,
        header: &Mov9RecoveryTrackHeader,
        media: &'a [Range<u64>],
        limits: Mov9PacketLimits,
        context: Mov9RecoveryV2Context,
        _profile: PinnedMovD32RegularRecoveryV2,
    ) -> Result<Mov9RecoveryV2PacketCursor<'a>> {
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
        let base = header.native_projection_base()?;
        Ok(Mov9RecoveryV2PacketCursor {
            legacy: None,
            direct: Some(DirectCursor {
                table: self,
                source: self.storage.cursor(),
                state: DirectState::default(),
                view: Mov9View::new(base, self.storage.sync_entry_count()),
                source_timescale: header.source().timescale,
                media: &media[0],
                context,
                limits,
            }),
            ready: None,
            failed: None,
        })
    }
    pub fn index_cursor_v2<'a>(
        &'a self,
        pool: &Budget,
        header: &Mov9RecoveryTrackHeader,
        media: &'a [Range<u64>],
        limits: Mov9NativeIndexLimits,
        context: Mov9RecoveryV2Context,
        profile: PinnedMovD32RegularRecoveryV2,
    ) -> Result<Mov9RecoveryV2IndexCursor<'a>> {
        let direct = self.initial_description.is_some()
            || (self.storage.sample_count() != 0
                && self.storage.offsets.get(0)? >= context.physical_length);
        if !direct {
            return Ok(self.index_cursor(pool, header, media, limits)?.into());
        }
        Ok(Mov9RecoveryV2IndexCursor {
            engine: IndexEngine::Direct(self.packet_cursor_v2(
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
    table: &'a Mov9RunRecoveryTable,
    source: SampleCursor<'a>,
    state: DirectState,
    view: Mov9View,
    source_timescale: u32,
    media: &'a Range<u64>,
    context: Mov9RecoveryV2Context,
    limits: Mov9PacketLimits,
}
impl DirectCursor<'_> {
    fn poll(&mut self, work: &mut WorkBudget) -> Result<Option<Mov9RecoveryV2Occurrence>> {
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
        let native = self.view.project_regular(&projected, 0)?;
        let range_error = projected.require_within(self.media.clone()).err();
        let read = if !projected.range.is_empty()
            && projected.range.start >= self.context.physical_length
        {
            let ceiling = self
                .media
                .start
                .checked_add(PinnedMovD32RegularRecoveryV2::OBSERVED_FORWARD_READ_CAPACITY)
                .ok_or(Error::Overflow)?;
            if projected.range.start <= ceiling {
                Mov9RecoveryV2Read::ObservedForwardEof
            } else {
                Mov9RecoveryV2Read::UnreviewedSeekRefusal
            }
        } else {
            Mov9RecoveryV2Read::Available
        };
        let association = if self.table.initial_description.is_some() {
            Mov9RecoveryV2Association::RawSizeAndClockOrdinalOnly
        } else {
            Mov9RecoveryV2Association::DeclarationCoordinate
        };
        let mut declaration = declaration;
        if self.table.sync_diagnostic.is_some() {
            declaration.access = Access::Unknown;
        }
        let row = Mov9RecoveryV2Occurrence {
            declaration,
            native,
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
pub struct Mov9RecoveryV2PacketCursor<'a> {
    // Exactly one engine is initialized by these sealed constructors. Keep
    // state inline rather than introduce a profile-switch heap allocation.
    legacy: Option<Mov9RecoveryPacketCursor<'a>>,
    direct: Option<DirectCursor<'a>>,
    ready: Option<Mov9RecoveryV2Occurrence>,
    failed: Option<Error>,
}
impl<'a> From<Mov9RecoveryPacketCursor<'a>> for Mov9RecoveryV2PacketCursor<'a> {
    fn from(inner: Mov9RecoveryPacketCursor<'a>) -> Self {
        Self {
            legacy: Some(inner),
            direct: None,
            ready: None,
            failed: None,
        }
    }
}
impl Mov9RecoveryV2PacketCursor<'_> {
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Mov9CursorStatus> {
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
    fn poll_inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Mov9CursorStatus> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.ready.is_some() {
            return Ok(Mov9CursorStatus::Ready);
        }
        if let Some(c) = &mut self.legacy {
            return match c.poll(work, cancel)? {
                Mov9CursorStatus::Ready => {
                    let r = c.take_occurrence()?;
                    self.ready = Some(Mov9RecoveryV2Occurrence {
                        raw_chunk_description: r.native.source_description,
                        declaration: r.source,
                        native: r.native,
                        source_timescale: r.source_timescale,
                        association: Mov9RecoveryV2Association::DeclarationCoordinate,
                        range_error: None,
                        read: Mov9RecoveryV2Read::Available,
                    });
                    Ok(Mov9CursorStatus::Ready)
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
            return Ok(Mov9CursorStatus::End);
        }
        if work.remaining() == 0 {
            return Ok(Mov9CursorStatus::Yield);
        }
        match c.poll(work)? {
            Some(r) => {
                self.ready = Some(r);
                Ok(Mov9CursorStatus::Ready)
            }
            None => Ok(Mov9CursorStatus::End),
        }
    }
    pub fn take_occurrence(&mut self) -> Result<Mov9RecoveryV2Occurrence> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        self.ready
            .take()
            .ok_or(Error::Invalid("no ready v2 observation"))
    }
}
#[derive(Clone, Debug)]
pub struct Mov9RecoveryV2IndexEntry {
    pub ordinal: u64,
    pub position: i64,
    pub timestamp: i64,
    pub flags: i32,
    pub size: u32,
    pub min_distance: i32,
    pub declaration: Sample,
    pub association: Mov9RecoveryV2Association,
    pub range_error: Option<Error>,
}
#[derive(Debug)]
enum IndexEngine<'a> {
    Legacy(Mov9RecoveryIndexCursor<'a>),
    Direct(Mov9RecoveryV2PacketCursor<'a>),
}
#[derive(Debug)]
pub struct Mov9RecoveryV2IndexCursor<'a> {
    engine: IndexEngine<'a>,
    next: u64,
    distance: u64,
    limit: u64,
    ready: Option<Mov9RecoveryV2IndexEntry>,
    failed: Option<Error>,
}
impl<'a> From<Mov9RecoveryIndexCursor<'a>> for Mov9RecoveryV2IndexCursor<'a> {
    fn from(c: Mov9RecoveryIndexCursor<'a>) -> Self {
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
impl Mov9RecoveryV2IndexCursor<'_> {
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Mov9CursorStatus> {
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
    fn poll_inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Mov9CursorStatus> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.ready.is_some() {
            return Ok(Mov9CursorStatus::Ready);
        }
        match &mut self.engine {
            IndexEngine::Legacy(c) => match c.poll(work, cancel)? {
                Mov9CursorStatus::Ready => {
                    let e = c.take_entry()?;
                    self.ready = Some(Mov9RecoveryV2IndexEntry {
                        ordinal: e.ordinal,
                        position: e.position,
                        timestamp: e.timestamp,
                        flags: e.flags,
                        size: e.size,
                        min_distance: e.min_distance,
                        declaration: e.source,
                        association: Mov9RecoveryV2Association::DeclarationCoordinate,
                        range_error: None,
                    });
                    Ok(Mov9CursorStatus::Ready)
                }
                other => Ok(other),
            },
            IndexEngine::Direct(c) => match c.poll(work, cancel)? {
                Mov9CursorStatus::Ready => {
                    if !work.try_charge(1) {
                        return Ok(Mov9CursorStatus::Yield);
                    }
                    count(self.next.checked_add(1).ok_or(Error::Overflow)?, self.limit)?;
                    let r = c.take_occurrence()?;
                    let size = r
                        .native
                        .source_range
                        .end
                        .checked_sub(r.native.source_range.start)
                        .ok_or(Error::Overflow)?;
                    count(size, 0x1fff_ffff)?;
                    if r.native.flags & !1 != 0 {
                        return Err(Error::Unsupported("v2 direct native index flags"));
                    }
                    let key = r.native.flags & 1 != 0;
                    let distance = if key || self.next == 0 {
                        0
                    } else {
                        self.distance.checked_add(1).ok_or(Error::Overflow)?
                    };
                    self.ready = Some(Mov9RecoveryV2IndexEntry {
                        ordinal: self.next,
                        position: i64::try_from(r.native.source_range.start)
                            .map_err(|_| Error::Unsupported("native index signed position"))?,
                        timestamp: r.native.dts_field.signed_ticks(),
                        flags: i32::from(key),
                        size: u32::try_from(size).map_err(|_| Error::Overflow)?,
                        min_distance: i32::try_from(distance).map_err(|_| Error::Overflow)?,
                        declaration: r.declaration,
                        association: r.association,
                        range_error: r.range_error,
                    });
                    self.distance = distance;
                    self.next += 1;
                    Ok(Mov9CursorStatus::Ready)
                }
                other => Ok(other),
            },
        }
    }
    pub fn take_entry(&mut self) -> Result<Mov9RecoveryV2IndexEntry> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        self.ready.take().ok_or(Error::Invalid("no ready v2 index"))
    }
}
