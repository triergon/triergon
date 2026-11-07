//! Explicit empirical reference description view. Raw source tables/configuration/clocks are
//! never repaired. The cursor emits an event even for a missing demux entry.
use super::*;
use crate::{CursorStatus, IndexLimits, PacketLimits, PacketView};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy, Debug)]
pub struct DescriptionRecoveryProfile;

/// Caller facts only. custom_read_maximum is retained as configuration/provenance;
/// no demux I/O outcome is inferred from it.
#[derive(Clone, Copy, Debug)]
pub struct DescriptionContext {
    pub physical_length: u64,
    pub custom_read_maximum: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescriptionRead {
    Available,
    /// An explicitly selected, independently guarded reference transport model.
    /// The source range remains invalid and no demux I/O confirmation is invented.
    ModeledInitialEof,
    UnavailablePhysicalStart,
    UnreviewedContext,
    NoPacketForAbsentCatalogue,
}
impl DescriptionContext {
    /// Source physical availability only. It proves neither demux EOF nor any
    /// demux seek/buffer/cursor state. Missing physical bytes stay an unknown
    /// demux terminal transition until an independently reviewed policy exists.
    pub fn read_disposition(
        self,
        start: u64,
        terminal_media: Range<u64>,
    ) -> Result<DescriptionRead> {
        if terminal_media.start > terminal_media.end || terminal_media.end != self.physical_length {
            return Err(Error::Unsupported(
                "v3 availability needs one physical terminal media extent",
            ));
        }
        Ok(if start < self.physical_length {
            DescriptionRead::Available
        } else {
            DescriptionRead::UnavailablePhysicalStart
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescriptionAssociation {
    /// Coordinates agree; invalid source configuration/clock is not validated.
    DeclarationCoordinate,
    /// Only raw size/time ordinals correspond, not a canonical frame/config.
    RawSizeAndClockOrdinalOnly,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescriptionEmission {
    Packet,
    SuppressedMissingCatalogueEntry,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DescriptionRowDiagnostic {
    pub row: u32,
    pub raw_description: u32,
    pub declared_first_chunk: u32,
    pub declared_samples_per_chunk: u32,
    pub field_start: u64,
    pub field_end: u64,
    pub strict_error: Error,
}

/// All original source declarations are retained, including clock rows omitted
/// by the demux catalogue. A suppressed event contains no demux packet.
#[derive(Clone, Debug)]
pub struct DescriptionEvent {
    pub declaration: Sample,
    pub packet: Option<PacketView>,
    pub emission: DescriptionEmission,
    pub association: DescriptionAssociation,
    pub raw_chunk_description: u32,
    pub physical_chunk: Option<u32>,
    pub observed_count_row: Option<u32>,
    pub source_timescale: u32,
    pub source_error: Option<Error>,
    pub range_error: Option<Error>,
    pub read: DescriptionRead,
    pub demux_index_distance: u64,
    #[cfg(feature = "reference-trace")]
    pub modeled_transport_read: Option<crate::ModeledInitialRead>,
}

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
            .ok_or(Error::Invalid("v3 missing raw storage"))?;
        if table.descriptions().len() != 1 {
            return Err(Error::Unsupported(
                "v3 single native catalogue entry profile",
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
                            "v3 noninitial missing/all-ones catalogue domain",
                        ));
                    }
                    self.special = Some(description);
                    self.invalid = self.invalid.checked_add(1).ok_or(Error::Overflow)?;
                }
                _ => return Err(Error::Unsupported("v3 unobserved raw description value")),
            }
            self.at += 1;
        }
        if self.special.is_some()
            && (self.invalid != 1 || table.stsc.count < 2 || table.stsc.word(1, 0)? != 2)
        {
            return Err(Error::Unsupported(
                "v3 mixed/missing immediate successor for special description",
            ));
        }
        if self.invalid != 0 && table.ctts.as_ref().is_some_and(|t| t.count > 2) {
            return Err(Error::Unsupported(
                "v3 direct composition domain has more than two rows",
            ));
        }
        self.ready = true;
        Ok(BuildStatus::Ready)
    }
    pub fn finish(self) -> Result<DescriptionRecoveryTable> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        if !self.ready {
            return Err(Error::Invalid("v3 builder not ready"));
        }
        Ok(DescriptionRecoveryTable {
            base: self.inner.finish()?,
            invalid: self.invalid,
            special: self.special,
        })
    }
}

#[derive(Debug)]
pub struct DescriptionRecoveryTable {
    base: RunRecoveryTable,
    invalid: u32,
    special: Option<u32>,
}
impl DescriptionRecoveryTable {
    pub fn raw(&self) -> &BoxView {
        self.base.raw()
    }
    pub fn descriptions(&self) -> &[SampleDescription] {
        self.base.descriptions()
    }
    pub fn coverage(&self) -> &[Option<RunCoverage>; 2] {
        self.base.coverage()
    }
    pub fn sync_diagnostic(&self) -> Option<SyncDiagnostic> {
        self.base.sync_diagnostic()
    }
    pub fn source_prefix_cursor(&self) -> RecoverySourceCursor<'_> {
        self.base.source_prefix_cursor()
    }
    pub fn raw_description_row_count(&self) -> u32 {
        self.base.storage.stsc.count
    }
    pub fn invalid_description_count(&self) -> u32 {
        self.invalid
    }
    pub fn description_diagnostic(&self, row: u32) -> Result<Option<DescriptionRowDiagnostic>> {
        let table = &self.base.storage;
        if row >= table.stsc.count {
            return Err(Error::Invalid("v3 diagnostic row"));
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
    pub fn stream_view_for_recovery_header(
        &self,
        header: &crate::RecoveryTrackHeader,
        brand: Option<[u8; 4]>,
    ) -> Result<crate::StreamView> {
        self.base.stream_view_for_recovery_header(header, brand)
    }
    pub fn description_packet_cursor<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::RecoveryTrackHeader,
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
                ready: None,
                failed: None,
                legacy_visited: 0,
                legacy_distance: 0,
            });
        }
        if media.len() != 1
            || media[0].start > media[0].end
            || media[0].end != context.physical_length
        {
            return Err(Error::Unsupported(
                "v3 direct needs one physical terminal media extent",
            ));
        }
        if limits.max_tracks == 0 || limits.max_media_extents == 0 {
            return Err(Error::Limit {
                requested: 1,
                maximum: limits.max_tracks.min(limits.max_media_extents),
            });
        }
        bounded(table.sample_count(), limits.max_source_samples)?;
        if table.ctts.as_ref().is_some_and(|t| t.count > 2) {
            return Err(Error::Unsupported(
                "v3 direct composition domain has more than two rows",
            ));
        }
        Ok(DescriptionPacketCursor {
            legacy: None,
            direct: Some(Direct {
                table: self,
                source: table.cursor(),
                state: State::default(),
                view: crate::PacketProjector::new(
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
                physical_reads: false,
            }),
            ready: None,
            failed: None,
            legacy_visited: 0,
            legacy_distance: 0,
        })
    }
    /// A canonical first source request crosses the known physical end.
    /// No demux observation or expected object participates in this predicate.
    pub fn first_sample_physical_prefix(
        &self,
        length: u64,
        _profile: crate::PhysicalReadProfile,
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
    /// Explicit physical-byte operation. The plain description constructors are unchanged.
    pub fn physical_packet_cursor<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::RecoveryTrackHeader,
        media: &'a [Range<u64>],
        limits: PacketLimits,
        context: DescriptionContext,
        _profile: crate::PhysicalReadProfile,
    ) -> Result<DescriptionPacketCursor<'a>> {
        if !self.raw().bytes().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        if media.len() != 1
            || media[0].start > media[0].end
            || media[0].end > context.physical_length
        {
            return Err(Error::Unsupported(
                "v4 needs one declared media extent inside the physical source",
            ));
        }
        if limits.max_tracks == 0 || limits.max_media_extents == 0 {
            return Err(Error::Limit {
                requested: 1,
                maximum: limits.max_tracks.min(limits.max_media_extents),
            });
        }
        let table = &self.base.storage;
        bounded(table.sample_count(), limits.max_source_samples)?;
        if table.ctts.as_ref().is_some_and(|t| t.count > 2) {
            return Err(Error::Unsupported(
                "v4 direct composition domain has more than two rows",
            ));
        }
        Ok(DescriptionPacketCursor {
            legacy: None,
            direct: Some(Direct {
                table: self,
                source: table.cursor(),
                state: State::default(),
                view: crate::PacketProjector::new(
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
                physical_reads: true,
            }),
            ready: None,
            failed: None,
            legacy_visited: 0,
            legacy_distance: 0,
        })
    }
    pub fn physical_index_cursor<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::RecoveryTrackHeader,
        media: &'a [Range<u64>],
        limits: IndexLimits,
        context: DescriptionContext,
        profile: crate::PhysicalReadProfile,
    ) -> Result<DescriptionIndexCursor<'a>> {
        Ok(DescriptionIndexCursor {
            inner: self.physical_packet_cursor(
                pool,
                header,
                media,
                limits.packets,
                context,
                profile,
            )?,
            next: 0,
            limit: limits.max_entries,
            ready: None,
            failed: None,
        })
    }
    #[cfg(feature = "reference-trace")]
    pub fn root_walk_packet_cursor<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::RecoveryTrackHeader,
        media: &'a [Range<u64>],
        limits: PacketLimits,
        model: &'a crate::RootWalkTransportModel,
        _profile: crate::RootWalkTransportProfile,
    ) -> Result<DescriptionPacketCursor<'a>> {
        let context = model.context();
        model.require_binding(self.raw(), pool, context, media)?;
        let mut cursor = self.physical_packet_cursor(
            pool,
            header,
            media,
            limits,
            context,
            crate::PhysicalReadProfile,
        )?;
        if let Some(direct) = &mut cursor.direct {
            direct.root_walk_transport_model = Some(model);
        }
        Ok(cursor)
    }
    #[cfg(feature = "reference-trace")]
    pub fn root_walk_index_cursor<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::RecoveryTrackHeader,
        media: &'a [Range<u64>],
        limits: IndexLimits,
        model: &'a crate::RootWalkTransportModel,
        profile: crate::RootWalkTransportProfile,
    ) -> Result<DescriptionIndexCursor<'a>> {
        Ok(DescriptionIndexCursor {
            inner: self.root_walk_packet_cursor(
                pool,
                header,
                media,
                limits.packets,
                model,
                profile,
            )?,
            next: 0,
            limit: limits.max_entries,
            ready: None,
            failed: None,
        })
    }
    #[cfg(feature = "reference-trace")]
    pub fn description_packet_cursor_with_transport<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::RecoveryTrackHeader,
        media: &'a [Range<u64>],
        limits: PacketLimits,
        model: &'a crate::RawVideoTransportModel,
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
    #[cfg(feature = "reference-trace")]
    pub fn description_index_cursor_with_transport<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::RecoveryTrackHeader,
        media: &'a [Range<u64>],
        limits: IndexLimits,
        model: &'a crate::RawVideoTransportModel,
        profile: DescriptionRecoveryProfile,
    ) -> Result<DescriptionIndexCursor<'a>> {
        Ok(DescriptionIndexCursor {
            inner: self.description_packet_cursor_with_transport(
                pool,
                header,
                media,
                limits.packets,
                model,
                profile,
            )?,
            next: 0,
            limit: limits.max_entries,
            ready: None,
            failed: None,
        })
    }
    pub fn description_index_cursor<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::RecoveryTrackHeader,
        media: &'a [Range<u64>],
        limits: IndexLimits,
        context: DescriptionContext,
        profile: DescriptionRecoveryProfile,
    ) -> Result<DescriptionIndexCursor<'a>> {
        Ok(DescriptionIndexCursor {
            inner: self.description_packet_cursor(
                pool,
                header,
                media,
                limits.packets,
                context,
                profile,
            )?,
            next: 0,
            limit: limits.max_entries,
            ready: None,
            failed: None,
        })
    }
}

fn bounded(value: u64, maximum: u64) -> Result<()> {
    if value > maximum {
        return Err(Error::Limit {
            requested: usize::try_from(value).map_err(|_| Error::Overflow)?,
            maximum: usize::try_from(maximum).map_err(|_| Error::Overflow)?,
        });
    }
    Ok(())
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
    view: crate::PacketProjector,
    source_timescale: u32,
    media: &'a Range<u64>,
    context: DescriptionContext,
    limits: PacketLimits,
    #[cfg(feature = "reference-trace")]
    transport: Option<&'a crate::RawVideoTransportModel>,
    #[cfg(feature = "reference-trace")]
    root_walk_transport_model: Option<&'a crate::RootWalkTransportModel>,
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
        let mut declaration = match source.poll(work)? {
            SampleStep::Yield => return Ok(None),
            SampleStep::End => return Ok(None),
            SampleStep::Sample(s) => s,
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
        if self.table.base.sync_diagnostic.is_some() {
            declaration.access = Access::Unknown;
        }
        let zero_or_sentinel = self.table.invalid != 0 && self.table.special != Some(2);
        let event_source_description = declaration.description_index;
        let event = DescriptionEvent {
            declaration,
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
        self.state.left = self
            .state
            .left
            .checked_sub(1)
            .ok_or(Error::Invalid("v3 zero observed chunk count"))?;
        self.state.ordinal += 1;
        self.state.distance = distance;
        self.state.demux_started |= !suppressed;
        self.source = source;
        Ok(Some(event))
    }
}

#[derive(Debug)]
pub struct DescriptionPacketCursor<'a> {
    legacy: Option<DirectPacketCursor<'a>>,
    direct: Option<Direct<'a>>,
    ready: Option<DescriptionEvent>,
    failed: Option<Error>,
    legacy_visited: u64,
    legacy_distance: u64,
}
impl DescriptionPacketCursor<'_> {
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(e) = result {
            self.failed = Some(e);
            self.ready = None;
        }
        result
    }
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.ready.is_some() {
            return Ok(CursorStatus::Ready);
        }
        if let Some(legacy) = &mut self.legacy {
            return match legacy.poll(work, cancel)? {
                CursorStatus::Ready => {
                    let old = legacy.take_occurrence()?;
                    let distance = if old.demux.flags & 1 != 0 || self.legacy_visited == 0 {
                        0
                    } else {
                        self.legacy_distance.checked_add(1).ok_or(Error::Overflow)?
                    };
                    self.ready = Some(DescriptionEvent {
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
            .ok_or(Error::Invalid("v3 missing engine"))?;
        if direct.ended() {
            return Ok(CursorStatus::End);
        }
        if let Some(event) = direct.poll(work)? {
            self.ready = Some(event);
            Ok(CursorStatus::Ready)
        } else if direct.ended() {
            Ok(CursorStatus::End)
        } else {
            Ok(CursorStatus::Yield)
        }
    }
    pub fn take_event(&mut self) -> Result<DescriptionEvent> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        self.ready.take().ok_or(Error::Invalid("v3 no ready event"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DescriptionIndexEntry {
    pub ordinal: u64,
    pub position: i64,
    pub timestamp: i64,
    pub flags: i32,
    pub size: u32,
    pub min_distance: i32,
}
#[derive(Clone, Debug)]
pub struct DescriptionIndexEvent {
    pub source_event: DescriptionEvent,
    pub demux_entry: Option<DescriptionIndexEntry>,
}
#[derive(Debug)]
pub struct DescriptionIndexCursor<'a> {
    inner: DescriptionPacketCursor<'a>,
    next: u64,
    limit: u64,
    ready: Option<DescriptionIndexEvent>,
    failed: Option<Error>,
}
impl DescriptionIndexCursor<'_> {
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(e) = result {
            self.failed = Some(e);
            self.ready = None;
        }
        result
    }
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CursorStatus> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.ready.is_some() {
            return Ok(CursorStatus::Ready);
        }
        match self.inner.poll(work, cancel)? {
            CursorStatus::Ready => {
                if !work.try_charge(1) {
                    return Ok(CursorStatus::Yield);
                }
                let event = self.inner.take_event()?;
                let demux_entry = if let Some(packet) = &event.packet {
                    bounded(self.next.checked_add(1).ok_or(Error::Overflow)?, self.limit)?;
                    let size = packet
                        .source_range
                        .end
                        .checked_sub(packet.source_range.start)
                        .ok_or(Error::Overflow)?;
                    bounded(size, 0x1fff_ffff)?;
                    if packet.flags & !1 != 0 {
                        return Err(Error::Unsupported("v3 native index flags"));
                    }
                    let entry = DescriptionIndexEntry {
                        ordinal: self.next,
                        position: i64::try_from(packet.source_range.start)
                            .map_err(|_| Error::Unsupported("v3 native signed index position"))?,
                        timestamp: packet.dts_field.signed_ticks(),
                        flags: i32::from(packet.flags & 1 != 0),
                        size: u32::try_from(size).map_err(|_| Error::Overflow)?,
                        min_distance: i32::try_from(event.demux_index_distance)
                            .map_err(|_| Error::Overflow)?,
                    };
                    self.next += 1;
                    Some(entry)
                } else {
                    None
                };
                self.ready = Some(DescriptionIndexEvent {
                    source_event: event,
                    demux_entry,
                });
                Ok(CursorStatus::Ready)
            }
            other => Ok(other),
        }
    }
    pub fn take_event(&mut self) -> Result<DescriptionIndexEvent> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        self.ready
            .take()
            .ok_or(Error::Invalid("v3 no ready index event"))
    }
}
