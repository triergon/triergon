use super::{
    AccessUnit, CodecId, Drive, Error, Event, MediaKind, PID, PacketInfo, ReaderEvent,
    ReaderLimits, TrackInfo, Transport, audio_eof_frame, eac3,
};
use triergon_mpegts::psi::DescriptorNamespace;

fn limits() -> ReaderLimits {
    ReaderLimits {
        descriptor_namespace: DescriptorNamespace::Atsc,
        ..ReaderLimits::default()
    }
}

#[test]
fn direct_audio_is_scoped_to_the_declared_namespace() {
    for (kind, codec, descriptors, frame) in [
        (
            0x81,
            CodecId::Ac3,
            &([0x81, 3, 8, 8, 5])[..],
            audio_eof_frame(true),
        ),
        (
            0x87,
            CodecId::Eac3,
            &([0xcc, 2, 0x80, 0xc2])[..],
            eac3::coded_primary(),
        ),
    ] {
        let mut ts = Transport::new();
        ts.tables(&[(kind, PID, descriptors)], 0);
        ts.pes(PID, 0xbd, &frame.repeat(2), Some(90_000), None);
        let mut unspecified = Drive::new(&ts.bytes, ReaderLimits::default(), 188, 4096);
        let mut unsupported = false;
        while let Some(event) = unspecified.next().unwrap() {
            match event {
                Event::Other(ReaderEvent::Unsupported { track, .. }) => {
                    assert_eq!(track.codec(), CodecId::Other(&[kind]));
                    unsupported = true;
                }
                Event::Packet(_) => panic!("private code point without its namespace"),
                _ => (),
            }
        }
        assert!(unsupported);

        let mut declared = Drive::new(&ts.bytes, limits(), 1, 1);
        let mut units = 0;
        while let Some(event) = declared.next().unwrap() {
            if let Event::Packet(packet) = event {
                assert!(matches!(packet.access_unit(), AccessUnit::Ac3(_)));
                let track = packet.track_info();
                assert_eq!(track.kind(), MediaKind::Audio);
                assert_eq!(track.codec(), codec);
                assert_eq!(track.codec_tag(), [kind]);
                assert_eq!(track.stream().descriptor_bytes.as_ref(), descriptors);
                assert_eq!(packet.payload(), frame);
                assert_eq!(packet.pts(), if units == 0 { Some(90_000) } else { None });
                assert_eq!(packet.dts(), None);
                assert_eq!(packet.sources().len(), 1);
                let start = units * frame.len();
                assert_eq!(
                    packet.sources()[0].payload_range,
                    start..start + frame.len()
                );
                units += 1;
            }
        }
        assert_eq!(units, 2);
    }
}

#[test]
fn direct_enhanced_audio_refuses_a_group_split_between_pes_packets() {
    let frame = eac3::coded_primary();
    let mut ts = Transport::new();
    ts.tables(&[(0x87, PID, &[0xcc, 2, 0x80, 0xc2])], 0);
    let cut = frame.len() / 2;
    ts.pes(PID, 0xbd, &frame[..cut], Some(90_000), None);
    ts.pes(PID, 0xbd, &frame[cut..], None, None);

    let mut drive = Drive::new(&ts.bytes, limits(), 11, 5);
    loop {
        match drive.next() {
            Err(error) => {
                assert_eq!(error, Error::Invalid("ATSC E-AC-3 access unit crosses PES"));
                break;
            }
            Ok(Some(Event::Packet(_)) | None) => panic!("a split group was accepted"),
            Ok(Some(_)) => (),
        }
    }
}

// A/52 Table E1.2: stereo, no optional metadata, convsync at bit 53
// for short independent frames. Bodies are opaque framing controls.
fn short_frame(block_code: u8, dependent: Option<u8>, sync: bool) -> Vec<u8> {
    let mut frame = vec![0; 32];
    frame[..8].copy_from_slice(&[0x0b, 0x77, 0, 15, 5 | (block_code << 4), 0x87, 0xc0, 0]);
    if let Some(id) = dependent {
        frame[2] = 0x40 | (id << 3);
    } else if sync {
        frame[6] |= 4;
    }
    frame
}

fn short_group(codes: &[u8], dependents: u8) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (index, &code) in codes.iter().enumerate() {
        bytes.extend(short_frame(code, None, index == 0));
        for id in 0..dependents {
            bytes.extend(short_frame(code, Some(id), false));
        }
    }
    bytes
}

#[test]
fn atsc_short_independent_groups_keep_syncframes_duration_and_original_clocks() {
    use super::Time;
    for codes in [&[0; 6][..], &[1; 3], &[2; 2], &[0, 1, 2]] {
        for dependents in [0, 2, 8] {
            let group = short_group(codes, dependents);
            let mut ts = Transport::new();
            ts.tables(&[(0x87, PID, &[])], 0);
            ts.pes(PID, 0xbd, &group.repeat(2), Some(90_000), Some(80_000));
            ts.pes(PID, 0xbd, &group, Some(180_000), None);
            for (window, work) in [(1, 1), (4096, 4096)] {
                let mut drive = Drive::new(&ts.bytes, limits(), window, work);
                let mut packets = Vec::new();
                while let Some(event) = drive.next().unwrap() {
                    if let Event::Packet(packet) = event {
                        packets.push(packet);
                    }
                }
                assert_eq!(packets.len(), 3);
                for (index, packet) in packets.iter().enumerate() {
                    assert_eq!(packet.payload(), group);
                    assert_eq!(packet.pts(), [Some(90_000), None, Some(180_000)][index]);
                    assert_eq!(packet.dts(), (index == 0).then_some(80_000));
                    assert_eq!(packet.duration(), Some(2880));
                    assert_eq!(
                        packet.sample_duration(),
                        Some(Time::new(1536, 48_000).unwrap())
                    );
                    let AccessUnit::Ac3(unit) = packet.access_unit() else {
                        panic!("E-AC-3 group")
                    };
                    let facts: Vec<_> = unit.syncframes().collect();
                    assert_eq!(facts.len(), codes.len() * (usize::from(dependents) + 1));
                    assert_eq!(facts[0].header.blocks, [1, 2, 3][usize::from(codes[0])]);
                    for (frame_index, facts) in facts.iter().enumerate() {
                        assert_eq!(
                            facts.offset,
                            (index * group.len() + frame_index * 32) as u64
                        );
                    }
                    assert_eq!(packet.sources().len(), 1);
                    assert_eq!(
                        packet.sources()[0].payload_range,
                        if index == 1 {
                            group.len()..2 * group.len()
                        } else {
                            0..group.len()
                        }
                    );
                }
            }
        }
    }
}

#[test]
fn atsc_short_groups_refuse_missing_sync_incomplete_blocks_and_bad_dependents() {
    let mut no_sync = short_group(&[2, 2], 0);
    no_sync[6] &= !4;
    let mut wrong_rate = short_group(&[2, 2], 0);
    wrong_rate[32 + 4] |= 0x40;
    let mut missing_dependent = short_group(&[2, 2], 1);
    missing_dependent.truncate(missing_dependent.len() - 32);
    let mut wrong_id = short_group(&[2, 2], 1);
    wrong_id[32 + 2] |= 8;
    let mut wrong_blocks = short_group(&[2, 2], 1);
    wrong_blocks[32 + 4] &= !0x30;
    for (body, expected) in [
        (no_sync, Error::Invalid("ATSC E-AC-3 first converter sync")),
        (
            short_group(&[2], 0),
            Error::Invalid("ATSC E-AC-3 incomplete six-block group"),
        ),
        (
            short_group(&[1, 1, 2], 0),
            Error::Invalid("ATSC E-AC-3 independent block sequence"),
        ),
        (
            wrong_rate,
            Error::Invalid("ATSC E-AC-3 independent block sequence"),
        ),
        (
            missing_dependent,
            Error::Invalid("ATSC E-AC-3 incomplete six-block group"),
        ),
        (
            wrong_id,
            Error::Invalid("DVB E-AC-3 dependent substream sequence"),
        ),
        (
            wrong_blocks,
            Error::Invalid("DVB E-AC-3 dependent substream sequence"),
        ),
    ] {
        let mut ts = Transport::new();
        ts.tables(&[(0x87, PID, &[])], 0);
        ts.pes(PID, 0xbd, &body, Some(90_000), None);
        let mut drive = Drive::new(&ts.bytes, limits(), 1, 1);
        loop {
            match drive.next() {
                Err(error) => {
                    assert_eq!(error, expected);
                    break;
                }
                Ok(Some(Event::Packet(_)) | None) => panic!("invalid short group accepted"),
                Ok(Some(_)) => (),
            }
        }
    }
}

#[test]
fn atsc_short_group_spans_many_carriers_but_never_two_pes() {
    // Six one-block intervals, each with all eight dependent IDs. Each
    // syncframe is independently length-delimited and has an opaque body.
    let group = short_group(&[0; 6], 8);
    let mut large = Vec::new();
    for frame in group.chunks_exact(32) {
        let start = large.len();
        large.extend_from_slice(frame);
        large[start + 2] |= 1;
        large[start + 3] = 0xff; // 512 words = 1024 bytes.
        large.resize(start + 1024, 0);
    }
    assert_eq!(large.len(), 55_296);
    let mut ts = Transport::new();
    ts.tables(&[(0x87, PID, &[])], 0);
    ts.pes(PID, 0xbd, &large, Some(90_000), None);
    let mut drive = Drive::new(&ts.bytes, limits(), 188, 4096);
    let mut count = 0;
    while let Some(event) = drive.next().unwrap() {
        if let Event::Packet(packet) = event {
            assert_eq!(packet.payload(), large);
            assert_eq!(packet.duration(), Some(2880));
            assert_eq!(packet.sources().len(), 1);
            count += 1;
        }
    }
    assert_eq!(count, 1);

    let group = short_group(&[2, 2], 1);
    let mut ts = Transport::new();
    ts.tables(&[(0x87, PID, &[])], 0);
    ts.pes(PID, 0xbd, &group[..64], Some(90_000), None);
    ts.pes(PID, 0xbd, &group[64..], Some(180_000), None);
    let mut drive = Drive::new(&ts.bytes, limits(), 1, 1);
    loop {
        match drive.next() {
            Err(error) => {
                assert_eq!(error, Error::Invalid("ATSC E-AC-3 access unit crosses PES"));
                break;
            }
            Ok(Some(Event::Packet(_)) | None) => panic!("group crossed PES"),
            Ok(Some(_)) => (),
        }
    }
}

#[test]
fn atsc_short_groups_count_access_units_instead_of_syncframes() {
    let group = short_group(&[0; 6], 8);
    let mut ts = Transport::new();
    ts.tables(&[(0x87, PID, &[])], 0);
    ts.pes(PID, 0xbd, &group.repeat(2), Some(90_000), None);
    let mut drive = Drive::new(
        &ts.bytes,
        ReaderLimits {
            maximum_access_units: 1,
            ..limits()
        },
        47,
        1,
    );
    let mut count = 0;
    loop {
        match drive.next() {
            Ok(Some(Event::Packet(packet))) => {
                assert_eq!(packet.payload(), group);
                count += 1;
            }
            Ok(Some(_)) => (),
            Err(Error::Limit {
                requested: 2,
                maximum: 1,
            }) => break,
            other => panic!("unexpected AU limit result {other:?}"),
        }
    }
    assert_eq!(count, 1);
}
