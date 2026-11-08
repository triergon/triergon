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
    assert_eq!(e.encode(&p, &mut out).unwrap().index, 0);
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
