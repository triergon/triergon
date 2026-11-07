//! The Matroska reader through the neutral container model only.
#[path = "support/ebml.rs"]
mod ebml;
use ebml::{element, mkv, uint};
use std::sync::atomic::AtomicBool;
use triergon_core::container::{
    ContainerReader, Event, MediaKind, Origin, PacketInfo, RandomAccess, TrackId, TrackInfo,
};
use triergon_core::{Budget, Bytes, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget};
use triergon_matroska::{Limits, typed::Reader};

/// One video track with explicit primaries and range only, then two blocks.
fn source() -> Vec<u8> {
    let colour = element(0x55b0, &[uint(0x55bb, 9), uint(0x55b9, 2)].concat());
    let video = element(0xe0, &[uint(0xb0, 16), uint(0xba, 16), colour].concat());
    let track = element(
        0xae,
        &[
            uint(0xd7, 7),
            uint(0x73c5, 7),
            uint(0x83, 1),
            element(0x86, b"V_AUTHORED"),
            element(0x63a2, b"cfg"),
            video,
        ]
        .concat(),
    );
    let cluster = element(
        0x1f43b675,
        &[
            uint(0xe7, 5),
            element(0xa3, &[0x87, 0, 0, 0x80, b'k']),
            element(0xa3, &[0x87, 0, 1, 0x00, b'd']),
        ]
        .concat(),
    );
    mkv(&[element(0x1654ae6b, &track), cluster].concat())
}

/// Generic over any container reader: what a generic caller sees.
fn packets<C: ContainerReader>(
    reader: &mut C,
    raw: &[u8],
    pool: &Budget,
) -> Vec<(TrackId, Vec<u8>, Option<i64>, RandomAccess)> {
    let cancel = AtomicBool::new(false);
    let mut out = Vec::new();
    loop {
        match reader.step(&mut WorkBudget::new(4096), &cancel).unwrap() {
            Step::Ready(Event::Packet(p)) => {
                out.push((p.track(), p.payload().to_vec(), p.pts(), p.random_access()));
                assert_eq!(p.dts(), None, "Matroska carries no decode time");
            }
            Step::Ready(_) | Step::Yield => {}
            Step::NeedInput(request) => {
                let at = request.offset as usize;
                if at >= raw.len() {
                    reader.finish(request.source, request.token).unwrap();
                } else {
                    let end = raw.len().min(at + request.maximum);
                    let bytes = Bytes::copy_from_slice(pool, &raw[at..end]).unwrap();
                    reader
                        .provide(
                            &InputWindow::new(request.source, request.token, request.offset, bytes)
                                .unwrap(),
                        )
                        .unwrap();
                }
            }
            Step::Backpressure(pressure) => panic!("ample pool: {pressure:?}"),
            Step::End(_) => return out,
        }
    }
}

#[test]
fn matroska_packets_and_tracks_through_the_neutral_views() {
    let raw = source();
    let pool = Budget::new(1 << 20).unwrap();
    let capabilities = SourceCapabilities {
        length: Some(raw.len() as u64),
        random_access: true,
    };
    let mut reader = Reader::new(&pool, SourceId(3), capabilities, Limits::default(), 1).unwrap();
    let packets = packets(&mut reader, &raw, &pool);
    assert_eq!(
        packets,
        [
            (
                TrackId(7),
                b"k".to_vec(),
                Some(5_000_000),
                RandomAccess::Sync
            ),
            (
                TrackId(7),
                b"d".to_vec(),
                Some(6_000_000),
                RandomAccess::NotSync
            ),
        ]
    );
    assert_eq!(reader.track_count(), 1);
    let track = ContainerReader::track(&reader, 0).unwrap();
    assert_eq!((track.id(), track.kind()), (TrackId(7), MediaKind::Video));
    assert_eq!(
        (track.codec_tag(), track.configuration()),
        (&b"V_AUTHORED"[..], &b"cfg"[..])
    );
    let colour = track.colour().unwrap();
    assert_eq!(
        (colour.primaries.value, colour.primaries.origin),
        (9, Origin::Container)
    );
    assert_eq!(colour.range.origin, Origin::Container);
    assert_eq!(
        (colour.matrix.value, colour.matrix.origin),
        (2, Origin::SpecificationDefault)
    );
    assert_eq!(colour.transfer.origin, Origin::SpecificationDefault);
    assert_eq!(colour.chroma_siting, None);
}
