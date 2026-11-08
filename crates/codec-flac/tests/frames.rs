//! Hand-built frames: every subframe type, stereo mode, residual coding and the error
//! paths FFmpeg refuses.
mod common;

use common::{configuration, frame, rice_residual, subframe_header, verbatim};
use triergon_codec::AudioDecoder;
use triergon_codec_flac::{
    AudioFrame, Budget, CrcPolicy, DecodeError, Decoder, IntegerFrame, Limits,
};

fn decoder(channels: u8, bits: u8) -> Decoder {
    Decoder::from_configuration(
        &configuration(44100, channels, bits, 4096, &[]),
        Limits::default(),
    )
    .unwrap()
}

fn integers(decoder: &mut Decoder, au: &[u8]) -> Result<Vec<Vec<i32>>, DecodeError> {
    let mut out = IntegerFrame::default();
    decoder.decode_integer(au, &mut out)?;
    Ok(out.planes().map(<[i32]>::to_vec).collect())
}

/// Sample size code 4: 16 bits.
const S16: u8 = 4;

#[test]
fn constant_and_verbatim_subframes() {
    let au = frame(4, 1, S16, &|b| {
        subframe_header(b, 0);
        b.put(-1234, 16);
        verbatim(b, 16, &[1, -2, 32767, -32768]);
    });
    let planes = integers(&mut decoder(2, 16), &au).unwrap();
    assert_eq!(planes, [vec![-1234; 4], vec![1, -2, 32767, -32768]]);
}

#[test]
fn fixed_predictors_of_every_order() {
    let residual = [3, -1, 4, -1, 5, -9, 2, 6, 0, 1, -2, 1];
    for order in 0..=4usize {
        let warm_up = [10i64, 12, 15, 11];
        let au = frame(12, 0, S16, &|b| {
            subframe_header(b, 8 + order as u32);
            for &w in &warm_up[..order] {
                b.put(w, 16);
            }
            rice_residual(b, 2, &residual[..12 - order]);
        });
        let planes = integers(&mut decoder(1, 16), &au).unwrap();
        let mut expected: Vec<i64> = warm_up[..order].to_vec();
        let mut residual = residual.iter().map(|&r| i64::from(r));
        let coefficients: &[i64] = [&[][..], &[1], &[2, -1], &[3, -3, 1], &[4, -6, 4, -1]][order];
        while expected.len() < 12 {
            let n = expected.len();
            let prediction: i64 = coefficients
                .iter()
                .enumerate()
                .map(|(j, c)| c * expected[n - 1 - j])
                .sum();
            expected.push(prediction + residual.next().unwrap());
        }
        let expected: Vec<i32> = expected.iter().map(|&v| v as i32).collect();
        assert_eq!(planes[0], expected, "order {order}");
    }
}

#[test]
fn lpc_with_shift_wasted_bits_and_escaped_partitions() {
    // Order 2, precision 5, shift 2, coefficients [5, -2] (newest first), one wasted bit,
    // partition order 1 with Rice2: a Rice partition, then an escaped one of 4 bits.
    let au = frame(8, 0, S16, &|b| {
        b.put(0, 1).put(0x20 | 1, 6).put(1, 1).put(1, 1); // LPC order 2, 1 wasted bit
        b.put(100, 15).put(-50, 15);
        b.put(4, 4).put(2, 5).put(5, 5).put(-2, 5);
        b.put(1, 2).put(1, 4); // Rice2, partition order 1
        b.put(3, 5);
        for r in [7, -3] {
            b.rice(r, 3);
        }
        b.put(31, 5).put(4, 5);
        for r in [-8, 7, 0, -1] {
            b.put(r, 4);
        }
    });
    let planes = integers(&mut decoder(1, 16), &au).unwrap();
    let mut expected = vec![100i64, -50];
    for r in [7, -3, -8, 7, 0, -1] {
        let n = expected.len();
        expected.push(r + ((5 * expected[n - 1] - 2 * expected[n - 2]) >> 2));
    }
    let expected: Vec<i32> = expected.iter().map(|&v| (v << 1) as i32).collect();
    assert_eq!(planes[0], expected);
}

/// Codes `left` and `right` in every decorrelated mode at `bits` bits per sample
/// (size code `size_code`) and checks that each decodes back.
fn check_stereo(left: &[i64], right: &[i64], bits: u32, size_code: u8) -> Decoder {
    let side: Vec<i64> = left.iter().zip(right).map(|(l, r)| l - r).collect();
    let mid: Vec<i64> = left.iter().zip(right).map(|(l, r)| (l + r) >> 1).collect();
    let mut flac = decoder(2, bits as u8);
    let (side, mid) = (side.as_slice(), mid.as_slice());
    for (assignment, channels) in [
        (8, [(left, bits), (side, bits + 1)]),
        (9, [(side, bits + 1), (right, bits)]),
        (10, [(mid, bits), (side, bits + 1)]),
    ] {
        let au = frame(left.len() as u32, assignment, size_code, &|b| {
            for (samples, width) in channels {
                verbatim(b, width, samples);
            }
        });
        let planes = integers(&mut flac, &au).unwrap();
        let expected = [left, right].map(|c| c.iter().map(|&v| v as i32).collect::<Vec<_>>());
        assert_eq!(planes, expected, "assignment {assignment}");
    }
    flac
}

#[test]
fn stereo_decorrelation_modes() {
    check_stereo(&[1000, -5, 7, -32768], &[990, 6, 7, 32767], 16, S16);
}

#[test]
fn thirty_two_bit_side_channels_have_33_bits() {
    let (max, min) = (i64::from(i32::MAX), i64::from(i32::MIN));
    let mut flac = check_stereo(&[max, min, 0, -1], &[min, max, 5, 1], 32, 7);
    let au = frame(2, 1, 7, &|b| {
        verbatim(b, 32, &[max, min]);
        verbatim(b, 32, &[0, 0]);
    });
    let mut float = AudioFrame::default();
    flac.decode_float(&au, &mut float).unwrap();
    assert_eq!(float.planes[0], [1.0, -1.0], "i32::MAX rounds to 2^31");
}

#[test]
fn float_output_scales_as_ffmpeg_converts() {
    // 8 bits: s16 output, x << 8 / 2^15. 24 bits: s32 output, x << 8 / 2^31.
    let au = frame(2, 0, 1, &|b| verbatim(b, 8, &[-128, 127]));
    let mut out = AudioFrame::default();
    decoder(1, 8).decode_float(&au, &mut out).unwrap();
    assert_eq!(out.planes[0], [-1.0, 127.0 / 128.0]);
    let au = frame(2, 0, 6, &|b| verbatim(b, 24, &[-8_388_608, 1]));
    decoder(1, 24).decode_float(&au, &mut out).unwrap();
    assert_eq!(out.planes[0], [-1.0, 1.0 / 8_388_608.0]);
    assert_eq!(out.sample_rate, 44100);
    assert_eq!(out.samples_per_channel, 2);
}

#[test]
fn ffmpeg_refusals() {
    // Negative LPC shift.
    let negative_shift = frame(4, 0, S16, &|b| {
        subframe_header(b, 0x20);
        b.put(0, 16).put(3, 4).put(-1, 5).put(1, 4);
        rice_residual(b, 0, &[0, 0, 0]);
    });
    let mut flac = decoder(1, 16);
    assert_eq!(
        integers(&mut flac, &negative_shift),
        Err(DecodeError::Unsupported("negative LPC shift"))
    );
    // A sample size other than the stream's.
    let eight_bit = frame(2, 0, 1, &|b| verbatim(b, 8, &[1, 2]));
    assert!(matches!(
        integers(&mut flac, &eight_bit),
        Err(DecodeError::Unsupported(_))
    ));
    // A block above STREAMINFO's maximum.
    let big = frame(4097, 0, S16, &|b| {
        subframe_header(b, 0);
        b.put(0, 16);
    });
    assert!(matches!(
        integers(&mut flac, &big),
        Err(DecodeError::Invalid(_))
    ));
    // A predictor order above the first partition: 4 samples, partition order 2.
    let order = frame(4, 0, S16, &|b| {
        subframe_header(b, 10);
        b.put(0, 16).put(0, 16).put(0, 2).put(2, 4);
    });
    assert!(matches!(
        integers(&mut flac, &order),
        Err(DecodeError::Invalid(_))
    ));
    // As many wasted bits as the sample size.
    let wasted = frame(2, 0, S16, &|b| {
        b.put(0, 1).put(0, 6).put(1, 1).put(0, 15).put(1, 1);
    });
    assert!(matches!(
        integers(&mut flac, &wasted),
        Err(DecodeError::Invalid(_))
    ));
    // The decoder stays usable.
    let ok = frame(2, 0, S16, &|b| verbatim(b, 16, &[1, 2]));
    assert_eq!(integers(&mut flac, &ok).unwrap(), [vec![1, 2]]);
}

#[test]
fn crc_mismatches_decode_unless_rejected() {
    let mut au = frame(2, 0, S16, &|b| verbatim(b, 16, &[1, 2]));
    *au.last_mut().unwrap() ^= 1;
    assert_eq!(integers(&mut decoder(1, 16), &au).unwrap(), [vec![1, 2]]);
    let mut strict = decoder(1, 16).with_crc_policy(CrcPolicy::Reject);
    assert_eq!(integers(&mut strict, &au), Err(DecodeError::CrcMismatch));
    *au.last_mut().unwrap() ^= 1;
    assert!(integers(&mut strict, &au).is_ok());
}

#[test]
fn every_truncation_and_bit_flip_fails_cleanly() {
    let au = frame(64, 10, S16, &|b| {
        subframe_header(b, 0x20 | 3);
        for w in [100, 90, 80, 70] {
            b.put(w, 16);
        }
        b.put(11, 4).put(9, 5);
        for c in [900, -300, 120, -40] {
            b.put(c, 12);
        }
        rice_residual(
            b,
            4,
            &(0..60).map(|i| (i * 7 % 31) - 15).collect::<Vec<_>>(),
        );
        subframe_header(b, 9);
        b.put(5, 17);
        b.put(0, 2).put(0, 4).put(15, 4).put(6, 5);
        for i in 0..63 {
            b.put((i % 64) - 32, 6);
        }
    });
    let mut flac = decoder(2, 16);
    let mut out = AudioFrame::default();
    flac.decode(&au, &mut out).unwrap();
    for len in 0..au.len() {
        assert!(
            flac.decode(&au[..len], &mut out).is_err(),
            "prefix of {len} bytes"
        );
        assert_eq!(
            out.samples_per_channel, 0,
            "a failed frame leaves no samples"
        );
    }
    let mut damaged = au.clone();
    for bit in 0..au.len() * 8 {
        damaged[bit / 8] ^= 1 << (bit % 8);
        let _ = flac.decode(&damaged, &mut out);
        damaged[bit / 8] ^= 1 << (bit % 8);
    }
    flac.decode(&au, &mut out).unwrap();
    assert_eq!(out.samples_per_channel, 64);
}

#[test]
fn too_small_a_budget_is_a_budget_error() {
    let au = frame(4096, 7, S16, &|b| {
        for _ in 0..8 {
            subframe_header(b, 0);
            b.put(0, 16);
        }
    });
    let config = configuration(44100, 8, 16, 4096, &[]);
    let limits = Limits {
        live_storage_bytes: 64 << 10,
    };
    let mut flac = Decoder::from_configuration(&config, limits).unwrap();
    let mut out = AudioFrame::default();
    let failure = flac.decode(&au, &mut out).unwrap_err();
    assert!(failure.0.starts_with("Budget("), "{failure:?}");

    // A shared pool returns to its idle level once the decoder and its output drop.
    let pool = Budget::new(1 << 20).unwrap();
    let idle = pool.used();
    {
        let mut flac = Decoder::from_configuration_in(&config, &pool).unwrap();
        let mut out = AudioFrame::default();
        flac.decode(&au, &mut out).unwrap();
        assert!(pool.used() >= idle + 8 * 4096 * 4);
    }
    assert_eq!(pool.used(), idle);
}

#[test]
fn channel_masks_and_headerless_streams() {
    let comments = |value: &str| {
        let comment = format!("WAVEFORMATEXTENSIBLE_CHANNEL_MASK={value}");
        let mut body = 0u32.to_le_bytes().to_vec();
        body.extend(1u32.to_le_bytes());
        body.extend((comment.len() as u32).to_le_bytes());
        body.extend(comment.as_bytes());
        body
    };
    let quad = frame(2, 3, S16, &|b| {
        for _ in 0..4 {
            subframe_header(b, 0);
            b.put(0, 16);
        }
    });
    let stereo = frame(2, 1, S16, &|b| {
        for _ in 0..2 {
            subframe_header(b, 0);
            b.put(0, 16);
        }
    });
    let names = |out: &AudioFrame| -> Vec<&str> {
        out.layout.speakers().iter().map(|s| s.name()).collect()
    };
    let config = configuration(44100, 4, 16, 4096, &[(4, comments("0x603"))]);
    let mut flac = Decoder::from_configuration(&config, Limits::default()).unwrap();
    let mut out = AudioFrame::default();
    flac.decode(&quad, &mut out).unwrap();
    assert_eq!(names(&out), ["FL", "FR", "SL", "SR"]);
    flac.decode(&stereo, &mut out).unwrap();
    assert_eq!(names(&out), ["FL", "FR"]);

    let mut headerless = Decoder::from_configuration(&[], Limits::default()).unwrap();
    headerless.decode(&quad, &mut out).unwrap();
    assert_eq!(names(&out), ["FL", "FR", "BL", "BR"]);
    assert_eq!(out.sample_rate, 44100);
    let deferred = frame(2, 0, 0, &|b| verbatim(b, 16, &[1, 2]));
    let mut fresh = Decoder::from_configuration(&[], Limits::default()).unwrap();
    assert!(
        fresh.decode(&deferred, &mut out).is_err(),
        "no sample size anywhere"
    );
    assert!(
        headerless.decode(&deferred, &mut out).is_ok(),
        "the first frame's size"
    );
}
