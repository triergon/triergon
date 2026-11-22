use std::io::Write;
use std::sync::atomic::AtomicBool;

use triergon_core::container::{
    Event, PacketInfo, RandomAccess, SeekMethod, SeekPolicy, SeekRequest, TrackId,
};
use triergon_core::{Budget, Bytes, Rounding, Step, Time, TimeBase, WorkBudget};
use triergon_harness_protocol::digest::{fold, fold_framed, signed};
use triergon_mpegts::{AccessUnit, PacketFormat, Reader, ReaderLimits, SourceEpochs};

use super::{CAPACITY, SOURCE, WINDOW, WORK, capabilities, explain, supply};
use crate::protocol::{Operation, Work};

pub(super) fn operation<'a>(input: &'a [u8], args: &[&str]) -> Result<Operation<'a>, String> {
    let [size, program, pid, target, policy, options @ ..] = args else {
        return Err("expected SIZE PROGRAM PID PTS90 POLICY [REPEAT_PTS90] [epoch=CLOCK:CONFIG epoch-start=BYTES]".into());
    };
    let format = match *size {
        "188" => PacketFormat::Ts188,
        "192" => PacketFormat::M2ts192,
        _ => return Err("expected TS188 or M2TS192".into()),
    };
    let policy = match *policy {
        "previous" => SeekPolicy::PreviousRandomAccess,
        "preroll" => SeekPolicy::PlaybackPreroll,
        _ => return Err("expected previous or preroll".into()),
    };
    let request = SeekRequest {
        track: TrackId(pid.parse().map_err(explain)?),
        target: time(target)?,
        policy,
    };
    let case = SeekCase::new(
        request,
        options,
        input.len(),
        size.parse().map_err(explain)?,
    )?;
    let limits = ReaderLimits {
        format,
        program: Some(program.parse().map_err(explain)?),
        ..ReaderLimits::default()
    };
    let pool = Budget::new(CAPACITY).map_err(explain)?;
    let bytes = Bytes::copy_from_slice(&pool, input).map_err(explain)?;
    Ok(Operation {
        name: "mpegts-previous-picture",
        run: Box::new(move |work, mut trace| {
            let mut reader = Reader::new_with_packet_tracks(
                &pool,
                SOURCE,
                capabilities(bytes.len(), true),
                limits,
                WINDOW,
                &[request.track],
            )
            .map_err(explain)?;
            case.run(&mut reader, &bytes, work, &mut trace)
        }),
    })
}

fn time(value: &str) -> Result<Time, String> {
    Time::new(i128::from(value.parse::<i64>().map_err(explain)?), 90_000).map_err(explain)
}

struct SeekCase {
    request: SeekRequest,
    epochs: SourceEpochs,
    repeat: Option<Time>,
}

impl SeekCase {
    fn new(
        request: SeekRequest,
        options: &[&str],
        length: usize,
        carrier: usize,
    ) -> Result<Self, String> {
        let (mut epochs, mut boundary, mut repeat) = (None, None, None);
        for option in options {
            if let Some(pair) = option.strip_prefix("epoch=") {
                let (clock, configuration) = pair.split_once(':').ok_or("expected CLOCK:CONFIG")?;
                if epochs
                    .replace(SourceEpochs {
                        clock: clock.parse().map_err(explain)?,
                        configuration: configuration.parse().map_err(explain)?,
                    })
                    .is_some()
                {
                    return Err("duplicate epoch".into());
                }
            } else if let Some(offset) = option.strip_prefix("epoch-start=") {
                let offset: usize = offset.parse().map_err(explain)?;
                if offset >= length
                    || !offset.is_multiple_of(carrier)
                    || boundary.replace(offset).is_some()
                {
                    return Err("invalid reference epoch boundary".into());
                }
            } else if repeat.replace(time(option)?).is_some() {
                return Err("duplicate repeat seek".into());
            }
        }
        if epochs.is_some() != boundary.is_some() {
            return Err("explicit epochs require an epoch-start reference boundary".into());
        }
        Ok(Self {
            request,
            epochs: epochs.unwrap_or(SourceEpochs {
                clock: 0,
                configuration: 2,
            }),
            repeat,
        })
    }

    fn run(
        &self,
        reader: &mut Reader,
        bytes: &Bytes,
        work: &mut Work,
        trace: &mut Option<&mut dyn Write>,
    ) -> Result<(), String> {
        reader
            .request_seek_in_epoch_and_index(self.request, self.epochs, 4096)
            .map_err(explain)?;
        self.drain(reader, bytes, work, trace, SeekMethod::BoundedScan)?;
        if let Some(target) = self.repeat {
            let request = SeekRequest {
                target,
                ..self.request
            };
            reader
                .request_seek_in_epoch(request, self.epochs)
                .map_err(explain)?;
            self.drain(reader, bytes, work, trace, SeekMethod::Index)?;
        }
        Ok(())
    }

    fn drain(
        &self,
        reader: &mut Reader,
        bytes: &Bytes,
        work: &mut Work,
        trace: &mut Option<&mut dyn Write>,
        method: SeekMethod,
    ) -> Result<(), String> {
        let cancel = AtomicBool::new(false);
        let start = work.packets;
        let mut landed = false;
        loop {
            match reader
                .step(&mut WorkBudget::new(WORK), &cancel)
                .map_err(explain)?
            {
                Step::Ready(Event::Seek(landing)) => {
                    if landed
                        || landing.method != method
                        || (method == SeekMethod::BoundedScan
                            && landing.scanned_bytes != bytes.len() as u64)
                    {
                        return Err(format!("expected {method:?}, observed {landing:?}"));
                    }
                    let mut times = [0; 3];
                    for (out, time) in times.iter_mut().zip([
                        landing.requested,
                        landing.landing,
                        landing.discard_until,
                    ]) {
                        *out = time
                            .to_i64_ticks(TimeBase::HZ_90000, Rounding::TowardZero)
                            .map_err(explain)?;
                        if Time::new(i128::from(*out), 90_000).map_err(explain)? != time {
                            return Err("seek time is outside the 90 kHz clock".into());
                        }
                        fold(&mut work.digest, signed(*out));
                    }
                    if let Some(trace) = trace {
                        writeln!(
                            trace,
                            "seek {} {} {} {}",
                            times[0], times[1], times[2], landing.scanned_bytes
                        )
                        .map_err(explain)?;
                    }
                    landed = true;
                }
                Step::Ready(Event::Packet(packet)) => {
                    if !landed
                        || packet.track() != self.request.track
                        || !matches!(packet.access_unit(), AccessUnit::Video(_))
                        || (work.packets == start && packet.epochs() != self.epochs)
                    {
                        return Err("unexpected seek output".into());
                    }
                    let pts = packet.pts().unwrap_or(i64::MIN);
                    let dts = packet.dts().unwrap_or(pts);
                    let key = u64::from(packet.random_access() == RandomAccess::Sync);
                    for word in [signed(pts), signed(dts), key] {
                        fold(&mut work.digest, word);
                    }
                    let data = packet.payload();
                    fold_framed(&mut work.digest, data);
                    work.packets += 1;
                    work.bytes += data.len() as u64;
                    if let Some(trace) = trace {
                        write!(trace, "packet {pts} {dts} {key} {} ", data.len())
                            .map_err(explain)?;
                        for byte in data {
                            write!(trace, "{byte:02x}").map_err(explain)?;
                        }
                        writeln!(trace).map_err(explain)?;
                    }
                }
                Step::NeedInput(request) => supply(reader, bytes, request).map_err(explain)?,
                Step::Ready(Event::Other(_)) | Step::Yield => {}
                Step::Backpressure(pressure) => {
                    return Err(explain(triergon_core::Error::Backpressure(pressure)));
                }
                Step::End(_) if work.packets > start => return Ok(()),
                Step::End(_) => return Err("seek ended without a video packet".into()),
            }
        }
    }
}
