//! Real streams from the FATE suite, when fetched into `.research/fate-samples/vorbis`:
//! damaged packets never panic and leave the decoder in step with an undamaged one.

use std::path::PathBuf;

use triergon_codec::{AudioDecoder, AudioFrame, Budget};
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
    let budget = Budget::new(limits.live_storage_bytes)?;
    Decoder::from_headers_in(&packets[0], &packets[1], &packets[2], &budget)
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
fn the_default_limit_decodes_every_stream_and_a_small_one_is_refused() {
    let directory =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.research/fate-samples/vorbis");
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for path in entries.flatten().map(|entry| entry.path()) {
        if path.extension().is_none_or(|extension| extension != "ogg") {
            continue;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let Some(packets) = stream(&name) else {
            continue;
        };
        let mut decoder = decoder(&packets, Limits::default()).unwrap();
        let mut frame = AudioFrame::default();
        // The first link's audio packets; a header packet starts the next link.
        for packet in packets[3..]
            .iter()
            .take_while(|p| p.first().is_none_or(|b| b & 1 == 0))
        {
            decoder.decode(packet, &mut frame).unwrap();
        }
    }
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
}
