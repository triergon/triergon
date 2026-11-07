//! ISO BMFF opens from its first bytes and reads through the neutral model.
#![cfg(feature = "isobmff")]
use std::sync::atomic::AtomicBool;
use triergon::{
    AnyOther, AnyPacket, AnyReader, Budget, Bytes, CodecId, ContainerReader, Error, Event,
    InputRequest, InputWindow, MediaKind, PacketInfo, SeekPolicy, SeekRequest, SourceCapabilities,
    SourceId, Step, Time, TrackId, WorkBudget, isobmff::ReaderEvent,
};

const FILE: &[u8] =
    include_bytes!("../../isobmff/tests/fixtures/encoded-config-v3/creation-all.mp4");

fn capabilities() -> SourceCapabilities {
    SourceCapabilities {
        length: Some(FILE.len() as u64),
        random_access: true,
    }
}

fn answer(reader: &mut AnyReader, pool: &Budget, request: InputRequest) {
    let at = request.offset as usize;
    if at >= FILE.len() {
        reader.finish(request.source, request.token).unwrap();
    } else {
        let end = FILE.len().min(at + request.maximum);
        let bytes = Bytes::copy_from_slice(pool, &FILE[at..end]).unwrap();
        let window = InputWindow::new(request.source, request.token, request.offset, bytes);
        reader.provide(&window.unwrap()).unwrap();
    }
}

/// Run to the end: the packets, whether the movie event arrived, any seek landing.
fn drain(reader: &mut AnyReader, pool: &Budget) -> (Vec<AnyPacket>, bool, Option<Time>) {
    let cancel = AtomicBool::new(false);
    let (mut packets, mut movie, mut landing) = (Vec::new(), false, None);
    loop {
        match reader.step(&mut WorkBudget::new(4096), &cancel).unwrap() {
            Step::Ready(Event::Packet(packet)) => packets.push(packet),
            Step::Ready(Event::Seek(receipt)) => landing = Some(receipt.landing),
            Step::Ready(Event::Other(AnyOther::Isobmff(ReaderEvent::Movie))) => movie = true,
            Step::Ready(Event::Other(_)) | Step::Yield => {}
            Step::NeedInput(request) => answer(reader, pool, request),
            Step::Backpressure(pressure) => panic!("{pressure:?}"),
            Step::End(_) => return (packets, movie, landing),
        }
    }
}

#[test]
fn any_reader_reads_isobmff_through_the_neutral_model() {
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = AnyReader::open(&FILE[..16], &pool, SourceId(1), capabilities()).unwrap();
    let (packets, movie, _) = drain(&mut reader, &pool);
    assert!(movie);
    assert_eq!(reader.track_count(), 2);
    let (video, audio) = (reader.track(0).unwrap(), reader.track(1).unwrap());
    assert_eq!(video.kind(), MediaKind::Video);
    assert_eq!(video.codec(), CodecId::H264);
    assert_eq!(video.codec_tag(), b"avc1");
    assert_eq!(audio.kind(), MediaKind::Audio);
    assert_eq!(audio.codec(), CodecId::Aac);
    assert_eq!(audio.codec_tag(), b"mp4a");
    assert!(packets.iter().any(|p| p.track() == TrackId(1)));
    assert!(packets.iter().any(|p| p.track() == TrackId(2)));
    let first = packets.iter().find(|p| p.track() == TrackId(1)).unwrap();
    let AnyPacket::Isobmff(native) = first else {
        panic!("an ISO BMFF packet");
    };
    assert_eq!(first.payload(), &FILE[range(native.sample().range.clone())]);
    assert!(first.dts().is_some());
}

fn range(range: std::ops::Range<u64>) -> std::ops::Range<usize> {
    range.start as usize..range.end as usize
}

#[test]
fn any_reader_seeks_isobmff() {
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = AnyReader::open(&FILE[..16], &pool, SourceId(1), capabilities()).unwrap();
    let (packets, _, _) = drain(&mut reader, &pool);
    let track = TrackId(1);
    let first = packets.iter().find(|p| p.track() == track).unwrap();
    let target = first.pts_time().unwrap().unwrap();
    reader
        .request_seek(SeekRequest {
            track,
            target,
            policy: SeekPolicy::PreviousRandomAccess,
        })
        .unwrap();
    let (replayed, _, landing) = drain(&mut reader, &pool);
    assert_eq!(landing, Some(target));
    let video = |list: &[AnyPacket]| list.iter().filter(|p| p.track() == track).count();
    assert_eq!(video(&replayed), video(&packets));
}

#[test]
fn a_forward_only_isobmff_source_is_refused_at_open() {
    let pool = Budget::new(1 << 20).unwrap();
    let forward = SourceCapabilities {
        length: None,
        random_access: false,
    };
    assert!(matches!(
        AnyReader::open(&FILE[..16], &pool, SourceId(1), forward),
        Err(Error::Unsupported(_))
    ));
}
