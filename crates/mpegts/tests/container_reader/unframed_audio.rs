use super::{Drive, ID, OFF, PID, Transport, adts, audio_eof_frame, eac3};
use triergon_core::container::{CodecId, Event, PacketInfo, TrackInfo};
use triergon_core::{
    Budget, Bytes, Error, InputRequest, InputWindow, SourceCapabilities, SourceFailure, SourceId,
    Step, WorkBudget,
};
use triergon_mpegts::compat::pes::{PrivateAudioCarriage, UnframedAudioPolicy};
use triergon_mpegts::psi::DescriptorNamespace;
use triergon_mpegts::{
    AccessUnit, EntityTransition, ReaderEvent, ReaderLimits, SourceEvent, TraceEvent,
};

const PAYLOAD: &[u8] = b"lane-ts-original-payload\0\x47\xff";
const NEED_PES: Error = Error::Unsupported("unframed audio requires one finite PES");
const SYNC: Error = Error::Unsupported("unframed audio contains a syncframe start");

fn limits() -> ReaderLimits {
    ReaderLimits {
        unframed_audio_policy: UnframedAudioPolicy::ReportAtEof,
        ..ReaderLimits::default()
    }
}

fn input_end(drive: &mut Drive<'_>) -> (InputRequest, SourceEvent) {
    let mut original = None;
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(1);
        match drive.reader.step(&mut work, &OFF).unwrap() {
            Step::Yield
            | Step::Ready(Event::Other(
                ReaderEvent::Transport(_) | ReaderEvent::Continuity { .. },
            )) => (),
            Step::Ready(Event::Other(ReaderEvent::Source(source))) => {
                if matches!(source.event(), TraceEvent::Pes(_)) {
                    assert!(original.replace(source).is_none());
                }
            }
            Step::NeedInput(request) => {
                let at = request.offset as usize;
                if at == drive.raw.len() {
                    drive.catalogue = drive
                        .reader
                        .tracks()
                        .iter()
                        .map(|track| (track.stream().pid, track.configuration_epoch()))
                        .collect();
                    return (request, original.unwrap());
                }
                let end = drive.raw.len().min(at + drive.chunk.min(request.maximum));
                let bytes = Bytes::copy_from_slice(&drive.pool, &drive.raw[at..end]).unwrap();
                drive
                    .reader
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
            }
            other => panic!("output before logical EOF: {other:?}"),
        }
    }
    panic!("no input request at the entity end");
}

#[test]
fn opaque_private_audio_preserves_track_and_pes_at_logical_eof() {
    use PrivateAudioCarriage::{DvbAc3Tag, DvbEac3Tag, PrivateType81Ac3, PrivateType87Eac3};
    for (raw, tag, carriage) in [
        (
            include_bytes!("../data/carriage-ac3-dvb.ts").as_slice(),
            6,
            DvbAc3Tag,
        ),
        (
            include_bytes!("../data/carriage-eac3-dvb.ts").as_slice(),
            6,
            DvbEac3Tag,
        ),
        (
            include_bytes!("../data/carriage-ac3-direct.ts").as_slice(),
            0x81,
            PrivateType81Ac3,
        ),
        (
            include_bytes!("../data/carriage-eac3-direct.ts").as_slice(),
            0x87,
            PrivateType87Eac3,
        ),
    ] {
        let mut strict = Drive::new(raw, ReaderLimits::default(), 188, 64);
        let mut unsupported = 0;
        while let Some(event) = strict.next().unwrap() {
            match event {
                Event::Other(ReaderEvent::Unsupported { track, .. }) => {
                    assert_eq!(track.codec(), CodecId::Other(&[tag]));
                    unsupported += 1;
                }
                Event::Other(
                    ReaderEvent::Source(_)
                    | ReaderEvent::Transport(_)
                    | ReaderEvent::Continuity { .. },
                ) => (),
                other => panic!("unexpected strict output: {other:?}"),
            }
        }
        assert_eq!(unsupported, 1);

        let mut drive = Drive::new(raw, limits(), 13, 1);
        let (end, original) = input_end(&mut drive);
        drive.reader.finish(end.source, end.token).unwrap();
        let pressure = drive.pool.reserve(drive.pool.available()).unwrap();
        assert!(matches!(drive.next(), Err(Error::Backpressure { .. })));
        drop(pressure);
        let Some(Event::Other(ReaderEvent::UnframedAudio(audio))) = drive.next().unwrap() else {
            panic!("missing EOF observation");
        };
        assert!(
            matches!(drive.next().unwrap(), Some(Event::Other(ReaderEvent::Source(source))) if matches!(source.event(), TraceEvent::Finished))
        );
        assert!(drive.next().unwrap().is_none());
        let [origin] = audio.sources() else {
            panic!("expected exactly one PES origin");
        };
        assert!(std::ptr::eq(origin.pes.event(), original.event()));
        assert_eq!(origin.payload_range, 0..PAYLOAD.len());
        assert_eq!(audio.elementary_range(), 0..PAYLOAD.len() as u64);
        assert_eq!(audio.carriage(), carriage);
        let track = audio.track_info();
        assert_eq!(
            track.stream().descriptor_namespace(),
            DescriptorNamespace::Unspecified
        );
        assert_eq!(track.codec(), CodecId::Other(&[tag]));
        let clock = audio.clock().unwrap();
        assert_eq!(clock.pts.ticks90, Some(90_000));
        assert_eq!(clock.dts.ticks90, None);
        assert!(std::ptr::eq(clock.pes.event(), original.event()));
        let used = drive.pool.used();
        let retained = audio.clone();
        assert_eq!(drive.pool.used(), used);
        drive.reader.close();
        drop(audio);
        drop(original);
        assert_eq!(retained.bytes().as_ref(), PAYLOAD);
        drop(retained);
        assert_eq!(drive.pool.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn a_contiguous_entity_end_and_later_transport_failure_emit_no_observation() {
    let raw = include_bytes!("../data/carriage-ac3-direct.ts");
    let mut drive = Drive::new(raw, limits(), 188, 1);
    let (end, _) = input_end(&mut drive);
    let next = SourceId(ID.0 + 1);
    drive
        .reader
        .finish_entity(
            end.source,
            end.token,
            next,
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            EntityTransition::Contiguous,
        )
        .unwrap();
    loop {
        match drive.reader.step(&mut WorkBudget::new(1), &OFF).unwrap() {
            Step::Yield
            | Step::Ready(Event::Other(
                ReaderEvent::Source(_) | ReaderEvent::Transport(_) | ReaderEvent::Continuity { .. },
            )) => (),
            Step::NeedInput(request) => {
                assert_eq!(request.source, next);
                assert_eq!(request.offset, 0);
                drive
                    .reader
                    .fail(next, request.token, SourceFailure::Transport)
                    .unwrap();
                break;
            }
            other => panic!("entity completion emitted {other:?}"),
        }
    }
    assert_eq!(
        drive
            .reader
            .step(&mut WorkBudget::new(1), &OFF)
            .unwrap_err(),
        Error::Transport
    );
}

#[test]
fn a_split_syncword_or_a_second_pes_cannot_become_unframed_audio() {
    let mut split_sync = vec![1; 176];
    // Nine header bytes precede the payload. This syncword straddles carriers.
    split_sync[174..].copy_from_slice(&[0x0b, 0x77]);
    for (payload, following, cap, expected) in [
        (split_sync.as_slice(), false, 8192, SYNC),
        (b"opaque\x0b".as_slice(), false, 8192, SYNC),
        (b"opaque".as_slice(), true, 8192, NEED_PES),
        (
            b"opaque".as_slice(),
            false,
            5,
            Error::Limit {
                requested: 6,
                maximum: 5,
            },
        ),
    ] {
        let mut ts = Transport::new();
        ts.tables(&[(0x81, PID, &[])], 0);
        ts.pes(PID, 0xbd, payload, None, None);
        if following {
            ts.pes(PID, 0xbd, b"second", None, None);
        }
        let mut drive = Drive::new(
            &ts.bytes,
            ReaderLimits {
                maximum_audio_frame_bytes: cap,
                ..limits()
            },
            1,
            1,
        );
        loop {
            match drive.next() {
                Ok(Some(Event::Other(
                    ReaderEvent::Source(_)
                    | ReaderEvent::Transport(_)
                    | ReaderEvent::Continuity { .. },
                ))) => (),
                Err(error) => {
                    assert_eq!(error, expected);
                    break;
                }
                other => panic!("unproved audio became an observation: {other:?}"),
            }
        }
    }
}

#[test]
fn opted_in_reader_keeps_coded_frames_in_the_declared_namespace() {
    for (tag, codec, frame) in [
        (0x81, CodecId::Ac3, audio_eof_frame(true)),
        (0x87, CodecId::Eac3, eac3::coded_primary()),
    ] {
        let mut ts = Transport::new();
        ts.tables(&[(tag, PID, &[])], 0);
        ts.pes(PID, 0xbd, &frame, Some(90_000), None);
        let mut drive = Drive::new(
            &ts.bytes,
            ReaderLimits {
                descriptor_namespace: DescriptorNamespace::Atsc,
                ..limits()
            },
            13,
            1,
        );
        let mut packets = 0;
        while let Some(event) = drive.next().unwrap() {
            match event {
                Event::Packet(packet) => {
                    assert!(matches!(packet.access_unit(), AccessUnit::Ac3(_)));
                    assert_eq!(packet.track_info().codec(), codec);
                    assert_eq!(packet.payload(), frame);
                    assert!(packet.sample_duration().is_some());
                    packets += 1;
                }
                Event::Other(
                    ReaderEvent::Source(_)
                    | ReaderEvent::Transport(_)
                    | ReaderEvent::Continuity { .. },
                ) => (),
                other => panic!("coded frame replaced by {other:?}"),
            }
        }
        assert_eq!(packets, 1);
    }
}

#[test]
fn the_observation_drains_beside_a_framed_track_and_a_conflicting_private_track() {
    let mut ts = Transport::new();
    ts.tables(
        &[
            (0x81, PID, &[]),
            (0x0f, PID + 1, &[]),
            (0x06, PID + 2, &[0x6a, 1, 0, 0x7a, 1, 0]),
        ],
        0,
    );
    let frame = adts(b"aac");
    ts.pes(PID, 0xbd, PAYLOAD, None, None);
    ts.pes(PID + 1, 0xc0, &frame, None, None);
    ts.pes(PID + 2, 0xbd, PAYLOAD, None, None);
    let mut drive = Drive::new(&ts.bytes, limits(), 9, 1);
    let (mut packets, mut opaque, mut unsupported) = (0, 0, 0);
    while let Some(event) = drive.next().unwrap() {
        match event {
            Event::Packet(packet) => {
                assert_eq!(packet.track_info().stream().pid, PID + 1);
                assert_eq!(packet.payload(), frame);
                packets += 1;
            }
            Event::Other(ReaderEvent::Unsupported { track, .. }) => {
                assert_eq!(track.stream().pid, PID + 2);
                unsupported += 1;
            }
            Event::Other(ReaderEvent::UnframedAudio(audio)) => {
                assert_eq!(packets, 1);
                assert_eq!(audio.track_info().stream().pid, PID);
                assert_eq!(audio.bytes().as_ref(), PAYLOAD);
                opaque += 1;
            }
            Event::Other(
                ReaderEvent::Source(_) | ReaderEvent::Transport(_) | ReaderEvent::Continuity { .. },
            ) => (),
            other => panic!("unexpected reader event: {other:?}"),
        }
    }
    assert_eq!((packets, opaque, unsupported), (1, 1, 1));
}
