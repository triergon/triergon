use triergon_codec_mpeg1::{Encoder, EncoderConfig, Error};
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
