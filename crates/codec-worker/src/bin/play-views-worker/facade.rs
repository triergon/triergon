mod probe;

use std::ffi::OsString;
use std::io::{Cursor, Write};

use triergon::mpegts::{AccessUnit, Packet};
use triergon::{
    AnyPacket, AnyReader, Blocking, Budget, ContainerReader, Event, PacketInfo, Probe,
    RandomAccess, Rounding, SeekPolicy, SeekRequest, SourceCapabilities, SourceId, Time, TimeBase,
    TrackId,
};
use triergon_harness_protocol::digest::{fold, fold_bytes, fold_framed, signed};

use super::protocol::{Operation, Work};

fn reader(input: &[u8], pool: &Budget, format: Probe) -> Result<AnyReader, String> {
    let capabilities = SourceCapabilities {
        length: Some(input.len() as u64),
        random_access: true,
    };
    let reader = AnyReader::open(input, pool, SourceId(1), capabilities)
        .map_err(|error| error.to_string())?;
    if triergon::probe(input) != format {
        return Err("the requested format differs from the facade probe".into());
    }
    Ok(reader)
}

fn transport_size(text: &str) -> Result<(u64, Probe), String> {
    match text {
        "188" => Ok((188, Probe::Ts)),
        "192" => Ok((192, Probe::M2ts)),
        _ => Err("expected transport carrier size 188 or 192".into()),
    }
}

fn native_clocks(packet: &impl PacketInfo) -> [u64; 3] {
    [
        signed(packet.pts().unwrap_or(i64::MIN)),
        signed(packet.dts().unwrap_or(i64::MIN)),
        signed(packet.duration().unwrap_or(0)),
    ]
}

fn access_units<'a>(input: &'a [u8], args: &[&str]) -> Result<Operation<'a>, String> {
    let [size, program] = args else {
        return Err("expected SIZE PROGRAM for a single-program default reader".into());
    };
    let (size, format) = transport_size(size)?;
    let program: u16 = program.parse().map_err(|_| "invalid program")?;
    let pool = Budget::new(256 << 20).map_err(|error| error.to_string())?;
    Ok(Operation {
        name: "mpegts-access-unit-packets",
        run: Box::new(move |work, mut trace| {
            let reader = reader(input, &pool, format)?;
            let mut driver = Blocking::seekable(reader, Cursor::new(input), &pool);
            while let Some(packet) = driver.next_packet().map_err(|error| error.to_string())? {
                let AnyPacket::MpegTs(packet) = packet else {
                    return Err("the facade returned a non-TS packet".into());
                };
                if packet.track_info().program() != program {
                    return Err("the facade selected a different transport program".into());
                }
                let track = packet.track();
                let reader = driver.reader();
                let index = (0..reader.track_count())
                    .find(|&i| reader.track(i).is_some_and(|item| item.id() == track))
                    .ok_or("packet track is absent")?;
                ts_packet(&packet, size, index, work, &mut trace)?;
            }
            Ok(())
        }),
    })
}

fn ts_packet(
    packet: &Packet,
    size: u64,
    index: usize,
    work: &mut Work,
    trace: &mut Option<&mut dyn Write>,
) -> Result<(), String> {
    let source = packet.sources().first().ok_or("packet has no PES source")?;
    let pes = source.pes.event().pes_source().ok_or("packet has no PES")?;
    let pts_source = packet
        .clock()
        .and_then(|clock| clock.pes.event().pes_source());
    let (key, position) = match packet.access_unit() {
        AccessUnit::MpegAudio(_) | AccessUnit::Dts(_) => (true, pts_source),
        AccessUnit::DtsExtension(_) => (false, Some(pes)),
        AccessUnit::DvbSubtitle(_) => (true, Some(pes)),
        unit => (
            match unit {
                AccessUnit::Video(video) => video.random_access,
                AccessUnit::Loas(_) => false,
                _ => true,
            },
            (source.payload_range.start == 0).then_some(pes),
        ),
    };
    let position = position
        .and_then(|pes| pes.header_source_ranges.as_slice().first())
        .map_or(-1, |range| (range.offset / size * size) as i64);
    let [pts, dts, duration] = native_clocks(packet);
    let payload = packet.payload();
    let fields = [
        index as u64,
        u64::from(pes.pid),
        pts,
        dts,
        duration,
        signed(position),
        u64::from(key),
        payload.len() as u64,
    ];
    for value in fields {
        fold(&mut work.digest, value);
    }
    fold_bytes(&mut work.digest, payload);
    for value in [1, 19, 1, u64::from(pes.header.stream_id)] {
        fold(&mut work.digest, value);
    }
    work.packets += 1;
    work.bytes += payload.len() as u64;
    if let Some(trace) = trace {
        writeln!(
            trace,
            "{index} {} {} {} {} {position} {} {} {} {}",
            pes.pid,
            pts as i64,
            dts as i64,
            duration as i64,
            u8::from(key),
            payload.len(),
            pes.header.stream_id,
            triergon_harness_protocol::md5::hex_digest(payload),
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn seek(
    reader: &mut Blocking<AnyReader, Cursor<&[u8]>>,
    request: SeekRequest,
    base: TimeBase,
    work: &mut Work,
    trace: &mut Option<&mut dyn Write>,
) -> Result<(), String> {
    reader.seek(request).map_err(|error| error.to_string())?;
    let mut landed = false;
    let mut packets = 0;
    while let Some(event) = reader.next_event().map_err(|error| error.to_string())? {
        match event {
            Event::Seek(receipt) if !landed => {
                let times = [receipt.requested, receipt.landing, receipt.discard_until]
                    .map(|time| time.to_ticks(base, Rounding::TowardZero));
                for time in times {
                    let time = time.map_err(|error| error.to_string())?;
                    fold(
                        &mut work.digest,
                        signed(i64::try_from(time).map_err(|error| error.to_string())?),
                    );
                }
                if let Some(trace) = trace {
                    writeln!(trace, "seek {receipt:?}").map_err(|error| error.to_string())?;
                }
                landed = true;
            }
            Event::Packet(packet) if packet.track() == request.track => {
                if !landed {
                    return Err("a packet preceded the facade seek receipt".into());
                }
                let pts = packet.pts().unwrap_or(i64::MIN);
                let dts = packet.dts().or(packet.pts()).unwrap_or(i64::MIN);
                let sync = u64::from(packet.random_access() == RandomAccess::Sync);
                for value in [signed(pts), signed(dts), sync] {
                    fold(&mut work.digest, value);
                }
                fold_framed(&mut work.digest, packet.payload());
                work.packets += 1;
                work.bytes += packet.payload().len() as u64;
                packets += 1;
                if let Some(trace) = trace {
                    writeln!(
                        trace,
                        "{pts} {dts} {sync} {} {}",
                        packet.payload().len(),
                        triergon_harness_protocol::md5::hex_digest(packet.payload())
                    )
                    .map_err(|error| error.to_string())?;
                }
            }
            _ => {}
        }
    }
    if !landed || packets == 0 {
        return Err("the facade seek did not deliver a packet suffix".into());
    }
    Ok(())
}

fn transport_seek<'a>(input: &'a [u8], args: &[&str]) -> Result<Operation<'a>, String> {
    let [size, program, pid, target, policy, rest @ ..] = args else {
        return Err("expected SIZE PROGRAM PID PTS90 POLICY [REPEAT_PTS90]".into());
    };
    let (_, format) = transport_size(size)?;
    let program: u16 = program.parse().map_err(|_| "invalid program")?;
    let track = TrackId(pid.parse().map_err(|_| "invalid PID")?);
    let base = TimeBase::HZ_90000;
    let time = |target: &str| {
        Time::from_ticks(
            target.parse::<i128>().map_err(|_| "invalid seek ticks")?,
            base,
        )
        .map_err(|_| "invalid seek time")
    };
    let target = time(target)?;
    let repeat = match rest {
        [] => None,
        [target] => Some(time(target)?),
        _ => return Err("the facade has no explicit transport epoch or entity seek".into()),
    };
    let policy = match *policy {
        "previous" => SeekPolicy::PreviousRandomAccess,
        "preroll" => SeekPolicy::PlaybackPreroll,
        _ => return Err("expected previous or preroll".into()),
    };
    let pool = Budget::new(256 << 20).map_err(|error| error.to_string())?;
    Ok(Operation {
        name: "mpegts-previous-picture",
        run: Box::new(move |work, mut trace| {
            let mut reader = reader(input, &pool, format)?;
            reader.reset().map_err(|error| error.to_string())?;
            let mut driver = Blocking::seekable(reader, Cursor::new(input), &pool);
            for target in [Some(target), repeat].into_iter().flatten() {
                seek(
                    &mut driver,
                    SeekRequest {
                        track,
                        target,
                        policy,
                    },
                    base,
                    work,
                    &mut trace,
                )?;
            }
            if let AnyReader::MpegTs(reader) = driver.reader()
                && (0..reader.track_count())
                    .filter_map(|i| reader.track(i))
                    .any(|t| t.program() != program)
            {
                return Err("the facade selected a different transport program".into());
            }
            Ok(())
        }),
    })
}

#[derive(Clone, Copy)]
enum ContainerSeek {
    Matroska {
        ticks: i128,
    },
    Mp4 {
        track: TrackId,
        ticks: i128,
        limit: usize,
    },
}

fn container_seek<'a>(
    input: &'a [u8],
    format: Probe,
    args: &[&str],
) -> Result<Operation<'a>, String> {
    let (selection, state) = match (format, args) {
        (Probe::Matroska, [ticks, state]) => (
            ContainerSeek::Matroska {
                ticks: ticks.parse().map_err(|_| "invalid seek time")?,
            },
            *state,
        ),
        (Probe::Isobmff, [track, ticks, limit, state]) => (
            ContainerSeek::Mp4 {
                track: TrackId(track.parse().map_err(|_| "invalid track ID")?),
                ticks: ticks.parse().map_err(|_| "invalid seek ticks")?,
                limit: limit.parse().map_err(|_| "invalid packet count")?,
            },
            *state,
        ),
        _ => return Err("expected mkv TICKS STATE or mp4 TRACK TICKS COUNT STATE".into()),
    };
    let after_eof = match state {
        "fresh" => false,
        "after-eof" => true,
        _ => return Err("expected fresh or after-eof".into()),
    };
    let pool = Budget::new(256 << 20).map_err(|error| error.to_string())?;
    Ok(Operation {
        name: "facade-container-seek",
        run: Box::new(move |work, trace| {
            let mut reader = reader(input, &pool, format)?;
            reader.reset().map_err(|error| error.to_string())?;
            let mut driver = Blocking::seekable(reader, Cursor::new(input), &pool);
            if after_eof {
                while driver
                    .next_packet()
                    .map_err(|error| error.to_string())?
                    .is_some()
                {}
            } else {
                driver.next_packet().map_err(|error| error.to_string())?;
            }
            let (track, base, ticks, policy, limit) = match selection {
                ContainerSeek::Matroska { ticks } => {
                    let reader = driver.reader();
                    let track = (0..reader.track_count())
                        .filter_map(|index| reader.track(index))
                        .find(|track| track.kind() == triergon::MediaKind::Video)
                        .ok_or("no video seek track")?;
                    (
                        track.id(),
                        TimeBase::NANOSECONDS,
                        ticks,
                        SeekPolicy::PlaybackPreroll,
                        usize::MAX,
                    )
                }
                ContainerSeek::Mp4 {
                    track,
                    ticks,
                    limit,
                } => {
                    let AnyReader::Isobmff(reader) = driver.reader() else {
                        return Err("expected an ISO BMFF reader".into());
                    };
                    let media = reader
                        .tracks()
                        .iter()
                        .find(|media| u64::from(media.header().track_id) == track.0)
                        .ok_or("no selected ISO BMFF track")?;
                    let base = media
                        .header()
                        .time_base()
                        .map_err(|error| error.to_string())?;
                    (track, base, ticks, SeekPolicy::PreviousRandomAccess, limit)
                }
            };
            let target = Time::from_ticks(ticks, base).map_err(|error| error.to_string())?;
            let request = SeekRequest {
                track,
                target,
                policy,
            };
            driver.seek(request).map_err(|error| error.to_string())?;
            container_suffix(&mut driver, request, base, limit, work, trace)
        }),
    })
}

fn container_suffix(
    driver: &mut Blocking<AnyReader, Cursor<&[u8]>>,
    request: SeekRequest,
    base: TimeBase,
    limit: usize,
    work: &mut Work,
    mut trace: Option<&mut dyn Write>,
) -> Result<(), String> {
    let mut receipt = false;
    let mut packets = 0;
    while packets < limit
        && let Some(event) = driver.next_event().map_err(|error| error.to_string())?
    {
        match event {
            Event::Seek(seek) if !receipt => {
                if seek.requested != request.target {
                    return Err("seek receipt changed its request".into());
                }
                let ticks = |time: Time| {
                    time.to_ticks(base, Rounding::TowardZero)
                        .map_err(|error| error.to_string())
                };
                let (requested, landing, discard) = (
                    ticks(seek.requested)?,
                    ticks(seek.landing)?,
                    ticks(seek.discard_until)?,
                );
                work.record(3, &[requested as u64, landing as u64, discard as u64], &[]);
                if let Some(trace) = &mut trace {
                    writeln!(trace, "{{\"receipt\":[{requested},{landing},{discard}]}}")
                        .map_err(|error| error.to_string())?;
                }
                receipt = true;
            }
            Event::Packet(packet) if receipt => {
                let reader = driver.reader();
                let stream = (0..reader.track_count())
                    .find(|&i| {
                        reader
                            .track(i)
                            .is_some_and(|track| track.id() == packet.track())
                    })
                    .ok_or("seek packet track is absent")?;
                let [pts, dts, duration] = native_clocks(&packet);
                let payload = packet.payload();
                let sync = packet.random_access() == RandomAccess::Sync;
                work.record(
                    1,
                    &[
                        stream as u64,
                        packet.track().0,
                        pts,
                        dts,
                        duration,
                        u64::from(sync),
                    ],
                    payload,
                );
                if let Some(trace) = &mut trace {
                    let tick =
                        |value: Option<i64>| value.map_or("null".into(), |tick| tick.to_string());
                    writeln!(
                        trace,
                        "{{\"stream\":{stream},\"track\":{},\"pts\":{},\"dts\":{},\"duration\":{},\"key\":{sync},\"payload\":\"{}\"}}",
                        packet.track().0, tick(packet.pts()), tick(packet.dts()), tick(packet.duration()),
                        triergon_harness_protocol::hex(payload),
                    ).map_err(|error| error.to_string())?;
                }
                packets += 1;
            }
            Event::Packet(_) => return Err("a packet preceded the facade seek receipt".into()),
            _ => {}
        }
    }
    if !receipt {
        return Err("the facade did not publish a seek receipt".into());
    }
    Ok(())
}

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    let args = arguments
        .iter()
        .map(|arg| arg.to_str().ok_or("expected UTF-8 arguments"))
        .collect::<Result<Vec<_>, _>>()?;
    match (prefix, args.as_slice()) {
        ("facade-probe-", args) => probe::prepare(input, args),
        ("any-ts-au-" | "ts-au-", args) => access_units(input, args),
        ("ts-seek-", args) | ("any-seek-", ["ts", args @ ..]) => transport_seek(input, args),
        ("any-seek-", ["mkv", args @ ..]) => container_seek(input, Probe::Matroska, args),
        ("any-seek-", ["mp4", args @ ..]) => container_seek(input, Probe::Isobmff, args),
        _ => Err(format!(
            "usage: {prefix}{{fold,trace,memory}} FILE [ARG...] | {prefix}bench FILE ITERATIONS [ARG...]"
        )),
    }
}
