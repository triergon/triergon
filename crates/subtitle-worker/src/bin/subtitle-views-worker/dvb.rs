use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::Cursor;
use std::sync::atomic::AtomicBool;

use triergon::{
    Budget, Bytes, CodecId, Error, InputCursor, InputWindow, SourceCapabilities, SourceId, Step,
    WorkBudget,
};
use triergon_mpegts::compat::pes::{CutPes, DemuxClock, PesCutter};
use triergon_mpegts::compat::stream::{self, ProgramReader};
use triergon_mpegts::psi::ElementaryStream;
use triergon_mpegts::transport::TransportFramer;
use triergon_mpegts::{PacketFormat, ParseLimits};

use super::protocol::Operation;
use super::text::Records;
use triergon::{AnyReader, ContainerReader, PacketInfo, TrackInfo};
use triergon_subtitles::compat::dvb::{PesParser, PesProgress};
use triergon_subtitles::dvb::{
    ClockOrigin, ClockValue, DvbPacket, PacketEnvelope, PacketIdentity, PacketTiming, SourceClock,
    SourceConfiguration,
};

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    let mode = match arguments {
        [mode] if mode == "ts" => true,
        [mode] if mode == "mkv" => false,
        _ => return Err(format!("usage: {prefix} mkv|ts")),
    };
    Ok(Operation {
        name: "dvb-source",
        run: Box::new(move |work, trace| {
            let mut records = Records { work, trace };
            let data = if mode {
                ts_source(input)
            } else {
                mkv_source(input)
            };
            let Ok(data) = data else {
                return records.refused();
            };
            let clock = data.configuration[0].clock();
            records.row(
                2,
                &[
                    0,
                    clock.time_base().numerator(),
                    clock.time_base().denominator(),
                    (5 * data.configuration.len()) as u64,
                ],
                &data
                    .configuration
                    .iter()
                    .flat_map(SourceConfiguration::service_private_view)
                    .collect::<Vec<_>>(),
            )?;
            records.row(2, &[1, data.language.len() as u64], &data.language)?;
            for packet in data.packets {
                let source = packet
                    .source
                    .as_ref()
                    .ok_or("missing DVB packet source facts")?;
                let timing = source.timing();
                let words = [
                    packet.sequence,
                    timing.pts.is_some().into(),
                    timing.pts.map_or(0, |value| value.ticks as u64),
                    timing.dts.is_some().into(),
                    timing.dts.map_or(0, |value| value.ticks as u64),
                    timing.duration.map_or(0, |value| value.ticks as u64),
                    packet.payload.len() as u64,
                ];
                records.row(1, &words, &packet.payload)?;
            }
            records.row(7, &[], &[])
        }),
    })
}

struct Source {
    configuration: Vec<SourceConfiguration>,
    language: Vec<u8>,
    packets: Vec<DvbPacket>,
}

fn ts_source(input: &[u8]) -> triergon::Result<Source> {
    let pool = Budget::new(256 * 1024 * 1024)?;
    let (stream, program, pes) = transport(input, &pool, CodecId::DvbSubtitle, 0)?;
    let mut clock = DemuxClock::new(program)?;
    let descriptor = stream
        .descriptors()
        .filter_map(Result::ok)
        .filter(|item| item.tag == 0x59)
        .last()
        .ok_or(Error::Invalid("missing DVB descriptor"))?;
    let original = Bytes::copy_from_slice(&pool, descriptor.source)?;
    let configuration = (0..descriptor.data.len() / 8)
        .map(|index| {
            SourceConfiguration::from_subtitling_descriptor(
                &pool,
                &original,
                index,
                SourceClock::new(1, 90_000)?,
                PacketEnvelope::Segments,
            )
        })
        .collect::<triergon::Result<Vec<_>>>()?;
    let service = configuration
        .first()
        .ok_or(Error::Invalid("no DVB service"))?;
    let language = configuration
        .iter()
        .filter_map(SourceConfiguration::language_bytes)
        .collect::<Vec<_>>()
        .join(&b',');
    let mut packets = Vec::new();
    let mut parser = PesParser::new(&pool);
    let cancel = AtomicBool::new(false);
    for (sequence, packet) in pes.into_iter().enumerate() {
        let (pts, _) = clock.place(packet.pts_raw33, packet.dts_raw33)?;
        parser.provide(&packet.data, pts)?;
        let unit = loop {
            match parser.step(&mut WorkBudget::new(1 << 20), &cancel)? {
                PesProgress::Packet(unit) => break unit,
                PesProgress::Yield => {}
                PesProgress::NeedPacket => return Err(Error::Invalid("DVB PES parser stalled")),
            }
        };
        let timing = PacketTiming {
            pts: unit.codec_pts().map(clock_value),
            dts: unit.codec_pts().map(clock_value),
            duration: None,
        };
        packets.push(service.packet(&pool, unit.codec(), timing, identity(sequence))?);
    }
    Ok(Source {
        configuration,
        language,
        packets,
    })
}

fn mkv_source(input: &[u8]) -> triergon::Result<Source> {
    let pool = Budget::new(256 * 1024 * 1024)?;
    let mut reader = triergon::open_blocking(Cursor::new(input), &pool)?;
    let mut pending = reader.next_packet()?;
    let AnyReader::Matroska(mkv) = reader.reader() else {
        return Err(Error::Invalid("DVB source requires Matroska"));
    };
    let track = (0..mkv.track_count())
        .filter_map(|index| mkv.track(index))
        .find(|track| track.codec() == CodecId::DvbSubtitle)
        .ok_or(Error::Invalid("no DVB Matroska track"))?;
    let language = mkv
        .tracks()
        .iter()
        .find(|item| item.number == track.id().0)
        .map_or_else(Vec::new, |item| {
            item.effective_language().as_bytes().to_vec()
        });
    let private = Bytes::copy_from_slice(&pool, track.configuration())?;
    let scale = mkv.timestamp_scale_ns();
    let config = SourceConfiguration::from_matroska_private(
        &pool,
        &private,
        SourceClock::new(scale, 1_000_000_000)?,
    )?;
    let track = track.id();
    let ticks = |ns: i64| clock_value(i128::from(ns).div_euclid(i128::from(scale)));
    let mut packets = Vec::new();
    while let Some(packet) = pending {
        if packet.track() == track {
            let data = Bytes::copy_from_slice(&pool, packet.payload())?;
            let timing = PacketTiming {
                pts: packet.pts().map(ticks),
                dts: packet.dts().map(ticks),
                duration: packet.duration().map(ticks),
            };
            packets.push(config.packet(&pool, &data, timing, identity(packets.len()))?);
        }
        pending = reader.next_packet()?;
    }
    Ok(Source {
        configuration: vec![config],
        language,
        packets,
    })
}

fn identity(sequence: usize) -> PacketIdentity {
    PacketIdentity {
        generation: 0,
        sequence: sequence as u64,
        container_position: None,
    }
}

fn clock_value(ticks: i128) -> ClockValue {
    ClockValue {
        ticks,
        origin: ClockOrigin::Inferred,
    }
}

pub(super) fn transport(
    input: &[u8],
    pool: &Budget,
    codec: CodecId,
    mut ordinal: usize,
) -> triergon::Result<(ElementaryStream, u16, Vec<CutPes>)> {
    let format = PacketFormat::detect(input).ok_or(Error::Invalid("subtitle transport framing"))?;
    let mut framer = TransportFramer::new(pool.clone(), format);
    let mut programs = ProgramReader::new(pool.clone(), ParseLimits::default())?;
    let caps = SourceCapabilities {
        length: Some(input.len() as u64),
        random_access: false,
    };
    let mut cursor = InputCursor::new(pool, SourceId(1), caps, 64 * 1024)?;
    let cancel = AtomicBool::new(false);
    let mut seen = BTreeSet::new();
    let mut selected = None;
    let mut packets = Vec::new();
    loop {
        let packet = match framer.poll(&mut cursor, &mut WorkBudget::new(1 << 20), &cancel) {
            Ok(Step::Ready(packet)) => packet,
            Ok(Step::NeedInput(request)) => {
                let offset = usize::try_from(request.offset).map_err(|_| Error::Overflow)?;
                if offset >= input.len() {
                    cursor.finish(request.source, request.token)?;
                } else {
                    let size = request.maximum.min(input.len() - offset);
                    let data = Bytes::copy_from_slice(pool, &input[offset..offset + size])?;
                    cursor.provide_owned(InputWindow::new(
                        request.source,
                        request.token,
                        request.offset,
                        data,
                    )?)?;
                }
                continue;
            }
            Ok(Step::End(_)) | Err(Error::Truncated) => break,
            Ok(_) => continue,
            Err(Error::Invalid(_)) => {
                framer.discard();
                continue;
            }
            Err(error) => return Err(error),
        };
        programs.push(&packet)?;
        while let Some(program) = programs.poll()? {
            if selected.is_none() {
                for stream in program.streams.iter() {
                    if seen.insert(stream.pid) && stream::codec(stream) == Some(codec) {
                        if ordinal == 0 {
                            let cutter =
                                PesCutter::new(stream.pid, pool.clone(), ParseLimits::default())?;
                            selected = Some((stream.clone(), program.number, cutter));
                            break;
                        }
                        ordinal -= 1;
                    }
                }
            }
            if let Some((_, number, cutter)) = &mut selected
                && *number == program.number
            {
                cutter.set_pcr_pid(program.pcr_pid);
            }
        }
        if let Some((_, _, cutter)) = &mut selected {
            cutter.push(&packet)?;
            packets.extend(std::iter::from_fn(|| cutter.pop()));
        }
    }
    let (stream, program, mut cutter) =
        selected.ok_or(Error::Invalid("subtitle transport stream absent"))?;
    cutter.finish()?;
    packets.extend(std::iter::from_fn(|| cutter.pop()));
    Ok((stream, program, packets))
}
