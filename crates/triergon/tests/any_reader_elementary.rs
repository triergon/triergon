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
