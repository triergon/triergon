//! Selected elementary packets for the transport seek and free-format comparisons.
use crate::runner::{Invocation, MAX_INPUT, Result, Totals, measure};
use crate::ts_input::{self, FIRST_SOURCE, Request};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use triergon_core::container::{
    Event, PacketInfo, RandomAccess, SeekMethod, SeekPolicy, SeekRequest, TrackId,
};
use triergon_core::{
    Budget, Error, InputWindow, Rounding, SourceId, Step, Time, TimeBase, WorkBudget,
};
use triergon_harness_protocol::digest::{fold, fold_framed, signed};
use triergon_mpegts::{AccessUnit, Reader, ReaderLimits, SeekEntity};

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

const SECOND_SOURCE: SourceId = SourceId(2);

#[derive(Clone, Copy, Default)]
struct FollowUp {
    repeated: Option<Time>,
    split: Option<usize>,
}

fn execute(
    pool: &Budget,
    input: &Request,
    track: TrackId,
    seek: Option<SeekRequest>,
    kind: Kind,
    follow_up: FollowUp,
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
    let entities = follow_up.split.map(|offset| {
        [
            SeekEntity {
                source: FIRST_SOURCE,
                length: offset as u64,
            },
            SeekEntity {
                source: SECOND_SOURCE,
                length: (input.input.len() - offset) as u64,
            },
        ]
    });
    if let Some(request) = seek {
        match (entities.as_ref(), follow_up.repeated.is_some()) {
            (Some(parts), true) => {
                reader.request_seek_in_entities_and_index(request, parts, 4096)?
            }
            (Some(parts), false) => reader.request_seek_in_entities(request, parts)?,
            (None, true) => reader.request_seek_and_index(request, 4096)?,
            (None, false) => reader.request_seek(request)?,
        };
    }
    let mut out = Totals::default();
    drain(
        &mut reader,
        input,
        track,
        kind,
        follow_up.split,
        false,
        &mut out,
    )?;
    if let (Some(mut request), Some(target)) = (seek, follow_up.repeated) {
        request.target = target;
        if let Some(parts) = &entities {
            reader.request_seek_in_entities(request, parts)?;
        } else {
            reader.request_seek(request)?;
        }
        drain(
            &mut reader,
            input,
            track,
            kind,
            follow_up.split,
            true,
            &mut out,
        )?;
    }
    Ok(out)
}

fn drain(
    reader: &mut Reader,
    input: &Request,
    track: TrackId,
    kind: Kind,
    split: Option<usize>,
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
                    AccessUnit::Adts(_)
                    | AccessUnit::Loas(_)
                    | AccessUnit::Ac3(_)
                    | AccessUnit::MpegAudio(_)
                    | AccessUnit::Dts(_)
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
                let (start, end) = match (request.source, split) {
                    (FIRST_SOURCE, split) => (0, split.unwrap_or(input.input.len())),
                    (SECOND_SOURCE, Some(start)) => (start, input.input.len()),
                    _ => return Err("unexpected transport entity".into()),
                };
                let offset = start + usize::try_from(request.offset)?;
                if offset == end {
                    reader.finish(request.source, request.token)?;
                } else {
                    let end = end.min(offset + request.maximum);
                    let window = InputWindow::new(
                        request.source,
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
            " SIZE PROGRAM PID PTS90 POLICY [REPEAT_PTS90] [split=BYTES]",
            5..=7,
            false,
        )?,
        Kind::AudioSeek => Invocation::parse(
            args,
            kind.prefix(),
            " SIZE PROGRAM [--dvb|--atsc] PID PTS90 POLICY [REPEAT_PTS90] [split=BYTES]",
            5..=8,
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
        && !(3..=5).contains(&(invocation.arguments.len() - track_index))
    {
        return Err("expected PID PTS90 POLICY [REPEAT_PTS90] [split=BYTES]".into());
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
    let mut follow_up = FollowUp::default();
    if seek.is_some() {
        for argument in &invocation.arguments[track_index + 3..] {
            if let Some(offset) = argument.strip_prefix("split=") {
                let offset = offset.parse()?;
                if !(1..input.input.len()).contains(&offset) {
                    return Err("transport entity split outside input".into());
                }
                if follow_up.split.replace(offset).is_some() {
                    return Err("repeated transport split option".into());
                }
            } else {
                let target = Time::new(i128::from(argument.parse::<i64>()?), 90_000)?;
                if follow_up.repeated.replace(target).is_some() {
                    return Err("repeated transport seek target".into());
                }
            }
        }
    }
    measure(&invocation, kind.operation(), Some(&pool), || {
        execute(&pool, &input, track, seek, kind, follow_up)
    })
}
