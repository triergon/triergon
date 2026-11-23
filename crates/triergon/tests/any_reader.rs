//! The facade opens a source from its first bytes and reads it generically.
#![cfg(feature = "matroska")]
mod common;

use triergon::{
    AnyReader, Budget, CodecId, ContainerReader, MediaKind, PacketInfo, Probed, SourceCapabilities,
    SourceFormat, SourceId, TrackId, probe, probe_whole,
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

/// Three TS188 packets: no more than their sync bytes.
fn transport() -> Vec<u8> {
    let mut ts = vec![0u8; 188 * 3];
    for packet in ts.chunks_mut(188) {
        packet[0] = 0x47;
    }
    ts
}

#[test]
fn probe_classifies_the_three_container_families() {
    assert_eq!(probe(&matroska()), Probed::Format(SourceFormat::Matroska));
    assert_eq!(
        probe(b"\0\0\0\x18ftypisom"),
        Probed::Format(SourceFormat::Isobmff)
    );
    assert_eq!(probe(&transport()), Probed::Format(SourceFormat::Ts));
    assert_eq!(probe(b"RIFF"), Probed::Need(8));
    assert_eq!(probe_whole(b"RIFF"), None);
}

#[test]
fn any_reader_reads_matroska_through_the_neutral_model() {
    let (reader, packets) = common::read(&matroska());
    let packets: Vec<_> = packets
        .iter()
        .map(|p| (p.track(), p.payload().to_vec(), p.pts()))
        .collect();
    assert_eq!(packets, [(TrackId(2), vec![7], Some(0))]);
    assert_eq!(reader.track_count(), 1);
    let track = reader.track(0).unwrap();
    assert_eq!(track.kind(), MediaKind::Audio);
    assert_eq!(
        (track.codec(), track.codec_tag()),
        (CodecId::Other, &b"A_AUTHORED"[..])
    );
}

#[test]
fn a_transport_stream_opens_only_with_the_mpegts_feature() {
    let ts = transport();
    let pool = Budget::new(1 << 20).unwrap();
    let capabilities = SourceCapabilities {
        length: Some(ts.len() as u64),
        random_access: true,
    };
    let opened = AnyReader::open(&ts, &pool, SourceId(2), capabilities);
    #[cfg(not(feature = "mpegts"))]
    assert_eq!(
        opened.err(),
        Some(triergon::Error::Unsupported("reader feature not enabled"))
    );
    #[cfg(feature = "mpegts")]
    assert!(matches!(opened, Ok(AnyReader::MpegTs(_))));
}
