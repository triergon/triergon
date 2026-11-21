//! Source provenance and the single-substream PES grouping rule of TS 102 114 F.4.3.
use super::{
    AccessUnit, AudioEofPolicy, CodecId, Drive, Error, Event, PID, PacketFormat, PacketInfo,
    ReaderEvent, ReaderLimits, Time, TrackInfo, Transport, audio_eof_namespace,
};

fn limits() -> ReaderLimits {
    ReaderLimits {
        descriptor_namespace: audio_eof_namespace(),
        ..ReaderLimits::default()
    }
}

fn descriptor(mask: u8) -> Vec<u8> {
    let mut bytes = vec![0x7f, 2 + 6 * mask.count_ones() as u8, 0x0e, 0];
    for index in 0..4 {
        if mask & (1 << index) != 0 {
            bytes[3] |= 0x40 >> index;
            bytes.extend([5, 1, 0x60, 0x48, 0, 0]);
        }
    }
    bytes
}

fn put(bytes: &mut [u8], at: &mut usize, width: usize, value: usize) {
    for bit in (0..width).rev() {
        bytes[*at / 8] |= ((value >> bit) as u8 & 1) << (7 - *at % 8);
        *at += 1;
    }
}

// Only the fixed framing and timing prefix is interpreted; asset bytes are opaque.
fn extension(index: usize, long: bool, asset: usize, timing: Option<(usize, usize)>) -> Vec<u8> {
    let header = if long { 288 } else { 16 };
    let mut bytes = vec![0; header + asset];
    bytes[..5].copy_from_slice(&[0x64, 0x58, 0x20, 0x25, 0xa5]);
    let mut at = 40;
    for (width, value) in [
        (2, index),
        (1, usize::from(long)),
        (if long { 12 } else { 8 }, header - 1),
        (if long { 20 } else { 16 }, header + asset - 1),
        (1, usize::from(timing.is_some())),
    ] {
        put(&mut bytes, &mut at, width, value);
    }
    if let Some((clock, duration)) = timing {
        put(&mut bytes, &mut at, 2, clock);
        put(&mut bytes, &mut at, 3, duration);
    }
    let mut crc = u16::MAX;
    for &byte in &bytes[5..header - 2] {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = (crc << 1) ^ if crc & 0x8000 != 0 { 0x1021 } else { 0 };
        }
    }
    bytes[header - 2..header].copy_from_slice(&crc.to_be_bytes());
    bytes[header..].fill(index as u8 + 0x11);
    bytes
}

fn aligned_pes(ts: &mut Transport, payload: &[u8], pts: Option<u64>, dts: Option<u64>) {
    let start = ts.bytes.len();
    ts.pes(PID, 0xbd, payload, pts, dts);
    let at = ts.bytes[start..start + 188]
        .windows(4)
        .position(|prefix| prefix == [0, 0, 1, 0xbd])
        .unwrap();
    ts.bytes[start + at + 6] |= 4;
}

#[test]
fn grouped_units_keep_original_headers_ranges_and_one_use_clocks() {
    let frames = [
        extension(2, false, 4, None),
        extension(2, false, 8, Some((1, 0))),
        extension(2, false, 12, None),
        extension(2, false, 4, None),
        extension(2, false, 4, Some((0, 2))),
    ];
    let mut ts = Transport::new();
    ts.tables(&[(6, PID, &descriptor(4))], 0);
    aligned_pes(&mut ts, &frames[..3].concat(), Some(90_000), Some(89_000));
    aligned_pes(&mut ts, &frames[3], Some(99_000), None);
    aligned_pes(&mut ts, &frames[4], None, None);
    let m2ts = ts.m2ts();
    for format in [PacketFormat::Ts188, PacketFormat::M2ts192] {
        let input = if format == PacketFormat::Ts188 {
            &ts.bytes
        } else {
            &m2ts
        };
        for (chunk, grant) in [(1, 1), (7, 1), (4096, 8192)] {
            let mut drive = Drive::new(input, ReaderLimits { format, ..limits() }, chunk, grant);
            let mut ordinal = 0;
            let mut offset = 0;
            while let Some(event) = drive.next().unwrap() {
                let Event::Packet(packet) = event else {
                    continue;
                };
                assert_eq!(packet.track_info().codec(), CodecId::Dts);
                assert!(matches!(packet.access_unit(), AccessUnit::DtsExtension(_)));
                assert_eq!(packet.payload(), frames[ordinal]);
                let end = offset + frames[ordinal].len() as u64;
                assert_eq!(packet.elementary_range(), offset..end);
                assert_eq!(
                    packet.pts(),
                    [Some(90_000), None, None, Some(99_000), None][ordinal]
                );
                assert_eq!(
                    packet.dts(),
                    [Some(89_000), None, None, None, None][ordinal]
                );
                let expected = match ordinal {
                    0 => None,
                    1..=3 => Some(Time::new(512, 44_100).unwrap()),
                    _ => Some(Time::new(1536, 32_000).unwrap()),
                };
                assert_eq!(packet.sample_duration(), expected);
                // A fractional 90 kHz duration stays exact in sample_duration;
                // the integral packet field does not round it.
                assert_eq!(
                    packet.duration(),
                    [None, None, None, None, Some(4320)][ordinal]
                );
                assert_eq!(packet.clock().is_some(), matches!(ordinal, 0 | 3 | 4));
                let [source] = packet.sources() else {
                    panic!("one original PES");
                };
                let pes = source.pes.event().pes_source().unwrap();
                assert_eq!(source.elementary_range, offset..end);
                let start = if ordinal < 3 { offset as usize } else { 0 };
                assert_eq!(source.payload_range, start..start + frames[ordinal].len());
                assert_eq!(pes.elementary_offset + start as u64, offset);
                let original: Vec<u8> = pes.payload().flatten().copied().collect();
                assert_eq!(&original[source.payload_range.clone()], packet.payload());
                assert_eq!(
                    pes.header.pts_raw33,
                    [Some(90_000), Some(90_000), Some(90_000), Some(99_000), None][ordinal]
                );
                assert_eq!(pes.header.dts_raw33, (ordinal < 3).then_some(89_000));
                assert!(pes.header.data_alignment_indicator);
                assert_ne!(pes.header.declared_packet_length, 0);
                let header: Vec<_> = pes
                    .header_source_ranges
                    .as_slice()
                    .iter()
                    .flat_map(|range| {
                        input[range.offset as usize..range.offset as usize + range.bytes]
                            .iter()
                            .copied()
                    })
                    .collect();
                assert_eq!(header, pes.header.as_bytes());
                offset = end;
                ordinal += 1;
            }
            assert_eq!(ordinal, frames.len());
        }
    }
}

fn unit(mask: u8, long: bool, timing: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    for index in 0..4 {
        if mask & (1 << index) != 0 {
            bytes.resize(bytes.len().next_multiple_of(4), 0);
            bytes.extend(extension(index, long, index + 1, timing.then_some((2, 1))));
        }
    }
    bytes
}

#[test]
fn multiple_extensions_without_core_are_one_complete_unit_per_pes() {
    for mask in [3, 5, 7, 10, 15] {
        for long in [false, true] {
            let units = [unit(mask, long, true), unit(mask, long, false)];
            let mut ts = Transport::new();
            ts.tables(&[(6, PID, &descriptor(mask))], 0);
            for payload in &units {
                aligned_pes(&mut ts, payload, Some(90_000), None);
            }
            for (chunk, grant) in [(7, 1), (4096, 8192)] {
                let mut drive = Drive::new(&ts.bytes, limits(), chunk, grant);
                let mut ordinal = 0;
                let mut offset = 0;
                while let Some(event) = drive.next().unwrap() {
                    let Event::Packet(packet) = event else {
                        continue;
                    };
                    assert_eq!(packet.payload(), units[ordinal]);
                    assert!(matches!(packet.access_unit(), AccessUnit::DtsExtension(_)));
                    assert_eq!(
                        packet.sample_duration(),
                        Some(Time::new(1024, 48_000).unwrap())
                    );
                    assert_eq!(packet.pts(), Some(90_000));
                    assert_eq!(packet.dts(), None);
                    assert_eq!(
                        packet.elementary_range(),
                        offset..offset + units[ordinal].len() as u64
                    );
                    assert_eq!(packet.sources()[0].payload_range, 0..units[ordinal].len());
                    offset += units[ordinal].len() as u64;
                    ordinal += 1;
                }
                assert_eq!(ordinal, 2);
            }
        }
    }
}

#[test]
fn primary_timing_is_not_borrowed_from_another_extension() {
    let payload = [
        extension(0, false, 4, None),
        extension(1, false, 4, Some((2, 1))),
    ]
    .concat();
    let mut ts = Transport::new();
    ts.tables(&[(6, PID, &descriptor(3))], 0);
    aligned_pes(&mut ts, &payload, None, None);
    let mut drive = Drive::new(&ts.bytes, limits(), 7, 1);
    let mut count = 0;
    while let Some(event) = drive.next().unwrap() {
        if let Event::Packet(packet) = event {
            assert_eq!(packet.payload(), payload);
            assert_eq!(packet.sample_duration(), None);
            assert_eq!(
                (packet.pts(), packet.dts(), packet.duration()),
                (None, None, None)
            );
            count += 1;
        }
    }
    assert_eq!(count, 1);
}

#[test]
fn refuses_invalid_multi_substream_groups_before_publishing_any_unit() {
    let first = extension(0, false, 4, Some((2, 1)));
    let second = extension(1, false, 4, Some((2, 1)));
    let valid = [first.clone(), second.clone()].concat();
    let mut crc = valid.clone();
    crc[second.len() + 13] ^= 1;
    let mut pad = unit(3, false, true);
    pad[17] = 1;
    for (payload, expected) in [
        (
            [second.clone(), first.clone()].concat(),
            Error::Invalid("DTS-HD extension substream index"),
        ),
        (
            [first.clone(), first.clone()].concat(),
            Error::Invalid("DTS-HD extension substream order"),
        ),
        (
            [first.clone(), extension(2, false, 4, None)].concat(),
            Error::Invalid("DTS-HD extension substream order"),
        ),
        (first, Error::Truncated),
        (valid[..valid.len() - 1].to_vec(), Error::Truncated),
        (
            [valid.clone(), valid.clone()].concat(),
            Error::Invalid("DTS-HD PES access unit extent"),
        ),
        (crc, Error::Invalid("DTS-HD extension header CRC")),
        (pad, Error::Invalid("DTS-HD substream alignment")),
    ] {
        let mut ts = Transport::new();
        ts.tables(&[(6, PID, &descriptor(3))], 0);
        aligned_pes(&mut ts, &payload, Some(90_000), None);
        for (chunk, grant) in [(7, 1), (4096, 8192)] {
            let mut drive = Drive::new(&ts.bytes, limits(), chunk, grant);
            loop {
                match drive.next() {
                    Ok(Some(Event::Other(_))) => (),
                    Err(error) => {
                        assert_eq!(error, expected);
                        break;
                    }
                    result => panic!("invalid group published or escaped refusal: {result:?}"),
                }
            }
        }
    }
}

#[test]
fn incomplete_multi_extension_pes_reports_the_original_fragment() {
    let mut payload = unit(3, false, true);
    payload.pop();
    let mut ts = Transport::new();
    ts.tables(&[(6, PID, &descriptor(3))], 0);
    aligned_pes(&mut ts, &payload, Some(90_000), Some(89_000));
    let mut drive = Drive::new(
        &ts.bytes,
        ReaderLimits {
            audio_eof_policy: AudioEofPolicy::ReportIncomplete,
            ..limits()
        },
        7,
        1,
    );
    let mut count = 0;
    while let Some(event) = drive.next().unwrap() {
        match event {
            Event::Other(ReaderEvent::IncompleteAudio(packet)) => {
                assert_eq!(packet.payload(), payload);
                assert_eq!((packet.pts(), packet.dts()), (Some(90_000), Some(89_000)));
                assert_eq!(packet.sources()[0].payload_range, 0..payload.len());
                count += 1;
            }
            Event::Packet(_) => panic!("incomplete group is not an access unit"),
            _ => (),
        }
    }
    assert_eq!(count, 1);
}

#[test]
fn multi_extension_carriage_requires_alignment_and_the_finite_pes_bound() {
    let payload = unit(3, false, true);
    for aligned in [false, true] {
        let mut ts = Transport::new();
        ts.tables(&[(6, PID, &descriptor(3))], 0);
        if aligned {
            aligned_pes(&mut ts, &payload, Some(90_000), None);
        } else {
            ts.pes(PID, 0xbd, &payload, Some(90_000), None);
        }
        let mut drive = Drive::new(
            &ts.bytes,
            ReaderLimits {
                maximum_dts_hd_pes_bytes: payload.len() - 1,
                ..limits()
            },
            7,
            1,
        );
        loop {
            match drive.next() {
                Ok(Some(Event::Other(_))) => (),
                Err(Error::Invalid("DTS-HD aligned PES")) if !aligned => break,
                Err(Error::Limit { requested, maximum }) if aligned => {
                    assert_eq!((requested, maximum), (payload.len(), payload.len() - 1));
                    break;
                }
                result => panic!("DTS-HD PES escaped its carriage checks: {result:?}"),
            }
        }
    }
}

#[test]
fn a_group_of_core_and_extension_units_remains_invalid_in_one_pes() {
    let core = &include_bytes!("../../../../harness/fixtures/mpegts/dts-core-mono.dts")[..512];
    let mut descriptor = descriptor(1);
    descriptor[1] += 6;
    descriptor[3] |= 0x80;
    descriptor.splice(4..4, [5, 1, 0x60, 0x08, 0, 0]);
    let one = [core, &extension(0, false, 4, None)].concat();
    let payload = [one.clone(), one].concat();
    let mut ts = Transport::new();
    ts.tables(&[(6, PID, &descriptor)], 0);
    aligned_pes(&mut ts, &payload, Some(90_000), None);
    let mut drive = Drive::new(&ts.bytes, limits(), 7, 1);
    loop {
        match drive.next() {
            Ok(Some(Event::Other(_))) => (),
            Err(Error::Invalid("DTS-HD PES access unit extent")) => break,
            result => panic!("multi-substream grouping must remain refused: {result:?}"),
        }
    }
}
