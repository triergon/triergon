//! Malformed parameter sets and slice headers return errors in every build, including
//! the debug builds that check arithmetic.

use triergon_codec_h264::Decoder;

fn noise(mut seed: u64) -> impl FnMut() -> u64 {
    move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    }
}

fn feed(nals: &[&[u8]]) {
    let mut decoder = Decoder::new();
    for nal in nals {
        let _ = decoder.send_nal(nal, None);
        while decoder.receive().is_some() {}
    }
    decoder.flush();
    while decoder.receive().is_some() {}
}

#[test]
fn a_truncated_sps_is_not_a_panic() {
    feed(&[&[0x67, 66, 0, 30, 0xf8]]);
}

/// Baseline profile, one macroblock, poc type 2; then a PPS.
const SPS: [u8; 6] = [0x67, 66, 0, 30, 0xda, 0x79];
const PPS: [u8; 4] = [0x68, 0xce, 0x38, 0x80];

#[test]
fn the_parameter_sets_of_these_tests_are_valid() {
    let mut decoder = Decoder::new();
    assert!(decoder.send_nal(&SPS, None).is_ok());
    assert!(decoder.send_nal(&PPS, None).is_ok());
}

#[test]
fn zero_bits_past_a_short_header_read_as_long_codes() {
    for header in [0x67u8, 0x68, 0x65, 0x41] {
        for length in 1..48 {
            let mut nal = vec![0; length];
            nal[0] = header;
            feed(&[&SPS, &PPS, &nal]);
        }
    }
}

#[test]
fn random_headers_after_valid_parameter_sets() {
    let mut random = noise(0x9e37_79b9_7f4a_7c15);
    for round in 0..40_000 {
        let header = [0x67u8, 0x68, 0x65, 0x41][round % 4];
        let length = 2 + (random() % 40) as usize;
        let mut nal = vec![header];
        if header == 0x67 {
            // A high profile reaches the chroma, depth and scaling list fields.
            nal.extend([[66, 100, 244, 122][(round / 4) % 4], 0, 30]);
        }
        // Long runs of zeros reach the widest Exp-Golomb codes.
        let zeros = random().is_multiple_of(4);
        nal.extend((1..length).map(|_| {
            let byte = random() as u8;
            if zeros && byte < 200 { 0 } else { byte }
        }));
        feed(&[&SPS, &PPS, &nal]);
    }
}
