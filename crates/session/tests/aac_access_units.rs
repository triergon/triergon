#![cfg(feature = "codecs")]

use triergon::Budget;
use triergon_session::TrackDecoder;

fn asc(
    object: u32,
    core: u32,
    channels: u32,
    extension: u32,
    explicit: Option<u32>,
    frame_960: bool,
) -> Vec<u8> {
    let mut bits = Vec::new();
    let mut put = |value: u32, width: usize| {
        bits.extend((0..width).rev().map(|i| ((value >> i) & 1) as u8));
    };
    put(object, 5);
    put(core, 4);
    put(channels, 4);
    put(extension, 4);
    if let Some(rate) = explicit {
        put(rate, 24);
    }
    put(2, 5);
    put(u32::from(frame_960), 1);
    put(0, 2);
    while bits.len() % 8 != 0 {
        bits.push(0);
    }
    bits.chunks(8)
        .map(|chunk| chunk.iter().fold(0, |v, bit| (v << 1) | bit))
        .collect()
}

#[test]
fn factory_accepts_explicit_stereo_double_rate_sbr() {
    let budget = Budget::new(32 << 20).unwrap();
    for config in [
        asc(5, 6, 2, 3, None, false),
        asc(5, 6, 2, 15, Some(48_000), false),
    ] {
        let decoder = TrackDecoder::aac_access_units_in(&config, &budget).unwrap();
        assert_eq!(decoder.codec(), triergon_session::AudioCodec::Aac);
    }
    assert!(TrackDecoder::aac_access_units_in(&[0x12, 0x10], &budget).is_ok());
}

#[test]
fn factory_refuses_unimplemented_sbr_shapes() {
    let budget = Budget::new(32 << 20).unwrap();
    for config in [
        asc(29, 6, 2, 3, None, false), // PS
        asc(5, 6, 1, 3, None, false),
        asc(5, 6, 2, 6, None, false),
        asc(5, 6, 2, 15, Some(47_999), false),
        asc(5, 6, 2, 3, None, true),
        asc(5, 2, 2, 0, None, false), // core rate outside decoder range
    ] {
        assert!(
            TrackDecoder::aac_access_units_in(&config, &budget).is_err(),
            "{config:02x?}"
        );
    }
}
