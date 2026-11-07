use triergon_core::{Error, Rounding, Time, TimeBase};

#[test]
fn integer_rescale_agrees_with_canonical_time_at_ties_and_extremes() {
    let bases = [
        (1, 1),
        (1, 2),
        (1, 90_000),
        (1, 1_000_000),
        (1, 1_000_000_000),
        (1001, 24_000),
        (u64::MAX, 1),
        (1, u64::MAX),
        (u64::MAX, u64::MAX - 1),
        (u64::MAX - 1, u64::MAX),
    ]
    .map(|(num, den)| TimeBase::new(num, den).unwrap());
    for from in bases {
        for to in bases {
            for ticks in [i64::MIN, -90_001, -3, -1, 0, 1, 3, 90_001, i64::MAX] {
                let time = Time::from_ticks(i128::from(ticks), from).unwrap();
                for rounding in UNIT53_ROUNDINGS {
                    assert_eq!(
                        from.rescale(ticks, to, rounding),
                        time.to_i64_ticks(to, rounding),
                        "{ticks}, {from:?}, {to:?}, {rounding:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn mixed_prime_clocks_preserve_their_full_rational() {
    let a = Time::from_ticks(1, TimeBase::new(1, 1_000_003).unwrap()).unwrap();
    let b = Time::from_ticks(1, TimeBase::new(1, 1_000_033).unwrap()).unwrap();
    let sum = a.checked_add(b).unwrap();
    assert_eq!(sum.numerator(), 2_000_036);
    assert_eq!(sum.denominator(), 1_000_036_000_099);
    assert_eq!(sum.checked_sub(b).unwrap(), a);
    assert!(a > b);
    assert_eq!(Time::new(2, 4).unwrap(), Time::new(1, 2).unwrap());
    assert_eq!(Time::new(0, u64::MAX.into()).unwrap().denominator(), 1);
}

#[test]
fn unsigned_fragment_time_above_i64_is_a_source_value_not_a_sentinel() {
    let clock = TimeBase::new(1, 90_000).unwrap();
    let time = Time::from_ticks(i128::from(u64::MAX), clock).unwrap();
    assert!(time > Time::new(0, 1).unwrap());
    assert_eq!(
        time.to_ticks(clock, Rounding::TowardZero).unwrap(),
        i128::from(u64::MAX)
    );
    assert_eq!(
        time.to_i64_ticks(clock, Rounding::TowardZero),
        Err(Error::Overflow)
    );
    assert_eq!(
        time.to_nanoseconds(Rounding::NearestAway),
        Err(Error::Overflow)
    );
}

#[test]
fn each_rounding_mode_is_explicit_at_negative_and_positive_ties() {
    let unit = TimeBase::new(1, 1).unwrap();
    for (n, expected) in [(1, [0, 1, 0, 1, 1]), (-1, [0, -1, -1, 0, -1])] {
        let time = Time::new(n, 2).unwrap();
        for (mode, value) in [
            Rounding::TowardZero,
            Rounding::AwayFromZero,
            Rounding::Floor,
            Rounding::Ceil,
            Rounding::NearestAway,
        ]
        .into_iter()
        .zip(expected)
        {
            assert_eq!(time.to_ticks(unit, mode).unwrap(), value);
        }
    }
    assert_eq!(
        Time::new(1, 2)
            .unwrap()
            .to_nanoseconds(Rounding::TowardZero)
            .unwrap(),
        500_000_000
    );
    assert_eq!(
        Time::new(1, 3)
            .unwrap()
            .to_nanoseconds(Rounding::Ceil)
            .unwrap(),
        333_333_334
    );
}

#[test]
fn signed_extremes_and_comparison_do_not_overflow_or_use_floats() {
    let minimum = Time::new(i128::MIN, 1).unwrap();
    let divided = Time::new(i128::MIN, u64::MAX.into()).unwrap();
    assert!(minimum < divided);
    assert!(divided < Time::new(-1, 1).unwrap());
    assert_eq!(
        minimum.checked_sub(minimum).unwrap(),
        Time::new(0, 1).unwrap()
    );
    assert_eq!(
        minimum
            .to_ticks(TimeBase::new(1, 1).unwrap(), Rounding::NearestAway)
            .unwrap(),
        i128::MIN
    );
    assert_eq!(
        minimum.checked_add(Time::new(-1, 1).unwrap()),
        Err(Error::Overflow)
    );
    assert!(
        Time::new(i128::MAX, u64::MAX.into()).unwrap()
            > Time::new(i128::MAX, (u64::MAX - 1).into())
                .unwrap()
                .checked_sub(Time::new(1, 1).unwrap())
                .unwrap()
    );
}

#[test]
fn relative_codec_time_and_negative_packet_time_are_added_exactly() {
    let a = Time::from_ticks(-1, TimeBase::new(1, 1_000_003).unwrap()).unwrap();
    let b = Time::from_ticks(1, TimeBase::new(1, 90_000).unwrap()).unwrap();
    let time = a.checked_add(b).unwrap();
    assert_eq!(time, Time::new(910_003, 90_000_270_000).unwrap());
    assert!(TimeBase::new(0, 1).is_err());
    assert!(TimeBase::new(1, 0).is_err());
    assert!(Time::new(1, 0).is_err());
    assert_ne!(Some(Time::new(0, 1).unwrap()), None);
}

#[test]
fn representable_results_survive_wider_intermediates() {
    let half = Time::new(i128::MAX, 2).unwrap();
    assert_eq!(
        half.checked_add(half).unwrap(),
        Time::new(i128::MAX, 1).unwrap()
    );
    let denominator = u64::MAX;
    let time = Time::new(i128::MAX, denominator.into()).unwrap();
    let ticks = time
        .to_ticks(
            TimeBase::new(1, denominator - 2).unwrap(),
            Rounding::TowardZero,
        )
        .unwrap();
    assert_eq!(ticks, 170_141_183_460_469_231_713_240_559_642_174_554_110);
}

#[test]
fn a_movie_media_and_codec_clock_can_require_more_than_u64_denominator() {
    let a = Time::from_ticks(1, TimeBase::new(1, 4_294_967_291).unwrap()).unwrap();
    let b = Time::from_ticks(1, TimeBase::new(1, 4_294_967_279).unwrap()).unwrap();
    let c = Time::from_ticks(1, TimeBase::new(1, 90_000).unwrap()).unwrap();
    let sum = a.checked_add(b).unwrap().checked_add(c).unwrap();
    // Independently calculated with Python's Fraction, not with this Time API.
    assert_eq!(
        sum,
        Time::new(
            18_447_517_073_331_571_189,
            1_660_206_958_129_824_407_010_000
        )
        .unwrap()
    );
    assert_eq!(sum.checked_sub(c).unwrap(), a.checked_add(b).unwrap());
}

const UNIT53_ROUNDINGS: [Rounding; 5] = [
    Rounding::TowardZero,
    Rounding::AwayFromZero,
    Rounding::Floor,
    Rounding::Ceil,
    Rounding::NearestAway,
];

#[test]
fn unit_conversion_53_exact_clocks_include_above_u64_and_both_signed_limits() {
    // Expected integer values authored independently with Python Fraction.
    let cases: &[(i128, u128, u64, i128)] = &[
        (0, 1, 1, 0),
        (1, 3, 90000, 30000),
        (-1, 3, 90000, -30000),
        (3, 2, 4, 6),
        (18446744073709551614, 1000, 1000, 18446744073709551614),
        (18446744073709551615, 1000, 1000, 18446744073709551615),
        (18446744073709551616, 1000, 1000, 18446744073709551616),
        (-18446744073709551616, 1000, 1000, -18446744073709551616),
        (18446744073709551616, 1, 1000, 18446744073709551616000),
        (-18446744073709551617, 1, 1000, -18446744073709551617000),
        (
            170141183460469231731687303715884105727,
            1,
            1,
            170141183460469231731687303715884105727,
        ),
        (
            -170141183460469231731687303715884105728,
            1,
            1,
            -170141183460469231731687303715884105728,
        ),
        (
            85070591730234615865843651857942052863,
            1,
            2,
            170141183460469231731687303715884105726,
        ),
        (
            -85070591730234615865843651857942052864,
            1,
            2,
            -170141183460469231731687303715884105728,
        ),
        (
            -170141183460469231731687303715884105728,
            2,
            2,
            -170141183460469231731687303715884105728,
        ),
        (
            170141183460469231731687303715884105727,
            3,
            3,
            170141183460469231731687303715884105727,
        ),
    ];
    for &(n, d, scale, expected) in cases {
        let time = Time::new(n, d).unwrap();
        let base = TimeBase::new(1, scale).unwrap();
        for mode in UNIT53_ROUNDINGS {
            assert_eq!(
                time.to_ticks(base, mode),
                Ok(expected),
                "n={n} d={d} scale={scale} mode={mode:?}"
            );
        }
    }
}

#[test]
fn unit_conversion_53_overflows_keep_public_signed_and_wide_product_bounds() {
    let cases: &[(i128, u128, u64)] = &[
        (170141183460469231731687303715884105727, 1, 2),
        (-170141183460469231731687303715884105728, 1, 2),
        (
            170141183460469231731687303715884105727,
            1,
            18446744073709551615,
        ),
        (
            -170141183460469231731687303715884105728,
            1,
            18446744073709551615,
        ),
        (85070591730234615865843651857942052864, 1, 2),
        (-85070591730234615865843651857942052865, 1, 2),
    ];
    for &(n, d, scale) in cases {
        let time = Time::new(n, d).unwrap();
        let base = TimeBase::new(1, scale).unwrap();
        for mode in UNIT53_ROUNDINGS {
            assert_eq!(
                time.to_ticks(base, mode),
                Err(Error::Overflow),
                "n={n} scale={scale} mode={mode:?}"
            );
        }
    }
}

#[test]
fn unit_conversion_53_nonunit_nonexact_and_wide_denominator_keep_all_roundings() {
    let cases: &[(i128, u128, u64, u64, [i128; 5])] = &[
        (1, 3, 1, 2, [0, 1, 0, 1, 1]),
        (-1, 3, 1, 2, [0, -1, -1, 0, -1]),
        (3, 2, 1, 1, [1, 2, 1, 2, 2]),
        (-3, 2, 1, 1, [-1, -2, -2, -1, -2]),
        (2, 3, 2, 5, [1, 2, 1, 2, 2]),
        (-2, 3, 2, 5, [-1, -2, -2, -1, -2]),
        (1, 3, 1, 1000, [333, 334, 333, 334, 333]),
        (-1, 3, 1, 1000, [-333, -334, -334, -333, -333]),
        (
            18_446_744_073,
            7,
            1,
            1_000_000_000,
            [
                2_635_249_153_285_714_285,
                2_635_249_153_285_714_286,
                2_635_249_153_285_714_285,
                2_635_249_153_285_714_286,
                2_635_249_153_285_714_286,
            ],
        ),
        (
            18_446_744_074,
            7,
            1,
            1_000_000_000,
            [
                2_635_249_153_428_571_428,
                2_635_249_153_428_571_429,
                2_635_249_153_428_571_428,
                2_635_249_153_428_571_429,
                2_635_249_153_428_571_429,
            ],
        ),
        (
            -18_446_744_073,
            7,
            1,
            1_000_000_000,
            [
                -2_635_249_153_285_714_285,
                -2_635_249_153_285_714_286,
                -2_635_249_153_285_714_286,
                -2_635_249_153_285_714_285,
                -2_635_249_153_285_714_286,
            ],
        ),
        (
            -18_446_744_074,
            7,
            1,
            1_000_000_000,
            [
                -2_635_249_153_428_571_428,
                -2_635_249_153_428_571_429,
                -2_635_249_153_428_571_429,
                -2_635_249_153_428_571_428,
                -2_635_249_153_428_571_429,
            ],
        ),
        (
            1,
            18446744073709551617,
            1,
            18446744073709551615,
            [0, 1, 0, 1, 1],
        ),
        (
            -1,
            18446744073709551617,
            1,
            18446744073709551615,
            [0, -1, -1, 0, -1],
        ),
        (45, 7, 3, 7, [15, 15, 15, 15, 15]),
    ];
    for &(n, d, base_n, base_d, expected) in cases {
        let time = Time::new(n, d).unwrap();
        let base = TimeBase::new(base_n, base_d).unwrap();
        for (mode, value) in UNIT53_ROUNDINGS.into_iter().zip(expected) {
            assert_eq!(
                time.to_ticks(base, mode),
                Ok(value),
                "n={n} d={d} base={base_n}/{base_d} mode={mode:?}"
            );
        }
    }
}

#[test]
fn wide_gcd_54_can_reenter_u64_after_one_or_more_euclidean_steps() {
    // Independent Python Fraction reduction; no expected values from this gcd.
    let cases: &[(i128, u128, i128, u128)] = &[
        (
            79228162514264337593543950342,
            12,
            39614081257132168796771975171,
            6,
        ),
        (
            1267650600228229401496703205377,
            1267650600228229401496703205375,
            1267650600228229401496703205377,
            1267650600228229401496703205375,
        ),
        (
            1329227995784915872903807060280344587,
            79228162514264337593543950339,
            1329227995784915872903807060280344587,
            79228162514264337593543950339,
        ),
        (
            -170141183460469231731687303715884105728,
            65536,
            -2596148429267413814265248164610048,
            1,
        ),
    ];
    for &(n, d, expected_n, expected_d) in cases {
        let value = Time::new(n, d).unwrap();
        assert_eq!(
            (value.numerator(), value.denominator()),
            (expected_n, expected_d)
        );
    }
}

#[test]
fn wide_gcd_54_keeps_wide_gcd_when_remainder_is_zero() {
    let cases: &[(i128, u128, i128, u128)] = &[
        (
            1329227995784915872903807060280344576,
            1267650600228229401496703205376,
            1048576,
            1,
        ),
        (
            -170141183460469231731687303715884105728,
            85070591730234615865843651857942052864,
            -2,
            1,
        ),
        (0, 340282366920938463463374607431768211455, 0, 1),
    ];
    for &(n, d, expected_n, expected_d) in cases {
        let value = Time::new(n, d).unwrap();
        assert_eq!(
            (value.numerator(), value.denominator()),
            (expected_n, expected_d)
        );
    }
}
