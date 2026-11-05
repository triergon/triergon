//! Original actual1728B failure and independent input-lifetime controls.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceId, WorkBudget,
};
use triergon_mpegts::clock::NativeClockProfile;
use triergon_mpegts::native_input::{
    NativeInputBootstrap, NativeInputContext, NativeInputStep, NativePositionBasis,
};
use triergon_mpegts::native_pes::{NativePesObserver, NativePesScope, NativeSourceContext};
use triergon_mpegts::{AssemblyStep, PacketFormat, ParseLimits, SourceEpochs, parse_transport};

const LONG: &[u8] = include_bytes!("data/native-input-v1/original-valid-rebind-192.m2ts");
const SHORT: &[u8] = include_bytes!("data/native-input-v1/opaque-PES-1-carrier-192-tail-3.m2ts");
const TS: &[u8] = include_bytes!("data/native-input-v1/original-valid-rebind-188.ts");

fn setup(bytes: &[u8], known: bool) -> (Budget, InputCursor, InputWindow) {
    let b = Budget::new(1 << 20).unwrap();
    let mut i = InputCursor::new(
        &b,
        SourceId(1),
        SourceCapabilities {
            length: known.then_some(bytes.len() as u64),
            random_access: true,
        },
        16384,
    )
    .unwrap();
    let w = InputWindow::new(
        i.source(),
        i.token(),
        0,
        Bytes::copy_from_slice(&b, bytes).unwrap(),
    )
    .unwrap();
    i.provide(&w).unwrap();
    (b, i, w)
}
fn complete(i: &InputCursor, w: &InputWindow, format: PacketFormat) -> NativeInputContext {
    let mut job = NativeInputBootstrap::new(i, w, format).unwrap();
    job.observe_entity_eof(i, w.source, &w.token, w.bytes().len() as u64)
        .unwrap();
    match job
        .poll(i, &mut WorkBudget::new(20000), &AtomicBool::new(false))
        .unwrap()
    {
        NativeInputStep::Ready(c) => c,
        _ => panic!("finite borrowed context did not complete"),
    }
}
fn observer(b: &Budget) -> NativePesObserver {
    NativePesObserver::new(
        NativeClockProfile::ArchivedPesV1,
        NativePesScope {
            program: 1,
            pid: 256,
            stream_type: 0x1b,
            stream_id: 0xe0,
            entity: SourceId(1),
            epochs: SourceEpochs::default(),
        },
        b.clone(),
        ParseLimits::default(),
    )
    .unwrap()
}
#[test]
fn actual_original_long_m2ts_first_pes_uses_carrier_start_with_real_context() {
    let (b, i, w) = setup(LONG, true);
    let c = complete(&i, &w, PacketFormat::M2ts192);
    let mut o = observer(&b);
    o.bind_native_input_v1(&c, &i).unwrap();
    let mut packets = Vec::new();
    for n in 2..5 {
        let mut p = parse_transport(
            Bytes::copy_from_slice(&b, &LONG[n * 192..(n + 1) * 192]).unwrap(),
            PacketFormat::M2ts192,
            (n * 192) as u64,
        )
        .unwrap();
        p.source_id = Some(SourceId(1));
        o.push_with_input(
            p,
            NativeSourceContext {
                source_event_ordinal: n as u64,
                epochs: SourceEpochs::default(),
                byte_run_epoch: None,
            },
            &i,
        )
        .unwrap();
        for _ in 0..100000 {
            match o
                .poll_with_input(&i, &mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                AssemblyStep::Output(p) => packets.push(p),
                AssemblyStep::Yield => {}
                AssemblyStep::NeedPayload => break,
                AssemblyStep::Finished => panic!("no logical EOF"),
            }
        }
    }
    assert_eq!(packets.len(), 1);
    assert_eq!(
        packets[0].payload().flatten().copied().collect::<Vec<_>>(),
        include_bytes!("data/native-input-v1/original-0.payload")
    );
    assert_eq!(packets[0].source_origin.offset, 384);
    assert_eq!(packets[0].source_header.pts_raw33, Some(90000));
    assert_eq!(packets[0].source_header.dts_raw33, None);
    assert_eq!(packets[0].native_flags, 0);
    assert_eq!(
        packets[0].native_position, 384,
        "literal pinned7f9372 native AVPacket.pos; original388 failure remains preserved"
    );
}
#[test]
fn short_192_stays_sync_start() {
    let (_, i, w) = setup(SHORT, true);
    assert_eq!(
        complete(&i, &w, PacketFormat::M2ts192).basis(),
        NativePositionBasis::SyncStart
    );
}
#[test]
fn ordinary_188_stays_carrier_start() {
    let (_, i, w) = setup(TS, true);
    assert_eq!(
        complete(&i, &w, PacketFormat::Ts188).basis(),
        NativePositionBasis::CarrierStart
    );
}
#[test]
fn declared_size_alone_is_not_eof() {
    let (_, i, w) = setup(LONG, true);
    let mut j = NativeInputBootstrap::new(&i, &w, PacketFormat::M2ts192).unwrap();
    let mut work = WorkBudget::new(10000);
    assert!(matches!(
        j.poll(&i, &mut work, &AtomicBool::new(false)).unwrap(),
        NativeInputStep::NeedEntityEof
    ));
    assert_eq!(work.spent(), 0);
    assert_eq!(j.inspected_bytes(), 0);
}
#[test]
fn work_zero_one_counts_actual_inspection_without_source_consumption() {
    let (_, i, w) = setup(LONG, true);
    let mut j = NativeInputBootstrap::new(&i, &w, PacketFormat::M2ts192).unwrap();
    j.observe_entity_eof(&i, w.source, &w.token, LONG.len() as u64)
        .unwrap();
    assert!(matches!(
        j.poll(&i, &mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        NativeInputStep::Yield
    ));
    assert_eq!(j.inspected_bytes(), 0);
    for n in 1..=LONG.len() {
        let mut work = WorkBudget::new(1);
        let step = j.poll(&i, &mut work, &AtomicBool::new(false)).unwrap();
        assert_eq!(work.spent(), 1);
        assert_eq!(j.inspected_bytes(), n);
        assert_eq!(matches!(step, NativeInputStep::Ready(_)), n == LONG.len());
    }
    assert_eq!(i.position(), 0);
}
#[test]
fn cancellation_does_no_inspection_and_preserves_retry() {
    let (_, i, w) = setup(LONG, true);
    let mut j = NativeInputBootstrap::new(&i, &w, PacketFormat::M2ts192).unwrap();
    j.observe_entity_eof(&i, w.source, &w.token, LONG.len() as u64)
        .unwrap();
    let cancel = AtomicBool::new(true);
    assert_eq!(
        j.poll(&i, &mut WorkBudget::new(1), &cancel).unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(j.inspected_bytes(), 0);
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(
        j.poll(&i, &mut WorkBudget::new(1), &cancel).unwrap(),
        NativeInputStep::Yield
    ));
    assert_eq!(j.inspected_bytes(), 1);
}
#[test]
fn unknown_declared_size_can_use_actual_eof() {
    let (_, i, w) = setup(LONG, false);
    assert_eq!(
        complete(&i, &w, PacketFormat::M2ts192).basis(),
        NativePositionBasis::CarrierStart
    );
}
#[test]
fn reset_invalidates_even_identical_source_bytes() {
    let (_, mut i, w) = setup(LONG, true);
    let c = complete(&i, &w, PacketFormat::M2ts192);
    i.reset(0).unwrap();
    assert!(c.validate_cursor(&i).is_err());
}
#[test]
fn foreign_pool_with_same_numeric_request_is_not_evidence() {
    let (_, i, w) = setup(LONG, true);
    let (_, other, _) = setup(LONG, true);
    assert!(NativeInputBootstrap::new(&other, &w, PacketFormat::M2ts192).is_err());
    assert_eq!(i.position(), 0);
}
#[test]
fn unprovided_owner_cannot_be_substituted() {
    let (b, i, _) = setup(LONG, true);
    let alternate = InputWindow::new(
        i.source(),
        i.token(),
        0,
        Bytes::copy_from_slice(&b, LONG).unwrap(),
    )
    .unwrap();
    assert!(NativeInputBootstrap::new(&i, &alternate, PacketFormat::M2ts192).is_err());
}
#[test]
fn competing_grid_is_visible_refusal() {
    let bytes = include_bytes!("data/native-input-v1/false188-7.bin");
    let (_, i, w) = setup(bytes, true);
    let mut j = NativeInputBootstrap::new(&i, &w, PacketFormat::M2ts192).unwrap();
    j.observe_entity_eof(&i, w.source, &w.token, bytes.len() as u64)
        .unwrap();
    assert!(matches!(
        j.poll(&i, &mut WorkBudget::new(20000), &AtomicBool::new(false)),
        Err(Error::Unsupported(_))
    ));
}
#[test]
fn damaged_sync_is_visible_refusal() {
    let bytes = include_bytes!("data/native-input-v1/seventh-sync-zero.bin");
    let (_, i, w) = setup(bytes, true);
    let mut j = NativeInputBootstrap::new(&i, &w, PacketFormat::M2ts192).unwrap();
    j.observe_entity_eof(&i, w.source, &w.token, bytes.len() as u64)
        .unwrap();
    assert!(matches!(
        j.poll(&i, &mut WorkBudget::new(20000), &AtomicBool::new(false)),
        Err(Error::Unsupported(_))
    ));
}
#[test]
fn borrowed_full_hidden_owner_stays_charged_without_new_allocation() {
    let b = Budget::new(10000).unwrap();
    let mut raw = Vec::with_capacity(6000);
    raw.extend_from_slice(LONG);
    let owner = Bytes::try_from_vec(&b, raw).unwrap();
    let capacity = owner.retained_capacity();
    let mut i = InputCursor::new(
        &b,
        SourceId(1),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        16384,
    )
    .unwrap();
    let w = InputWindow::new(i.source(), i.token(), 0, owner).unwrap();
    i.provide(&w).unwrap();
    let before = b.used();
    let hold = b.reserve(b.available()).unwrap();
    let c = complete(&i, &w, PacketFormat::M2ts192);
    assert_eq!(b.used(), b.limit());
    assert_eq!(c.backing_bytes().retained_capacity(), capacity);
    drop(i);
    drop(w);
    drop(hold);
    assert_eq!(b.used(), before);
    drop(c);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn bound_observer_cannot_bypass_input_token_on_poll() {
    let (b, mut i, w) = setup(LONG, true);
    let c = complete(&i, &w, PacketFormat::M2ts192);
    let mut o = observer(&b);
    o.bind_native_input_v1(&c, &i).unwrap();
    assert!(
        o.poll(&mut WorkBudget::new(100), &AtomicBool::new(false))
            .is_err()
    );
    i.reset(0).unwrap();
    assert!(
        o.poll_with_input(&i, &mut WorkBudget::new(100), &AtomicBool::new(false))
            .is_err()
    );
}

#[test]
fn bound_observer_does_not_retain_unrelated_startup_owner_after_inspection() {
    let (b, mut i, w) = setup(LONG, true);
    let c = complete(&i, &w, PacketFormat::M2ts192);
    let mut o = observer(&b);
    o.bind_native_input_v1(&c, &i).unwrap();
    drop(c);
    drop(w);
    let mut work = WorkBudget::new(LONG.len() as u64);
    match i
        .read_discard(LONG.len(), &mut work, &AtomicBool::new(false))
        .unwrap()
    {
        triergon_core::Step::Ready(n) => assert_eq!(n, LONG.len()),
        _ => panic!("real original window must be consumed, not counter-adjusted"),
    }
    assert_eq!(work.spent(), LONG.len() as u64);
    assert_eq!(i.position(), LONG.len() as u64);
    assert_eq!(
        b.used(),
        Budget::control_storage_bytes(),
        "bound observer needs only checked source/token/range/basis facts, not the unrelated full input allocation"
    );
    assert!(matches!(
        o.poll_with_input(&i, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        AssemblyStep::NeedPayload
    ));
}
