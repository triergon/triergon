mod common;
use safe_media_core::{Budget, Error};
use safe_media_isobmff::{BoxView, FragmentDecodeTime};

fn view(budget: &Budget, version: u8, flags: [u8; 3], value: &[u8]) -> BoxView {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&u32::try_from(12 + value.len()).unwrap().to_be_bytes());
    bytes.extend_from_slice(b"tfdt");
    bytes.push(version);
    bytes.extend_from_slice(&flags);
    bytes.extend_from_slice(value);
    BoxView::new(common::bytes(budget, &bytes), 0).unwrap()
}

#[test]
fn normative_tfdt_preserves_unsigned_32_and_64_bit_source_ticks() {
    let budget = Budget::new(4096).unwrap();
    let small =
        FragmentDecodeTime::parse(view(&budget, 0, [0; 3], &u32::MAX.to_be_bytes())).unwrap();
    assert_eq!(small.version(), 0);
    assert_eq!(small.ticks(), u64::from(u32::MAX));
    let raw = view(&budget, 1, [0; 3], &(1u64 << 63).to_be_bytes());
    let large = FragmentDecodeTime::parse(raw.clone()).unwrap();
    assert_eq!(large.ticks(), 1u64 << 63);
    assert_eq!(large.raw().bytes().as_ref(), raw.bytes().as_ref());
}

#[test]
fn tfdt_invalid_width_version_flags_are_not_repaired() {
    let budget = Budget::new(4096).unwrap();
    assert!(FragmentDecodeTime::parse(view(&budget, 1, [0; 3], &[0; 4])).is_err());
    assert!(FragmentDecodeTime::parse(view(&budget, 0, [0; 3], &[0; 8])).is_err());
    assert!(matches!(
        FragmentDecodeTime::parse(view(&budget, 2, [0; 3], &[0; 8])),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        FragmentDecodeTime::parse(view(&budget, 0, [0, 0, 1], &[0; 4])),
        Err(Error::Unsupported(_))
    ));
}
