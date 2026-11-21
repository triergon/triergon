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
use triergon_mpegts::{AccessUnit, Reader, ReaderLimits, SeekEntity, SourceEpochs};

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
    epochs: Option<SourceEpochs>,
}

fn options(kind: Kind, arguments: &[String], length: usize, carrier: usize) -> Result<FollowUp> {
    let mut follow_up = FollowUp::default();
    let mut epoch_start = None;
    for argument in arguments {
        if let Some(offset) = argument.strip_prefix("split=") {
            let offset = offset.parse()?;
            if !(1..length).contains(&offset) {
                return Err("transport entity split outside input".into());
            }
            if follow_up.split.replace(offset).is_some() {
                return Err("repeated transport split option".into());
            }
        } else if let Some(pair) = argument.strip_prefix("epoch=") {
            let (clock, configuration) =
                pair.split_once(':').ok_or("expected epoch=CLOCK:CONFIG")?;
            let epochs = SourceEpochs {
                clock: clock.parse()?,
                configuration: configuration.parse()?,
            };
            if follow_up.epochs.replace(epochs).is_some() {
                return Err("repeated transport epoch option".into());
            }
        } else if let Some(offset) = argument.strip_prefix("epoch-start=") {
            let offset: usize = offset.parse()?;
            if offset >= length || !offset.is_multiple_of(carrier) {
                return Err("transport epoch start must be a carrier boundary inside input".into());
            }
            if epoch_start.replace(offset).is_some() {
                return Err("repeated transport epoch start option".into());
            }
        } else {
            let target = Time::new(i128::from(argument.parse::<i64>()?), 90_000)?;
            if follow_up.repeated.replace(target).is_some() {
                return Err("repeated transport seek target".into());
            }
        }
    }
    if follow_up.epochs.is_some() != epoch_start.is_some()
        || (follow_up.epochs.is_some() && !matches!(kind, Kind::Seek))
    {
        return Err("epoch and epoch-start require a picture seek".into());
    }
    // The physical boundary identifies the reference control only. The reader
    // selects its landing solely with the complete source epoch pair.
    Ok(follow_up)
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
        if let Some(epochs) = follow_up.epochs {
            match (entities.as_ref(), follow_up.repeated.is_some()) {
                (Some(parts), true) => reader
                    .request_seek_in_entities_in_epoch_and_index(request, parts, epochs, 4096)?,
                (Some(parts), false) => {
                    reader.request_seek_in_entities_in_epoch(request, parts, epochs)?
                }
                (None, true) => reader.request_seek_in_epoch_and_index(request, epochs, 4096)?,
                (None, false) => reader.request_seek_in_epoch(request, epochs)?,
            };
        } else {
            match (entities.as_ref(), follow_up.repeated.is_some()) {
                (Some(parts), true) => {
                    reader.request_seek_in_entities_and_index(request, parts, 4096)?
                }
                (Some(parts), false) => reader.request_seek_in_entities(request, parts)?,
                (None, true) => reader.request_seek_and_index(request, 4096)?,
                (None, false) => reader.request_seek(request)?,
            };
        }
    }
    let mut out = Totals::default();
    drain(&mut reader, input, track, kind, follow_up, false, &mut out)?;
    if let (Some(mut request), Some(target)) = (seek, follow_up.repeated) {
        request.target = target;
        if let Some(epochs) = follow_up.epochs {
            if let Some(parts) = &entities {
                reader.request_seek_in_entities_in_epoch(request, parts, epochs)?;
            } else {
                reader.request_seek_in_epoch(request, epochs)?;
            }
        } else if let Some(parts) = &entities {
            reader.request_seek_in_entities(request, parts)?;
        } else {
            reader.request_seek(request)?;
        }
        drain(&mut reader, input, track, kind, follow_up, true, &mut out)?;
    }
    Ok(out)
}

fn drain(
    reader: &mut Reader,
    input: &Request,
    track: TrackId,
    kind: Kind,
    follow_up: FollowUp,
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
                if follow_up.epochs.is_some()
                    && !indexed
                    && (landing.method != SeekMethod::BoundedScan
                        || landing.scanned_bytes != input.input.len() as u64)
                {
                    return Err("explicit epoch seek did not scan the complete input".into());
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
                if out.packets == start
                    && follow_up
                        .epochs
                        .is_some_and(|epochs| packet.epochs() != epochs)
                {
                    return Err("transport landing belongs to another source epoch".into());
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
                    | AccessUnit::DtsExtension(_)
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
                let (start, end) = match (request.source, follow_up.split) {
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
                return Err(Error::Backpressure(pressure).into());
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
            " SIZE PROGRAM PID PTS90 POLICY [REPEAT_PTS90] [split=BYTES] | SIZE PROGRAM PID PTS90 POLICY epoch=CLOCK:CONFIG epoch-start=BYTES",
            5..=9,
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
        && !(3..=7).contains(&(invocation.arguments.len() - track_index))
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
    let follow_up = if seek.is_some() {
        options(
            kind,
            &invocation.arguments[track_index + 3..],
            input.input.len(),
            invocation.arguments[0].parse()?,
        )?
    } else {
        FollowUp::default()
    };
    measure(&invocation, kind.operation(), Some(&pool), || {
        execute(&pool, &input, track, seek, kind, follow_up)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(kind: Kind, args: &[&str]) -> Result<FollowUp> {
        options(
            kind,
            &args.iter().map(ToString::to_string).collect::<Vec<_>>(),
            1880,
            188,
        )
    }

    #[test]
    fn epoch_control_keeps_both_source_identifiers() {
        let parsed = parse(Kind::Seek, &["epoch=1:3", "epoch-start=564"]).unwrap();
        assert_eq!(
            parsed.epochs,
            Some(SourceEpochs {
                clock: 1,
                configuration: 3
            })
        );
        assert!(parsed.repeated.is_none());
        assert!(parsed.split.is_none());
        assert!(parse(Kind::Seek, &["epoch-start=564", "epoch=0:2"]).is_ok());
        assert!(parse(Kind::Seek, &["epoch=1:2", "epoch-start=564", "90000"]).is_ok());
        assert!(parse(Kind::Seek, &["epoch=1:2", "epoch-start=564", "split=564"]).is_ok());
        let combined = parse(
            Kind::Seek,
            &["epoch=1:2", "epoch-start=564", "split=752", "103000"],
        )
        .unwrap();
        assert_eq!(combined.split, Some(752));
        assert_eq!(combined.repeated, Some(Time::new(103000, 90000).unwrap()));
        assert_eq!(
            combined.epochs,
            Some(SourceEpochs {
                clock: 1,
                configuration: 2
            })
        );
    }

    #[test]
    fn incomplete_or_incompatible_epoch_controls_are_refused() {
        for args in [
            vec!["epoch=1:2"],
            vec!["epoch-start=564"],
            vec!["epoch=1", "epoch-start=564"],
            vec!["epoch=1:2:3", "epoch-start=564"],
            vec!["epoch=1:2", "epoch-start=565"],
            vec!["epoch=1:2", "epoch-start=1880"],
            vec!["epoch=1:2", "epoch-start=564", "epoch=1:2"],
            vec!["epoch=1:2", "epoch-start=564", "epoch-start=564"],
        ] {
            assert!(parse(Kind::Seek, &args).is_err(), "{args:?}");
        }
        assert!(parse(Kind::AudioSeek, &["epoch=1:2", "epoch-start=564"]).is_err());
    }

    #[test]
    fn repeated_audio_fixtures_read_only_the_physical_suffix() {
        for (name, header, length, step, first, repeated, width) in [
            (
                "mpeg1-layer1-physical-repeated-preroll",
                [0xff, 0xff, 0x14, 0xc0],
                32_usize,
                720,
                7,
                1,
                188,
            ),
            (
                "mpeg1-layer2-physical-repeated-preroll",
                [0xff, 0xfd, 0x14, 0xc0],
                96,
                2160,
                1,
                7,
                188,
            ),
            (
                "mpeg1-layer1-m2ts-multi-carrier-repeated-preroll",
                [0xff, 0xff, 0x94, 0xc0],
                288,
                720,
                7,
                1,
                192,
            ),
            (
                "mpeg1-layer2-m2ts-multi-carrier-repeated-preroll",
                [0xff, 0xfd, 0xe4, 0xc0],
                1152,
                2160,
                1,
                7,
                192,
            ),
        ] {
            let pool = Budget::new(2 << 20).unwrap();
            let extension = if width == 188 { "ts" } else { "m2ts" };
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
                "../../harness/features/mpegts-previous-audio/inputs/{name}.{extension}"
            ));
            let input = ts_input::parse(&pool, &path, &[width.to_string(), "7".into()]).unwrap();
            let mut caps = ts_input::capabilities(&input.input);
            caps.random_access = true;
            let track = TrackId(256);
            let mut reader = Reader::new_with_packet_tracks(
                &pool,
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
            )
            .unwrap();
            let mut request = SeekRequest {
                track,
                target: Time::new(90_001 + first * step, 90_000).unwrap(),
                policy: SeekPolicy::PlaybackPreroll,
            };
            reader.request_seek_and_index(request, 4096).unwrap();
            drain(
                &mut reader,
                &input,
                track,
                Kind::AudioSeek,
                FollowUp::default(),
                false,
                &mut Totals::default(),
            )
            .unwrap();

            request.target = Time::new(90_001 + repeated * step, 90_000).unwrap();
            let carriers = (length + 14).div_ceil(184);
            let floor = (17 + (2 + carriers as u64) * repeated as u64) * width;
            let start = reader.request_seek(request).unwrap();
            assert_eq!(start.offset, floor, "{name}: physical PAT offset");
            let mut next = floor;
            let mut landed = false;
            let mut index = repeated;
            let mut frame = vec![0; length];
            frame[..4].copy_from_slice(&header);
            let cancel = AtomicBool::new(false);
            loop {
                match reader.step(&mut WorkBudget::new(43), &cancel).unwrap() {
                    Step::NeedInput(read) => {
                        assert_eq!(read.source, FIRST_SOURCE);
                        assert!(read.offset >= floor, "{name}: read discarded prefix");
                        assert_eq!(read.offset, next, "{name}: contiguous suffix reads");
                        let offset = read.offset as usize;
                        if offset == input.input.len() {
                            reader.finish(read.source, read.token).unwrap();
                        } else {
                            let end = input.input.len().min(offset + read.maximum.min(7));
                            reader
                                .provide(
                                    &InputWindow::new(
                                        read.source,
                                        read.token,
                                        read.offset,
                                        input.input.slice(offset..end).unwrap(),
                                    )
                                    .unwrap(),
                                )
                                .unwrap();
                            next = end as u64;
                        }
                    }
                    Step::Ready(Event::Seek(result)) => {
                        assert!(!landed);
                        assert_eq!(result.method, SeekMethod::Index);
                        assert_eq!(result.scanned_bytes, 0);
                        assert!(!result.replays_from_start);
                        assert_eq!(result.requested, request.target);
                        assert_eq!(result.discard_until, request.target);
                        assert_eq!(
                            result.landing,
                            Time::new(90_000 + repeated * step, 90_000).unwrap()
                        );
                        landed = true;
                    }
                    Step::Ready(Event::Packet(packet)) => {
                        assert!(landed);
                        let pts = (90_000 + index * step) as i64;
                        assert_eq!(packet.track(), track);
                        assert_eq!(packet.pts(), Some(pts));
                        assert_eq!(packet.dts(), (index % 2 != 0).then_some(pts));
                        assert_eq!(packet.payload(), frame);
                        index += 1;
                    }
                    Step::Ready(Event::Other(_)) | Step::Yield => (),
                    Step::End(_) => break,
                    Step::Backpressure(pressure) => panic!("{pressure:?}"),
                }
            }
            assert!(landed);
            assert_eq!(index, 10, "{name}: every frame through EOF");
            assert_eq!(next, input.input.len() as u64);
        }
    }
}
