use triergon_core::{Error, Time, TimeBase};

#[test]
fn checked_tick_width_preserves_cancellation_widening_and_signed_bounds() {
    let narrow = i128::from(u64::MAX);
    let wide = 1_i128 << 64;
    let cases = [
        (0, u64::MAX, 1, 0, 1),
        (narrow, 1, u64::MAX, 1, 1),
        (-narrow, 1, u64::MAX, -1, 1),
        (1_i128 << 63, 2, 1, wide, 1),
        (wide, 1, 1, wide, 1),
        (-wide, 1, 1, -wide, 1),
        (wide, 1, u64::MAX, wide, u128::from(u64::MAX)),
        (-wide, 1, u64::MAX, -wide, u128::from(u64::MAX)),
        (i128::MIN, 1, 1_u64 << 63, -wide, 1),
        (i128::MIN, 1, 1, i128::MIN, 1),
        (i128::MAX, 1, 1, i128::MAX, 1),
    ];
    for (ticks, numerator, denominator, expected_numerator, expected_denominator) in cases {
        let actual =
            Time::from_ticks(ticks, TimeBase::new(numerator, denominator).unwrap()).unwrap();
        assert_eq!(
            actual.numerator(),
            expected_numerator,
            "ticks={ticks} base={numerator}/{denominator}"
        );
        assert_eq!(
            actual.denominator(),
            expected_denominator,
            "ticks={ticks} base={numerator}/{denominator}"
        );
    }
    for (ticks, numerator) in [(narrow, u64::MAX), (-narrow, u64::MAX), (i128::MIN, 2)] {
        assert_eq!(
            Time::from_ticks(ticks, TimeBase::new(numerator, 1).unwrap()),
            Err(Error::Overflow)
        );
    }
}
