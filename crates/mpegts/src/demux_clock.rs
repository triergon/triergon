//! The demuxer's clock, beside the source clock in [`crate::clock`].
//! Source raw33/None values, epochs and payloads remain authoritative. This view
//! uses its own fixed interval and never replaces a source timestamp.
use crate::clock::{PERIOD90, check_raw33};
use crate::{Pes, PesHeader, SourceEpochs};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};

/// The demuxer operation these views reproduce, distinct from the default
/// parser operation and from any diagnostic correct_ts_overflow=0 observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DemuxProfile {
    PesPackets,
}
#[cfg(feature = "reference-trace")]
impl DemuxProfile {
    /// The profile's name in reference traces.
    pub fn label(self) -> &'static str {
        "ArchivedPesV1"
    }
    pub fn reference_revision(self) -> &'static str {
        "d32b387f2b0a484599d4587d651891f0c63c4238"
    }
    pub fn worker_sha256(self) -> &'static str {
        "19c06e53c62852b409eec9771cb11d9d7f5164b4618545a449d7e993f99346df"
    }
    pub fn operation(self) -> &'static str {
        "mpegts-native-pes-observation-v1"
    }
    pub fn flags(self) -> u32 {
        48
    }
    pub fn default_overflow_correction(self) -> bool {
        true
    }
    pub fn find_stream_info(self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimestampOrigin {
    Absent,
    SourcePts,
    SourceDts,
    PtsOnlyCompatibility,
}

/// Why this separate view selected a representative. This is reference-context
/// state, never a source anchor or a clock/configuration/I/O epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockOrigin {
    pub first_reference_raw90: u64,
    pub interval_origin90: i128,
}

/// A separate view, not replacement packet fields or a source/demux waiver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DemuxTimeView {
    pub profile: DemuxProfile,
    pub program: u16,
    pub pid: u16,
    pub source_epochs: SourceEpochs,
    pub source_pts_raw33: Option<u64>,
    pub source_dts_raw33: Option<u64>,
    pub pts90: Option<i128>,
    pub dts90: Option<i128>,
    pub pts_origin: TimestampOrigin,
    pub dts_origin: TimestampOrigin,
    pub origin: Option<ClockOrigin>,
}

/// Demux encoded-header time, without validated source timestamp fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodedTimeView {
    pub encoded_pts_raw33: u64,
    pub encoded_dts_raw33: Option<u64>,
    pub pts90: i128,
    pub dts90: i128,
    pub origin: ClockOrigin,
}

fn initial_interval(reference: u64) -> ClockOrigin {
    // The fixed 90-kHz behavioural profile admits one congruent representative
    // in an interval of width PERIOD90. Its 60-second lead is contract data.
    // At this timebase the near-end window is narrower than an eighth period.
    // No observed peer timestamp is an input to this interval construction.
    const LEAD90: i128 = 60 * 90_000;
    let reference90 = i128::from(reference);
    let candidate = reference90 - LEAD90;
    let interval_origin90 = if reference90 >= PERIOD90 - LEAD90 {
        candidate - PERIOD90
    } else {
        candidate.max(0)
    };
    ClockOrigin {
        first_reference_raw90: reference,
        interval_origin90,
    }
}

fn representative(raw: Option<u64>, origin: Option<ClockOrigin>) -> Result<Option<i128>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let start = origin
        .ok_or(Error::Invalid("native clock origin missing"))?
        .interval_origin90;
    // start is produced only by initial_interval and is bounded by two
    // periods. This is the unique congruent value in [start, start + PERIOD90).
    Ok(Some(start + (i128::from(raw) - start).rem_euclid(PERIOD90)))
}

/// One explicitly selected program, complete PES source order, one unchanged
/// reference context. Source epochs do not themselves reset a demux context.
/// The caller supplies every accepted PES for this one program in source order;
/// program sharing, parser/AU output and seek/reset operations require their own
/// profile/evidence. This view allocates nothing and does not affect TransportStream.
#[derive(Debug)]
pub struct DemuxClock {
    program: u16,
    origin: Option<ClockOrigin>,
}
impl DemuxClock {
    /// Place encoded PTS from a faulted transport packet in this demux interval.
    /// Raw DTS stays absent; `dts90` is only the compatibility PTS-only result.
    /// This creates no source timestamp or clock anchor.
    pub fn project_transport_fault(
        &mut self,
        packet: &crate::demux_transport::TransportFaultPacket,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<EncodedTimeView>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if work.remaining() == 0 {
            return Ok(None);
        }
        if packet.program != self.program {
            return Err(Error::Invalid("transport fault clock program mismatch"));
        }
        self.project_encoded_pts(
            packet.pid,
            packet.encoded_header.encoded_pts_raw33(),
            work,
            cancel,
        )
    }
    /// Explicit compatibility input from checked encoding in a rejected carrier.
    /// Shares the demux interval context, never creates a source PesHeader/time.
    #[cfg(feature = "reference-trace")]
    pub fn project_encoded_header(
        &mut self,
        pid: u16,
        header: &crate::demux_recovery::EncodedHeader,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<EncodedTimeView>> {
        self.project_encoded_pts(pid, header.encoded_pts_raw33(), work, cancel)
    }
    fn project_encoded_pts(
        &mut self,
        pid: u16,
        raw: u64,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<EncodedTimeView>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if !work.try_charge(1) {
            return Ok(None);
        }
        crate::checked_pid(pid)?;
        check_raw33(raw)?;
        let origin = self.origin.unwrap_or_else(|| initial_interval(raw));
        let ticks = representative(Some(raw), Some(origin))?
            .ok_or(Error::Invalid("native encoded time missing"))?;
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        self.origin = Some(origin);
        Ok(Some(EncodedTimeView {
            encoded_pts_raw33: raw,
            encoded_dts_raw33: None,
            pts90: ticks,
            dts90: ticks,
            origin,
        }))
    }
    pub fn new(program: u16) -> Result<Self> {
        if program == 0 {
            return Err(Error::Invalid("native clock selected program"));
        }
        Ok(Self {
            program,
            origin: None,
        })
    }
    pub fn selected_program(&self) -> u16 {
        self.program
    }
    /// Time for an explicit demux packet header, including a headerless
    /// continuation. None means that output has no applicable time header; its
    /// separate packet still retains the original causal source header/raw33.
    /// This is the same time operation as complete-source-PES project.
    /// State commits only after a complete result and cancellation check.
    pub fn project_header(
        &mut self,
        pid: u16,
        epochs: SourceEpochs,
        header: Option<&PesHeader>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<DemuxTimeView>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if !work.try_charge(1) {
            return Ok(None);
        }
        crate::checked_pid(pid)?;
        let pts = header.and_then(|value| value.pts_raw33);
        let dts = header.and_then(|value| value.dts_raw33);
        for raw in [pts, dts].into_iter().flatten() {
            check_raw33(raw)?;
        }
        let flags = header
            .and_then(|value| value.as_bytes().get(7))
            .map_or(0, |byte| byte >> 6);
        let dts_origin = match (flags, pts, dts) {
            (0, None, None) => TimestampOrigin::Absent,
            (2, Some(_), None) => TimestampOrigin::PtsOnlyCompatibility,
            (3, Some(_), Some(_)) => TimestampOrigin::SourceDts,
            _ => return Err(Error::Invalid("native clock source time flags")),
        };
        let demux_dts = dts.or(pts);
        let next_origin = self.origin.or_else(|| demux_dts.map(initial_interval));
        let result = DemuxTimeView {
            profile: DemuxProfile::PesPackets,
            program: self.program,
            pid,
            source_epochs: epochs,
            source_pts_raw33: pts,
            source_dts_raw33: dts,
            pts90: representative(pts, next_origin)?,
            dts90: representative(demux_dts, next_origin)?,
            pts_origin: if pts.is_some() {
                TimestampOrigin::SourcePts
            } else {
                TimestampOrigin::Absent
            },
            dts_origin,
            origin: next_origin,
        };
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        self.origin = next_origin;
        Ok(Some(result))
    }
    /// None is a work yield; timestamp absence is represented inside the result.
    /// A fresh object represents a fresh demux context; source epochs do not reset it.
    pub fn project(
        &mut self,
        source: &Pes,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<DemuxTimeView>> {
        self.project_header(
            source.pid,
            source.epochs,
            Some(&source.header),
            work,
            cancel,
        )
    }
}

/// Optional encoded time for a rejected source header, never source PTS/DTS.
#[cfg(feature = "reference-trace")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiagnosticTimeView {
    pub encoded_pts_raw33: Option<u64>,
    pub encoded_dts_raw33: Option<u64>,
    pub pts90: Option<i128>,
    pub dts90: Option<i128>,
    pub origin: Option<ClockOrigin>,
}
#[cfg(feature = "reference-trace")]
impl DemuxClock {
    pub fn project_diagnostic_header(
        &mut self,
        pid: u16,
        header: &crate::demux_recovery::DiagnosticHeader,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<DiagnosticTimeView>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if !work.try_charge(1) {
            return Ok(None);
        }
        crate::checked_pid(pid)?;
        let raw = header.encoded_pts_raw33();
        if let Some(raw) = raw {
            check_raw33(raw)?;
        }
        let next = self.origin.or_else(|| raw.map(initial_interval));
        let ticks = representative(raw, next)?;
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        self.origin = next;
        Ok(Some(DiagnosticTimeView {
            encoded_pts_raw33: raw,
            encoded_dts_raw33: None,
            pts90: ticks,
            dts90: ticks,
            origin: next,
        }))
    }
}
