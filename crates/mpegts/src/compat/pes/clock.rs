//! The demuxer's clock, beside the source clock in [`crate::clock`].
//! Source raw33/None values, epochs and payloads remain authoritative. This view
//! uses its own fixed interval and never replaces a source timestamp.
use crate::SourceEpochs;
use crate::clock::{PERIOD90, check_raw33};
use crate::pes::{Pes, PesHeader};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};

/// The demuxer operation these views reproduce, distinct from the default
/// parser operation and from any diagnostic correct_ts_overflow=0 observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DemuxProfile {
    /// The demuxer's PES packet output, one packet per PES as it cuts them.
    PesPackets,
}
#[cfg(feature = "reference-trace")]
impl DemuxProfile {
    /// The profile's name in reference traces.
    pub fn label(self) -> &'static str {
        "demux-pes"
    }
    /// The revision of the reference demuxer this profile reproduces.
    pub fn reference_revision(self) -> &'static str {
        "d32b387f2b0a484599d4587d651891f0c63c4238"
    }
    /// SHA-256 digest, in hex, of the reference worker that recorded the profile's traces.
    pub fn worker_sha256(self) -> &'static str {
        "19c06e53c62852b409eec9771cb11d9d7f5164b4618545a449d7e993f99346df"
    }
    /// The operation name the profile is recorded under in comparison traces.
    pub fn operation(self) -> &'static str {
        "mpegts-demux-pes-observation"
    }
    /// The demuxer's format flags in the reference run.
    pub fn flags(self) -> u32 {
        48
    }
    /// Whether the reference run kept the demuxer's default timestamp overflow correction.
    pub fn default_overflow_correction(self) -> bool {
        true
    }
    /// Whether the reference run probed stream information before reading packets.
    pub fn find_stream_info(self) -> bool {
        false
    }
}

/// Where a placed timestamp came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimestampOrigin {
    /// The header carried no timestamp.
    Absent,
    /// The PES header's PTS.
    SourcePts,
    /// The PES header's DTS.
    SourceDts,
    /// The PES header's PTS, standing in for the DTS of a PTS-only header as the demuxer does.
    PtsOnlyCompatibility,
}

/// Why this separate view selected a representative. This is reference-context
/// state, never a source anchor or a clock/configuration/I/O epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockOrigin {
    /// The program's first 33-bit timestamp, in 90 kHz ticks, which opened the interval.
    pub first_reference_raw90: u64,
    /// Start of the 2^33-tick interval timestamps are placed in, in 90 kHz ticks; may be negative.
    pub interval_origin90: i128,
}

/// A separate view, not replacement packet fields or a source/demux waiver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DemuxTimeView {
    /// The demux profile the time was placed under.
    pub profile: DemuxProfile,
    /// The program number of the clock.
    pub program: u16,
    /// The 13-bit PID of the PES.
    pub pid: u16,
    /// The clock and configuration epochs of the PES.
    pub source_epochs: SourceEpochs,
    /// The PES header's raw 33-bit PTS, when present.
    pub source_pts_raw33: Option<u64>,
    /// The PES header's raw 33-bit DTS, when present.
    pub source_dts_raw33: Option<u64>,
    /// The PTS placed in the demuxer's interval, in 90 kHz ticks.
    pub pts90: Option<i128>,
    /// The DTS placed in the demuxer's interval, in 90 kHz ticks; the PTS when the header has no
    /// DTS.
    pub dts90: Option<i128>,
    /// Where `pts90` came from.
    pub pts_origin: TimestampOrigin,
    /// Where `dts90` came from.
    pub dts_origin: TimestampOrigin,
    /// The interval the timestamps were placed in; `None` until a timestamp opens one.
    pub origin: Option<ClockOrigin>,
}

/// Demux encoded-header time, without validated source timestamp fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodedTimeView {
    /// The PTS as encoded in the faulted header, 33 bits.
    pub encoded_pts_raw33: u64,
    /// Always `None`: an encoded DTS is not placed.
    pub encoded_dts_raw33: Option<u64>,
    /// The encoded PTS placed in the demuxer's interval, in 90 kHz ticks.
    pub pts90: i128,
    /// Equal to `pts90`, as for a PTS-only header.
    pub dts90: i128,
    /// The interval the PTS was placed in.
    pub origin: ClockOrigin,
}

/// The window of 2³³ ticks the demuxer places every timestamp of a program in,
/// opened by the program's first 33-bit timestamp `reference`.
///
/// FFmpeg's mpegts demuxer reports each later timestamp as the one value
/// congruent to it modulo 2³³ in the window, which starts 60 s (5,400,000
/// ticks) before `reference`, as observed with first timestamps from 0 to
/// 2³³ − 1 and followers on either side of them:
///
/// - below 5,400,000 the window starts at 0: after a first timestamp of 1 a
///   later 0 stays 0, and after a first 0 a later 2³³ − 1 stays
///   8,589,934,591;
/// - from 5,400,000 to 2³³ − 5,400,001 it starts 60 s earlier: with
///   5,400,001 first, a later 1 stays 1 and a later 0 becomes 2³³;
/// - from 2³³ − 5,400,000 up it starts 60 s before `reference` − 2³³, so the
///   first timestamp itself comes out negative: 2³³ − 5,400,000 as
///   −5,400,000 and 2³³ − 1 as −1, with a later 0 as 0.
fn initial_interval(reference: u64) -> ClockOrigin {
    const LEAD90: i128 = 60 * 90_000;
    let reference90 = i128::from(reference);
    let candidate = reference90 - LEAD90;
    let interval_origin90 = if reference90 >= PERIOD90 - LEAD90 {
        // Within 60 s of the wrap: the window takes the first timestamp
        // as negative, so that the wrapped ones after it continue from 0.
        candidate - PERIOD90
    } else {
        // Within 60 s of zero the window does not start below 0.
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
        .ok_or(Error::Invalid("demux clock origin missing"))?
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
    /// Optional encoded timing from finite source-fault recovery. No source
    /// timestamp or anchor is created, including for forbidden timing flags.
    pub fn project_pes_fault(
        &mut self,
        packet: &crate::compat::pes::fault::PesFaultPacket,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<DiagnosticTimeView>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if work.remaining() == 0 {
            return Ok(None);
        }
        if packet.program != self.program {
            return Err(Error::Invalid("transport fault clock program mismatch"));
        }
        self.project_optional_encoded(
            packet.pid,
            packet.encoded_header.encoded_pts_raw33(),
            work,
            cancel,
        )
    }
    /// Place encoded PTS from a faulted transport packet in this demux interval.
    /// Raw DTS stays absent; `dts90` is only the compatibility PTS-only result.
    /// This creates no source timestamp or clock anchor.
    pub fn project_transport_fault(
        &mut self,
        packet: &crate::compat::pes::fault::TransportFaultPacket,
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
        header: &crate::reference_trace::recovery::EncodedHeader,
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
            .ok_or(Error::Invalid("encoded demux timestamp missing"))?;
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
    /// A clock for `program` with no interval opened yet.
    ///
    /// Fails with `Invalid` for program 0.
    pub fn new(program: u16) -> Result<Self> {
        if program == 0 {
            return Err(Error::Invalid(
                "demux clock requires a nonzero program number",
            ));
        }
        Ok(Self {
            program,
            origin: None,
        })
    }
    /// The program number this clock places timestamps for.
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
            _ => {
                return Err(Error::Invalid(
                    "PES timestamps disagree with the header timing flags",
                ));
            }
        };
        let (next_origin, pts90, dts90) = self.placed(pts, dts)?;
        let result = DemuxTimeView {
            profile: DemuxProfile::PesPackets,
            program: self.program,
            pid,
            source_epochs: epochs,
            source_pts_raw33: pts,
            source_dts_raw33: dts,
            pts90,
            dts90,
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
    /// A packet's raw PTS and DTS in the demuxer's interval, the DTS falling
    /// back to the PTS, as [`Self::project_header`] places them.
    pub fn place(
        &mut self,
        pts: Option<u64>,
        dts: Option<u64>,
    ) -> Result<(Option<i128>, Option<i128>)> {
        for raw in [pts, dts].into_iter().flatten() {
            check_raw33(raw)?;
        }
        let (origin, pts90, dts90) = self.placed(pts, dts)?;
        self.origin = origin;
        Ok((pts90, dts90))
    }
    fn placed(
        &self,
        pts: Option<u64>,
        dts: Option<u64>,
    ) -> Result<(Option<ClockOrigin>, Option<i128>, Option<i128>)> {
        let dts = dts.or(pts);
        let origin = self.origin.or_else(|| dts.map(initial_interval));
        Ok((
            origin,
            representative(pts, origin)?,
            representative(dts, origin)?,
        ))
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiagnosticTimeView {
    /// The PTS as encoded in the rejected header, 33 bits, when present.
    pub encoded_pts_raw33: Option<u64>,
    /// Always `None`: an encoded DTS is not placed.
    pub encoded_dts_raw33: Option<u64>,
    /// The encoded PTS placed in the demuxer's interval, in 90 kHz ticks, when present.
    pub pts90: Option<i128>,
    /// Equal to `pts90`, as for a PTS-only header.
    pub dts90: Option<i128>,
    /// The interval in use; `None` until a timestamp opens one.
    pub origin: Option<ClockOrigin>,
}
impl DemuxClock {
    /// Places the optional encoded PTS of a rejected header in this clock's interval; `None` when
    /// `work` is spent.
    ///
    /// Fails with `Invalid` for a PID above 0x1FFE or a value wider than 33 bits, and with
    /// `Cancelled` when `cancel` is set.
    #[cfg(feature = "reference-trace")]
    pub fn project_diagnostic_header(
        &mut self,
        pid: u16,
        header: &crate::reference_trace::recovery::DiagnosticHeader,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<DiagnosticTimeView>> {
        self.project_optional_encoded(pid, header.encoded_pts_raw33(), work, cancel)
    }
    fn project_optional_encoded(
        &mut self,
        pid: u16,
        raw: Option<u64>,
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
