#![cfg(feature = "mpegts")]
use std::sync::atomic::AtomicBool;
use triergon::mpegts::{AccessUnit, ReaderEvent, TraceEvent};
use triergon::{
    AnyOther, AnyPacket, AnyReader, Budget, Bytes, ContainerReader, Event, InputWindow, PacketInfo,
    Probe, SourceCapabilities, SourceId, Step, WorkBudget, probe,
};

#[test]
fn facade_delivers_source_aus_and_every_original_pes_without_decoder_injection() {
    let input = include_bytes!("../../playback/tests/fixtures/hls/baseline-stereo.ts");
    let pool = Budget::new(32 << 20).unwrap();
    let caps = SourceCapabilities {
        length: None,
        random_access: false,
    };
    let mut reader = AnyReader::open(input, &pool, SourceId(301), caps).unwrap();
    assert!(matches!(reader, AnyReader::MpegTs(_)));
    let mut video_source = Vec::new();
    let mut audio_source = Vec::new();
    let mut video_packets = Vec::new();
    let mut audio_packets = Vec::new();
    let mut retained = None;
    let mut count = [0, 0];
    let mut carrier_end = 0;
    for _ in 0..2_000_000 {
        match reader
            .step(&mut WorkBudget::new(31), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(Event::Packet(AnyPacket::MpegTs(packet))) => {
                let target = match packet.access_unit() {
                    AccessUnit::Video(unit) => {
                        assert!(unit.has_vcl);
                        count[0] += 1;
                        &mut video_packets
                    }
                    AccessUnit::Adts(_) => {
                        count[1] += 1;
                        &mut audio_packets
                    }
                    AccessUnit::Ac3(_) | AccessUnit::Loas(_) | AccessUnit::DvbSubtitle(_) => {
                        panic!("authored AVC/AAC input")
                    }
                    AccessUnit::AdtsFragment(_) | AccessUnit::IncompleteAudio { .. } => {
                        panic!("default policy rejects fragments")
                    }
                };
                target.extend_from_slice(packet.payload());
                if retained.is_none() {
                    let used = pool.used();
                    retained = Some(packet.clone());
                    assert_eq!(pool.used(), used);
                }
            }
            Step::Ready(Event::Other(AnyOther::MpegTs(ReaderEvent::Transport(packet)))) => {
                assert_eq!(packet.source_offset, carrier_end as u64);
                let end = carrier_end + packet.source.len();
                assert_eq!(packet.source.as_ref(), &input[carrier_end..end]);
                carrier_end = end;
            }
            Step::Ready(Event::Other(AnyOther::MpegTs(ReaderEvent::Continuity { .. }))) => (),
            Step::Ready(Event::Other(AnyOther::MpegTs(ReaderEvent::Source(source)))) => {
                if let TraceEvent::Pes(pes) = source.event() {
                    let target = if pes.header.stream_id == 0xe0 {
                        &mut video_source
                    } else {
                        &mut audio_source
                    };
                    for bytes in pes.payload() {
                        target.extend_from_slice(bytes);
                    }
                }
            }
            Step::Ready(Event::Other(AnyOther::MpegTs(ReaderEvent::VideoHeaders(packet)))) => {
                video_packets.extend_from_slice(packet.payload());
            }
            Step::Ready(Event::Other(AnyOther::MpegTs(event))) => {
                panic!("unexpected gap: {event:?}")
            }
            Step::Ready(_) | Step::Yield => (),
            Step::NeedInput(r) => {
                let at = r.offset as usize;
                if at == input.len() {
                    reader.finish(r.source, r.token).unwrap();
                } else {
                    let end = input.len().min(at + r.maximum.min(7));
                    let bytes = Bytes::copy_from_slice(&pool, &input[at..end]).unwrap();
                    reader
                        .provide(&InputWindow::new(r.source, r.token, r.offset, bytes).unwrap())
                        .unwrap();
                }
            }
            Step::Backpressure(p) => panic!("{p:?}"),
            Step::End(_) => {
                assert!(count.into_iter().all(|n| n > 0));
                assert_eq!(carrier_end, input.len());
                assert_eq!(video_source, video_packets);
                assert_eq!(audio_source, audio_packets);
                assert_eq!(reader.track_count(), 2);
                assert!((0..2).all(|i| !reader.track(i).unwrap().codec_tag().is_empty()));
                let AnyReader::MpegTs(inner) = &mut reader else {
                    unreachable!()
                };
                inner.close();
                assert!(!retained.as_ref().unwrap().payload().is_empty());
                drop(retained);
                drop(reader);
                assert_eq!(pool.used(), Budget::control_storage_bytes());
                return;
            }
        }
    }
    panic!("facade did not finish");
}

#[test]
fn m2ts_probe_selects_the_192_byte_reader_and_short_prefixes_stay_unknown() {
    let bytes = include_bytes!("../../mpegts/tests/data/m2ts192-opaque-prefix.m2ts");
    assert_eq!(probe(bytes), Probe::M2ts);
    assert_eq!(probe(&bytes[..388]), Probe::Unknown);
    let pool = Budget::new(1 << 20).unwrap();
    let reader = AnyReader::open(
        bytes,
        &pool,
        SourceId(9),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
    )
    .unwrap();
    assert!(matches!(reader, AnyReader::MpegTs(_)));
}
