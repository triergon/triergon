//! Additional independent retention/control tests.
#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, SourceId, Time, WorkBudget};
use triergon_subtitles::reference_trace::carriage::{
    CallerClock, Limits, Operation, Session, Status,
};
use triergon_subtitles::text::{Encoding, Format, Input, RawClock, Timing};

fn input(pool: &Budget) -> Input {
    Input {
        format: Format::OpaqueAss,
        encoding: Encoding::Opaque,
        payload: Bytes::copy_from_slice(pool, b"0,0,Style,,0,0,0,,author\\Ntext").unwrap(),
        codec_id: Some(Bytes::copy_from_slice(pool, b"S_TEXT/ASS").unwrap()),
        codec_private: Some(
            Bytes::copy_from_slice(pool, b"[Script Info]\r\n;verbatim\r\n").unwrap(),
        ),
        charset_label: Some(Bytes::copy_from_slice(pool, b"legacy-unknown-label").unwrap()),
        addition: None,
        timing: Timing::unknown(),
        source_id: SourceId(5),
        generation: 19,
        sequence: 21,
        configuration_epoch: 7,
        container_position: None,
    }
}
fn clock() -> CallerClock {
    CallerClock {
        numerator: 1,
        denominator: 1000,
        pts: None,
        dts: None,
        duration: None,
    }
}
fn output(session: &mut Session) -> triergon_subtitles::reference_trace::carriage::Snapshot {
    for _ in 0..32 {
        match session
            .poll(&mut WorkBudget::new(4096), &AtomicBool::new(false))
            .unwrap()
        {
            Status::Ready => return session.take_output().unwrap(),
            Status::Yield => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    panic!("no output")
}
#[test]
fn packet_and_configuration_alone_survive_every_original_root_and_snapshot_drop() {
    let pool = Budget::new(1 << 20).unwrap();
    let baseline = pool.used();
    let original = input(&pool);
    let mut offered = Some(original.clone());
    let mut session = Session::new(&pool, Limits::default());
    session
        .provide(&mut offered, Operation::Sample(clock()))
        .unwrap();
    let result = output(&mut session);
    let packet = result.packets.as_slice()[0].data.clone();
    let config = result.stream.as_ref().unwrap().extradata.clone().unwrap();
    drop(result);
    drop(session);
    drop(original);
    drop(offered);
    assert_eq!(packet.as_ref(), b"0,0,Style,,0,0,0,,author\\Ntext");
    assert_eq!(config.as_ref(), b"[Script Info]\r\n;verbatim\r\n");
    assert_eq!(
        pool.used(),
        baseline + packet.retained_storage_bytes() + config.retained_storage_bytes()
    );
    drop(packet);
    assert_eq!(pool.used(), baseline + config.retained_storage_bytes());
    drop(config);
    assert_eq!(pool.used(), baseline);
}
#[test]
fn raw_unreduced_clock_and_known_sentinel_are_not_normalized() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut source = input(&pool);
    source.timing = Timing::from_ticks(RawClock {
        numerator: 2,
        denominator: 180000,
        pts_ticks: Some(i128::from(i64::MIN)),
        dts_ticks: None,
        duration_ticks: Some(0),
    })
    .unwrap();
    let caller = CallerClock {
        numerator: 2,
        denominator: 180000,
        pts: Some(i64::MIN),
        dts: None,
        duration: Some(0),
    };
    let mut offered = Some(source);
    let mut session = Session::new(&pool, Limits::default());
    session
        .provide(&mut offered, Operation::Sample(caller))
        .unwrap();
    let result = output(&mut session);
    assert_eq!(result.packets.as_slice()[0].packet_time_base, [2, 180000]);
    assert_eq!(result.packets.as_slice()[0].pts_raw, i64::MIN);
    assert_eq!(
        result.input.timing.raw_clock().unwrap().pts_ticks,
        Some(i128::from(i64::MIN))
    );
    let Operation::Sample(actual) = result.operation else {
        panic!("lost caller knownness")
    };
    assert_eq!(actual.pts, Some(i64::MIN));
    assert_eq!(actual.duration, Some(0));
}
#[test]
fn exact_source_time_is_not_rounded_into_the_callers_clock() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut source = input(&pool);
    source.timing = Timing::exact(Some(Time::new(1, 3).unwrap()), None);
    let mut offered = Some(source);
    let mut session = Session::new(&pool, Limits::default());
    let wrong = CallerClock {
        pts: Some(333),
        ..clock()
    };
    assert!(matches!(
        session.provide(&mut offered, Operation::Sample(wrong)),
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        offered.as_ref().unwrap().timing.start(),
        Some(Time::new(1, 3).unwrap())
    );
}
#[test]
fn output_pressure_retains_immutable_owners_and_never_overwrites_the_slot() {
    let pool = Budget::new(1 << 20).unwrap();
    let original = input(&pool);
    let mut offered = Some(original.clone());
    let mut session = Session::new(&pool, Limits::default());
    session
        .provide(&mut offered, Operation::Sample(clock()))
        .unwrap();
    assert_eq!(
        session
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        Status::Ready
    );
    for _ in 0..7 {
        let mut work = WorkBudget::new(4096);
        assert_eq!(
            session.poll(&mut work, &AtomicBool::new(false)).unwrap(),
            Status::NeedOutput
        );
        assert_eq!(work.spent(), 0);
    }
    let mut next = Some(original);
    assert!(
        session
            .provide(&mut next, Operation::Sample(clock()))
            .is_err()
    );
    assert!(next.is_some());
    let first = session.take_output().unwrap();
    assert_eq!(first.input.sequence, 21);
    session
        .provide(&mut next, Operation::Sample(clock()))
        .unwrap();
    assert!(next.is_none());
}
#[test]
fn capacity_pressure_can_be_released_and_retried_without_losing_pending_data() {
    let pool = Budget::new(1 << 20).unwrap();
    let original = input(&pool);
    let mut offered = Some(original.clone());
    let mut session = Session::new(&pool, Limits::default());
    session
        .provide(&mut offered, Operation::Sample(clock()))
        .unwrap();
    let pressure = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        session.poll(&mut WorkBudget::new(7), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    drop(pressure);
    let result = output(&mut session);
    assert_eq!(
        result.packets.as_slice()[0].data.as_ref(),
        original.payload.as_ref()
    );
}
