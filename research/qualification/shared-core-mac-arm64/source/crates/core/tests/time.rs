use safe_media_core::{Error, Rounding, Time, TimeBase};

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
