use std::ffi::OsString;
use std::io::Cursor;
use std::sync::atomic::AtomicBool;

use triergon::matroska::BlockGroupSource;
use triergon::{
    AnyPacket, Budget, Bytes, ContainerReader, Error, PacketInfo, SourceId, WorkBudget,
};
use triergon_subtitles::compat::document::Syntax;
use triergon_subtitles::reference_trace::carriage::{
    CallerClock, Discovery, DocumentInput, Limits, Operation as Read, ProgressKind, Session,
    Snapshot, Source, Status,
};
use triergon_subtitles::text::{Format, RawClock, Timing};

use super::protocol::Operation;
use super::text::{Records, packet_input};

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    let [mode, discovery] = arguments else {
        return Err(format!("usage: {prefix} srt|vtt|mov|mkv header|streaminfo"));
    };
    let mode = mode.to_str().ok_or("bad carriage input")?.to_owned();
    let discovery = match discovery.to_str() {
        Some("header") => Discovery::Header,
        Some("streaminfo") => Discovery::StreamInfo,
        _ => return Err("bad carriage discovery".into()),
    };
    Ok(Operation {
        name: "carriage-trace",
        run: Box::new(move |work, trace| {
            let mut records = Records { work, trace };
            let pool = Budget::new(128 * 1024 * 1024).map_err(|error| error.to_string())?;
            if mode == "mov" || mode == "mkv" {
                return container(&mut records, input, &pool);
            }
            let (syntax, filename) = match mode.as_str() {
                "srt" => (Syntax::SubRip, b"input.srt".as_slice()),
                "vtt" => (Syntax::WebVtt, b"input.vtt".as_slice()),
                _ => return Err("bad carriage input".into()),
            };
            let payload =
                Bytes::copy_from_slice(&pool, input).map_err(|error| error.to_string())?;
            let file_name =
                Bytes::copy_from_slice(&pool, filename).map_err(|error| error.to_string())?;
            let source = Source::Document(DocumentInput {
                syntax,
                payload,
                source_id: SourceId(1),
                generation: 0,
                sequence: 0,
                configuration_epoch: 0,
            });
            let snapshot = snapshot(
                &pool,
                source,
                Read::Document {
                    discovery,
                    file_name,
                },
            )
            .map_err(|error| error.to_string())?;
            records.row(3, &[0, snapshot.error_code.unwrap_or(0) as u64], &[])?;
            if discovery == Discovery::StreamInfo
                && let Some(progress) = snapshot
                    .progress
                    .iter()
                    .find(|p| p.kind == ProgressKind::Discovery)
                && let Some(code) = progress.returned
            {
                records.row(3, &[1, code as u64], &[])?;
            }
            if let Some(stream) = &snapshot.stream {
                stream_record(&mut records, stream, false)?;
            }
            for (ordinal, packet) in snapshot.packets.iter().enumerate() {
                packet_record(&mut records, ordinal as u64, packet)?;
            }
            if let Some(progress) = snapshot.progress.iter().rev().find(|p| {
                p.kind == ProgressKind::ReadFrame && p.returned.is_some_and(|code| code < 0)
            }) && let Some(code) = progress.returned
            {
                records.row(7, &[code as u64], &[])?;
            }
            Ok(())
        }),
    })
}

fn snapshot(pool: &Budget, source: Source, reader: Read) -> triergon::Result<Snapshot> {
    let mut session = Session::new(pool, Limits::default());
    session.provide(&mut Some(source), reader)?;
    let cancel = AtomicBool::new(false);
    loop {
        match session.poll(&mut WorkBudget::new(4096), &cancel)? {
            Status::Yield => {}
            Status::Ready => break,
            _ => return Err(Error::Invalid("carriage did not publish a snapshot")),
        }
    }
    session
        .take_output()
        .ok_or(Error::Invalid("missing carriage snapshot"))
}

fn stream_record(
    records: &mut Records<'_, '_>,
    stream: &triergon_subtitles::reference_trace::carriage::Stream,
    mov: bool,
) -> Result<(), String> {
    let mut extra = stream.extradata.as_deref().unwrap_or_default();
    if mov && !extra.is_empty() {
        // The description follows the reserved bytes and data-reference index
        // of the ISO BMFF SampleEntry. The decoder receives only the description.
        extra = extra.get(8..).ok_or("short tx3g SampleEntry")?;
    }
    records.row(
        2,
        &[
            stream.codec_id as u64,
            stream.time_base[0] as u64,
            stream.time_base[1] as u64,
            extra.len() as u64,
        ],
        extra,
    )
}

// A demux packet position points to the Block body, before the TrackNumber.
fn group_block_position(group: &BlockGroupSource) -> Result<i64, String> {
    let position = group
        .position()
        .map_err(|error| error.to_string())?
        .ok_or("missing BlockGroup position")?;
    let mut children = group.raw_body();
    while let Some(&first) = children.first() {
        let id_width = first.leading_zeros() as usize + 1;
        let size = children
            .get(id_width..)
            .ok_or("short BlockGroup child ID")?;
        let &first_size = size.first().ok_or("missing BlockGroup child size")?;
        let width = first_size.leading_zeros() as usize + 1;
        if id_width > 4 || width > 8 {
            return Err("invalid BlockGroup child header".into());
        }
        let mut length = u64::from(first_size) & (0xff >> width);
        for &byte in size.get(1..width).ok_or("short BlockGroup child size")? {
            length = (length << 8) | u64::from(byte);
        }
        let body = size.get(width..).ok_or("short BlockGroup child")?;
        let length = usize::try_from(length).map_err(|error| error.to_string())?;
        let remainder = body.get(length..).ok_or("short BlockGroup child body")?;
        if first == 0xa1 {
            let offset = (group.raw_body().len() - body.len()) as u64;
            return i64::try_from(position.data + offset).map_err(|error| error.to_string());
        }
        children = remainder;
    }
    Err("missing Block in BlockGroup".into())
}

fn packet_record(
    records: &mut Records<'_, '_>,
    ordinal: u64,
    packet: &triergon_subtitles::reference_trace::carriage::Packet,
) -> Result<(), String> {
    records.row(
        1,
        &[
            0,
            ordinal,
            packet.position_raw as u64,
            packet.pts_raw as u64,
            packet.dts_raw as u64,
            packet.duration_raw as u64,
            packet.flags as u64,
            packet.data.len() as u64,
        ],
        &packet.data,
    )?;
    for side in packet.side_data.iter() {
        records.row(
            3,
            &[2, side.side_data_type as u64, side.bytes.len() as u64],
            &side.bytes,
        )?;
    }
    Ok(())
}

fn container(records: &mut Records<'_, '_>, input: &[u8], pool: &Budget) -> Result<(), String> {
    let Ok(mut reader) = triergon::open_blocking(Cursor::new(input), pool) else {
        return records.row(3, &[0, (-1_i64) as u64], &[]);
    };
    records.row(3, &[0, 0], &[])?;
    let mut ordinal = 0;
    while let Some(packet) = reader.next_packet().map_err(|error| error.to_string())? {
        let Some(track) = (0..reader.reader().track_count())
            .filter_map(|i| reader.reader().track(i))
            .find(|track| track.id() == packet.track())
        else {
            continue;
        };
        let format = match track.codec() {
            triergon::CodecId::Tx3g => Format::Tx3g,
            triergon::CodecId::Ass | triergon::CodecId::Ssa => Format::OpaqueAss,
            triergon::CodecId::WebVtt => Format::MatroskaWebVtt,
            triergon::CodecId::Utf8Text => Format::Utf8Packet,
            _ => continue,
        };
        let mut source = packet_input(
            Bytes::copy_from_slice(pool, packet.payload()).map_err(|error| error.to_string())?,
            format,
        );
        if !track.configuration().is_empty() {
            source.codec_private = Some(
                Bytes::copy_from_slice(pool, track.configuration())
                    .map_err(|error| error.to_string())?,
            );
        }
        let mut base = packet.time_base();
        let mut pts = packet.pts();
        let mut dts = packet.dts();
        let mut duration = packet.duration();
        if let triergon::AnyReader::Matroska(mkv) = reader.reader() {
            let scale = mkv.timestamp_scale_ns();
            let n = i64::try_from(scale).map_err(|error| error.to_string())?;
            base =
                triergon::TimeBase::new(scale, 1_000_000_000).map_err(|error| error.to_string())?;
            pts = pts.map(|ticks| ticks.div_euclid(n));
            dts = dts.map(|ticks| ticks.div_euclid(n));
            duration = duration.map(|ticks| ticks.div_euclid(n));
        }
        let clock = CallerClock {
            numerator: i32::try_from(base.numerator()).map_err(|error| error.to_string())?,
            denominator: i32::try_from(base.denominator()).map_err(|error| error.to_string())?,
            pts,
            dts,
            duration,
        };
        source.timing = Timing::from_ticks(RawClock {
            numerator: base.numerator(),
            denominator: base.denominator(),
            pts_ticks: pts.map(i128::from),
            dts_ticks: dts.map(i128::from),
            duration_ticks: duration.map(i128::from),
        })
        .map_err(|error| error.to_string())?;
        let mut snapshot = snapshot(pool, Source::Sample(source), Read::Sample(clock))
            .map_err(|error| error.to_string())?;
        if let AnyPacket::Matroska(mkv) = &packet {
            let position = mkv.group.as_ref().map(group_block_position).transpose()?;
            for projected in snapshot.packets.iter_mut() {
                projected.position_raw = position.unwrap_or(-1);
                projected.flags = i32::from(mkv.packet.keyframe);
            }
        }
        let mov = if let AnyPacket::Isobmff(packet) = &packet {
            let sample = packet.sample();
            let position = i64::try_from(sample.range.start).map_err(|error| error.to_string())?;
            for projected in snapshot.packets.iter_mut() {
                projected.position_raw = position;
                projected.flags = i32::from(sample.is_container_sync());
            }
            true
        } else {
            false
        };
        if ordinal == 0
            && let Some(stream) = &snapshot.stream
        {
            stream_record(records, stream, mov)?;
        }
        for projected in snapshot.packets.iter() {
            packet_record(records, ordinal, projected)?;
            if let AnyPacket::Matroska(mkv) = &packet
                && let Some(group) = &mkv.group
                && let Some(addition) = group.additions().find(|addition| addition.id() == 1)
            {
                let bytes = [&addition.id().to_be_bytes(), addition.data()].concat();
                records.row(3, &[2, 15, bytes.len() as u64], &bytes)?;
            }
            ordinal += 1;
        }
    }
    records.row(7, &[(-541_478_725_i64) as u64], &[])
}
