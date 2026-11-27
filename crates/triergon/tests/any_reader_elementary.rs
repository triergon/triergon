//! Elementary audio streams open from their first bytes and read through the neutral model.
#![cfg(feature = "elementary")]
mod common;

use triergon::{
    AnyReader, Budget, CodecId, ContainerReader, MediaKind, PacketInfo, Probed, SourceFormat,
    open_blocking, open_sequential, probe,
};
use triergon_bitstream::crc::{crc8, crc16_a52};

#[test]
fn any_reader_reads_mpeg_audio_frames() {
    // Three MPEG-1 Layer III frames, 128 kbit/s at 44.1 kHz: 417 bytes each.
    let mut frame = vec![0u8; 417];
    frame[..4].copy_from_slice(&0xfffb_9064u32.to_be_bytes());
    let file = frame.repeat(3);
    assert_eq!(probe(&file), Probed::Format(SourceFormat::MpegAudio));
    let (reader, packets) = common::read(&file);
    let track = reader.track(0).unwrap();
    assert_eq!(track.kind(), MediaKind::Audio);
    assert_eq!(track.codec(), CodecId::MpegAudio);
    assert_eq!(packets.len(), 3);
    assert!(packets.iter().all(|p| p.payload().len() == 417));
    assert_eq!(packets[2].pts(), Some(2 * 1152));
}

/// A FLAC frame without the stream marker: fixed blocking, 4096 samples, 44.1 kHz,
/// mono, 16 bits, frame `number`, with valid CRCs.
fn flac_frame(number: u8, len: usize) -> Vec<u8> {
    let mut frame = vec![0xff, 0xf8, 0xc9, 0x08, number];
    frame.push(crc8(0, &frame));
    frame.extend((0..len).map(|i| (i % 97) as u8));
    let check = crc16_a52(0, &frame);
    frame.extend(check.to_be_bytes());
    frame
}

#[test]
fn any_reader_reads_flac_that_starts_at_a_frame() {
    let frames: Vec<u8> = (0..4).flat_map(|i| flac_frame(i, 200)).collect();
    // Bytes before the first frame, as a stream joined mid-way has.
    let file = [vec![0x21; 300], frames].concat();
    assert_eq!(probe(&file), Probed::Format(SourceFormat::Flac));
    let (reader, packets) = common::read(&file);
    let track = reader.track(0).unwrap();
    assert_eq!(track.codec(), CodecId::Flac);
    assert!(track.configuration().is_empty(), "no STREAMINFO to give");
    assert_eq!(packets.len(), 4);
    assert!(packets.iter().all(|p| p.payload().len() == 208));
    assert_eq!(packets[3].pts(), Some(3 * 4096));
}

#[test]
fn blocking_probe_preserves_late_elementary_candidates_and_their_priority() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut bytes = vec![0x21; 4000];
    bytes.extend(flac_frame(7, 200));
    bytes.extend(flac_frame(8, 200));
    assert!(matches!(
        open_sequential(&bytes[..], &pool).unwrap().reader(),
        AnyReader::Flac(_)
    ));
    let mut mpeg = vec![0; 417];
    mpeg[..4].copy_from_slice(&0xfffb_9064u32.to_be_bytes());
    bytes.extend(mpeg.repeat(3));
    assert_eq!(probe(&bytes), Probed::Format(SourceFormat::MpegAudio));
    assert!(matches!(
        open_sequential(&bytes[..], &pool).unwrap().reader(),
        AnyReader::MpegAudio(_)
    ));
    assert!(matches!(
        open_blocking(std::io::Cursor::new(bytes), &pool)
            .unwrap()
            .reader(),
        AnyReader::MpegAudio(_)
    ));
}
