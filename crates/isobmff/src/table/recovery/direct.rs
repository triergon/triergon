//! Direct recovery: a sample table whose first chunk row has description index zero, or
//! whose first chunk starts past the physical end of the file, read as FFmpeg's MOV demuxer
//! reads it. The zero is accepted only in the first row of a table with one sample
//! description, followed by a row that starts at chunk 2 with index 1. The source tables
//! stay as stated; where FFmpeg lays chunks out differently, each packet reports its own
//! byte range beside the declared sample.
use super::*;
use crate::{
    CursorStatus,
    compat::mov::{
        cursor::PacketLimits,
        field_recovery::RecoveryTrackHeader,
        index::IndexLimits,
        view::{PacketProjector, PacketView},
    },
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

/// Opt-in token for direct recovery, passed to [`RunRecoveryBuilder::new_direct`] and the
/// direct cursors of [`RunRecoveryTable`].
#[derive(Clone, Copy, Debug)]
pub struct DirectRecoveryProfile;
impl DirectRecoveryProfile {
    /// Exact original public reference allocates this AVIO buffer. Demux
    /// controls establish the common forward-EOF interval for ordinary/tiny
    /// inputs; this is not an ISO rule or a universal AVIO algorithm claim.
    pub(crate) const OBSERVED_FORWARD_READ_CAPACITY: u64 = 32768;
}
/// The zero description index in the first `stsc` row that direct recovery accepts, and how
/// FFmpeg reads the first chunk instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InitialDescriptionDiagnostic {
    /// Absolute file offset of the row's `sample_description_index` field.
    pub field_start: u64,
    /// Absolute file offset just past that 4-byte field.
    pub field_end: u64,
    /// The stated index; always 0.
    pub raw_description: u32,
    /// `samples_per_chunk` of the first row, which FFmpeg does not use.
    pub declared_first_chunk_samples: u32,
    /// `samples_per_chunk` of the second row, which FFmpeg uses for the first chunk.
    pub observed_first_chunk_samples: u32,
    /// 1-based sample description FFmpeg uses for the samples; always 1.
    pub demux_catalogue_entry: u32,
    /// The error strict parsing reports for the row.
    pub strict_error: Error,
}
/// What the caller knows about the source for direct recovery.
#[derive(Clone, Copy, Debug)]
pub struct DirectContext {
    /// Length of the source file in bytes.
    pub physical_length: u64,
}
/// How a direct-recovery packet relates to the declared sample it is paired with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectAssociation {
    /// The packet reads the declared sample's own byte range. Its clock or extent may still
    /// be invalid; the occurrence's `range_error` reports the extent.
    DeclarationCoordinate,
    /// The packet shares only its ordinal, and so its size and clock rows, with the declared
    /// sample; its byte range comes from FFmpeg's chunk layout and is not that sample's.
    RawSizeAndClockOrdinalOnly,
}
/// Whether a direct-recovery packet's bytes can be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectRead {
    /// The packet is empty or starts before the physical end of the file.
    Available,
    /// The packet starts at or past the physical end, within 32,768 bytes of the media
    /// extent's start, where FFmpeg reads forward to the end of the file and reports end of
    /// file.
    ObservedForwardEof,
    /// The packet starts further past the media extent's start, where FFmpeg would have to
    /// seek past the end; this view does not model the outcome.
    UnreviewedSeekRefusal,
}
/// One packet of a [`DirectPacketCursor`]: the declared sample and the packet FFmpeg
/// reports in its place.
#[derive(Clone, Debug)]
pub struct DirectOccurrence {
    /// The sample as the table declares it; its access is unknown when the sync table was
    /// rejected.
    pub declaration: Sample,
    /// The packet FFmpeg reports.
    pub demux: PacketView,
    /// The `mdhd` timescale as stored; zero when the header declares none.
    pub source_timescale: u32,
    /// How `demux` relates to `declaration`.
    pub association: DirectAssociation,
    /// The `stsc` sample description index of the sample's chunk, as stated.
    pub raw_chunk_description: u32,
    /// Why the packet's byte range leaves the media extent, if it does.
    pub range_error: Option<Error>,
    /// Whether the packet's bytes can be read.
    pub read: DirectRead,
}
impl RunRecoveryBuilder {
    /// Starts direct recovery of `raw`: run recovery that also accepts a zero
    /// `sample_description_index` in the first `stsc` row, when the table has one sample
    /// description and the second row starts at chunk 2 with index 1. The table then
    /// reports an [`InitialDescriptionDiagnostic`]. Fails as [`new`](Self::new) does.
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
    /// The zero description index direct recovery accepted in the first `stsc` row, if any.
    pub fn initial_description_diagnostic(&self) -> Option<InitialDescriptionDiagnostic> {
        self.initial_description
    }
    /// Walks the packets FFmpeg reports for this track. A table without an initial
    /// description diagnostic whose first chunk starts before `context`'s physical end reads
    /// as [`packet_cursor`](Self::packet_cursor) does, every packet available. Otherwise the
    /// cursor follows FFmpeg's chunk layout and needs one media extent reaching the physical
    /// end, one sample description and at most two `ctts` rows (`Error::Unsupported`
    /// otherwise), and `limits` that admit a track, an extent and every sample
    /// (`Error::Limit`). Fails with `Error::ForeignPool` when the table is not charged to
    /// `pool`.
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
        bounded(self.storage.sample_count(), limits.max_source_samples)?;
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
            latch: Latched::default(),
        })
    }
    /// Builds FFmpeg's index of this track, one entry per packet, choosing the layout as
    /// [`direct_packet_cursor`](Self::direct_packet_cursor) does.
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
            latch: Latched::default(),
        })
    }
}
pub(super) fn bounded(value: u64, maximum: u64) -> Result<()> {
    if value > maximum {
        return Err(Error::Limit {
            requested: usize::try_from(value).map_err(|_| Error::Overflow)?,
            maximum: usize::try_from(maximum).map_err(|_| Error::Overflow)?,
        });
    }
    Ok(())
}
/// A cursor's ready item and latched failure.
#[derive(Debug)]
pub(super) struct Latched<T> {
    pub(super) ready: Option<T>,
    failed: Option<Error>,
}
impl<T> Default for Latched<T> {
    fn default() -> Self {
        Self {
            ready: None,
            failed: None,
        }
    }
}
impl<T> Latched<T> {
    /// What a poll returns without advancing the cursor: the latched failure,
    /// a cancellation, or the item still waiting to be taken.
    pub(super) fn early(&self, cancel: &AtomicBool) -> Option<Result<CursorStatus>> {
        if let Some(e) = self.failed {
            return Some(Err(e));
        }
        if cancel.load(Ordering::Relaxed) {
            return Some(Err(Error::Cancelled));
        }
        self.ready.is_some().then_some(Ok(CursorStatus::Ready))
    }
    /// Any error, cancellation included, latches and drops the ready item.
    pub(super) fn settle(&mut self, result: Result<CursorStatus>) -> Result<CursorStatus> {
        if let Err(e) = result {
            self.failed = Some(e);
            self.ready = None;
        }
        result
    }
    /// The ready item unless a failure is latched.
    pub(super) fn take(&mut self, missing: &'static str) -> Result<T> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        self.ready.take().ok_or(Error::Invalid(missing))
    }
}
/// Native index fields of a demux packet; the error texts name the profile.
pub(super) fn native_index_entry(
    packet: &PacketView,
    ordinal: u64,
    distance: u64,
    flags_error: &'static str,
    position_error: &'static str,
) -> Result<NativeIndexEntry> {
    let size = packet
        .source_range
        .end
        .checked_sub(packet.source_range.start)
        .ok_or(Error::Overflow)?;
    bounded(size, 0x1fff_ffff)?;
    if packet.flags & !1 != 0 {
        return Err(Error::Unsupported(flags_error));
    }
    Ok(NativeIndexEntry {
        ordinal,
        position: i64::try_from(packet.source_range.start)
            .map_err(|_| Error::Unsupported(position_error))?,
        timestamp: packet.dts_field.signed_ticks(),
        flags: i32::from(packet.flags & 1 != 0),
        size: u32::try_from(size).map_err(|_| Error::Overflow)?,
        min_distance: i32::try_from(distance).map_err(|_| Error::Overflow)?,
    })
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
            Next::Yield => return Err(Error::Invalid("internal v2 yield sentinel")),
            Next::End => return Ok(None),
            Next::Item(s) => s,
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
        bounded(
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
        let row = DirectOccurrence {
            declaration: recovery_access(declaration, self.table.sync_diagnostic.is_some()),
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
/// FFmpeg's packets for a [`RunRecoveryTable`] under direct recovery, from
/// [`RunRecoveryTable::direct_packet_cursor`].
#[derive(Debug)]
pub struct DirectPacketCursor<'a> {
    // Exactly one engine is initialized by these sealed constructors. Keep
    // state inline rather than introduce a profile-switch heap allocation.
    legacy: Option<RecoveryPacketCursor<'a>>,
    direct: Option<DirectCursor<'a>>,
    latch: Latched<DirectOccurrence>,
}
impl<'a> From<RecoveryPacketCursor<'a>> for DirectPacketCursor<'a> {
    fn from(inner: RecoveryPacketCursor<'a>) -> Self {
        Self {
            legacy: Some(inner),
            direct: None,
            latch: Latched::default(),
        }
    }
}
impl DirectPacketCursor<'_> {
    /// Advances to the next packet; take it with [`take_occurrence`](Self::take_occurrence).
    /// A failure, cancellation included, is sticky.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        let result = match self.latch.early(cancel) {
            Some(early) => early,
            None => self.poll_inner(work, cancel),
        };
        self.latch.settle(result)
    }
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        if let Some(c) = &mut self.legacy {
            return match c.poll(work, cancel)? {
                CursorStatus::Ready => {
                    let r = c.take_occurrence()?;
                    self.latch.ready = Some(DirectOccurrence {
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
                self.latch.ready = Some(r);
                Ok(CursorStatus::Ready)
            }
            None => Ok(CursorStatus::End),
        }
    }
    /// Takes the ready packet; fails with the sticky failure, or with `Error::Invalid` when
    /// no packet is ready.
    pub fn take_occurrence(&mut self) -> Result<DirectOccurrence> {
        self.latch.take("no ready v2 observation")
    }
}
/// One entry of FFmpeg's index under direct recovery, with the declared sample it stands
/// for.
#[derive(Clone, Debug)]
pub struct DirectIndexEntry {
    /// The index entry FFmpeg reports.
    pub entry: NativeIndexEntry,
    /// The sample as the table declares it.
    pub declaration: Sample,
    /// How the entry's packet relates to `declaration`.
    pub association: DirectAssociation,
    /// Why the packet's byte range leaves the media extent, if it does.
    pub range_error: Option<Error>,
}
// One engine is chosen per index; keep it inline rather than allocate it.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum IndexEngine<'a> {
    Legacy(RecoveryIndexCursor<'a>),
    Direct(DirectPacketCursor<'a>),
}
/// FFmpeg's index of a [`RunRecoveryTable`] under direct recovery, from
/// [`RunRecoveryTable::direct_index_cursor`].
#[derive(Debug)]
pub struct DirectIndexCursor<'a> {
    engine: IndexEngine<'a>,
    next: u64,
    distance: u64,
    limit: u64,
    latch: Latched<DirectIndexEntry>,
}
impl<'a> From<RecoveryIndexCursor<'a>> for DirectIndexCursor<'a> {
    fn from(c: RecoveryIndexCursor<'a>) -> Self {
        Self {
            engine: IndexEngine::Legacy(c),
            next: 0,
            distance: 0,
            limit: u64::MAX,
            latch: Latched::default(),
        }
    }
}
impl DirectIndexCursor<'_> {
    /// Advances to the next entry; take it with [`take_entry`](Self::take_entry). A failure,
    /// cancellation included, is sticky.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        let result = match self.latch.early(cancel) {
            Some(early) => early,
            None => self.poll_inner(work, cancel),
        };
        self.latch.settle(result)
    }
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        match &mut self.engine {
            IndexEngine::Legacy(c) => match c.poll(work, cancel)? {
                CursorStatus::Ready => {
                    let e = c.take_entry()?;
                    self.latch.ready = Some(DirectIndexEntry {
                        entry: NativeIndexEntry {
                            ordinal: e.ordinal,
                            position: e.position,
                            timestamp: e.timestamp,
                            flags: e.flags,
                            size: e.size,
                            min_distance: e.min_distance,
                        },
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
                    bounded(self.next.checked_add(1).ok_or(Error::Overflow)?, self.limit)?;
                    let r = c.take_occurrence()?;
                    // Cannot overflow: the distance never exceeds the bounded ordinal.
                    let distance = if r.demux.flags & 1 != 0 || self.next == 0 {
                        0
                    } else {
                        self.distance.checked_add(1).ok_or(Error::Overflow)?
                    };
                    let entry = native_index_entry(
                        &r.demux,
                        self.next,
                        distance,
                        "v2 direct native index flags",
                        "native index signed position",
                    )?;
                    self.latch.ready = Some(DirectIndexEntry {
                        entry,
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
    /// Takes the ready entry; fails with the sticky failure, or with `Error::Invalid` when no
    /// entry is ready.
    pub fn take_entry(&mut self) -> Result<DirectIndexEntry> {
        self.latch.take("no ready v2 index")
    }
}
