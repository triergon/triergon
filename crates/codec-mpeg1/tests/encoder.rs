use triergon_codec_mpeg1::{Budget, Encoder, EncoderConfig, Error, Limits, StorageError};
use triergon_pixel::{Geometry, Picture};
fn cfg() -> EncoderConfig {
    EncoderConfig {
        geometry: Geometry::new(16, 16).unwrap(),
        bit_rate: 1_000_000,
        max_rate: 1_400_000,
        vbv_bits: 142_857,
        gop: 30,
    }
}
#[test]
fn direct_zero_gop_is_rejected_before_allocation() {
    assert!(matches!(
        Encoder::new(EncoderConfig { gop: 0, ..cfg() }),
        Err(Error::Gop(0))
    ));
}
#[test]
fn a_max_rate_the_header_would_code_as_variable_bit_rate_is_refused() {
    // bit_rate 0x3FFFF identifies variable bit rate (ISO/IEC 11172-2 §2.4.3.2).
    for max_rate in [104_856_801, 104_857_200] {
        assert!(matches!(
            Encoder::new(EncoderConfig { max_rate, ..cfg() }),
            Err(Error::BitRate(rate)) if rate == max_rate
        ));
    }
    let mut e = Encoder::new(EncoderConfig {
        max_rate: 104_856_800,
        ..cfg()
    })
    .unwrap();
    let mut out = Vec::new();
    e.encode(&Picture::new(Geometry::new(16, 16).unwrap()), &mut out)
        .unwrap();
    assert_eq!(out[..4], [0, 0, 1, 0xB3]);
    let bit_rate = u32::from_be_bytes([0, out[8], out[9], out[10]]) >> 6;
    assert_eq!(bit_rate, 0x3FFFE);
}
#[test]
fn mismatched_picture_returns_owner_unchanged_before_encoder_mutation() {
    let mut e = Encoder::new(cfg()).unwrap();
    let p = Picture::new(Geometry::new(32, 16).unwrap());
    let mut out = vec![1, 2, 3];
    assert!(matches!(
        e.encode(&p, &mut out),
        Err(Error::PictureGeometry { .. })
    ));
    assert_eq!(out, [1, 2, 3]);
    let p = Picture::new(Geometry::new(16, 16).unwrap());
    let first = e.encode(&p, &mut out).unwrap();
    assert!(first.key && first.index == 0);
    assert_eq!(first.bits, out.len() * 8);
    let next = e.encode(&p, &mut out).unwrap();
    assert!(!next.key && next.index == 1);
    assert_eq!(next.bits, out.len() * 8);
}
#[test]
fn a_budget_too_small_for_the_pictures_is_a_typed_error() {
    let limits = Limits {
        live_storage_bytes: 4096,
    };
    assert!(matches!(
        Encoder::with_limits(cfg(), limits),
        Err(Error::Budget(StorageError::Limit { .. }))
    ));
}
#[test]
fn a_refused_output_charge_changes_no_state_and_the_pool_drains_on_drop() {
    let budget = Budget::new(1 << 20).unwrap();
    let idle = budget.used();
    let mut e = Encoder::new_in(cfg(), &budget).unwrap();
    let p = Picture::new(Geometry::new(16, 16).unwrap());
    let mut out = Vec::new();
    let held = budget.reserve(budget.available()).unwrap();
    assert!(matches!(
        e.encode(&p, &mut out),
        Err(Error::Budget(StorageError::Backpressure { .. }))
    ));
    drop(held);
    let first = e.encode(&p, &mut out).unwrap();
    assert!(first.key && first.index == 0);
    assert_eq!(e.encode(&p, &mut out).unwrap().index, 1);
    drop(e);
    assert_eq!(budget.used(), idle);
}

#[test]
fn motion_scratch_backpressure_retries_the_same_picture_exactly() {
    let budget = Budget::new(1 << 20).unwrap();
    let mut retry = Encoder::new_in(cfg(), &budget).unwrap();
    let mut control = Encoder::new(cfg()).unwrap();
    let mut input = Picture::new(cfg().geometry);
    for (index, sample) in input.y_mut().iter_mut().enumerate() {
        *sample = (index % 197) as u8;
    }
    let (mut out, mut expected) = (Vec::new(), Vec::new());
    assert_eq!(
        retry.encode(&input, &mut out).unwrap(),
        control.encode(&input, &mut expected).unwrap()
    );

    input.y_mut()[0..16].fill(32);
    input.cb_mut().fill(93);
    input.cr_mut().fill(151);
    // The 16x16 picture needs two 4x4 coarse planes. Only one would fit.
    let held = budget.reserve(budget.available() - 16).unwrap();
    for _ in 0..2 {
        assert!(matches!(
            retry.encode(&input, &mut out),
            Err(Error::Budget(StorageError::Backpressure { .. }))
        ));
        assert_eq!(out, expected);
    }
    drop(held);
    for index in 1..=2 {
        let actual = retry.encode(&input, &mut out).unwrap();
        assert_eq!(actual.index, index);
        assert_eq!(actual, control.encode(&input, &mut expected).unwrap());
        assert_eq!(out, expected);
        for (a, b) in retry
            .reconstruction()
            .planes
            .into_iter()
            .zip(control.reconstruction().planes)
        {
            match (a, b) {
                (
                    triergon_pixel::Plane::U8 { data: a, .. },
                    triergon_pixel::Plane::U8 { data: b, .. },
                ) => assert_eq!(a, b),
                _ => panic!("MPEG-1 reconstructs eight-bit samples"),
            }
        }
    }
}

#[test]
fn repeated_luma_needs_no_motion_scratch_even_when_chroma_changes() {
    let budget = Budget::new(1 << 20).unwrap();
    let mut encoder = Encoder::new_in(cfg(), &budget).unwrap();
    let mut input = Picture::new(cfg().geometry);
    let mut out = Vec::new();
    encoder.encode(&input, &mut out).unwrap();
    input.cb_mut().fill(97);
    let _held = budget.reserve(budget.available()).unwrap();
    let picture = encoder.encode(&input, &mut out).unwrap();
    assert_eq!(picture.index, 1);
    assert!(!picture.key);
}
