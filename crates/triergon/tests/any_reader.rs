//! The facade opens a source from its first bytes and reads it generically.
#![cfg(feature = "matroska")]
use std::sync::atomic::AtomicBool;
use triergon::{
    AnyReader, Budget, Bytes, ContainerReader, Event, InputWindow, MediaKind, PacketInfo, Probe,
    SourceCapabilities, SourceId, Step, TrackId, WorkBudget, probe,
};

fn element(id: u64, data: &[u8]) -> Vec<u8> {
    let mut out: Vec<_> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|b| *b == 0)
        .collect();
    let width = (1..=8)
        .find(|w| (data.len() as u64) < (1_u64 << (7 * w)) - 1)
        .unwrap();
    out.extend(((1_u64 << (7 * width)) | data.len() as u64).to_be_bytes()[8 - width..].iter());
    out.extend(data);
    out
}
fn uint(id: u64, value: u64) -> Vec<u8> {
    element(id, &value.to_be_bytes())
}

fn matroska() -> Vec<u8> {
    let track = element(
        0xae,
        &[
            uint(0xd7, 2),
            uint(0x73c5, 2),
            uint(0x83, 2),
            element(0x86, b"A_AUTHORED"),
        ]
        .concat(),
    );
    let cluster = element(
        0x1f43b675,
        &[uint(0xe7, 0), element(0xa3, &[0x82, 0, 0, 0x80, 7])].concat(),
    );
    [
        element(0x1a45dfa3, &element(0x4282, b"matroska")),
        element(0x18538067, &[element(0x1654ae6b, &track), cluster].concat()),
    ]
    .concat()
}

#[test]
fn probe_classifies_the_three_container_families() {
    assert_eq!(probe(&matroska()), Probe::Matroska);
    assert_eq!(probe(b"\0\0\0\x18ftypisom"), Probe::Isobmff);
    let mut ts = vec![0u8; 188 * 3];
    for packet in ts.chunks_mut(188) {
        packet[0] = 0x47;
    }
    assert_eq!(probe(&ts), Probe::Ts);
    assert_eq!(probe(b"RIFF"), Probe::Unknown);
}

#[test]
fn any_reader_reads_matroska_through_the_neutral_model() {
    let raw = matroska();
    let pool = Budget::new(1 << 20).unwrap();
    let capabilities = SourceCapabilities {
        length: Some(raw.len() as u64),
        random_access: true,
    };
    let mut reader = AnyReader::open(&raw[..8], &pool, SourceId(1), capabilities).unwrap();
    let cancel = AtomicBool::new(false);
    let mut packets = Vec::new();
    loop {
        match reader.step(&mut WorkBudget::new(4096), &cancel).unwrap() {
            Step::Ready(Event::Packet(p)) => {
                packets.push((p.track(), p.payload().to_vec(), p.pts()));
            }
            Step::Ready(_) | Step::Yield => {}
            Step::NeedInput(r) => {
                let at = r.offset as usize;
                if at >= raw.len() {
                    reader.finish(r.source, r.token).unwrap();
                } else {
                    let bytes =
                        Bytes::copy_from_slice(&pool, &raw[at..raw.len().min(at + r.maximum)])
                            .unwrap();
                    reader
                        .provide(&InputWindow::new(r.source, r.token, r.offset, bytes).unwrap())
                        .unwrap();
                }
            }
            Step::Backpressure(p) => panic!("{p:?}"),
            Step::End(_) => break,
        }
    }
    assert_eq!(packets, [(TrackId(2), vec![7], Some(0))]);
    assert_eq!(reader.track_count(), 1);
    assert_eq!(reader.track(0).unwrap().kind(), MediaKind::Audio);
    let refused = AnyReader::open(b"\0\0\0\x18ftypisom", &pool, SourceId(2), capabilities);
    assert!(matches!(refused, Err(triergon::Error::Unsupported(_))));
}
