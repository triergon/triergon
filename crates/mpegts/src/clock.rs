//! Bounded source-clock interpretation. `compat::pes` holds the separate demux view.
//! Raw33/None, source epochs and payloads remain authoritative. Source lifting
//! uses admitted same-clock bounds; the demux view uses its own fixed interval.
//! Independently authored from source facts, mathematics and the observed contract.
use triergon_core::{Error, Result, Time};

pub(crate) const PERIOD90: i128 = 1_i128 << 33;
const SCALE27: i128 = 300;
const HALF_PERIOD27: i128 = PERIOD90 * SCALE27 / 2;

/// Source clock identity; not an I/O token, parser generation or byte-run epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockKey {
    /// The transport stream ID from the PAT.
    pub tsid: u16,
    /// The program number whose PMT names the clock.
    pub program: u16,
    /// The 13-bit PID that carries the program's PCR.
    pub pcr_pid: u16,
    /// The source clock epoch: 0 at the start, advanced at each accepted PCR discontinuity and
    /// each explicit stream discontinuity.
    pub epoch: u64,
}

/// A clock sample that anchors source timestamps, unwrapped and exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceClockSample {
    /// A PTS-domain sample in 90 kHz ticks.
    Pts90(i128),
    /// A PCR-domain sample in 27 MHz ticks.
    Pcr27(i128),
}

/// Caller admits this source relationship/bound; no anchor comes from demux
/// peer output. PCR fractions remain exact rather than rounded to 90-kHz ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceClockAnchor {
    /// The clock the anchor belongs to; timestamps of another clock stay unresolved.
    pub key: ClockKey,
    /// The anchoring sample.
    pub sample: SourceClockSample,
    /// Inclusive distance bound, strictly below half the source clock period.
    pub maximum_distance27: u64,
}

/// How [`source_timestamp`] lifts a 33-bit timestamp to an unwrapped tick count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceTimePolicy {
    /// Leave every timestamp unresolved.
    Unanchored,
    /// Take the raw 33-bit value as the tick count, as if the clock started at epoch zero.
    RelativeEpochZero,
    /// Unwrap to the instant nearest the caller's anchor, within its bound.
    Bounded(SourceClockAnchor),
}

/// The outcome of interpreting one source timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceTimeStatus {
    /// The source carried no timestamp.
    Absent,
    /// The policy is [`SourceTimePolicy::Unanchored`], so no tick count is chosen.
    AnchorBoundMissing,
    /// The tick count is the raw value under [`SourceTimePolicy::RelativeEpochZero`].
    RelativeEpochChoice,
    /// The tick count is unwrapped relative to the anchor and within its bound.
    Anchored,
    /// The timestamp lies exactly half a clock period from the anchor, so neither unwrap is nearer.
    AmbiguousHalfPeriod,
    /// The anchor belongs to a different [`ClockKey`].
    AnchorIdentityMismatch,
    /// The nearest unwrap lies further from the anchor than its bound.
    OutsideAnchorBound,
}

/// One source timestamp with its interpretation; the raw value is always kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceTimestamp {
    /// The raw 33-bit PTS or DTS in 90 kHz ticks, `None` when the source carried none.
    pub raw33: Option<u64>,
    /// The unwrapped tick count in 90 kHz ticks, when `status` resolved one.
    pub ticks90: Option<i128>,
    /// `ticks90` as an exact time, when resolved.
    pub exact: Option<Time>,
    /// Why the timestamp is or is not resolved.
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

pub(crate) fn check_raw33(raw: u64) -> Result<()> {
    if i128::from(raw) >= PERIOD90 {
        return Err(Error::Invalid("timestamp raw33"));
    }
    Ok(())
}

/// Source interpretation; never mutates a timestamp/header or creates
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
