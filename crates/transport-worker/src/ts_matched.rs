//! Selected elementary packets for the transport seek and free-format comparisons.
use crate::runner::{Invocation, MAX_INPUT, Result, Totals, measure};
use crate::ts_input::{self, FIRST_SOURCE, Request};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use triergon_core::container::{
    Event, PacketInfo, RandomAccess, SeekMethod, SeekPolicy, SeekRequest, TrackId,
};
use triergon_core::{Budget, Error, InputWindow, Rounding, Step, Time, TimeBase, WorkBudget};
use triergon_harness_protocol::digest::{fold, fold_framed, signed};
use triergon_mpegts::{AccessUnit, Reader, ReaderLimits};

#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Seek,
    AudioSeek,
    FreeAudio,
}

impl Kind {
    pub(crate) fn for_mode(mode: &str) -> Option<Self> {
        [Self::Seek, Self::AudioSeek, Self::FreeAudio]
            .into_iter()
            .find(|kind| mode.starts_with(kind.prefix()))
    }

    const fn prefix(self) -> &'static str {
        match self {
            Self::Seek => "ts-seek-",
            Self::AudioSeek => "ts-audio-seek-",
            Self::FreeAudio => "ts-free-audio-",
        }
    }

    const fn operation(self) -> &'static str {
        match self {
            Self::Seek => "mpegts-previous-picture",
            Self::AudioSeek => "mpegts-previous-audio",
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
    kind: Kind,
    repeat: Option<Time>,
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
            descriptor_namespace: input.namespace,
            ..ReaderLimits::default()
        },
        32 * 1024,
        &[track],
    )?;
    if let Some(request) = seek {
        if repeat.is_some() {
            reader.request_seek_and_index(request, 4096)?;
        } else {
            reader.request_seek(request)?;
        }
    }
    let mut out = Totals::default();
    drain(&mut reader, input, track, kind, false, &mut out)?;
    if let (Some(mut request), Some(target)) = (seek, repeat) {
        request.target = target;
        reader.request_seek(request)?;
        drain(&mut reader, input, track, kind, true, &mut out)?;
    }
    Ok(out)
}

fn drain(
    reader: &mut Reader,
    input: &Request,
    track: TrackId,
    kind: Kind,
    indexed: bool,
    out: &mut Totals,
) -> Result<()> {
    let cancel = AtomicBool::new(false);
    let seek = !matches!(kind, Kind::FreeAudio);
    let start = out.packets;
    let mut landed = false;
    loop {
        match reader.step(&mut WorkBudget::new(64 * 1024), &cancel)? {
            Step::Ready(Event::Seek(landing)) => {
                if !seek || landed || (indexed && landing.method != SeekMethod::Index) {
                    return Err("unexpected transport seek result".into());
                }
                for time in [landing.requested, landing.landing, landing.discard_until] {
                    fold(&mut out.digest, signed(ticks(time)?));
                }
                landed = true;
            }
            Step::Ready(Event::Packet(packet)) => {
                if packet.track() != track || (seek && !landed) {
                    return Err("unexpected transport packet".into());
                }
                let missing = i64::MIN;
                fold(&mut out.digest, signed(packet.pts().unwrap_or(missing)));
                match packet.access_unit() {
                    AccessUnit::MpegAudio(frame) if !seek && frame.header().bitrate_kbps == 0 => {
                        fold(&mut out.digest, signed(packet.dts().unwrap_or(missing)));
                        fold(&mut out.digest, packet.elementary_range().start);
                        fold(&mut out.digest, u64::from(frame.header().sample_rate));
                        fold(
                            &mut out.digest,
                            u64::from(frame.header().samples_per_frame()),
                        );
                    }
                    AccessUnit::Video(_) if matches!(kind, Kind::Seek) => {
                        // DTS in the comparison is the effective decode clock.
                        // The source reader keeps a missing encoded DTS absent.
                        let decode_time = packet.dts().or(packet.pts()).unwrap_or(missing);
                        fold(&mut out.digest, signed(decode_time));
                        fold(
                            &mut out.digest,
                            u64::from(packet.random_access() == RandomAccess::Sync),
                        );
                    }
                    AccessUnit::Adts(_) | AccessUnit::Ac3(_) | AccessUnit::MpegAudio(_)
                        if matches!(kind, Kind::AudioSeek) =>
                    {
                        fold(
                            &mut out.digest,
                            signed(packet.dts().or(packet.pts()).unwrap_or(missing)),
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
            Step::End(_) if out.packets > start => return Ok(()),
            Step::End(_) => return Err("transport comparison delivered no packets".into()),
        }
    }
}

pub(crate) fn run(kind: Kind, args: &[String]) -> Result<()> {
    let invocation = match kind {
        Kind::Seek => Invocation::parse(
            args,
            kind.prefix(),
            " SIZE PROGRAM PID PTS90 POLICY [REPEAT_PTS90]",
            5..=6,
            false,
        )?,
        Kind::AudioSeek => Invocation::parse(
            args,
            kind.prefix(),
            " SIZE PROGRAM [--dvb|--atsc] PID PTS90 POLICY [REPEAT_PTS90]",
            5..=7,
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
    let has_namespace = matches!(kind, Kind::AudioSeek)
        && matches!(invocation.arguments[2].as_str(), "--dvb" | "--atsc");
    let track_index = 2 + usize::from(has_namespace);
    if matches!(kind, Kind::Seek | Kind::AudioSeek)
        && !(3..=4).contains(&(invocation.arguments.len() - track_index))
    {
        return Err("expected PID PTS90 POLICY [REPEAT_PTS90]".into());
    }
    let input = ts_input::parse(
        &pool,
        Path::new(invocation.path),
        &invocation.arguments[..track_index],
    )?;
    let track = TrackId(invocation.arguments[track_index].parse()?);
    let seek = match kind {
        Kind::Seek | Kind::AudioSeek => {
            let target = Time::new(
                i128::from(invocation.arguments[track_index + 1].parse::<i64>()?),
                90_000,
            )?;
            let policy = match invocation.arguments[track_index + 2].as_str() {
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
    let repeat = if seek.is_some() {
        invocation
            .arguments
            .get(track_index + 3)
            .map(|target| -> Result<Time> {
                Ok(Time::new(i128::from(target.parse::<i64>()?), 90_000)?)
            })
            .transpose()?
    } else {
        None
    };
    measure(&invocation, kind.operation(), Some(&pool), || {
        execute(&pool, &input, track, seek, kind, repeat)
    })
}
