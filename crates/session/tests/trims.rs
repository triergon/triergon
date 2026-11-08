//! A clock-paced audio session drops what the container marks for discarding.
#![cfg(feature = "codecs")]
use triergon::ogg::page::{BEGIN_OF_STREAM, END_OF_STREAM, write_page};
use triergon::{Budget, ContainerReader, open_blocking};
use triergon_session::{AudioSession, Discard, PcmBlock, QueueLimits, TrackDecoder};

/// Ogg Opus: five 20 ms CELT packets, a 312-sample pre-skip, and a final granule (which
/// counts the pre-skip) 100 samples before the end of the last packet.
fn opus_file() -> Vec<u8> {
    let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00";
    let tags = b"OpusTags\x04\x00\x00\x00test\x00\x00\x00\x00";
    let mut file = Vec::new();
    write_page(
        BEGIN_OF_STREAM,
        0,
        9,
        0,
        &[head.len() as u8],
        head,
        &mut file,
    );
    write_page(0, 0, 9, 1, &[tags.len() as u8], tags, &mut file);
    let body = [0xfc, 0xff, 0xfe].repeat(5);
    write_page(
        END_OF_STREAM,
        5 * 960 - 100,
        9,
        2,
        &[3; 5],
        &body,
        &mut file,
    );
    file
}

#[test]
fn an_audio_session_applies_pre_skip_and_end_trim() {
    let pool = Budget::new(16 << 20).unwrap();
    let mut source = open_blocking(std::io::Cursor::new(opus_file()), &pool).unwrap();
    let mut session = AudioSession::new(QueueLimits::AUDIO);
    let mut loaded = false;
    while let Some(packet) = source.next_packet().unwrap() {
        if !loaded {
            let track = source.reader().track(0).unwrap();
            session.load(Some(TrackDecoder::new(track).unwrap().into_decoder()));
            loaded = true;
        }
        let payload = triergon::PacketInfo::payload(&packet).to_vec();
        session
            .feed_discarding(&payload, None, Discard::of(&packet))
            .unwrap();
    }
    session.end_of_stream();
    let mut block = PcmBlock::default();
    session.pull(None, 0, &mut block);
    assert_eq!(block.sample_rate, 48_000);
    assert_eq!(block.samples.len(), 2 * (5 * 960 - 312 - 100));
}

#[test]
fn a_track_decoder_charges_its_configuration_to_the_callers_pool() {
    let pool = Budget::new(16 << 20).unwrap();
    let mut source = open_blocking(std::io::Cursor::new(opus_file()), &pool).unwrap();
    source.next_packet().unwrap();
    let track = source.reader().track(0).unwrap();
    let tiny = Budget::new(Budget::control_storage_bytes() + 8).unwrap();
    let refused = TrackDecoder::new_in(track, &tiny).unwrap_err();
    assert!(refused.0.starts_with("Limit"), "{}", refused.0);
    let decoder = TrackDecoder::new_in(track, &Budget::new(1 << 20).unwrap()).unwrap();
    assert_eq!(decoder.codec(), triergon_session::AudioCodec::Opus);
}
