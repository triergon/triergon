//! Elementary audio streams open from their first bytes and read through the neutral model.
#![cfg(feature = "elementary")]
mod common;

use triergon::{CodecId, ContainerReader, MediaKind, PacketInfo, Probe, probe};

#[test]
fn any_reader_reads_mpeg_audio_frames() {
    // Three MPEG-1 Layer III frames, 128 kbit/s at 44.1 kHz: 417 bytes each.
    let mut frame = vec![0u8; 417];
    frame[..4].copy_from_slice(&0xfffb_9064u32.to_be_bytes());
    let file = frame.repeat(3);
    assert_eq!(probe(&file), Probe::MpegAudio);
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
    fn crc(data: &[u8], width: u32, polynomial: u32) -> u32 {
        let top = 1 << (width - 1);
        let mask = (1u64 << width) as u32 - 1;
        data.iter().fold(0, |mut crc, &byte| {
            crc ^= u32::from(byte) << (width - 8);
            for _ in 0..8 {
                crc = if crc & top != 0 {
                    (crc << 1) ^ polynomial
                } else {
                    crc << 1
                } & mask;
            }
            crc
        })
    }
    let mut frame = vec![0xff, 0xf8, 0xc9, 0x08, number];
    frame.push(crc(&frame, 8, 0x07) as u8);
    frame.extend((0..len).map(|i| (i % 97) as u8));
    let check = crc(&frame, 16, 0x8005) as u16;
    frame.extend(check.to_be_bytes());
    frame
}

#[test]
fn any_reader_reads_flac_that_starts_at_a_frame() {
    let frames: Vec<u8> = (0..4).flat_map(|i| flac_frame(i, 200)).collect();
    // Bytes before the first frame, as a stream joined mid-way has.
    let file = [vec![0x21; 300], frames].concat();
    assert_eq!(probe(&file), Probe::Flac);
    let (reader, packets) = common::read(&file);
    let track = reader.track(0).unwrap();
    assert_eq!(track.codec(), CodecId::Flac);
    assert!(track.configuration().is_empty(), "no STREAMINFO to give");
    assert_eq!(packets.len(), 4);
    assert!(packets.iter().all(|p| p.payload().len() == 208));
    assert_eq!(packets[3].pts(), Some(3 * 4096));
}
