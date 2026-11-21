//! Run recovery: sample tables whose timing or sync runs cover more samples
//! than the table holds, and the description and direct recovery variants.
use super::*;

mod description;
mod direct;
pub use description::*;
pub use direct::*;

/// Explicit opt-in to the observed reference MOV run-prefix operation. This does not
/// change source validity, Strict defaults, packet order or codec configuration.
#[derive(Clone, Copy, Debug)]
pub struct RunRecoveryProfile;
#[cfg(feature = "reference-trace")]
impl RunRecoveryProfile {
    pub const REFERENCE_WORKER_SHA256: &'static str =
        "7ff6742eaafa59884b6dc95f65c701c26b18975c265aa8401d61154eb2ef1204";
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunKind {
    Decode,
    Composition,
}
/// Original run coverage and the first offending source count field. Row zero
/// is the first encoded row. The full raw table, including unused runs, is owned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunCoverage {
    pub kind: RunKind,
    pub(crate) first_excess_row: u32,
    pub count_field_start: u64,
    pub count_field_end: u64,
    pub declared_samples: u64,
    pub bounded_samples: u64,
    pub surplus_samples: u64,
    pub(crate) strict_error: Error,
}
/// Separate bounded builder. Only the final positive STTS run may overcover;
/// CTTS may have an overcovering positive prefix and positive trailing runs.
/// All other validation/limits/pool identity checks are the Strict checks.
#[derive(Debug)]
pub struct RunRecoveryBuilder {
    inner: SampleTableBuilder,
}
impl RunRecoveryBuilder {
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
        inner.recover_sync = true;
        Ok(Self { inner })
    }
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<BuildStatus> {
        if let Some(error) = self.inner.failed {
            return Err(error);
        }
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            self.inner.failed = Some(Error::Cancelled);
            return Err(Error::Cancelled);
        }
        self.inner.poll(work)
    }
    pub fn finish(mut self) -> Result<RunRecoveryTable> {
        if let Some(error) = self.inner.failed {
            return Err(error);
        }
        if self.inner.stage != Stage::Ready {
            return Err(Error::Invalid("run recovery builder not ready"));
        }
        Ok(RunRecoveryTable {
            storage: self
                .inner
                .table
                .take()
                .ok_or(Error::Invalid("missing recovery storage"))?,
            coverage: self.inner.coverage,
            sync_diagnostic: self.inner.sync_diagnostic,
            initial_description: self.inner.initial_description,
        })
    }
}
/// This deliberately has no Deref/conversion/accessor to a Strict SampleTable.
/// Private storage is shared with the partially validated Strict builder; only
/// this labelled type can expose the bounded observations after a recovery.
#[derive(Debug)]
pub struct RunRecoveryTable {
    storage: SampleTable,
    coverage: [Option<RunCoverage>; 2],
    sync_diagnostic: Option<SyncDiagnostic>,
    initial_description: Option<InitialDescriptionDiagnostic>,
}
impl RunRecoveryTable {
    pub fn raw(&self) -> &BoxView {
        self.storage.raw()
    }
    pub fn descriptions(&self) -> &[SampleDescription] {
        self.storage.descriptions()
    }
    pub fn coverage(&self) -> &[Option<RunCoverage>; 2] {
        &self.coverage
    }
    /// Encoded facts for the first STSZ count samples. This is not a claim that
    /// the source table is valid. Coverage diagnostics must accompany the result.
    pub fn source_prefix_cursor(&self) -> RecoverySourceCursor<'_> {
        RecoverySourceCursor {
            inner: self.storage.cursor(),
            unknown_access: self.sync_diagnostic.is_some(),
        }
    }
    pub fn sync_diagnostic(&self) -> Option<SyncDiagnostic> {
        self.sync_diagnostic
    }
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
        let mut stream = crate::compat::mov::view::StreamView::with_projection_base(
            header.source(),
            entry,
            header.demux_projection_base()?,
        )?;
        if stream.codec == "rawvideo" {
            match major_brand {
                Some(brand) if brand == *b"qt  " => {}
                Some(_) => stream.vendor_id = None,
                None => return Err(Error::Unsupported("native major-brand context absent")),
            }
        }
        Ok(stream)
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
    /// Demux extraction is separate from source validity/clock knownness.
    pub fn packet_cursor<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::compat::mov::field_recovery::RecoveryTrackHeader,
        media: &'a [std::ops::Range<u64>],
        limits: crate::compat::mov::cursor::PacketLimits,
    ) -> Result<RecoveryPacketCursor<'a>> {
        if self.initial_description.is_some() {
            return Err(Error::Unsupported(
                "initial-description recovery requires explicit v2 cursor",
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
    pub fn index_cursor<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::compat::mov::field_recovery::RecoveryTrackHeader,
        media: &'a [std::ops::Range<u64>],
        limits: crate::compat::mov::index::IndexLimits,
    ) -> Result<RecoveryIndexCursor<'a>> {
        if self.initial_description.is_some() {
            return Err(Error::Unsupported(
                "initial-description recovery requires explicit v2 cursor",
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncDiagnostic {
    pub first_bad_row: u32,
    pub source_value: u32,
    pub(crate) field_start: u64,
    pub(crate) field_end: u64,
    pub(crate) strict_error: Error,
}
/// A recovered declaration whose sync rows were rejected has unknown access.
fn recovery_access(mut source: Sample, unknown_access: bool) -> Sample {
    if unknown_access {
        source.access = Access::Unknown;
    }
    source
}
#[derive(Debug)]
pub struct RecoverySourceCursor<'a> {
    inner: SampleCursor<'a>,
    unknown_access: bool,
}
impl RecoverySourceCursor<'_> {
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<SampleStep> {
        match self.inner.poll(work)? {
            SampleStep::Sample(source) => Ok(SampleStep::Sample(recovery_access(
                source,
                self.unknown_access,
            ))),
            other => Ok(other),
        }
    }
}
/// Demux ticks are observations; source_timescale zero means that conversion
/// to source seconds is invalid, even though the original raw ticks are known.
#[derive(Debug)]
pub struct RecoveryOccurrence {
    pub track_id: u32,
    pub source: Sample,
    pub demux: crate::compat::mov::view::PacketView,
    pub source_timescale: u32,
}
#[derive(Debug)]
pub struct RecoveryPacketCursor<'a> {
    inner: crate::compat::mov::cursor::PacketCursor<'a>,
    unknown_access: bool,
    source_timescale: u32,
}
impl RecoveryPacketCursor<'_> {
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<crate::compat::mov::edit_list::CursorStatus> {
        self.inner.poll(work, cancel)
    }
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
#[derive(Debug)]
pub struct RecoveryIndexEntry {
    pub(crate) ordinal: u64,
    pub(crate) position: i64,
    pub(crate) timestamp: i64,
    pub flags: i32,
    pub(crate) size: u32,
    pub min_distance: i32,
    pub source: Sample,
    pub source_timescale: u32,
}
#[derive(Debug)]
pub struct RecoveryIndexCursor<'a> {
    inner: crate::compat::mov::index::IndexCursor<'a>,
    unknown_access: bool,
    source_timescale: u32,
}
impl RecoveryIndexCursor<'_> {
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<crate::compat::mov::edit_list::CursorStatus> {
        self.inner.poll(work, cancel)
    }
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
