//! Additive correctness guard, not a bug RED or performance test.
//! Literal answers use M=2^64-1, P=M*(M+1)=2^128-2^64:
//! 1/M+1/(M+1)=(2*M+1)/P and 1/M-1/(M+1)=1/P.
use triergon_core::Time;

#[test]
fn adjacent_full_width_clock_denominators_keep_exact_signed_sums() {
    let m = u128::from(u64::MAX);
    let above = 1_u128 << 64;
    let product = 340_282_366_920_938_463_444_927_863_358_058_659_840_u128;
    let sum = 36_893_488_147_419_103_231_i128;
    for (an, ad, bn, bd, add, sub) in [
        (1, m, 1, m, (2, m), (0, 1)),
        (1, m, 1, above, (sum, product), (1, product)),
        (1, above, 1, m, (sum, product), (-1, product)),
        (-1, m, -1, above, (-sum, product), (-1, product)),
    ] {
        let a = Time::new(an, ad).unwrap();
        let b = Time::new(bn, bd).unwrap();
        let actual_add = a.checked_add(b).unwrap();
        let actual_sub = a.checked_sub(b).unwrap();
        assert_eq!((actual_add.numerator(), actual_add.denominator()), add);
        assert_eq!((actual_sub.numerator(), actual_sub.denominator()), sub);
    }
}
