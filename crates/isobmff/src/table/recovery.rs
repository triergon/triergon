//! Sample tables of malformed MOV files, read as FFmpeg's MOV demuxer reads them.
//! Run recovery ([`RunRecoveryBuilder::new`]) accepts timing runs past the sample count and
//! out-of-order or out-of-range sync entries; direct recovery
//! ([`RunRecoveryBuilder::new_direct`]) also a zero first description index; description
//! recovery ([`DescriptionRecoveryBuilder`]) invalid description indexes in any row. The
//! raw tables stay as stated, and each table reports what strict parsing rejects.
use super::*;

mod description;
mod direct;
pub use description::*;
pub use direct::*;

/// Opt-in token for field and run recovery. The constructors that read a malformed MOV file
/// as FFmpeg's MOV demuxer does take it, so that recovery is always chosen explicitly. It
/// changes no source fact, packet order or codec configuration, and strict parsing
/// elsewhere is unaffected.
#[derive(Clone, Copy, Debug)]
pub struct RunRecoveryProfile;
#[cfg(feature = "reference-trace")]
impl RunRecoveryProfile {
    /// SHA-256 of the FFmpeg reference worker that recovery traces are compared with.
    pub const REFERENCE_WORKER_SHA256: &'static str =
        "7ff6742eaafa59884b6dc95f65c701c26b18975c265aa8401d61154eb2ef1204";
}
/// The timing table a [`RunCoverage`] describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunKind {
    /// Decode deltas (`stts`).
    Decode,
    /// Composition offsets (`ctts`).
    Composition,
}
/// How far one timing table's runs overrun the sample count. The raw table, unused runs
/// included, stays in [`RunRecoveryTable::raw`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunCoverage {
    /// The table that overruns.
    pub kind: RunKind,
    /// 0-based row of the first run that passes the sample count.
    pub first_excess_row: u32,
    /// Absolute file offset of the `sample_count` field of the first row whose run passes
    /// the sample count.
    pub count_field_start: u64,
    /// Absolute file offset just past that 4-byte field.
    pub count_field_end: u64,
    /// Samples all runs of the table cover together.
    pub declared_samples: u64,
    /// Samples the sample sizes declare; the runs are read up to this count.
    pub bounded_samples: u64,
    /// `declared_samples` minus `bounded_samples`; never zero.
    pub surplus_samples: u64,
    /// The error strict parsing reports for the table.
    pub strict_error: Error,
}
/// Builds a [`RunRecoveryTable`] from an `stbl` box whose runs overrun its sample count, as
/// FFmpeg's MOV demuxer reads it. Only the last `stts` run may pass the sample count, and it
/// must start inside it; any `ctts` run may. `stss` entries out of order or past the last
/// sample are accepted too, and every sample then has unknown access. All other checks and
/// limits are those of [`SampleTableBuilder`].
#[derive(Debug)]
pub struct RunRecoveryBuilder {
    inner: SampleTableBuilder,
}
impl RunRecoveryBuilder {
    /// Starts run recovery of `raw`, charging storage to `budget`. Fails with
    /// `Error::Invalid` when `raw` is not an `stbl` box or its bytes are not charged to
    /// `budget`.
    pub fn new(
        raw: BoxView,
        budget: Budget,
        limits: Limits,
        _profile: RunRecoveryProfile,
    ) -> Result<Self> {
        let mut inner = SampleTableBuilder::with_timing_policy(
            raw,
            budget,
            limits,
            TimingRunPolicy::BoundedPrefix,
        )?;
        inner.acceptance = Acceptance::Runs;
        Ok(Self { inner })
    }
    /// Advances the build. A failure other than backpressure is sticky, as is
    /// `Error::Cancelled` once `cancel` is set.
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<BuildStatus> {
        if let Stage::Failed(error) = self.inner.stage {
            return Err(error);
        }
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            self.inner.stage = Stage::Failed(Error::Cancelled);
            return Err(Error::Cancelled);
        }
        self.inner.poll(work)
    }
    /// Returns the table once [`poll`](Self::poll) has returned [`BuildStatus::Ready`];
    /// fails with `Error::Invalid` before that, or with the failure `poll` returned.
    pub fn finish(self) -> Result<RunRecoveryTable> {
        let storage = match self.inner.stage {
            Stage::Ready(table) => table,
            Stage::Failed(error) => return Err(error),
            _ => return Err(Error::Invalid("run recovery builder not ready")),
        };
        let found = self.inner.findings;
        Ok(RunRecoveryTable {
            storage,
            coverage: found.coverage,
            sync_diagnostic: found.sync_diagnostic,
            initial_description: found.initial_description,
        })
    }
}
/// A sample table read under run or direct recovery: the raw tables, what strict parsing
/// rejects in them, and cursors over the samples they declare. It deliberately does not
/// convert into a [`SampleTable`], so that its samples are only read with these diagnostics
/// at hand.
#[derive(Debug)]
pub struct RunRecoveryTable {
    storage: SampleTable,
    coverage: [Option<RunCoverage>; 2],
    sync_diagnostic: Option<SyncDiagnostic>,
    initial_description: Option<InitialDescriptionDiagnostic>,
}
impl RunRecoveryTable {
    /// The `stbl` box as stated, unused runs included.
    pub fn raw(&self) -> &BoxView {
        self.storage.raw()
    }
    /// The `stsd` sample descriptions, in order; the first has index 1.
    pub fn descriptions(&self) -> &[SampleDescription] {
        self.storage.descriptions()
    }
    /// The overrun of the decode runs (`[0]`) and composition runs (`[1]`); `None` where a
    /// table covers exactly the sample count or is absent.
    pub fn coverage(&self) -> &[Option<RunCoverage>; 2] {
        &self.coverage
    }
    /// Walks the samples the sample sizes declare, in decode order, timed by the runs that
    /// cover them; runs past the last sample are not read. The table is still invalid:
    /// report [`coverage`](Self::coverage) and [`sync_diagnostic`](Self::sync_diagnostic)
    /// with the samples.
    pub fn source_prefix_cursor(&self) -> RecoverySourceCursor<'_> {
        RecoverySourceCursor {
            inner: self.storage.cursor(),
            unknown_access: self.sync_diagnostic.is_some(),
        }
    }
    /// The first `stss` entry strict parsing rejects, if any. When there is one, every
    /// sample has [`Access::Unknown`].
    pub fn sync_diagnostic(&self) -> Option<SyncDiagnostic> {
        self.sync_diagnostic
    }
    /// The stream FFmpeg reports for this track, built from its first sample description
    /// and timed in `header.demux_projection_base()`. `major_brand` is the `ftyp` major
    /// brand: a raw-video stream drops its vendor tag unless it is `qt  `. Fails with
    /// `Error::Unsupported` for a table without samples, a raw-video stream without a major
    /// brand, or a codec other than raw video and `tx3g`.
    pub fn stream_view_for_recovery_header(
        &self,
        header: &crate::compat::mov::field_recovery::RecoveryTrackHeader,
        major_brand: Option<[u8; 4]>,
    ) -> Result<crate::compat::mov::view::StreamView> {
        let entry = self
            .storage
            .descriptions()
            .first()
            .ok_or(Error::Invalid("missing recovery description"))?;
        if self.storage.sample_count() == 0 {
            return Err(Error::Unsupported("empty recovery stream"));
        }
        crate::compat::mov::view::StreamView::with_projection_base(
            header.source(),
            entry,
            header.demux_projection_base()?,
        )?
        .with_major_brand(major_brand)
    }
    fn track(
        &self,
        track_id: u32,
        media_base: triergon_core::TimeBase,
    ) -> crate::compat::mov::cursor::DemuxTrack<'_> {
        crate::compat::mov::cursor::DemuxTrack {
            track_id,
            table: &self.storage,
            fragments: &[],
            media_base,
            edits: None,
        }
    }
    /// Walks the packets FFmpeg's MOV demuxer reports for this track alone, checked against
    /// the file's media extents `media` and timed in `header.demux_projection_base()`.
    /// Fails with `Error::Unsupported` for a table with an
    /// [`initial_description_diagnostic`](Self::initial_description_diagnostic); read that
    /// one with [`direct_packet_cursor`](Self::direct_packet_cursor).
    pub fn packet_cursor<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::compat::mov::field_recovery::RecoveryTrackHeader,
        media: &'a [std::ops::Range<u64>],
        limits: crate::compat::mov::cursor::PacketLimits,
    ) -> Result<RecoveryPacketCursor<'a>> {
        if self.initial_description.is_some() {
            return Err(Error::Unsupported(
                "initial-description recovery requires the direct packet cursor",
            ));
        }
        let inner = crate::compat::mov::cursor::PacketCursor::new_complete_source(
            pool,
            &[self.track(header.source().track_id, header.demux_projection_base()?)],
            media,
            limits,
            crate::compat::mov::cursor::DemuxProfile::NoParseNoFillin,
        )?;
        Ok(RecoveryPacketCursor {
            inner,
            unknown_access: self.sync_diagnostic.is_some(),
            source_timescale: header.source().timescale,
        })
    }
    /// Builds FFmpeg's index of this track, one entry per packet; the first entry may be a
    /// non-key sample. Fails as [`packet_cursor`](Self::packet_cursor) does.
    pub fn index_cursor<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::compat::mov::field_recovery::RecoveryTrackHeader,
        media: &'a [std::ops::Range<u64>],
        limits: crate::compat::mov::index::IndexLimits,
    ) -> Result<RecoveryIndexCursor<'a>> {
        if self.initial_description.is_some() {
            return Err(Error::Unsupported(
                "initial-description recovery requires the direct packet cursor",
            ));
        }
        let mut inner = crate::compat::mov::index::IndexCursor::new_complete_source(
            pool,
            self.track(header.source().track_id, header.demux_projection_base()?),
            media,
            limits,
            crate::compat::mov::cursor::DemuxProfile::NoParseNoFillin,
        )?;
        inner.allow_recovery_initial_nonkey();
        Ok(RecoveryIndexCursor {
            inner,
            unknown_access: self.sync_diagnostic.is_some(),
            source_timescale: header.source().timescale,
        })
    }
}

/// The first `stss` entry strict parsing rejects: one not after the entry before it, or
/// past the last sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncDiagnostic {
    /// 0-based row of the entry in `stss`.
    pub first_bad_row: u32,
    /// The entry's stated 1-based sample number.
    pub source_value: u32,
    /// Absolute file offset of the entry in `stss`.
    pub field_start: u64,
    /// Absolute file offset just past that 4-byte entry.
    pub field_end: u64,
    /// The error strict parsing reports for the entry.
    pub strict_error: Error,
}
/// A recovered declaration whose sync rows were rejected has unknown access.
fn recovery_access(mut source: Sample, unknown_access: bool) -> Sample {
    if unknown_access {
        source.access = Access::Unknown;
    }
    source
}
/// The declared samples of a recovered table, from
/// [`RunRecoveryTable::source_prefix_cursor`].
#[derive(Debug)]
pub struct RecoverySourceCursor<'a> {
    inner: SampleCursor<'a>,
    unknown_access: bool,
}
impl RecoverySourceCursor<'_> {
    /// The next declared sample, in decode order.
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<Next<Sample>> {
        match self.inner.poll(work)? {
            Next::Item(source) => Ok(Next::Item(recovery_access(source, self.unknown_access))),
            other => Ok(other),
        }
    }
}
/// One packet of a [`RecoveryPacketCursor`]: the declared sample and the packet FFmpeg
/// reports for it.
#[derive(Debug)]
pub struct RecoveryOccurrence {
    /// `track_ID` of the `tkhd` box.
    pub track_id: u32,
    /// The sample as the table declares it; its access is unknown when the sync table was
    /// rejected.
    pub source: Sample,
    /// The packet FFmpeg reports.
    pub demux: crate::compat::mov::view::PacketView,
    /// The `mdhd` timescale. Zero means `source`'s ticks cannot be converted to seconds,
    /// although the raw ticks are known.
    pub source_timescale: u32,
}
/// FFmpeg's packets for a [`RunRecoveryTable`], from [`RunRecoveryTable::packet_cursor`].
#[derive(Debug)]
pub struct RecoveryPacketCursor<'a> {
    inner: crate::compat::mov::cursor::PacketCursor<'a>,
    unknown_access: bool,
    source_timescale: u32,
}
impl RecoveryPacketCursor<'_> {
    /// Advances to the next packet; take it with [`take_occurrence`](Self::take_occurrence).
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<crate::CursorStatus> {
        self.inner.poll(work, cancel)
    }
    /// Takes the ready packet.
    pub fn take_occurrence(&mut self) -> Result<RecoveryOccurrence> {
        let row = self.inner.take_occurrence()?;
        Ok(RecoveryOccurrence {
            source: recovery_access(row.source, self.unknown_access),
            demux: row.demux,
            track_id: row.track_id,
            source_timescale: self.source_timescale,
        })
    }
}
/// One entry of FFmpeg's index of a [`RunRecoveryTable`], from
/// [`RunRecoveryTable::index_cursor`].
#[derive(Debug)]
pub struct RecoveryIndexEntry {
    /// 0-based position of the entry in the index.
    pub ordinal: u64,
    /// Absolute file offset of the packet's bytes.
    pub position: i64,
    /// The packet's decode time as FFmpeg's signed field holds it, in ticks of the demux
    /// time base.
    pub timestamp: i64,
    /// The entry's flags as FFmpeg's index reports them; 1 marks a key entry.
    pub flags: i32,
    /// Size of the packet in bytes.
    pub size: u32,
    /// Entries since the last key entry; 0 at a key entry and at the first entry.
    pub min_distance: i32,
    /// The sample as the table declares it; its access is unknown when the sync table was
    /// rejected.
    pub source: Sample,
    /// The `mdhd` timescale; zero when the header was recovered with a zero timescale.
    pub source_timescale: u32,
}
/// FFmpeg's index of a [`RunRecoveryTable`], one entry at a time, from
/// [`RunRecoveryTable::index_cursor`].
#[derive(Debug)]
pub struct RecoveryIndexCursor<'a> {
    inner: crate::compat::mov::index::IndexCursor<'a>,
    unknown_access: bool,
    source_timescale: u32,
}
impl RecoveryIndexCursor<'_> {
    /// Advances to the next entry; take it with [`take_entry`](Self::take_entry).
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<crate::CursorStatus> {
        self.inner.poll(work, cancel)
    }
    /// Takes the ready entry.
    pub fn take_entry(&mut self) -> Result<RecoveryIndexEntry> {
        let e = self.inner.take_entry()?;
        Ok(RecoveryIndexEntry {
            ordinal: e.ordinal,
            position: e.position,
            timestamp: e.timestamp,
            flags: e.flags,
            size: e.size,
            min_distance: e.min_distance,
            source: recovery_access(e.occurrence.source, self.unknown_access),
            source_timescale: self.source_timescale,
        })
    }
}
