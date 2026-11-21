//! Selected elementary packets for the transport seek and free-format comparisons.
use crate::runner::{Invocation, MAX_INPUT, Result, Totals, measure};
use crate::ts_input::{self, FIRST_SOURCE, Request};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use triergon_core::container::{Event, PacketInfo, RandomAccess, SeekPolicy, SeekRequest, TrackId};
use triergon_core::{Budget, Error, InputWindow, Rounding, Step, Time, TimeBase, WorkBudget};
use triergon_harness_protocol::digest::{fold, fold_framed, signed};
use triergon_mpegts::{AccessUnit, Reader, ReaderLimits};

#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Seek,
    FreeAudio,
}

impl Kind {
    pub(crate) fn for_mode(mode: &str) -> Option<Self> {
        [Self::Seek, Self::FreeAudio]
            .into_iter()
            .find(|kind| mode.starts_with(kind.prefix()))
    }

    const fn prefix(self) -> &'static str {
        match self {
            Self::Seek => "ts-seek-",
            Self::FreeAudio => "ts-free-audio-",
        }
    }

    const fn operation(self) -> &'static str {
        match self {
            Self::Seek => "mpegts-previous-picture",
            Self::FreeAudio => "mpegts-free-format-frames",
        }
    }
}

fn ticks(time: Time) -> Result<i64> {
    let ticks = time.to_ticks(TimeBase::new(1, 90_000)?, Rounding::TowardZero)?;
    if Time::new(ticks, 90_000)? != time {
        return Err("transport seek time outside the 90 kHz clock".into());
    }
    Ok(i64::try_from(ticks)?)
}

fn execute(
    pool: &Budget,
    input: &Request,
    track: TrackId,
    seek: Option<SeekRequest>,
) -> Result<Totals> {
    let mut caps = ts_input::capabilities(&input.input);
    caps.random_access = true;
    let mut reader = Reader::new_with_packet_tracks(
        pool,
        FIRST_SOURCE,
        caps,
        ReaderLimits {
            format: input.format,
            program: Some(input.program),
            ..ReaderLimits::default()
        },
        32 * 1024,
        &[track],
    )?;
    if let Some(request) = seek {
        reader.request_seek(request)?;
    }
    let cancel = AtomicBool::new(false);
    let mut out = Totals::default();
    let mut landed = false;
    loop {
        match reader.step(&mut WorkBudget::new(64 * 1024), &cancel)? {
            Step::Ready(Event::Seek(landing)) => {
                if seek.is_none() || landed {
                    return Err("unexpected transport seek result".into());
                }
                for time in [landing.requested, landing.landing, landing.discard_until] {
                    fold(&mut out.digest, signed(ticks(time)?));
                }
                landed = true;
            }
            Step::Ready(Event::Packet(packet)) => {
                if packet.track() != track || (seek.is_some() && !landed) {
                    return Err("unexpected transport packet".into());
                }
                let missing = i64::MIN;
                fold(&mut out.digest, signed(packet.pts().unwrap_or(missing)));
                match packet.access_unit() {
                    AccessUnit::MpegAudio(frame)
                        if seek.is_none() && frame.header().bitrate_kbps == 0 =>
                    {
                        fold(&mut out.digest, signed(packet.dts().unwrap_or(missing)));
                        fold(&mut out.digest, packet.elementary_range().start);
                        fold(&mut out.digest, u64::from(frame.header().sample_rate));
                        fold(
                            &mut out.digest,
                            u64::from(frame.header().samples_per_frame()),
                        );
                    }
                    AccessUnit::Video(_) if seek.is_some() => {
                        // DTS in the comparison is the effective decode clock.
                        // The source reader keeps a missing encoded DTS absent.
                        let decode_time = packet.dts().or(packet.pts()).unwrap_or(missing);
                        fold(&mut out.digest, signed(decode_time));
                        fold(
                            &mut out.digest,
                            u64::from(packet.random_access() == RandomAccess::Sync),
                        );
                    }
                    _ => return Err("unexpected transport carriage".into()),
                }
                fold_framed(&mut out.digest, packet.payload());
                out.packets += 1;
                out.bytes += packet.payload().len() as u64;
            }
            Step::NeedInput(request) => {
                let offset = usize::try_from(request.offset)?;
                if offset == input.input.len() {
                    reader.finish(FIRST_SOURCE, request.token)?;
                } else {
                    let end = input.input.len().min(offset + request.maximum);
                    let window = InputWindow::new(
                        FIRST_SOURCE,
                        request.token,
                        request.offset,
                        input.input.slice(offset..end)?,
                    )?;
                    reader.provide(&window)?;
                }
            }
            Step::Ready(Event::Other(_)) | Step::Yield => (),
            Step::Backpressure(pressure) => {
                return Err(Error::Backpressure {
                    requested: pressure.requested,
                    available: pressure.available,
                }
                .into());
            }
            Step::End(_) if out.packets != 0 => return Ok(out),
            Step::End(_) => return Err("transport comparison delivered no packets".into()),
        }
    }
}

pub(crate) fn run(kind: Kind, args: &[String]) -> Result<()> {
    let invocation = match kind {
        Kind::Seek => Invocation::parse(
            args,
            kind.prefix(),
            " SIZE PROGRAM PID PTS90 POLICY",
            5..=5,
            false,
        )?,
        Kind::FreeAudio => Invocation::parse(
            args,
            kind.prefix(),
            " SIZE PROGRAM PID [layer1-padding]",
            3..=4,
            false,
        )?,
    };
    let pool = Budget::new(usize::try_from(MAX_INPUT)? + (64 << 20))?;
    let input = ts_input::parse(
        &pool,
        Path::new(invocation.path),
        &invocation.arguments[..2],
    )?;
    let track = TrackId(invocation.arguments[2].parse()?);
    let seek = match kind {
        Kind::Seek => {
            let target = Time::new(i128::from(invocation.arguments[3].parse::<i64>()?), 90_000)?;
            let policy = match invocation.arguments[4].as_str() {
                "previous" => SeekPolicy::PreviousRandomAccess,
                "preroll" => SeekPolicy::PlaybackPreroll,
                _ => return Err("expected previous or preroll".into()),
            };
            Some(SeekRequest {
                track,
                target,
                policy,
            })
        }
        Kind::FreeAudio => {
            if invocation
                .arguments
                .get(3)
                .is_some_and(|option| option != "layer1-padding")
            {
                return Err("expected layer1-padding".into());
            }
            None
        }
    };
    measure(&invocation, kind.operation(), Some(&pool), || {
        execute(&pool, &input, track, seek)
    })
}
