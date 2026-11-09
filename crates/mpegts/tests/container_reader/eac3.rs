use super::{
    AccessUnit, Drive, Event, PID, PacketInfo, ReaderEvent, ReaderLimits, Time, Transport,
};
use super::{AudioEofPolicy, CodecId, Error, TraceEvent, TrackInfo, audio_eof_namespace};
use triergon_framing::Ac3FrameDetails;

const EAC3_TAG: &[u8] = &[0x7a, 1, 0];
const CODED_TS: &[u8] = include_bytes!("../../../../harness/fixtures/hls/dvb-hevc-eac3-32000.ts");

pub(super) fn coded_primary() -> Vec<u8> {
    // The first audio PES in the authored 32 kHz fixture contains one
    // 576-byte syncframe. Only the four coded slices are rewrapped below.
    [5848..6016, 6020..6204, 6208..6392, 6540..6580]
        .into_iter()
        .flat_map(|range| CODED_TS[range].iter().copied())
        .collect()
}

fn dependent(substream_id: u8) -> Vec<u8> {
    // Authored E-AC-3 envelope probe for 32 kHz and six blocks, with the
    // dependent ID set for each sequence under test.
    let probe = include_str!("../../../framing/fixtures/eac3-bsi-v1.tsv")
        .lines()
        .find_map(|line| {
            let (name, rest) = line.split_once('\t')?;
            (name == "original-carriage-eac3-fs-2-blocks-3-stream-1")
                .then(|| rest.split('\t').next().unwrap())
        })
        .unwrap();
    let mut frame: Vec<_> = (0..probe.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&probe[at..at + 2], 16).unwrap())
        .collect();
    frame[2] = (frame[2] & 0xc7) | (substream_id << 3);
    frame
}

fn limits() -> ReaderLimits {
    ReaderLimits {
        descriptor_namespace: audio_eof_namespace(),
        ..ReaderLimits::default()
    }
}

fn transport(descriptors: &[u8]) -> Transport {
    let mut ts = Transport::new();
    ts.tables(&[(0x06, PID, descriptors)], 0);
    ts
}

#[test]
fn coded_dvb_transport_accepts_the_corroborating_elementary_registration() {
    let mut drive = Drive::new(CODED_TS, limits(), 188, 1024);
    let mut count = 0;
    while let Some(event) = drive.next().unwrap() {
        if let Event::Packet(packet) = event
            && let AccessUnit::Ac3(unit) = packet.access_unit()
        {
            if count == 0 {
                assert_eq!(packet.payload(), coded_primary());
            }
            assert_eq!(unit.syncframes().count(), 1);
            assert_eq!(unit.facts().header.sample_rate_hz, 32_000);
            assert_eq!(
                packet.sample_duration(),
                Some(Time::new(1536, 32_000).unwrap())
            );
            count += 1;
        }
    }
    assert_eq!(count, 42);
}

#[test]
fn queued_eac3_pes_survives_lane_pressure_and_short_elementary_windows() {
    let frame = coded_primary();
    let mut ts = transport(EAC3_TAG);
    ts.pes(PID, 0xbd, &frame, Some(90_000), Some(80_000));
    let limits = ReaderLimits {
        elementary_input_window: 7,
        ..limits()
    };
    let mut drive = Drive::new(&ts.bytes, limits, 188, 1);
    let original = loop {
        let event = drive.next().unwrap().expect("source PES before EOF");
        if let Event::Other(ReaderEvent::Source(source)) = event
            && matches!(source.event(), TraceEvent::Pes(pes) if pes.pid == PID)
        {
            break source;
        }
    };
    let pressure = drive.pool.reserve(drive.pool.available()).unwrap();
    assert!(matches!(
        drive.next(),
        Err(Error::Backpressure { available: 0, .. })
    ));
    drop(pressure);
    let packet = loop {
        if let Event::Packet(packet) = drive.next().unwrap().expect("audio packet before EOF") {
            break packet;
        }
    };
    drive.reader.close();
    assert_eq!(packet.payload(), frame);
    assert_eq!((packet.pts(), packet.dts()), (Some(90_000), Some(80_000)));
    let [source] = packet.sources() else {
        panic!("one original PES");
    };
    assert_eq!(source.payload_range, 0..frame.len());
    assert!(std::ptr::eq(source.pes.event(), original.event()));
}

#[test]
fn coded_primary_and_dependents_share_one_span_and_keep_their_pes_anchors() {
    let primary = coded_primary();
    let dependent0 = dependent(0);
    let dependent1 = dependent(1);
    let group = [&primary[..], &dependent0, &dependent1].concat();
    let cut = primary.len() + 5;
    let descriptors = [0x7a, 1, 0, 0xfe, 1, 0x55, 0x7a, 1, 0];
    let mut ts = transport(&descriptors);
    ts.pes(PID, 0xbd, &group[..cut], Some(90_000), Some(80_000));
    ts.pes(
        PID,
        0xbd,
        &[&group[cut..], &primary].concat(),
        Some(180_000),
        None,
    );
    let mut drive = Drive::new(&ts.bytes, limits(), 1, 1);
    let mut packets = Vec::new();
    while let Some(event) = drive.next().unwrap() {
        if let Event::Packet(packet) = event {
            packets.push(packet);
        }
    }
    assert_eq!(packets.len(), 2);
    let first = packets[0].clone();
    let last = &packets[1];
    drive.reader.close();
    assert_eq!(first.payload(), group);
    assert_eq!(first.track_info().codec(), CodecId::Eac3);
    assert_eq!(first.track_info().codec_tag(), [6]);
    assert_eq!(
        first.track_info().stream().descriptor_bytes.as_ref(),
        descriptors
    );
    assert_eq!(first.pts(), Some(90_000));
    assert_eq!(first.dts(), Some(80_000));
    assert_eq!(
        first.sample_duration(),
        Some(Time::new(1536, 32_000).unwrap())
    );
    assert_eq!(first.duration(), Some(4320));
    assert_eq!(first.elementary_range(), 0..group.len() as u64);
    assert_eq!(first.sources().len(), 2);
    assert_eq!(first.sources()[0].payload_range, 0..cut);
    assert_eq!(
        first.sources()[1].elementary_range,
        cut as u64..group.len() as u64
    );
    assert_eq!(first.sources()[1].payload_range, 0..group.len() - cut);
    let AccessUnit::Ac3(unit) = first.access_unit() else {
        panic!("expected E-AC-3 group");
    };
    assert_eq!(unit.bytes().as_ref(), group);
    let frames: Vec<_> = unit.syncframes().collect();
    assert_eq!(frames.len(), 3);
    assert_eq!(unit.facts(), frames[0]);
    assert_eq!(frames[1].offset, primary.len() as u64);
    assert_eq!(frames[2].offset, (primary.len() + dependent0.len()) as u64);
    for (id, frame) in frames[1..].iter().enumerate() {
        assert!(matches!(
            frame.header.details,
            Ac3FrameDetails::Eac3 { stream_type: 1, substream_id, .. } if usize::from(substream_id) == id
        ));
    }
    assert_eq!(last.payload(), primary);
    assert_eq!(last.pts(), Some(180_000));
    assert_eq!(last.dts(), None);
    assert_eq!(last.sources()[0].payload_range.start, group.len() - cut);
    assert_eq!(last.sample_duration(), first.sample_duration());
    let charge = drive.pool.used();
    let retained_unit = unit.clone();
    assert_eq!(drive.pool.used(), charge);
    drop(first);
    drop(packets);
    assert_eq!(retained_unit.bytes().as_ref(), group);
    assert_eq!(retained_unit.syncframes().count(), 3);
    drop(retained_unit);
    assert_eq!(
        drive.pool.used(),
        triergon_core::Budget::control_storage_bytes()
    );
}

#[test]
fn eac3_requires_unambiguous_dvb_descriptor_evidence() {
    let frame = coded_primary();
    for (stream_type, descriptor, config) in [
        (0x06, EAC3_TAG, ReaderLimits::default()),
        (0x87, EAC3_TAG, limits()),
        (0x06, &[0x6a, 1, 0, 0x7a, 1, 0][..], limits()),
        (0x06, &[0x7a, 2, 4, 0][..], limits()),
        (0x06, b"\x05\x04AC-3\x7a\x01\x00", limits()),
        (0x06, b"\x05\x04EAC3", limits()),
    ] {
        let mut ts = Transport::new();
        ts.tables(&[(stream_type, PID, descriptor)], 0);
        ts.pes(PID, 0xbd, &frame, Some(90_000), None);
        let mut drive = Drive::new(&ts.bytes, config, 47, 11);
        let mut refused = 0;
        let mut source_pes = 0;
        while let Some(event) = drive.next().unwrap() {
            match event {
                Event::Packet(_) => panic!("unsupported carriage emitted an AU"),
                Event::Other(ReaderEvent::Unsupported { .. }) => refused += 1,
                Event::Other(ReaderEvent::Source(source))
                    if matches!(source.event(), TraceEvent::Pes(_)) =>
                {
                    source_pes += 1;
                }
                _ => (),
            }
        }
        assert_eq!((refused, source_pes), (1, 1));
    }
}

fn expect_error(body: &[u8], stream_id: u8, config: ReaderLimits, expected: Error) {
    let mut ts = transport(EAC3_TAG);
    ts.pes(PID, stream_id, body, Some(90_000), None);
    let mut drive = Drive::new(&ts.bytes, config, 13, 3);
    loop {
        match drive.next() {
            Err(error) => {
                assert_eq!(error, expected);
                break;
            }
            Ok(Some(Event::Packet(_) | Event::Other(ReaderEvent::IncompleteAudio(_))) | None) => {
                panic!("unvalidated group was accepted");
            }
            Ok(Some(_)) => (),
        }
    }
}

#[test]
fn malformed_dependents_and_syncframe_limit_prevent_publication() {
    let first = coded_primary();
    let dep = dependent(0);
    let mut wrong_rate = dep.clone();
    wrong_rate[4] &= 0x3f;
    let mut short_blocks = dep.clone();
    short_blocks[4] &= 0xcf;
    expect_error(
        &dep,
        0xbd,
        limits(),
        Error::Invalid("DVB E-AC-3 dependent without primary"),
    );
    for tail in [
        dependent(1),
        wrong_rate,
        short_blocks,
        [&dep[..], &dep].concat(),
    ] {
        expect_error(
            &[&first[..], &tail].concat(),
            0xbd,
            limits(),
            Error::Invalid("DVB E-AC-3 dependent substream sequence"),
        );
    }
    expect_error(
        &first,
        0xbd,
        ReaderLimits {
            maximum_audio_frame_bytes: first.len() - 1,
            ..limits()
        },
        Error::Limit {
            requested: first.len(),
            maximum: first.len() - 1,
        },
    );
    expect_error(
        &first,
        0xc0,
        limits(),
        Error::Invalid("DVB E-AC-3 PES stream ID"),
    );
}

#[test]
fn group_may_exceed_the_default_syncframe_limit() {
    let mut group = coded_primary();
    // Complete envelope probes extend the coded primary's group beyond the
    // configured per-frame ceiling. The framer keeps their coded bodies opaque.
    for id in 0..4 {
        let mut frame = dependent(id);
        let bytes = 2048;
        let size_code = bytes / 2 - 1;
        frame[2] = (frame[2] & !7) | (size_code >> 8) as u8;
        frame[3] = size_code as u8;
        frame.resize(bytes, 0);
        group.extend(frame);
    }
    assert!(group.len() > limits().maximum_audio_frame_bytes);

    for incomplete in [false, true] {
        let mut payload = group.clone();
        if incomplete {
            payload.extend_from_slice(&dependent(4)[..31]);
        }
        let mut ts = transport(EAC3_TAG);
        ts.pes(PID, 0xbd, &payload, Some(90_000), None);
        let mut drive = Drive::new(
            &ts.bytes,
            ReaderLimits {
                audio_eof_policy: if incomplete {
                    AudioEofPolicy::ReportIncomplete
                } else {
                    AudioEofPolicy::RejectIncomplete
                },
                ..limits()
            },
            47,
            7,
        );
        let mut count = 0;
        while let Some(event) = drive.next().unwrap() {
            let packet = match event {
                Event::Packet(packet) if !incomplete => {
                    let AccessUnit::Ac3(unit) = packet.access_unit() else {
                        panic!("expected E-AC-3 group");
                    };
                    assert_eq!(unit.syncframes().count(), 5);
                    assert_eq!(packet.duration(), Some(4320));
                    packet
                }
                Event::Other(ReaderEvent::IncompleteAudio(packet)) if incomplete => {
                    assert_eq!(packet.sample_duration(), None);
                    packet
                }
                Event::Packet(_) | Event::Other(ReaderEvent::IncompleteAudio(_)) => {
                    panic!("incorrect E-AC-3 completion");
                }
                _ => continue,
            };
            assert_eq!(packet.payload(), payload);
            assert_eq!(packet.pts(), Some(90_000));
            count += 1;
        }
        assert_eq!(count, 1);
    }
}

#[test]
fn eight_dependents_count_as_one_access_unit() {
    let first = coded_primary();
    let mut group = first.clone();
    for id in 0..8 {
        group.extend(dependent(id));
    }
    let mut ts = transport(EAC3_TAG);
    ts.pes(PID, 0xbd, &[&group[..], &first].concat(), None, None);
    let mut drive = Drive::new(
        &ts.bytes,
        ReaderLimits {
            maximum_access_units: 1,
            ..limits()
        },
        47,
        1,
    );
    let mut complete = 0;
    loop {
        match drive.next() {
            Ok(Some(Event::Packet(packet))) => {
                let AccessUnit::Ac3(unit) = packet.access_unit() else {
                    panic!("expected E-AC-3 group");
                };
                assert_eq!(unit.syncframes().count(), 9);
                assert_eq!(packet.payload(), group);
                complete += 1;
            }
            Ok(Some(_)) => (),
            Err(Error::Limit {
                requested: 2,
                maximum: 1,
            }) => break,
            other => panic!("unexpected AU count result {other:?}"),
        }
    }
    assert_eq!(complete, 1);
}

#[test]
fn truncated_next_syncframe_leaves_the_group_unfinished() {
    let first = coded_primary();
    let dep = dependent(0);
    for tail in [&dep[..dep.len() - 1], &first[..first.len() - 1]] {
        let payload = [&first[..], tail].concat();
        expect_error(&payload, 0xbd, limits(), Error::Truncated);

        let mut ts = transport(EAC3_TAG);
        ts.pes(PID, 0xbd, &payload, Some(90_000), None);
        let mut drive = Drive::new(
            &ts.bytes,
            ReaderLimits {
                audio_eof_policy: AudioEofPolicy::ReportIncomplete,
                ..limits()
            },
            13,
            1,
        );
        let mut incomplete = 0;
        while let Some(event) = drive.next().unwrap() {
            match event {
                Event::Packet(_) => panic!("unfinished group emitted as a complete unit"),
                Event::Other(ReaderEvent::IncompleteAudio(packet)) => {
                    assert_eq!(packet.payload(), payload);
                    assert_eq!(packet.sample_duration(), None);
                    assert_eq!(packet.pts(), Some(90_000));
                    incomplete += 1;
                }
                _ => (),
            }
        }
        assert_eq!(incomplete, 1);
    }
}

#[test]
fn discontinuity_interrupts_pending_group_and_starts_fresh() {
    let first = coded_primary();
    let group = [&first[..], &dependent(0)].concat();
    let mut ts = transport(EAC3_TAG);
    ts.pes(PID, 0xbd, &group, Some(90_000), None);
    ts.pcr(180_000, true);
    ts.pes(PID, 0xbd, &first, Some(180_000), None);
    let mut drive = Drive::new(&ts.bytes, limits(), 1, 1);
    let mut interrupted = 0;
    let mut complete = 0;
    while let Some(event) = drive.next().unwrap() {
        match event {
            Event::Packet(packet) => {
                assert_eq!(packet.payload(), first);
                assert_eq!(packet.pts(), Some(180_000));
                assert_eq!(packet.elementary_range(), 0..first.len() as u64);
                complete += 1;
            }
            Event::Other(ReaderEvent::Interrupted {
                elementary_range, ..
            }) => {
                assert_eq!(elementary_range, 0..group.len() as u64);
                interrupted += 1;
            }
            _ => (),
        }
    }
    assert_eq!((interrupted, complete), (1, 1));
}

#[test]
fn direct_atsc_eac3_preserves_two_units_in_one_pes() {
    use triergon_mpegts::psi::DescriptorNamespace;

    let frame = coded_primary();
    let descriptors = [0xcc, 2, 0x80, 0xc2];
    let mut ts = Transport::new();
    ts.tables(&[(0x87, PID, &descriptors)], 0);
    ts.pes(PID, 0xbd, &frame.repeat(2), Some(90_000), None);
    let mut drive = Drive::new(
        &ts.bytes,
        ReaderLimits {
            descriptor_namespace: DescriptorNamespace::Atsc,
            ..ReaderLimits::default()
        },
        1,
        1,
    );
    let mut units = 0;
    while let Some(event) = drive.next().unwrap() {
        if let Event::Packet(packet) = event {
            let AccessUnit::Ac3(unit) = packet.access_unit() else {
                panic!("expected E-AC-3");
            };
            assert_eq!(unit.syncframes().count(), 1);
            assert_eq!(packet.track_info().codec(), CodecId::Eac3);
            assert_eq!(packet.track_info().codec_tag(), [0x87]);
            assert_eq!(
                packet.track_info().stream().descriptor_bytes.as_ref(),
                descriptors
            );
            assert_eq!(packet.payload(), frame);
            assert_eq!(packet.pts(), if units == 0 { Some(90_000) } else { None });
            assert_eq!(packet.duration(), Some(4320));
            let [source] = packet.sources() else {
                panic!("one original PES");
            };
            assert_eq!(
                source.payload_range,
                units * frame.len()..(units + 1) * frame.len()
            );
            units += 1;
        }
    }
    assert_eq!(units, 2);
}

#[test]
fn direct_atsc_eac3_refuses_an_access_unit_split_between_pes_packets() {
    use triergon_mpegts::psi::DescriptorNamespace;

    let frame = coded_primary();
    let mut ts = Transport::new();
    ts.tables(&[(0x87, PID, &[0xcc, 2, 0x80, 0xc2])], 0);
    let cut = frame.len() / 2;
    ts.pes(PID, 0xbd, &frame[..cut], Some(90_000), None);
    ts.pes(PID, 0xbd, &frame[cut..], None, None);
    let mut drive = Drive::new(
        &ts.bytes,
        ReaderLimits {
            descriptor_namespace: DescriptorNamespace::Atsc,
            ..ReaderLimits::default()
        },
        11,
        5,
    );
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
