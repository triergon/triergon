use std::ffi::OsString;
use std::io::Write;
use std::sync::atomic::AtomicBool;

use triergon_core::container::{Event, PacketInfo};
use triergon_core::{
    Budget, Bytes, InputRequest, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_mpegts::clock::{
    ClockKey, SourceClockAnchor, SourceClockSample, SourceTimePolicy, SourceTimeStatus,
    SourceTimestamp, source_timestamp,
};
use triergon_mpegts::compat::annex_b::VideoEofPolicy;
use triergon_mpegts::stream::TraceEvent;
use triergon_mpegts::{Reader, ReaderEvent, ReaderLimits};

use super::protocol::{Operation, Work};

mod seek;
mod views;

const SOURCE: SourceId = SourceId(1);
const WINDOW: usize = 32 * 1024;
const WORK: u64 = 64 * 1024;
const CAPACITY: usize = 128 * 1024 * 1024;

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    let args = arguments
        .iter()
        .map(|arg| arg.to_str().ok_or("expected UTF-8 arguments"))
        .collect::<Result<Vec<_>, _>>()?;
    match prefix {
        "ts-bounded-time-" => bounded_time(input, &args),
        "ts-epoch-index-seek-" => seek::operation(input, &args),
        "ts-reader-views-" | "ts-reader-container-" => views::operation(prefix, input, &args),
        _ => Err(format!("unimplemented transport reader view {prefix}")),
    }
}

pub(super) fn explain(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn capabilities(len: usize, random_access: bool) -> SourceCapabilities {
    SourceCapabilities {
        length: Some(len as u64),
        random_access,
    }
}

fn supply(reader: &mut Reader, bytes: &Bytes, request: InputRequest) -> triergon_core::Result<()> {
    let offset = usize::try_from(request.offset).map_err(|_| triergon_core::Error::Overflow)?;
    if offset == bytes.len() {
        reader.finish(request.source, request.token)
    } else {
        let data = bytes.slice(offset..bytes.len().min(offset.saturating_add(request.maximum)))?;
        reader.provide(&InputWindow::new(
            request.source,
            request.token,
            request.offset,
            data,
        )?)
    }
}

struct ClockCase {
    anchor: SourceClockAnchor,
    pid: u16,
    pes: bool,
    reference: bool,
}

fn bounded_time<'a>(input: &'a [u8], args: &[&str]) -> Result<Operation<'a>, String> {
    let [
        mode,
        pid,
        tsid,
        program,
        pcr_pid,
        epoch,
        unit,
        sample,
        distance,
    ] = args
    else {
        return Err("expected MODE PID TSID PROGRAM PCR_PID EPOCH UNIT SAMPLE DISTANCE27".into());
    };
    let (pes, reference) = match *mode {
        "pes" => (true, false),
        "au" => (false, false),
        "ref-pes" => (true, true),
        "ref-au" => (false, true),
        _ => return Err("expected pes, au, ref-pes or ref-au".into()),
    };
    let sample = sample.parse().map_err(explain)?;
    let sample = match *unit {
        "pts90" => SourceClockSample::Pts90(sample),
        "pcr27" => SourceClockSample::Pcr27(sample),
        _ => return Err("expected pts90 or pcr27".into()),
    };
    let case = ClockCase {
        anchor: SourceClockAnchor {
            key: ClockKey {
                tsid: tsid.parse().map_err(explain)?,
                program: program.parse().map_err(explain)?,
                pcr_pid: pcr_pid.parse().map_err(explain)?,
                epoch: epoch.parse().map_err(explain)?,
            },
            sample,
            maximum_distance27: distance.parse().map_err(explain)?,
        },
        pid: pid.parse().map_err(explain)?,
        pes,
        reference,
    };
    let pool = Budget::new(CAPACITY).map_err(explain)?;
    let bytes = Bytes::copy_from_slice(&pool, input).map_err(explain)?;
    Ok(Operation {
        name: "ts-bounded-time",
        run: Box::new(move |work, trace| case.run(&pool, &bytes, work, trace)),
    })
}

impl ClockCase {
    fn run(
        &self,
        pool: &Budget,
        bytes: &Bytes,
        work: &mut Work,
        mut trace: Option<&mut dyn Write>,
    ) -> Result<(), String> {
        let policy = SourceTimePolicy::Bounded(self.anchor);
        let limits = ReaderLimits {
            program: Some(self.anchor.key.program),
            time_policy: policy,
            video_eof_policy: VideoEofPolicy::ReportUnframed,
            ..ReaderLimits::default()
        };
        let mut reader = Reader::new(
            pool,
            SOURCE,
            capabilities(bytes.len(), false),
            limits,
            WINDOW,
        )
        .map_err(explain)?;
        let cancel = AtomicBool::new(false);
        let (mut tsid, mut pcr_pid) = (None, None);
        loop {
            match reader
                .step(&mut WorkBudget::new(WORK), &cancel)
                .map_err(explain)?
            {
                Step::NeedInput(request) => supply(&mut reader, bytes, request).map_err(explain)?,
                Step::Ready(Event::Packet(packet))
                    if !self.pes && packet.track().0 == u64::from(self.pid) =>
                {
                    let clock = packet.clock().ok_or("access unit has no source clock")?;
                    self.record(work, &mut trace, 0, clock.pts)?;
                    self.record(work, &mut trace, 1, clock.dts)?;
                }
                Step::Ready(Event::Other(ReaderEvent::Source(source))) if self.pes => {
                    match source.event() {
                        TraceEvent::Pat(pat) => tsid = Some(pat.header.extension),
                        TraceEvent::ProgramMap {
                            map, active: true, ..
                        } => pcr_pid = Some(map.pcr_pid),
                        TraceEvent::Pes(pes) if pes.pid == self.pid => {
                            let key = ClockKey {
                                tsid: tsid.ok_or("PES has no active PAT")?,
                                program: limits.program.ok_or("no selected program")?,
                                pcr_pid: pcr_pid.ok_or("PES has no active PMT")?,
                                epoch: pes.epochs.clock,
                            };
                            for (field, raw) in [pes.header.pts_raw33, pes.header.dts_raw33]
                                .into_iter()
                                .enumerate()
                            {
                                let stamp = source_timestamp(raw, key, policy).map_err(explain)?;
                                self.record(work, &mut trace, field as u64, stamp)?;
                            }
                        }
                        _ => {}
                    }
                }
                Step::Yield | Step::Ready(_) => {}
                Step::Backpressure(pressure) => {
                    return Err(explain(triergon_core::Error::Backpressure(pressure)));
                }
                Step::End(_) => return row(work, &mut trace, 7, &[], &[]),
            }
        }
    }

    fn record(
        &self,
        work: &mut Work,
        trace: &mut Option<&mut dyn Write>,
        field: u64,
        stamp: SourceTimestamp,
    ) -> Result<(), String> {
        let pid = u64::from(self.pid);
        if self.reference {
            if self.pes && field == 1 {
                return Ok(());
            }
            let ticks = stamp
                .ticks90
                .ok_or("reference control requires a resolved clock")?;
            let ticks = i64::try_from(ticks).map_err(explain)?;
            return row(work, trace, 3, &[pid, field, ticks as u64], &[]);
        }
        let status = match stamp.status {
            SourceTimeStatus::Absent => "absent",
            SourceTimeStatus::Anchored => "anchored",
            SourceTimeStatus::AmbiguousHalfPeriod => "ambiguous",
            SourceTimeStatus::AnchorIdentityMismatch => "identity",
            SourceTimeStatus::OutsideAnchorBound => "outside",
            _ => return Err("bounded source clock returned an unbounded status".into()),
        };
        let kind = if matches!(
            stamp.status,
            SourceTimeStatus::Absent | SourceTimeStatus::Anchored
        ) {
            3
        } else {
            8
        };
        let exact = stamp.exact.map_or_else(
            || "-".into(),
            |time| format!("{}/{}", time.numerator(), time.denominator()),
        );
        let raw = stamp
            .raw33
            .map_or_else(|| "-".into(), |value| value.to_string());
        let ticks = stamp
            .ticks90
            .map_or_else(|| "-".into(), |value| value.to_string());
        let text = format!("{pid} {field} {status} {raw} {ticks} {exact}");
        work.record(kind, &[text.len() as u64], text.as_bytes());
        if let Some(trace) = trace {
            writeln!(trace, "{kind} {text}").map_err(explain)?;
        }
        Ok(())
    }
}

pub(super) fn row(
    work: &mut Work,
    trace: &mut Option<&mut dyn Write>,
    kind: u64,
    words: &[u64],
    data: &[u8],
) -> Result<(), String> {
    work.record(kind, words, data);
    if let Some(trace) = trace {
        write!(trace, "{kind}").map_err(explain)?;
        for word in words {
            write!(trace, " {word}").map_err(explain)?;
        }
        if !data.is_empty() {
            write!(trace, " ").map_err(explain)?;
            for byte in data {
                write!(trace, "{byte:02x}").map_err(explain)?;
            }
        }
        writeln!(trace).map_err(explain)?;
    }
    Ok(())
}
