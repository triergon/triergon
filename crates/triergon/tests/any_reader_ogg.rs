//! Ogg opens from its first bytes and reads through the neutral model.
#![cfg(feature = "ogg")]
mod common;

use triergon::{
    CodecId, ContainerReader, MediaKind, PacketInfo, Probe,
    ogg::page::{BEGIN_OF_STREAM, END_OF_STREAM, write_page},
    probe,
};

/// An Opus stream: headers, then five 20 ms CELT packets with a 312-sample pre-skip and
/// a final granule that ends 100 samples before the last packet does.
fn opus_file() -> Vec<u8> {
    let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
    let tags = b"OpusTags\x04\x00\x00\x00test\x00\x00\x00\x00".to_vec();
    let mut file = Vec::new();
    write_page(
        BEGIN_OF_STREAM,
        0,
        9,
        0,
        &[head.len() as u8],
        &head,
        &mut file,
    );
    write_page(0, 0, 9, 1, &[tags.len() as u8], &tags, &mut file);
    let packet = [0xfc, 0xff, 0xfe];
    let body: Vec<u8> = packet.repeat(5);
    let granule = 312 + 5 * 960 - 312 - 100;
    write_page(END_OF_STREAM, granule, 9, 2, &[3; 5], &body, &mut file);
    file
}

#[test]
fn any_reader_reads_ogg_opus_with_its_trims() {
    let file = opus_file();
    assert_eq!(probe(&file), Probe::Ogg);
    let (reader, packets) = common::read(&file);
    assert_eq!(reader.track_count(), 1);
    let track = reader.track(0).unwrap();
    assert_eq!(track.kind(), MediaKind::Audio);
    assert_eq!(track.codec(), CodecId::Opus);
    assert!(track.configuration().starts_with(b"OpusHead"));
    assert_eq!(packets.len(), 5);
    assert!(packets.iter().all(|p| p.payload() == [0xfc, 0xff, 0xfe]));
    assert_eq!(packets[0].trim().start, Some(312));
    assert_eq!(packets[4].trim().end, Some(100));
    assert_eq!(packets[1].pts(), Some(960 - 312));
}
