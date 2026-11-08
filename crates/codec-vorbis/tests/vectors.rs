//! Real streams from the FATE suite, when fetched into `.research/fate-samples/vorbis`:
//! damaged packets never panic and leave the decoder in step with an undamaged one.

use std::path::PathBuf;

use triergon_codec::{AudioDecoder, AudioFrame};
use triergon_codec_vorbis::{DecodeError, Decoder, Limits};

fn stream(name: &str) -> Option<Vec<Vec<u8>>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.research/fate-samples/vorbis")
        .join(name);
    let data = std::fs::read(path).ok()?;
    let packets = triergon::ogg::read_packets(&data).ok()?;
    Some(packets.into_iter().map(|packet| packet.data).collect())
}

fn decoder(packets: &[Vec<u8>], limits: Limits) -> Result<Decoder, DecodeError> {
    Decoder::from_headers(&packets[0], &packets[1], &packets[2], limits)
}

#[test]
fn damaged_packets_leave_the_decoder_usable() {
    for name in ["1.0-test_small.ogg", "6.ogg", "lsp-test_small.ogg"] {
        let Some(packets) = stream(name) else {
            continue;
        };
        let audio = &packets[3..packets.len().min(200)];
        let mut damaged = decoder(&packets, Limits::default()).unwrap();
        let mut frame = AudioFrame::default();
        let mut seed = 0x9e37_79b9u32;
        for packet in audio {
            for len in [0, 1, packet.len() / 3, packet.len() - 1] {
                let _ = damaged.decode(&packet[..len], &mut frame);
            }
            let mut garbage = packet.clone();
            for byte in &mut garbage[1..] {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *byte ^= (seed >> 24) as u8;
            }
            let _ = damaged.decode(&garbage, &mut frame);
        }
        // After a reset the damaged decoder decodes exactly as a fresh one.
        damaged.reset();
        let mut clean = decoder(&packets, Limits::default()).unwrap();
        let mut reference = AudioFrame::default();
        for packet in audio {
            clean.decode(packet, &mut reference).unwrap();
            damaged.decode(packet, &mut frame).unwrap();
            assert_eq!(
                frame.samples_per_channel, reference.samples_per_channel,
                "{name}"
            );
            assert_eq!(frame.planes, reference.planes, "{name}");
        }
    }
}

#[test]
fn setups_need_their_tables_charged() {
    let Some(packets) = stream("1.0-test_small.ogg") else {
        return;
    };
    let small = Limits {
        live_storage_bytes: 32 << 10,
    };
    assert!(matches!(
        decoder(&packets, small),
        Err(DecodeError::Budget(_))
    ));
    assert!(decoder(&packets, Limits::default()).is_ok());
}
