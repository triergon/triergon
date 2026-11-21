//! Description recovery: a sample table whose chunk rows state invalid description indexes,
//! read as FFmpeg's MOV demuxer reads it. The source tables stay as stated. The cursors
//! report an event for every declared sample, including those FFmpeg reports no packet
//! for, and the physical cursors pair with
//! [`PhysicalReader`](crate::compat::mov::recovery::PhysicalReader) to read a packet the
//! file ends inside.
use super::*;
use crate::{
    CursorStatus,
    compat::mov::{cursor::PacketLimits, index::IndexLimits, view::PacketView},
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

/// Opt-in token for description recovery, passed to [`DescriptionRecoveryBuilder::new`] and
/// the description cursors of [`DescriptionRecoveryTable`].
#[derive(Clone, Copy, Debug)]
pub struct DescriptionRecoveryProfile;

/// What the caller knows about the source for description recovery.
#[derive(Clone, Copy, Debug)]
pub struct DescriptionContext {
    /// Length of the source file in bytes.
    pub physical_length: u64,
    /// Most bytes one call of the caller's read callback supplies. Only the comparison
    /// transport models of `reference_trace` read it; the cursors here infer no read
    /// outcome from it.
    pub custom_read_maximum: usize,
}
/// Whether a description-recovery packet's bytes can be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescriptionRead {
    /// The packet starts before the physical end of the file.
    Available,
    /// The first packet starts past the physical end, and the transport model given to the
    /// cursor shows FFmpeg's read reaching end of file there. The packet's byte range stays
    /// invalid.
    ModeledInitialEof,
    /// The packet starts at or past the physical end; what FFmpeg's read does there is not
    /// modelled.
    UnavailablePhysicalStart,
    /// A packet read through run recovery whose read outcome this view does not model.
    UnreviewedContext,
    /// The event has no packet, so nothing is read.
    NoPacketForAbsentCatalogue,
}
impl DescriptionContext {
    /// Whether a packet starting at absolute offset `start` can be read:
    /// [`Available`](DescriptionRead::Available) before the physical end,
    /// [`UnavailablePhysicalStart`](DescriptionRead::UnavailablePhysicalStart) otherwise.
    /// This is a fact about the file, not about FFmpeg's read. Fails with
    /// `Error::Unsupported` unless `terminal_media` is a range ending at the physical end.
    pub fn read_disposition(
        self,
        start: u64,
        terminal_media: Range<u64>,
    ) -> Result<DescriptionRead> {
        if terminal_media.start > terminal_media.end || terminal_media.end != self.physical_length {
            return Err(Error::Unsupported(
                "description recovery availability needs one physical terminal media extent",
            ));
        }
        Ok(if start < self.physical_length {
            DescriptionRead::Available
        } else {
            DescriptionRead::UnavailablePhysicalStart
        })
    }
}

/// How a description-recovery packet relates to the declared sample it is paired with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescriptionAssociation {
    /// The packet reads the declared sample's own byte range; its configuration and clock
    /// are not checked.
    DeclarationCoordinate,
    /// The packet shares only its ordinal, and so its size and clock rows, with the declared
    /// sample; its byte range comes from FFmpeg's chunk layout and is not that sample's.
    RawSizeAndClockOrdinalOnly,
}
/// Whether FFmpeg reports a packet for a declared sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescriptionEmission {
    /// FFmpeg reports a packet.
    Packet,
    /// The chunk's description index 2 names an entry the single-entry `stsd` lacks, so
    /// FFmpeg reports no packet.
    SuppressedMissingCatalogueEntry,
}
/// An `stsc` row whose `sample_description_index` strict parsing rejects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DescriptionRowDiagnostic {
    /// 0-based row in `stsc`.
    pub row: u32,
    /// The stated index; never 1.
    pub raw_description: u32,
    /// The row's `first_chunk`.
    pub declared_first_chunk: u32,
    /// The row's `samples_per_chunk`.
    pub declared_samples_per_chunk: u32,
    /// Absolute file offset of the row's `sample_description_index` field.
    pub field_start: u64,
    /// Absolute file offset just past that 4-byte field.
    pub field_end: u64,
    /// The error strict parsing reports for the row.
    pub strict_error: Error,
}

/// One declared sample of a [`DescriptionPacketCursor`], with the packet FFmpeg reports for
/// it. Every declared sample has an event, including those FFmpeg reports no packet for.
#[derive(Clone, Debug)]
pub struct DescriptionEvent {
    /// The sample as the table declares it; its access is unknown when the sync table was
    /// rejected.
    pub declaration: Sample,
    /// The packet FFmpeg reports; `None` when it reports none.
    pub packet: Option<PacketView>,
    /// Whether FFmpeg reports a packet.
    pub emission: DescriptionEmission,
    /// How `packet` relates to `declaration`.
    pub association: DescriptionAssociation,
    /// `sample_description_index` of the `stsc` row the packet's chunk belongs to.
    pub raw_chunk_description: u32,
    /// 1-based chunk number in FFmpeg's layout; `None` when the cursor reads the table
    /// through run recovery.
    pub physical_chunk: Option<u32>,
    /// 0-based `stsc` row whose `samples_per_chunk` FFmpeg uses for the chunk; `None` when
    /// the cursor reads the table through run recovery.
    pub observed_count_row: Option<u32>,
    /// The `mdhd` timescale; zero when the header was recovered with a zero timescale.
    pub source_timescale: u32,
    /// The `Error::Invalid` strict parsing returns for the `stsc` rows, when the declared
    /// or the chunk's description index is not 1.
    pub source_error: Option<Error>,
    /// Why the packet's byte range leaves the media extent, if it does.
    pub range_error: Option<Error>,
    /// Whether the packet's bytes can be read.
    pub read: DescriptionRead,
    /// Packets since the last key packet as FFmpeg's index counts them, declared samples
    /// without a packet included; 0 for a key packet and for the first declared sample.
    pub demux_index_distance: u64,
    /// How FFmpeg's buffered reader reaches end of file for the first packet when it starts
    /// past the physical end and the cursor was given a transport model; `None` otherwise.
    #[cfg(feature = "reference-trace")]
    pub modeled_transport_read:
        Option<crate::reference_trace::mov::raw_video_transport::ModeledInitialRead>,
}

/// Builds a [`DescriptionRecoveryTable`] from an `stbl` box whose `stsc` rows state invalid
/// description indexes, as FFmpeg's MOV demuxer reads it. Run recovery applies as well.
#[derive(Debug)]
pub struct DescriptionRecoveryBuilder {
    inner: RunRecoveryBuilder,
    base_ready: bool,
    at: u32,
    invalid: u32,
    special: Option<u32>,
    ready: bool,
    failed: Option<Error>,
}
impl DescriptionRecoveryBuilder {
    /// Starts description recovery of `raw`, charging storage to `budget`. Fails with
    /// `Error::ForeignPool` when `raw`'s bytes are not charged to `budget`, and with
    /// `Error::Invalid` when it is not an `stbl` box.
    pub fn new(
        raw: BoxView,
        budget: Budget,
        limits: Limits,
        _profile: DescriptionRecoveryProfile,
    ) -> Result<Self> {
        if !raw.bytes().belongs_to(&budget) {
            return Err(Error::ForeignPool);
        }
        let mut inner = RunRecoveryBuilder::new(raw, budget, limits, RunRecoveryProfile)?;
        inner.inner.recover_descriptions = true;
        Ok(Self {
            inner,
            base_ready: false,
            at: 0,
            invalid: 0,
            special: None,
            ready: false,
            failed: None,
        })
    }
    /// Advances the build. The table must have one sample description, and each row's
    /// description index must be 1 or 0; the first row may instead state 2 (an entry `stsd`
    /// lacks) or `u32::MAX` when it is the only invalid row and the second row starts at
    /// chunk 2. With any invalid row, `ctts` may have at most two rows. Anything else fails
    /// with `Error::Unsupported`. A failure other than backpressure is sticky.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(e) = result
            && !matches!(e, Error::Backpressure { .. })
        {
            self.failed = Some(e);
        }
        result
    }
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.ready {
            return Ok(BuildStatus::Ready);
        }
        if !self.base_ready {
            if self.inner.poll(work, cancel)? != BuildStatus::Ready {
                return Ok(BuildStatus::Yield);
            }
            self.base_ready = true;
        }
        let table = self
            .inner
            .inner
            .table
            .as_ref()
            .ok_or(Error::Invalid("description recovery missing raw storage"))?;
        if table.descriptions().len() != 1 {
            return Err(Error::Unsupported(
                "description recovery needs one sample description",
            ));
        }
        while self.at < table.stsc.count {
            if !work.try_charge(1) {
                return Ok(BuildStatus::Yield);
            }
            let description = table.stsc.word(self.at, 8)?;
            match description {
                1 => {}
                0 => {
                    self.invalid = self.invalid.checked_add(1).ok_or(Error::Overflow)?;
                }
                2 | u32::MAX => {
                    if self.at != 0 {
                        return Err(Error::Unsupported(
                            "description recovery noninitial missing/all-ones catalogue domain",
                        ));
                    }
                    self.special = Some(description);
                    self.invalid = self.invalid.checked_add(1).ok_or(Error::Overflow)?;
                }
                _ => {
                    return Err(Error::Unsupported(
                        "description recovery unobserved raw description value",
                    ));
                }
            }
            self.at += 1;
        }
        if self.special.is_some()
            && (self.invalid != 1 || table.stsc.count < 2 || table.stsc.word(1, 0)? != 2)
        {
            return Err(Error::Unsupported(
                "description recovery mixed/missing immediate successor for special description",
            ));
        }
        if self.invalid != 0 && table.ctts.as_ref().is_some_and(|t| t.count > 2) {
            return Err(Error::Unsupported(
                "description recovery direct composition domain has more than two rows",
            ));
        }
        self.ready = true;
        Ok(BuildStatus::Ready)
    }
    /// Returns the table once [`poll`](Self::poll) has returned [`BuildStatus::Ready`];
    /// fails with `Error::Invalid` before that, or with the failure `poll` returned.
    pub fn finish(self) -> Result<DescriptionRecoveryTable> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        if !self.ready {
            return Err(Error::Invalid("description recovery builder not ready"));
        }
        Ok(DescriptionRecoveryTable {
            base: self.inner.finish()?,
            invalid: self.invalid,
            special: self.special,
        })
    }
}

/// A sample table read under description recovery: the raw tables, the rows strict parsing
/// rejects, and cursors over the samples they declare.
#[derive(Debug)]
pub struct DescriptionRecoveryTable {
    base: RunRecoveryTable,
    invalid: u32,
    special: Option<u32>,
}
impl DescriptionRecoveryTable {
    /// The `stbl` box as stated.
    pub fn raw(&self) -> &BoxView {
        self.base.raw()
    }
    /// The `stsd` sample descriptions; description recovery requires exactly one.
    pub fn descriptions(&self) -> &[SampleDescription] {
        self.base.descriptions()
    }
    /// As [`RunRecoveryTable::coverage`].
    pub fn coverage(&self) -> &[Option<RunCoverage>; 2] {
        self.base.coverage()
    }
    /// As [`RunRecoveryTable::sync_diagnostic`].
    pub fn sync_diagnostic(&self) -> Option<SyncDiagnostic> {
        self.base.sync_diagnostic()
    }
    /// As [`RunRecoveryTable::source_prefix_cursor`]; samples keep their stated description
    /// indexes.
    pub fn source_prefix_cursor(&self) -> RecoverySourceCursor<'_> {
        self.base.source_prefix_cursor()
    }
    /// Number of rows in `stsc`.
    pub fn raw_description_row_count(&self) -> u32 {
        self.base.storage.stsc.count
    }
    /// Number of `stsc` rows whose description index is not 1.
    pub fn invalid_description_count(&self) -> u32 {
        self.invalid
    }
    /// The diagnostic for 0-based `stsc` row `row`; `None` when its description index is 1.
    /// Fails with `Error::Invalid` when `row` is past the last row.
    pub fn description_diagnostic(&self, row: u32) -> Result<Option<DescriptionRowDiagnostic>> {
        let table = &self.base.storage;
        if row >= table.stsc.count {
            return Err(Error::Invalid("description recovery diagnostic row"));
        }
        let raw = table.stsc.word(row, 8)?;
        if raw == 1 {
            return Ok(None);
        }
        let field_start = table
            .stsc
            .raw
            .header()
            .start
            .checked_add(u64::from(table.stsc.raw.header().header_size))
            .and_then(|v| v.checked_add(16 + u64::from(row) * 12))
            .ok_or(Error::Overflow)?;
        Ok(Some(DescriptionRowDiagnostic {
            row,
            raw_description: raw,
            declared_first_chunk: table.stsc.word(row, 0)?,
            declared_samples_per_chunk: table.stsc.word(row, 4)?,
            field_start,
            field_end: field_start.checked_add(4).ok_or(Error::Overflow)?,
            strict_error: Error::Invalid("stsc order/count/description"),
        }))
    }
    /// As [`RunRecoveryTable::stream_view_for_recovery_header`].
    pub fn stream_view_for_recovery_header(
        &self,
        header: &crate::compat::mov::field_recovery::RecoveryTrackHeader,
        brand: Option<[u8; 4]>,
    ) -> Result<crate::compat::mov::view::StreamView> {
        self.base.stream_view_for_recovery_header(header, brand)
    }
    /// Walks an event for every declared sample. A table without invalid rows whose first
    /// chunk starts before the physical end reads as [`RunRecoveryTable::packet_cursor`]
    /// does. Otherwise the cursor follows FFmpeg's chunk layout and needs one media extent
    /// ending at the physical end and at most two `ctts` rows (`Error::Unsupported`
    /// otherwise), and `limits` that admit a track, an extent and every sample
    /// (`Error::Limit`). Fails with `Error::ForeignPool` when the table is not charged to
    /// `pool`.
    pub fn description_packet_cursor<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::compat::mov::field_recovery::RecoveryTrackHeader,
        media: &'a [Range<u64>],
        limits: PacketLimits,
        context: DescriptionContext,
        _profile: DescriptionRecoveryProfile,
    ) -> Result<DescriptionPacketCursor<'a>> {
        if !self.raw().bytes().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        let table = &self.base.storage;
        let direct = self.invalid != 0
            || (table.sample_count() != 0 && table.offsets.get(0)? >= context.physical_length);
        if !direct {
            let legacy = self.base.direct_packet_cursor(
                pool,
                header,
                media,
                limits,
                DirectContext {
                    physical_length: context.physical_length,
                },
                DirectRecoveryProfile,
            )?;
            return Ok(DescriptionPacketCursor {
                legacy: Some(legacy),
                direct: None,
                latch: Latched::default(),
                legacy_visited: 0,
                legacy_distance: 0,
            });
        }
        if media.len() != 1
            || media[0].start > media[0].end
            || media[0].end != context.physical_length
        {
            return Err(Error::Unsupported(
                "description recovery needs one physical terminal media extent",
            ));
        }
        self.direct_cursor(
            header,
            media,
            limits,
            context,
            "description recovery direct composition domain has more than two rows",
            false,
        )
    }
    /// The direct engine after the caller's media-extent check.
    fn direct_cursor<'a>(
        &'a self,
        header: &crate::compat::mov::field_recovery::RecoveryTrackHeader,
        media: &'a [Range<u64>],
        limits: PacketLimits,
        context: DescriptionContext,
        composition_error: &'static str,
        physical_reads: bool,
    ) -> Result<DescriptionPacketCursor<'a>> {
        if limits.max_tracks == 0 || limits.max_media_extents == 0 {
            return Err(Error::Limit {
                requested: 1,
                maximum: limits.max_tracks.min(limits.max_media_extents),
            });
        }
        let table = &self.base.storage;
        bounded(table.sample_count(), limits.max_source_samples)?;
        if table.ctts.as_ref().is_some_and(|t| t.count > 2) {
            return Err(Error::Unsupported(composition_error));
        }
        Ok(DescriptionPacketCursor {
            legacy: None,
            direct: Some(Direct {
                table: self,
                source: table.cursor(),
                state: State::default(),
                view: crate::compat::mov::view::PacketProjector::new(
                    header.demux_projection_base()?,
                    table.sync_entry_count(),
                ),
                source_timescale: header.source().timescale,
                media: &media[0],
                context,
                limits,
                #[cfg(feature = "reference-trace")]
                transport: None,
                #[cfg(feature = "reference-trace")]
                root_walk_transport_model: None,
                physical_reads,
            }),
            latch: Latched::default(),
            legacy_visited: 0,
            legacy_distance: 0,
        })
    }
    /// Whether the first sample's declared range starts before `length` and ends after it,
    /// so that only a prefix of it exists in a file of `length` bytes. False for a table
    /// with invalid rows or without samples.
    pub fn first_sample_physical_prefix(
        &self,
        length: u64,
        _profile: crate::compat::mov::physical_read::PhysicalReadProfile,
    ) -> Result<bool> {
        let raw = &self.base.storage;
        if self.invalid != 0 || raw.sample_count() == 0 {
            return Ok(false);
        }
        let start = raw.offsets.get(0)?;
        let end = start
            .checked_add(u64::from(raw.sizes.get(0)?))
            .ok_or(Error::Overflow)?;
        Ok(start < length && end > length)
    }
    /// Walks an event for every declared sample in FFmpeg's chunk layout, for reading packet
    /// bytes with [`PhysicalReader`](crate::compat::mov::recovery::PhysicalReader). A
    /// packet is [`Available`](DescriptionRead::Available) when it starts before the
    /// physical end. Needs one media extent inside the physical source and at most two
    /// `ctts` rows (`Error::Unsupported` otherwise); fails with `Error::ForeignPool` when
    /// the table is not charged to `pool`.
    pub fn physical_packet_cursor<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::compat::mov::field_recovery::RecoveryTrackHeader,
        media: &'a [Range<u64>],
        limits: PacketLimits,
        context: DescriptionContext,
        _profile: crate::compat::mov::physical_read::PhysicalReadProfile,
    ) -> Result<DescriptionPacketCursor<'a>> {
        if !self.raw().bytes().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        if media.len() != 1
            || media[0].start > media[0].end
            || media[0].end > context.physical_length
        {
            return Err(Error::Unsupported(
                "physical recovery needs one declared media extent inside the physical source",
            ));
        }
        self.direct_cursor(
            header,
            media,
            limits,
            context,
            "physical recovery direct composition domain has more than two rows",
            true,
        )
    }
    /// FFmpeg's index over the events of
    /// [`physical_packet_cursor`](Self::physical_packet_cursor).
    pub fn physical_index_cursor<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::compat::mov::field_recovery::RecoveryTrackHeader,
        media: &'a [Range<u64>],
        limits: IndexLimits,
        context: DescriptionContext,
        profile: crate::compat::mov::physical_read::PhysicalReadProfile,
    ) -> Result<DescriptionIndexCursor<'a>> {
        let inner =
            self.physical_packet_cursor(pool, header, media, limits.packets, context, profile)?;
        Ok(DescriptionIndexCursor::new(inner, limits.max_entries))
    }
    /// [`physical_packet_cursor`](Self::physical_packet_cursor) in the context of a
    /// root-walk transport model, for trace comparison: a first packet past the physical
    /// end reports [`ModeledInitialEof`](DescriptionRead::ModeledInitialEof) where the model
    /// shows FFmpeg's read reaching end of file. Fails when `model` was not built for this
    /// table, `pool` and `media`.
    #[cfg(feature = "reference-trace")]
    pub fn root_walk_packet_cursor<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::compat::mov::field_recovery::RecoveryTrackHeader,
        media: &'a [Range<u64>],
        limits: PacketLimits,
        model: &'a crate::reference_trace::mov::root_walk_transport::RootWalkTransportModel,
        _profile: crate::reference_trace::mov::root_walk_transport::RootWalkTransportProfile,
    ) -> Result<DescriptionPacketCursor<'a>> {
        let context = model.context();
        model.require_binding(self.raw(), pool, context, media)?;
        let mut cursor = self.physical_packet_cursor(
            pool,
            header,
            media,
            limits,
            context,
            crate::compat::mov::physical_read::PhysicalReadProfile,
        )?;
        if let Some(direct) = &mut cursor.direct {
            direct.root_walk_transport_model = Some(model);
        }
        Ok(cursor)
    }
    /// FFmpeg's index over the events of
    /// [`root_walk_packet_cursor`](Self::root_walk_packet_cursor), for trace comparison.
    #[cfg(feature = "reference-trace")]
    pub fn root_walk_index_cursor<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::compat::mov::field_recovery::RecoveryTrackHeader,
        media: &'a [Range<u64>],
        limits: IndexLimits,
        model: &'a crate::reference_trace::mov::root_walk_transport::RootWalkTransportModel,
        profile: crate::reference_trace::mov::root_walk_transport::RootWalkTransportProfile,
    ) -> Result<DescriptionIndexCursor<'a>> {
        let inner =
            self.root_walk_packet_cursor(pool, header, media, limits.packets, model, profile)?;
        Ok(DescriptionIndexCursor::new(inner, limits.max_entries))
    }
    /// [`description_packet_cursor`](Self::description_packet_cursor) in the context of a
    /// raw-video transport model, for trace comparison: a first packet past the physical
    /// end reports [`ModeledInitialEof`](DescriptionRead::ModeledInitialEof) where the model
    /// shows FFmpeg's read reaching end of file. Fails when `model` was not built for this
    /// table, `pool` and `media`.
    #[cfg(feature = "reference-trace")]
    pub fn description_packet_cursor_with_transport<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::compat::mov::field_recovery::RecoveryTrackHeader,
        media: &'a [Range<u64>],
        limits: PacketLimits,
        model: &'a crate::reference_trace::mov::raw_video_transport::RawVideoTransportModel,
        profile: DescriptionRecoveryProfile,
    ) -> Result<DescriptionPacketCursor<'a>> {
        let context = model.context();
        model.require_binding(self.raw(), pool, context, media)?;
        let mut cursor =
            self.description_packet_cursor(pool, header, media, limits, context, profile)?;
        if let Some(direct) = &mut cursor.direct {
            direct.transport = Some(model);
        }
        Ok(cursor)
    }
    /// As [`description_index_cursor`](Self::description_index_cursor), with the context of
    /// a raw-video transport model, for trace comparison.
    #[cfg(feature = "reference-trace")]
    pub fn description_index_cursor_with_transport<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::compat::mov::field_recovery::RecoveryTrackHeader,
        media: &'a [Range<u64>],
        limits: IndexLimits,
        model: &'a crate::reference_trace::mov::raw_video_transport::RawVideoTransportModel,
        profile: DescriptionRecoveryProfile,
    ) -> Result<DescriptionIndexCursor<'a>> {
        let inner = self.description_packet_cursor_with_transport(
            pool,
            header,
            media,
            limits.packets,
            model,
            profile,
        )?;
        Ok(DescriptionIndexCursor::new(inner, limits.max_entries))
    }
    /// FFmpeg's index over the events of
    /// [`description_packet_cursor`](Self::description_packet_cursor); a declared sample
    /// without a packet takes no entry.
    pub fn description_index_cursor<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::compat::mov::field_recovery::RecoveryTrackHeader,
        media: &'a [Range<u64>],
        limits: IndexLimits,
        context: DescriptionContext,
        profile: DescriptionRecoveryProfile,
    ) -> Result<DescriptionIndexCursor<'a>> {
        let inner =
            self.description_packet_cursor(pool, header, media, limits.packets, context, profile)?;
        Ok(DescriptionIndexCursor::new(inner, limits.max_entries))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct State {
    ordinal: u64,
    chunk: u32,
    row: u32,
    left: u32,
    byte: u64,
    lookup: Option<u32>,
    count_row: u32,
    distance: u64,
    demux_started: bool,
}
#[derive(Debug)]
struct Direct<'a> {
    table: &'a DescriptionRecoveryTable,
    source: SampleCursor<'a>,
    state: State,
    view: crate::compat::mov::view::PacketProjector,
    source_timescale: u32,
    media: &'a Range<u64>,
    context: DescriptionContext,
    limits: PacketLimits,
    #[cfg(feature = "reference-trace")]
    transport: Option<&'a crate::reference_trace::mov::raw_video_transport::RawVideoTransportModel>,
    #[cfg(feature = "reference-trace")]
    root_walk_transport_model:
        Option<&'a crate::reference_trace::mov::root_walk_transport::RootWalkTransportModel>,
    physical_reads: bool,
}
impl Direct<'_> {
    fn ended(&self) -> bool {
        self.state.ordinal == self.table.base.storage.sample_count()
            || (self.state.left == 0
                && self.state.lookup.is_none()
                && self.state.chunk == self.table.base.storage.offsets.table.count)
    }
    fn poll(&mut self, work: &mut WorkBudget) -> Result<Option<DescriptionEvent>> {
        let raw = &self.table.base.storage;
        if self.ended() {
            return Ok(None);
        }
        if self.state.left == 0 && self.state.lookup.is_none() {
            if !work.try_charge(1) {
                return Ok(None);
            }
            let number = self.state.chunk.checked_add(1).ok_or(Error::Overflow)?;
            if self.state.row + 1 < raw.stsc.count
                && raw.stsc.word(self.state.row + 1, 0)? == number
            {
                self.state.row += 1;
            }
            self.state.byte = raw.offsets.get(self.state.chunk)?;
            self.state.chunk = number;
            self.state.lookup = Some(self.state.row);
        }
        while let Some(row) = self.state.lookup {
            if !work.try_charge(1) {
                return Ok(None);
            }
            let description = raw.stsc.word(row, 8)?;
            if matches!(description, 0 | u32::MAX) && row + 1 < raw.stsc.count {
                self.state.lookup = Some(row + 1);
            } else {
                self.state.left = raw.stsc.word(row, 4)?;
                self.state.count_row = row;
                self.state.lookup = None;
            }
        }
        let mut source = self.source;
        let declaration = match source.poll(work)? {
            Next::Yield => return Ok(None),
            Next::End => return Ok(None),
            Next::Item(s) => s,
        };
        bounded(
            self.state.ordinal.checked_add(1).ok_or(Error::Overflow)?,
            self.limits.max_occurrences,
        )?;
        let raw_description = raw.stsc.word(self.state.row, 8)?;
        let mut read_sample = declaration.clone();
        read_sample.range = self.state.byte
            ..self
                .state
                .byte
                .checked_add(u64::from(raw.sizes.get(self.state.ordinal)?))
                .ok_or(Error::Overflow)?;
        read_sample.description_index = raw_description;
        let projected = self.view.project_regular(&read_sample, 0)?;
        let distance = if projected.flags & 1 != 0 || self.state.ordinal == 0 {
            0
        } else {
            self.state.distance.checked_add(1).ok_or(Error::Overflow)?
        };
        let suppressed = raw_description == 2;
        #[cfg(feature = "reference-trace")]
        let modeled_transport_read = if !suppressed && !self.state.demux_started {
            self.transport
                .and_then(|model| model.initial_read_plan(projected.source_range.clone()))
                .or_else(|| {
                    self.root_walk_transport_model
                        .and_then(|model| model.initial_read_plan(projected.source_range.clone()))
                })
        } else {
            None
        };
        #[cfg(not(feature = "reference-trace"))]
        let modeled_transport_read = None::<()>;
        let read = if suppressed {
            DescriptionRead::NoPacketForAbsentCatalogue
        } else if modeled_transport_read.is_some() {
            DescriptionRead::ModeledInitialEof
        } else if self.physical_reads {
            if projected.source_range.start < self.context.physical_length {
                DescriptionRead::Available
            } else {
                DescriptionRead::UnavailablePhysicalStart
            }
        } else {
            self.context
                .read_disposition(projected.source_range.start, self.media.clone())?
        };
        let range_error = read_sample.require_within(self.media.clone()).err();
        let zero_or_sentinel = self.table.invalid != 0 && self.table.special != Some(2);
        let event_source_description = declaration.description_index;
        let event = DescriptionEvent {
            declaration: recovery_access(declaration, self.table.base.sync_diagnostic.is_some()),
            packet: (!suppressed).then_some(projected),
            emission: if suppressed {
                DescriptionEmission::SuppressedMissingCatalogueEntry
            } else {
                DescriptionEmission::Packet
            },
            association: if zero_or_sentinel {
                DescriptionAssociation::RawSizeAndClockOrdinalOnly
            } else {
                DescriptionAssociation::DeclarationCoordinate
            },
            raw_chunk_description: raw_description,
            physical_chunk: Some(self.state.chunk),
            observed_count_row: Some(self.state.count_row),
            source_timescale: self.source_timescale,
            source_error: (raw_description != 1 || event_source_description != 1)
                .then_some(Error::Invalid("stsc order/count/description")),
            range_error,
            read,
            demux_index_distance: distance,
            #[cfg(feature = "reference-trace")]
            modeled_transport_read,
        };
        self.state.byte = read_sample.range.end;
        self.state.left = self.state.left.checked_sub(1).ok_or(Error::Invalid(
            "description recovery zero observed chunk count",
        ))?;
        self.state.ordinal += 1;
        self.state.distance = distance;
        self.state.demux_started |= !suppressed;
        self.source = source;
        Ok(Some(event))
    }
}

/// The events of a [`DescriptionRecoveryTable`], one per declared sample in decode order.
#[derive(Debug)]
pub struct DescriptionPacketCursor<'a> {
    legacy: Option<DirectPacketCursor<'a>>,
    direct: Option<Direct<'a>>,
    latch: Latched<DescriptionEvent>,
    legacy_visited: u64,
    legacy_distance: u64,
}
impl DescriptionPacketCursor<'_> {
    /// Advances to the next event; take it with [`take_event`](Self::take_event). A
    /// failure, cancellation included, is sticky.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        let result = match self.latch.early(cancel) {
            Some(early) => early,
            None => self.poll_inner(work, cancel),
        };
        self.latch.settle(result)
    }
    /// Takes the ready event; fails with the sticky failure, or with `Error::Invalid` when
    /// no event is ready.
    pub fn take_event(&mut self) -> Result<DescriptionEvent> {
        self.latch.take("description recovery no ready event")
    }
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        if let Some(legacy) = &mut self.legacy {
            return match legacy.poll(work, cancel)? {
                CursorStatus::Ready => {
                    let old = legacy.take_occurrence()?;
                    let distance = if old.demux.flags & 1 != 0 || self.legacy_visited == 0 {
                        0
                    } else {
                        self.legacy_distance.checked_add(1).ok_or(Error::Overflow)?
                    };
                    self.latch.ready = Some(DescriptionEvent {
                        declaration: old.declaration,
                        raw_chunk_description: old.raw_chunk_description,
                        packet: Some(old.demux),
                        emission: DescriptionEmission::Packet,
                        association: match old.association {
                            DirectAssociation::DeclarationCoordinate => {
                                DescriptionAssociation::DeclarationCoordinate
                            }
                            DirectAssociation::RawSizeAndClockOrdinalOnly => {
                                DescriptionAssociation::RawSizeAndClockOrdinalOnly
                            }
                        },
                        physical_chunk: None,
                        observed_count_row: None,
                        source_timescale: old.source_timescale,
                        source_error: None,
                        range_error: old.range_error,
                        read: match old.read {
                            DirectRead::Available => DescriptionRead::Available,
                            DirectRead::ObservedForwardEof => DescriptionRead::UnreviewedContext,
                            DirectRead::UnreviewedSeekRefusal => DescriptionRead::UnreviewedContext,
                        },
                        demux_index_distance: distance,
                        #[cfg(feature = "reference-trace")]
                        modeled_transport_read: None,
                    });
                    self.legacy_visited =
                        self.legacy_visited.checked_add(1).ok_or(Error::Overflow)?;
                    self.legacy_distance = distance;
                    Ok(CursorStatus::Ready)
                }
                other => Ok(other),
            };
        }
        let direct = self
            .direct
            .as_mut()
            .ok_or(Error::Invalid("description recovery missing engine"))?;
        if direct.ended() {
            return Ok(CursorStatus::End);
        }
        if let Some(event) = direct.poll(work)? {
            self.latch.ready = Some(event);
            Ok(CursorStatus::Ready)
        } else if direct.ended() {
            Ok(CursorStatus::End)
        } else {
            Ok(CursorStatus::Yield)
        }
    }
}

/// An entry of FFmpeg's index for a recovered track, as FFmpeg reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeIndexEntry {
    /// 0-based position in the index.
    pub ordinal: u64,
    /// Absolute file offset of the packet's first byte.
    pub position: i64,
    /// The packet's decode time, in ticks of the track's packet time base.
    pub timestamp: i64,
    /// 1 for a key packet, 0 otherwise.
    pub flags: i32,
    /// Packet size in bytes; at most `0x1fff_ffff`.
    pub size: u32,
    /// Packets since the last key packet; 0 at a key packet. Under description recovery
    /// the count includes declared samples without a packet.
    pub min_distance: i32,
}
/// One event of a [`DescriptionIndexCursor`]: a declared sample and the index entry FFmpeg
/// makes for its packet.
#[derive(Clone, Debug)]
pub struct DescriptionIndexEvent {
    /// The packet cursor's event for the declared sample.
    pub source_event: DescriptionEvent,
    /// The index entry for the packet; `None` when FFmpeg reports no packet.
    pub demux_entry: Option<NativeIndexEntry>,
}
/// FFmpeg's index of a [`DescriptionRecoveryTable`], one declared sample at a time.
#[derive(Debug)]
pub struct DescriptionIndexCursor<'a> {
    inner: DescriptionPacketCursor<'a>,
    next: u64,
    limit: u64,
    latch: Latched<DescriptionIndexEvent>,
}
impl<'a> DescriptionIndexCursor<'a> {
    fn new(inner: DescriptionPacketCursor<'a>, limit: u64) -> Self {
        Self {
            inner,
            next: 0,
            limit,
            latch: Latched::default(),
        }
    }
    /// Advances to the next event; take it with [`take_event`](Self::take_event). A
    /// failure, cancellation included, is sticky.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        let result = match self.latch.early(cancel) {
            Some(early) => early,
            None => self.poll_inner(work, cancel),
        };
        self.latch.settle(result)
    }
    /// Takes the ready event; fails with the sticky failure, or with `Error::Invalid` when
    /// no event is ready.
    pub fn take_event(&mut self) -> Result<DescriptionIndexEvent> {
        self.latch.take("description recovery no ready index event")
    }
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        match self.inner.poll(work, cancel)? {
            CursorStatus::Ready => {
                if !work.try_charge(1) {
                    return Ok(CursorStatus::Yield);
                }
                let event = self.inner.take_event()?;
                let demux_entry = if let Some(packet) = &event.packet {
                    bounded(self.next.checked_add(1).ok_or(Error::Overflow)?, self.limit)?;
                    let entry = native_index_entry(
                        packet,
                        self.next,
                        event.demux_index_distance,
                        "description recovery native index flags",
                        "description recovery native signed index position",
                    )?;
                    self.next += 1;
                    Some(entry)
                } else {
                    None
                };
                self.latch.ready = Some(DescriptionIndexEvent {
                    source_event: event,
                    demux_entry,
                });
                Ok(CursorStatus::Ready)
            }
            other => Ok(other),
        }
    }
}
