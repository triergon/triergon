use std::ffi::OsString;
use std::io::Cursor;

use triergon::{Budget, CodecId, ContainerReader, PacketInfo};
use triergon_subtitles::pgs::{
    Composition, Envelope, ObjectFragment, Palette, SegmentIter, Windows,
};

use super::protocol::Operation;
use super::text::Records;

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    let [mode, extra @ ..] = arguments else {
        return Err(format!(
            "usage: {prefix} packet|sup|capsule|mkv|ts [STREAM]"
        ));
    };
    let packets = match (mode.to_str(), extra) {
        (Some("packet"), []) => Ok(vec![(Envelope::Packet, input.to_vec())]),
        (Some("sup"), []) => Ok(vec![(Envelope::Sup, input.to_vec())]),
        (Some("capsule"), []) => capsule(input),
        (Some("mkv"), [ordinal]) => matroska(input, stream(ordinal)?),
        (Some("ts"), [ordinal]) => transport(input, stream(ordinal)?),
        _ => return Err(format!("invalid PGS envelope: {}", mode.to_string_lossy())),
    };
    Ok(Operation {
        name: "pgs-segments",
        run: Box::new(move |work, trace| {
            let mut records = Records { work, trace };
            let Ok(packets) = &packets else {
                return records.refused();
            };
            for (index, (envelope, packet)) in packets.iter().enumerate() {
                records.row(2, &[index as u64, packet.len() as u64], &[])?;
                segments(&mut records, packet, *envelope)?;
            }
            records.row(7, &[], &[])
        }),
    })
}

fn stream(value: &OsString) -> Result<usize, String> {
    value
        .to_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| "invalid stream ordinal".into())
}

type Packets = Vec<(Envelope, Vec<u8>)>;

fn matroska(input: &[u8], ordinal: usize) -> triergon::Result<Packets> {
    let pool = Budget::new(256 * 1024 * 1024)?;
    let mut reader = triergon::open_blocking(Cursor::new(input), &pool)?;
    let mut packets = Vec::new();
    while let Some(packet) = reader.next_packet()? {
        let track = (0..reader.reader().track_count())
            .filter_map(|index| reader.reader().track(index))
            .filter(|track| track.codec() == CodecId::Pgs)
            .nth(ordinal);
        if track.is_some_and(|track| packet.track() == track.id()) {
            packets.push((Envelope::Packet, packet.payload().to_vec()));
        }
    }
    Ok(packets)
}

fn transport(input: &[u8], ordinal: usize) -> triergon::Result<Packets> {
    let pool = Budget::new(256 * 1024 * 1024)?;
    let (_, _, packets) = super::dvb::transport(input, &pool, CodecId::Pgs, ordinal)?;
    Ok(packets
        .into_iter()
        .map(|packet| (Envelope::Packet, packet.data.to_vec()))
        .collect())
}

fn capsule(input: &[u8]) -> triergon::Result<Packets> {
    use triergon::Error;
    if !input.starts_with(b"SMBMO001\x01") {
        return Err(Error::Invalid("PGS capsule signature"));
    }
    let mut data = input.get(21..).ok_or(Error::Truncated)?;
    fn take<'a>(data: &mut &'a [u8], size: usize) -> triergon::Result<&'a [u8]> {
        let (value, rest) = data.split_at_checked(size).ok_or(Error::Truncated)?;
        *data = rest;
        Ok(value)
    }
    fn count(data: &mut &[u8]) -> triergon::Result<usize> {
        Ok(u32::from_be_bytes(take(data, 4)?.try_into().map_err(|_| Error::Truncated)?) as usize)
    }
    let config = count(&mut data)?;
    take(&mut data, config)?;
    let entries = count(&mut data)?;
    let mut packets = Vec::new();
    for _ in 0..entries {
        let header = take(&mut data, 19)?;
        let length = count(&mut data)?;
        let packet = take(&mut data, length)?;
        if header[0] == 0 {
            packets.push((Envelope::Packet, packet.to_vec()));
        }
    }
    if !data.is_empty() {
        return Err(Error::Invalid("PGS capsule trailing bytes"));
    }
    Ok(packets)
}

fn segments(records: &mut Records<'_, '_>, input: &[u8], envelope: Envelope) -> Result<(), String> {
    for segment in SegmentIter::new(input, envelope) {
        let Ok(segment) = segment else {
            return records.refused();
        };
        let mut head = vec![
            0,
            segment.offset as u64,
            segment.kind.into(),
            segment.raw.len() as u64,
            segment.payload.len() as u64,
            u64::from(segment.clock.is_some()),
        ];
        if let Some(clock) = segment.clock {
            head.extend([u64::from(clock.pts), u64::from(clock.dts)]);
        }
        records.row(3, &head, segment.raw)?;
        let result = typed(records, segment)?;
        if result.is_err() {
            records.row(8, &[segment.offset as u64, segment.kind.into()], &[])?;
        }
    }
    Ok(())
}

fn typed(
    records: &mut Records<'_, '_>,
    segment: triergon_subtitles::pgs::Segment<'_>,
) -> Result<triergon::Result<()>, String> {
    match segment.kind {
        0x14 => {
            let value = match Palette::parse(segment) {
                Ok(value) => value,
                Err(error) => return Ok(Err(error)),
            };
            records.row(
                3,
                &[
                    1,
                    value.id.into(),
                    value.version.into(),
                    value.entries().len() as u64,
                ],
                &[],
            )?;
            for e in value.entries() {
                records.row(
                    3,
                    &[
                        2,
                        e.id.into(),
                        e.y.into(),
                        e.cr.into(),
                        e.cb.into(),
                        e.alpha.into(),
                    ],
                    &[],
                )?;
            }
        }
        0x15 => {
            let value = match ObjectFragment::parse(segment) {
                Ok(value) => value,
                Err(error) => return Ok(Err(error)),
            };
            let mut words = vec![
                3,
                value.id.into(),
                value.version.into(),
                value.flags.into(),
                value.last.into(),
                value.first.is_some().into(),
            ];
            if let Some(first) = value.first {
                words.extend([
                    u64::from(first.data_length),
                    first.width.into(),
                    first.height.into(),
                ]);
            }
            words.push(value.data.len() as u64);
            records.row(3, &words, value.data)?;
        }
        0x16 => {
            let value = match Composition::parse(segment) {
                Ok(value) => value,
                Err(error) => return Ok(Err(error)),
            };
            records.row(
                3,
                &[
                    4,
                    value.width.into(),
                    value.height.into(),
                    value.frame_rate.into(),
                    value.number.into(),
                    value.state.into(),
                    value.palette_update_flags.into(),
                    value.palette_id.into(),
                    value.object_count.into(),
                ],
                &[],
            )?;
            for object in value.objects() {
                let object = match object {
                    Ok(object) => object,
                    Err(error) => return Ok(Err(error)),
                };
                let mut words = vec![
                    5,
                    object.id.into(),
                    object.window.into(),
                    object.flags.into(),
                    object.x.into(),
                    object.y.into(),
                    object.forced().into(),
                    object.crop.is_some().into(),
                ];
                if let Some(crop) = object.crop {
                    words.extend([
                        u64::from(crop.x),
                        crop.y.into(),
                        crop.width.into(),
                        crop.height.into(),
                    ]);
                }
                records.row(3, &words, &[])?;
            }
        }
        0x17 => {
            let value = match Windows::parse(segment) {
                Ok(value) => value,
                Err(error) => return Ok(Err(error)),
            };
            records.row(3, &[6, value.iter().len() as u64], &[])?;
            for window in value.iter() {
                records.row(
                    3,
                    &[
                        7,
                        window.id.into(),
                        window.x.into(),
                        window.y.into(),
                        window.width.into(),
                        window.height.into(),
                    ],
                    &[],
                )?;
            }
        }
        _ => {}
    }
    Ok(Ok(()))
}
