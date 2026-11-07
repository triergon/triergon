use triergon_core::{Time, TimeBase};
use triergon_isobmff::compat::mov::clock::ClockField;

#[test]
fn demux_signed_and_null_fields_retain_known_exact_source_clocks() {
    let base = TimeBase::new(1, 1000).unwrap();
    for (ticks, bits, exposed) in [
        ((1i128 << 63) - 1, (1u64 << 63) - 1, Some(i64::MAX)),
        (1i128 << 63, 1u64 << 63, None),
        ((1i128 << 63) + 1, (1u64 << 63) + 1, Some(i64::MIN + 1)),
        ((1i128 << 64) - 1, u64::MAX, Some(-1)),
        (1i128 << 64, 0, Some(0)),
        ((1i128 << 64) + 1, 1, Some(1)),
    ] {
        let original = Time::from_ticks(ticks, base).unwrap();
        let field = ClockField::from_exact_ticks(ticks).unwrap();
        assert_eq!(field.exact_ticks, ticks);
        assert_eq!(field.demux_bits, bits);
        assert_eq!(field.api_ticks(), exposed);
        assert_eq!(original, Time::from_ticks(ticks, base).unwrap());
        assert!(original.numerator() > 0);
    }
}
