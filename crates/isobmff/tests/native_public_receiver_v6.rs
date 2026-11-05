use triergon_core::{Budget, Time, TimeBase};
use triergon_isobmff::{
    Mov9ClockField, Mov9PacketView, Mov9PublicClockV6, Mov9PublicPacketTimesV6,
    Mov9ReceiverOriginV6, PinnedMovD32PublicReceiverV6,
};
const M: i128 = i64::MAX as i128;
const W: i128 = 1i128 << 48;
const BASE: i128 = M - W;
const THRESHOLD: i128 = M - 2 * W;
fn field(x: i128) -> Mov9ClockField {
    Mov9ClockField::from_exact_native_ticks(x).unwrap()
}
fn receiver(x: i128) -> Mov9PublicClockV6 {
    Mov9PublicClockV6::from_demux(field(x), PinnedMovD32PublicReceiverV6).unwrap()
}
#[test]
fn below_threshold_remains_literal() {
    assert_eq!(
        receiver(THRESHOLD - 1).public_raw_ticks(),
        (THRESHOLD - 1) as i64
    );
}
#[test]
fn exact_threshold_is_excluded() {
    assert_eq!(receiver(THRESHOLD).public_raw_ticks(), THRESHOLD as i64);
    assert_eq!(
        receiver(THRESHOLD).origin(),
        Mov9ReceiverOriginV6::DemuxValue
    );
}
#[test]
fn first_relative_value_uses_distinct_subtraction_origin() {
    assert_eq!(receiver(THRESHOLD + 1).public_raw_ticks(), (-W + 1) as i64);
    assert_eq!(
        receiver(THRESHOLD + 1).origin(),
        Mov9ReceiverOriginV6::PublicRelativeReference
    );
}
#[test]
fn origin_minus_one_is_known_negative() {
    assert_eq!(receiver(BASE - 1).public_api_ticks(), Some(-1));
}
#[test]
fn origin_is_known_zero() {
    assert_eq!(receiver(BASE).public_api_ticks(), Some(0));
}
#[test]
fn origin_plus_one_is_known_one() {
    assert_eq!(receiver(BASE + 1).public_api_ticks(), Some(1));
}
#[test]
fn signed_maximum_has_finite_public_value() {
    assert_eq!(receiver(M).public_raw_ticks(), W as i64);
}
#[test]
fn original_actual_control01_literal_value() {
    assert_eq!(receiver(M - 19).public_api_ticks(), Some(281474976710637));
}
#[test]
fn original_actual_control02_literal_value() {
    assert_eq!(receiver(M - 9).public_api_ticks(), Some(281474976710647));
}
#[test]
fn negative_literal_is_not_relative() {
    assert_eq!(receiver(-1).public_api_ticks(), Some(-1));
    assert_eq!(receiver(-W).origin(), Mov9ReceiverOriginV6::DemuxValue);
}
#[test]
fn sentinel_bits_do_not_destroy_unsigned_source_knownness() {
    let source = 1i128 << 63;
    let time = Time::from_ticks(source, TimeBase::new(1, 1000).unwrap()).unwrap();
    let r = receiver(source);
    assert_eq!(r.demux_field().exact_native_ticks, source);
    assert_eq!(r.public_raw_ticks(), i64::MIN);
    assert_eq!(r.public_api_ticks(), None);
    assert_eq!(r.origin(), Mov9ReceiverOriginV6::ApiSentinel);
    assert_eq!(
        time.to_ticks(
            TimeBase::new(1, 1000).unwrap(),
            triergon_core::Rounding::TowardZero
        )
        .unwrap(),
        source
    );
}
#[test]
fn source_beyond_u64_stays_exact_while_demux_bits_are_separate() {
    let source = i128::from(u64::MAX) + 9;
    let r = receiver(source);
    assert_eq!(r.demux_field().exact_native_ticks, source);
    assert_eq!(r.demux_field().signed_ticks(), 8);
    assert_eq!(r.public_api_ticks(), Some(8));
}
fn view(pts: i128, dts: i128) -> Mov9PacketView {
    let p = field(pts);
    let d = field(dts);
    Mov9PacketView {
        source_ordinal: 7,
        source_description: 2,
        source_range: 123..135,
        pts: p.api_ticks(),
        dts: d.api_ticks(),
        pts_field: p,
        dts_field: d,
        duration: 12,
        flags: 1,
    }
}
#[test]
fn pts_projection_does_not_change_literal_dts() {
    let v = view(BASE + 4, -7);
    let r = Mov9PublicPacketTimesV6::from_demux(&v, PinnedMovD32PublicReceiverV6).unwrap();
    assert_eq!(r.pts.public_api_ticks(), Some(4));
    assert_eq!(r.dts.public_api_ticks(), Some(-7));
}
#[test]
fn dts_projection_does_not_fill_absent_pts() {
    let v = view(1i128 << 63, BASE + 5);
    let r = Mov9PublicPacketTimesV6::from_demux(&v, PinnedMovD32PublicReceiverV6).unwrap();
    assert_eq!(r.pts.public_api_ticks(), None);
    assert_eq!(r.dts.public_api_ticks(), Some(5));
}
#[test]
fn demux_view_and_legacy_api_method_are_unchanged() {
    let v = view(M - 19, M - 9);
    let original = v.clone();
    let r = Mov9PublicPacketTimesV6::from_demux(&v, PinnedMovD32PublicReceiverV6).unwrap();
    assert_eq!(v, original);
    assert_eq!(v.pts_field.api_ticks(), Some((M - 19) as i64));
    assert_eq!(v.dts_field.api_ticks(), Some((M - 9) as i64));
    assert_eq!(r.pts.demux_field(), v.pts_field);
    assert_eq!(r.dts.demux_field(), v.dts_field);
}
#[test]
fn no_heap_or_pool_charge_is_created_by_scalar_projection() {
    let pool = Budget::new(4096).unwrap();
    let hold = pool.reserve(pool.available()).unwrap();
    let r = receiver(4);
    assert_eq!(r.public_api_ticks(), Some(4));
    assert_eq!(pool.available(), 0);
    drop(hold);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn receiver_has_no_mutable_epoch_or_first_packet_origin() {
    let a = receiver(THRESHOLD);
    let b = receiver(BASE + 7);
    let c = receiver(-4);
    assert_eq!(a, receiver(THRESHOLD));
    assert_eq!(b, receiver(BASE + 7));
    assert_eq!(c, receiver(-4));
}
