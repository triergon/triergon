//! Separate bounded source-clock interpretation and a pin-bound native PES view.
//! Raw33/None, source epochs and payloads remain authoritative. Source lifting
//! uses admitted same-clock bounds; the native view uses its own fixed interval.
//! Independently authored from source facts, mathematics and the observed contract.
use crate::{Pes, PesHeader, SourceEpochs};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, Time, WorkBudget};

const PERIOD90: i128 = 1_i128 << 33;
const SCALE27: i128 = 300;
const HALF_PERIOD27: i128 = PERIOD90 * SCALE27 / 2;

/// Source clock identity; not an I/O token, parser generation or byte-run epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockKey {
    pub tsid: u16,
    pub program: u16,
    pub pcr_pid: u16,
    pub epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceClockSample {
    Pts90(i128),
    Pcr27(i128),
}

/// Caller admits this source relationship/bound; no anchor comes from native
/// peer output. PCR fractions remain exact rather than rounded to 90-kHz ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceClockAnchor {
    pub key: ClockKey,
    pub sample: SourceClockSample,
    /// Inclusive distance bound, strictly below half the source clock period.
    pub maximum_distance27: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceTimePolicy {
    Unanchored,
    RelativeEpochZero,
    Bounded(SourceClockAnchor),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceTimeStatus {
    Absent,
    AnchorBoundMissing,
    RelativeEpochChoice,
    Anchored,
    AmbiguousHalfPeriod,
    AnchorIdentityMismatch,
    OutsideAnchorBound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceTimestamp {
    pub raw33: Option<u64>,
    pub ticks90: Option<i128>,
    pub exact: Option<Time>,
    pub status: SourceTimeStatus,
}

fn unresolved(raw33: Option<u64>, status: SourceTimeStatus) -> SourceTimestamp {
    SourceTimestamp {
        raw33,
        ticks90: None,
        exact: None,
        status,
    }
}

fn resolved(raw33: u64, ticks90: i128, status: SourceTimeStatus) -> Result<SourceTimestamp> {
    Ok(SourceTimestamp {
        raw33: Some(raw33),
        ticks90: Some(ticks90),
        exact: Some(Time::new(ticks90, 90_000)?),
        status,
    })
}

fn check_raw33(raw: u64) -> Result<()> {
    if i128::from(raw) >= PERIOD90 {
        return Err(Error::Invalid("timestamp raw33"));
    }
    Ok(())
}

/// Additive source interpretation; never mutates a timestamp/header or creates
/// a source DTS. Only already validated source fields are admissible inputs.
/// Actual output overflow is Error::Overflow; raw33 outside its width is Invalid.
pub fn source_timestamp(
    raw33: Option<u64>,
    key: ClockKey,
    policy: SourceTimePolicy,
) -> Result<SourceTimestamp> {
    let Some(raw) = raw33 else {
        return Ok(unresolved(None, SourceTimeStatus::Absent));
    };
    check_raw33(raw)?;
    let anchor = match policy {
        SourceTimePolicy::Unanchored => {
            return Ok(unresolved(raw33, SourceTimeStatus::AnchorBoundMissing));
        }
        SourceTimePolicy::RelativeEpochZero => {
            return resolved(raw, i128::from(raw), SourceTimeStatus::RelativeEpochChoice);
        }
        SourceTimePolicy::Bounded(anchor) => anchor,
    };
    if anchor.key != key {
        return Ok(unresolved(raw33, SourceTimeStatus::AnchorIdentityMismatch));
    }
    if i128::from(anchor.maximum_distance27) >= HALF_PERIOD27 {
        return Err(Error::Invalid("source clock anchor bound"));
    }
    // Only a small modular residue is multiplied by 300, never the potentially
    // extreme i128 source anchor. Euclidean remainder preserves PCR fractions.
    let (whole90, fraction27) = match anchor.sample {
        SourceClockSample::Pts90(ticks) => (ticks, 0),
        SourceClockSample::Pcr27(ticks) => (ticks.div_euclid(SCALE27), ticks.rem_euclid(SCALE27)),
    };
    let mut delta90 = i128::from(raw) - whole90.rem_euclid(PERIOD90);
    let distance27 = delta90 * SCALE27 - fraction27;
    if distance27.unsigned_abs() == HALF_PERIOD27 as u128 {
        return Ok(unresolved(raw33, SourceTimeStatus::AmbiguousHalfPeriod));
    }
    if distance27 > HALF_PERIOD27 {
        delta90 -= PERIOD90;
    } else if distance27 < -HALF_PERIOD27 {
        delta90 += PERIOD90;
    }
    let distance27 = delta90 * SCALE27 - fraction27;
    if distance27.unsigned_abs() > u128::from(anchor.maximum_distance27) {
        return Ok(unresolved(raw33, SourceTimeStatus::OutsideAnchorBound));
    }
    let ticks90 = whole90.checked_add(delta90).ok_or(Error::Overflow)?;
    resolved(raw, ticks90, SourceTimeStatus::Anchored)
}

/// Exact archived operation identity, distinct from the default-parser D0 ABI
/// operation and from any diagnostic correct_ts_overflow=0 observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeClockProfile {
    ArchivedPesV1,
}
impl NativeClockProfile {
    pub fn source_pin(self) -> &'static str {
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
pub enum NativeTimestampOrigin {
    Absent,
    SourcePts,
    SourceDts,
    PtsOnlyCompatibility,
}

/// Why this separate view selected a representative. This is reference-context
/// state, never a source anchor or a clock/configuration/I/O epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeClockOrigin {
    pub first_reference_raw90: u64,
    pub interval_origin90: i128,
}

/// A separate view, not replacement packet fields or a source/native waiver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativePesTimeView {
    pub profile: NativeClockProfile,
    pub program: u16,
    pub pid: u16,
    pub source_epochs: SourceEpochs,
    pub source_pts_raw33: Option<u64>,
    pub source_dts_raw33: Option<u64>,
    pub pts90: Option<i128>,
    pub dts90: Option<i128>,
    pub pts_origin: NativeTimestampOrigin,
    pub dts_origin: NativeTimestampOrigin,
    pub origin: Option<NativeClockOrigin>,
}

/// Native encoded-header time, without validated source timestamp fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeEncodedTimeView {
    pub encoded_pts_raw33: u64,
    pub encoded_dts_raw33: Option<u64>,
    pub pts90: i128,
    pub dts90: i128,
    pub origin: NativeClockOrigin,
}

fn initial_native_interval(reference: u64) -> NativeClockOrigin {
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
    NativeClockOrigin {
        first_reference_raw90: reference,
        interval_origin90,
    }
}

fn native_representative(
    raw: Option<u64>,
    origin: Option<NativeClockOrigin>,
) -> Result<Option<i128>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let start = origin
        .ok_or(Error::Invalid("native clock origin missing"))?
        .interval_origin90;
    // start is produced only by initial_native_interval and is bounded by two
    // periods. This is the unique congruent value in [start, start + PERIOD90).
    Ok(Some(start + (i128::from(raw) - start).rem_euclid(PERIOD90)))
}

/// One explicitly selected program, complete PES source order, one unchanged
/// reference context. Source epochs do not themselves reset a native context.
/// The caller supplies every accepted PES for this one program in source order;
/// program sharing, parser/AU output and seek/reset operations require their own
/// profile/evidence. This view allocates nothing and does not affect TransportStream.
#[derive(Debug)]
pub struct NativePesClockV1 {
    program: u16,
    origin: Option<NativeClockOrigin>,
}
impl NativePesClockV1 {
    /// Explicit compatibility input from checked encoding in a rejected carrier.
    /// Shares the native interval context, never creates a source PesHeader/time.
    pub fn project_encoded_header(
        &mut self,
        pid: u16,
        header: &crate::native_recovery::NativeEncodedHeader,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<NativeEncodedTimeView>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if !work.try_charge(1) {
            return Ok(None);
        }
        crate::checked_pid(pid)?;
        let raw = header.encoded_pts_raw33();
        check_raw33(raw)?;
        let origin = self.origin.unwrap_or_else(|| initial_native_interval(raw));
        let ticks = native_representative(Some(raw), Some(origin))?
            .ok_or(Error::Invalid("native encoded time missing"))?;
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        self.origin = Some(origin);
        Ok(Some(NativeEncodedTimeView {
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
    /// Time for an explicit native packet header, including a headerless
    /// continuation. None means that output has no applicable time header; its
    /// separate packet still retains the original causal source header/raw33.
    /// This is the same pin-bound time operation as complete-source-PES project.
    /// State commits only after a complete result and cancellation check.
    pub fn project_native_header(
        &mut self,
        pid: u16,
        epochs: SourceEpochs,
        header: Option<&PesHeader>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<NativePesTimeView>> {
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
            (0, None, None) => NativeTimestampOrigin::Absent,
            (2, Some(_), None) => NativeTimestampOrigin::PtsOnlyCompatibility,
            (3, Some(_), Some(_)) => NativeTimestampOrigin::SourceDts,
            _ => return Err(Error::Invalid("native clock source time flags")),
        };
        let native_dts = dts.or(pts);
        let next_origin = self
            .origin
            .or_else(|| native_dts.map(initial_native_interval));
        let result = NativePesTimeView {
            profile: NativeClockProfile::ArchivedPesV1,
            program: self.program,
            pid,
            source_epochs: epochs,
            source_pts_raw33: pts,
            source_dts_raw33: dts,
            pts90: native_representative(pts, next_origin)?,
            dts90: native_representative(native_dts, next_origin)?,
            pts_origin: if pts.is_some() {
                NativeTimestampOrigin::SourcePts
            } else {
                NativeTimestampOrigin::Absent
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
    /// A fresh object represents a fresh native context; source epochs do not reset it.
    pub fn project(
        &mut self,
        source: &Pes,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<NativePesTimeView>> {
        self.project_native_header(
            source.pid,
            source.epochs,
            Some(&source.header),
            work,
            cancel,
        )
    }
}

/// Optional encoded time for a rejected source header, never source PTS/DTS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeDiagnosticTimeView {
    pub encoded_pts_raw33: Option<u64>,
    pub encoded_dts_raw33: Option<u64>,
    pub pts90: Option<i128>,
    pub dts90: Option<i128>,
    pub origin: Option<NativeClockOrigin>,
}
impl NativePesClockV1 {
    pub fn project_diagnostic_header_v1(
        &mut self,
        pid: u16,
        header: &crate::native_recovery::NativeDiagnosticHeader,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<NativeDiagnosticTimeView>> {
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
        let next = self.origin.or_else(|| raw.map(initial_native_interval));
        let ticks = native_representative(raw, next)?;
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        self.origin = next;
        Ok(Some(NativeDiagnosticTimeView {
            encoded_pts_raw33: raw,
            encoded_dts_raw33: None,
            pts90: ticks,
            dts90: ticks,
            origin: next,
        }))
    }
}
