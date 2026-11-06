//! Observable cursor/framer resource interactions. These guards do not require
//! or call read_append directly.
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Error, InputCursor, SourceCapabilities, SourceFailure, SourceId, Step, WorkBudget,
};
use triergon_mpegts::{PacketFormat, TransportFramer};

fn cursor(pool: &Budget, length: Option<u64>) -> InputCursor {
    InputCursor::new(
        pool,
        SourceId(51),
        SourceCapabilities {
            length,
            random_access: false,
        },
        12,
    )
    .unwrap()
}

#[test]
fn reserved_inline_carriers_need_no_temporary_owner_and_retry_freeze_pressure() {
    for (format, fixture) in [
        (
            PacketFormat::Ts188,
            include_bytes!("data/ts188-basic.ts").as_slice(),
        ),
        (
            PacketFormat::M2ts192,
            include_bytes!("data/m2ts192-opaque-prefix.m2ts").as_slice(),
        ),
    ] {
        let wire = &fixture[..format.bytes()];
        let pool = Budget::new(16 * 1024).unwrap();
        let control = Budget::control_storage_bytes();
        assert_eq!(pool.used(), control);
        let mut input = cursor(&pool, Some(wire.len() as u64));
        let mut framer = TransportFramer::new(pool.clone(), format);
        let idle = pool.used();
        let cancel = AtomicBool::new(false);
        let mut zero = WorkBudget::new(0);
        assert!(matches!(
            framer.poll(&mut input, &mut zero, &cancel).unwrap(),
            Step::Yield
        ));
        assert_eq!(pool.used(), idle);
        assert_eq!(input.position(), 0);

        // A real carry reservation consumes one unit before the empty cursor
        // reports starvation, even when no work remains for a source byte.
        let mut reserve = WorkBudget::new(1);
        assert!(matches!(
            framer.poll(&mut input, &mut reserve, &cancel).unwrap(),
            Step::NeedInput(request) if request.offset == 0 && request.maximum == 12
        ));
        assert_eq!(reserve.spent(), 1);
        let blocker = pool.reserve(pool.available()).unwrap();
        let held = pool.used();
        let mut cancelled = WorkBudget::new(1);
        assert!(matches!(
            framer.poll(&mut input, &mut cancelled, &AtomicBool::new(true)),
            Err(Error::Cancelled)
        ));
        assert_eq!(cancelled.spent(), 0);
        assert_eq!(input.position(), 0);

        for (offset, byte) in wire.iter().enumerate() {
            let request = input.request();
            assert_eq!(request.offset, offset as u64);
            input
                .provide_inline(request.source, request.token, request.offset, &[*byte])
                .unwrap();
            let mut work = WorkBudget::new(1);
            assert!(matches!(
                framer.poll(&mut input, &mut work, &cancel).unwrap(),
                Step::Yield
            ));
            assert_eq!(work.spent(), 1);
            assert_eq!(input.position(), offset as u64 + 1);
            assert_eq!(pool.used(), held);
            assert!(input.retained_window().is_none());
            assert_eq!(framer.pending_or_last_packet_offset(), Some(0));
        }
        input.finish(input.source(), input.token()).unwrap();
        let mut freeze = WorkBudget::new(1);
        assert!(matches!(
            framer.poll(&mut input, &mut freeze, &cancel),
            Err(Error::Backpressure { .. })
        ));
        assert_eq!(freeze.spent(), 1);
        assert_eq!(input.position(), wire.len() as u64);
        assert_eq!(framer.pending_or_last_packet_offset(), Some(0));
        assert!(framer.take_rejection().is_none());
        drop(blocker);

        let mut published = None;
        for _ in 0..4096 {
            let mut work = WorkBudget::new(1);
            match framer.poll(&mut input, &mut work, &cancel).unwrap() {
                Step::Ready(packet) => {
                    published = Some(packet);
                    break;
                }
                Step::Yield => {}
                other => panic!("unexpected post-pressure step: {other:?}"),
            }
            assert!(work.spent() <= 1);
        }
        let packet = published.expect("bounded carrier inspection must publish once");
        assert_eq!(packet.source.as_ref(), wire);
        assert_eq!(packet.source_offset, 0);
        assert_eq!(packet.source_id, Some(SourceId(51)));
        assert_eq!(
            packet.prefix.as_ref().map(|bytes| bytes.as_ref()),
            (format == PacketFormat::M2ts192).then_some(&wire[..4])
        );
        assert!(matches!(
            framer
                .poll(&mut input, &mut WorkBudget::new(1), &cancel)
                .unwrap(),
            Step::End(at) if at.offset == wire.len() as u64
        ));
        let alias = packet.clone();
        drop(packet);
        drop(framer);
        drop(input);
        assert_eq!(alias.source.as_ref(), wire);
        assert_eq!(pool.used(), control + alias.source.retained_storage_bytes());
        drop(alias);
        assert_eq!(pool.used(), control);
    }
}

#[test]
fn pool_cancel_work_reserve_and_transport_failure_precedence_stays_observable() {
    let pool = Budget::new(4096).unwrap();
    let mut input = cursor(&pool, None);
    input
        .fail(input.source(), input.token(), SourceFailure::Transport)
        .unwrap();
    let other = Budget::new(4096).unwrap();
    let mut foreign = TransportFramer::new(other, PacketFormat::Ts188);
    let mut work = WorkBudget::new(1);
    assert!(matches!(
        foreign.poll(&mut input, &mut work, &AtomicBool::new(true)),
        Err(Error::ForeignPool)
    ));
    assert_eq!(work.spent(), 0);

    let mut framer = TransportFramer::new(pool.clone(), PacketFormat::Ts188);
    let blocker = pool.reserve(pool.available()).unwrap();
    let mut zero = WorkBudget::new(0);
    assert!(matches!(
        framer
            .poll(&mut input, &mut zero, &AtomicBool::new(false))
            .unwrap(),
        Step::Yield
    ));
    let mut reserve = WorkBudget::new(1);
    assert!(matches!(
        framer.poll(&mut input, &mut reserve, &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(reserve.spent(), 1);
    assert_eq!(input.position(), 0);
    drop(blocker);

    let mut admitted = WorkBudget::new(1);
    assert!(matches!(
        framer.poll(&mut input, &mut admitted, &AtomicBool::new(false)),
        Err(Error::Transport)
    ));
    assert_eq!(admitted.spent(), 1);
    let mut cancelled = WorkBudget::new(1);
    assert!(matches!(
        framer.poll(&mut input, &mut cancelled, &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert_eq!(cancelled.spent(), 0);
    let mut failed = WorkBudget::new(1);
    assert!(matches!(
        framer.poll(&mut input, &mut failed, &AtomicBool::new(false)),
        Err(Error::Transport)
    ));
    assert_eq!(failed.spent(), 0);
    assert_eq!(input.position(), 0);
    assert_eq!(framer.pending_or_last_packet_offset(), None);
}
