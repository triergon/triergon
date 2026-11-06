#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputWindow, SourceCapabilities, SourceFailure, SourceId, Step,
    WorkBudget,
};
use triergon_framing::{
    Dts14CarrierBoundary, Dts14CarrierLimits, Dts14CarrierPacket, Dts14CarrierReader,
    Dts14CarrierSyntax, DtsWordOrder,
};
const FIXED: &[u8] = include_bytes!("../fixtures/dts-native-v1/fixed-big.bin");
const GOOD: &[u8] =
    include_bytes!("../fixtures/dts-native-v1/primary-sync-example-variable-big.bin");
fn limits() -> Dts14CarrierLimits {
    Dts14CarrierLimits {
        maximum_packet_bytes: 60000,
        maximum_packets: 4096,
        maximum_input_window: 4096,
    }
}
fn acquire(
    data: &[u8],
    order: DtsWordOrder,
    chunk: usize,
    work: u64,
) -> (Budget, Vec<Dts14CarrierPacket>) {
    let pool = Budget::new(2_000_000).unwrap();
    let mut r = Dts14CarrierReader::new(
        &pool,
        SourceId(71),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        order,
        limits(),
    )
    .unwrap();
    let mut out = Vec::new();
    for _ in 0..2_000_000 {
        match r
            .step(&mut WorkBudget::new(work), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(p) => out.push(p),
            Step::Yield => {}
            Step::End(e) => {
                assert_eq!(e.offset, data.len() as u64);
                drop(r);
                return (pool, out);
            }
            Step::NeedInput(q) => {
                let at = q.offset as usize;
                if at == data.len() {
                    r.finish(q.source, q.token).unwrap();
                } else {
                    let end = data.len().min(at + chunk.min(q.maximum));
                    let b = Bytes::copy_from_slice(&pool, &data[at..end]).unwrap();
                    r.provide(&InputWindow::new(q.source, q.token, q.offset, b).unwrap())
                        .unwrap();
                }
            }
        }
    }
    panic!("bounded acquisition exhausted");
}
#[test]
fn nonconventional_marker_is_one_owned_opaque_eof_packet() {
    for chunk in [1, 31] {
        for work in [1, 31] {
            let (pool, p) = acquire(FIXED, DtsWordOrder::BigEndian, chunk, work);
            assert_eq!(p.len(), 1);
            assert_eq!(p[0].source_bytes().as_ref(), FIXED);
            assert_eq!(p[0].syntax, Dts14CarrierSyntax::OpaqueCarrier);
            assert!(p[0].canonical_bytes().is_none());
            assert_eq!(p[0].boundary, Dts14CarrierBoundary::ExplicitSourceEnd);
            drop(p);
            assert_eq!(pool.used(), Budget::control_storage_bytes());
        }
    }
}
#[test]
fn conventional_variable_carrier_has_literal_ranges_and_headers() {
    for chunk in [1, 31] {
        for work in [1, 31] {
            let (pool, p) = acquire(GOOD, DtsWordOrder::BigEndian, chunk, work);
            assert_eq!(p.len(), 3);
            for (i, (at, end)) in [(0, 112), (112, 240), (240, 400)].into_iter().enumerate() {
                assert_eq!(p[i].identity.offset, at as u64);
                assert_eq!(p[i].source_bytes().as_ref(), &GOOD[at..end]);
                assert!(matches!(p[i].syntax, Dts14CarrierSyntax::ExactNormal(_)));
                assert_eq!(p[i].canonical_bytes().unwrap().len(), (end - at) * 7 / 8);
            }
            assert_eq!(
                p[0].boundary,
                Dts14CarrierBoundary::FollowingCarrierSync { offset: 112 }
            );
            drop(p);
            assert_eq!(pool.used(), Budget::control_storage_bytes());
        }
    }
}
#[test]
fn endian_views_do_not_change_source_bytes_or_order() {
    let mut le = GOOD.to_vec();
    for pair in le.as_chunks_mut::<2>().0 {
        pair.swap(0, 1);
    }
    let (_, a) = acquire(GOOD, DtsWordOrder::BigEndian, 1, 31);
    let (_, b) = acquire(&le, DtsWordOrder::LittleEndian, 31, 1);
    for (a, b) in a.iter().zip(&b) {
        assert_eq!(
            a.canonical_bytes().map(|v| v.as_ref()),
            b.canonical_bytes().map(|v| v.as_ref())
        );
        assert_eq!(a.syntax, b.syntax);
    }
    assert_eq!(
        b.iter()
            .flat_map(|p| p.source_bytes().as_ref().iter().copied())
            .collect::<Vec<_>>(),
        le
    );
}
#[test]
fn raw_high_bits_are_never_repaired_into_conventional_sync() {
    let (_, p) = acquire(FIXED, DtsWordOrder::BigEndian, 1, 31);
    assert_eq!(
        &p[0].source_bytes()[..6],
        &[0x1f, 0xff, 0x68, 0, 0x87, 0xf0]
    );
    assert_eq!(p[0].syntax, Dts14CarrierSyntax::OpaqueCarrier);
}
#[test]
fn empty_finish_does_not_publish_packet() {
    let (b, p) = acquire(&[], DtsWordOrder::BigEndian, 1, 1);
    assert!(p.is_empty());
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn missing_header_is_refusal_not_validated_normal() {
    let (_, p) = acquire(&GOOD[..8], DtsWordOrder::BigEndian, 1, 1);
    assert_eq!(p.len(), 1);
    assert!(matches!(p[0].syntax, Dts14CarrierSyntax::Refused(_)));
}
#[test]
fn odd_conventional_tail_remains_raw_truncated_refusal() {
    let (_, p) = acquire(&GOOD[..111], DtsWordOrder::BigEndian, 1, 1);
    assert_eq!(p[0].source_bytes().len(), 111);
    assert_eq!(p[0].syntax, Dts14CarrierSyntax::Refused(Error::Truncated));
    assert!(p[0].canonical_bytes().is_none());
}
#[test]
fn unresolved_extent_is_not_complete_syntax() {
    let (_, p) = acquire(&GOOD[..110], DtsWordOrder::BigEndian, 31, 1);
    assert!(matches!(
        p[0].syntax,
        Dts14CarrierSyntax::Refused(Error::Unsupported(_))
    ));
}
#[test]
fn arbitrary_opaque_bytes_are_not_empty_or_syntax() {
    let (_, p) = acquire(&[9, 8, 7, 6, 5, 4, 3], DtsWordOrder::BigEndian, 1, 1);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].source_bytes().as_ref(), &[9, 8, 7, 6, 5, 4, 3]);
    assert_eq!(p[0].syntax, Dts14CarrierSyntax::OpaqueCarrier);
}
fn reader() -> (Budget, Dts14CarrierReader) {
    let b = Budget::new(2_000_000).unwrap();
    let r = Dts14CarrierReader::new(
        &b,
        SourceId(71),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        DtsWordOrder::BigEndian,
        limits(),
    )
    .unwrap();
    (b, r)
}
#[test]
fn zero_work_cannot_read_input() {
    let (_, mut r) = reader();
    assert!(matches!(
        r.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(Step::Yield)
    ));
    assert_eq!(r.position(), 0);
}
#[test]
fn cancelled_zero_work_is_cancelled_and_sticky() {
    let (b, mut r) = reader();
    assert_eq!(
        r.step(&mut WorkBudget::new(0), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(
        r.step(&mut WorkBudget::new(31), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Cancelled
    );
    drop(r);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn transport_never_flushes_buffer_as_eof() {
    let (b, mut r) = reader();
    let q = match r
        .step(&mut WorkBudget::new(31), &AtomicBool::new(false))
        .unwrap()
    {
        Step::NeedInput(q) => q,
        _ => panic!(),
    };
    r.provide(
        &InputWindow::new(
            q.source,
            q.token,
            q.offset,
            Bytes::copy_from_slice(&b, &FIXED[..100]).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    loop {
        if let Step::NeedInput(q) = r
            .step(&mut WorkBudget::new(31), &AtomicBool::new(false))
            .unwrap()
        {
            r.fail(q.source, q.token, SourceFailure::Transport).unwrap();
            break;
        }
    }
    assert_eq!(
        r.step(&mut WorkBudget::new(31), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Transport
    );
    drop(r);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn stale_input_after_reset_is_rejected() {
    let (b, mut r) = reader();
    let q = r.request();
    r.reset(0).unwrap();
    let w = InputWindow::new(
        q.source,
        q.token,
        q.offset,
        Bytes::copy_from_slice(&b, &[1]).unwrap(),
    )
    .unwrap();
    assert!(r.provide(&w).is_err());
}
#[test]
fn foreign_source_and_odd_reset_are_rejected() {
    let (_, mut r) = reader();
    let q = r.request();
    assert!(r.finish(SourceId(72), q.token).is_err());
    assert!(r.reset(1).is_err());
    assert_eq!(r.position(), 0);
}
#[test]
fn hard_bound_does_not_silently_truncate_packet() {
    let b = Budget::new(2_000_000).unwrap();
    let mut l = limits();
    l.maximum_packet_bytes = 8;
    let mut r = Dts14CarrierReader::new(
        &b,
        SourceId(71),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        DtsWordOrder::BigEndian,
        l,
    )
    .unwrap();
    for _ in 0..1000 {
        match r.step(&mut WorkBudget::new(31), &AtomicBool::new(false)) {
            Err(Error::Limit { .. }) => {
                drop(r);
                assert_eq!(b.used(), Budget::control_storage_bytes());
                return;
            }
            Err(e) => panic!("{e:?}"),
            Ok(Step::NeedInput(q)) => {
                let at = q.offset as usize;
                let end = (at + 1).min(FIXED.len());
                r.provide(
                    &InputWindow::new(
                        q.source,
                        q.token,
                        q.offset,
                        Bytes::copy_from_slice(&b, &FIXED[at..end]).unwrap(),
                    )
                    .unwrap(),
                )
                .unwrap();
            }
            _ => {}
        }
    }
    panic!("no bound refusal");
}

#[test]
fn cancellation_releases_an_admitted_window_before_reader_drop() {
    let (b, mut r) = reader();
    let q = r.request();
    r.provide(
        &InputWindow::new(
            q.source,
            q.token,
            q.offset,
            Bytes::copy_from_slice(&b, &FIXED[..100]).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(b.used() > Budget::control_storage_bytes());
    assert_eq!(
        r.step(&mut WorkBudget::new(0), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(b.used(), Budget::control_storage_bytes());
    assert_eq!(
        r.step(&mut WorkBudget::new(31), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Cancelled
    );
}

#[test]
fn tiny_packets_retain_bounded_actual_capacity_after_producer_close() {
    let (pool, packets) = acquire(GOOD, DtsWordOrder::BigEndian, 31, 1);
    for packet in &packets {
        assert!(packet.source_bytes().retained_capacity() <= 256);
    }
    let owned: usize = packets
        .iter()
        .map(|p| {
            p.source_bytes().retained_storage_bytes()
                + p.canonical_bytes().map_or(0, Bytes::retained_storage_bytes)
        })
        .sum();
    assert_eq!(pool.used(), Budget::control_storage_bytes() + owned);
    assert!(pool.used() > Budget::control_storage_bytes());
    drop(packets);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn embedded_carrier_before_declared_extent_does_not_cut_a_packet() {
    let mut input = GOOD.to_vec();
    input[96..102].copy_from_slice(&GOOD[..6]);
    let (_, p) = acquire(&input, DtsWordOrder::BigEndian, 1, 1);
    assert_eq!(p.len(), 3);
    assert_eq!(p[0].source_bytes().as_ref(), &input[..112]);
    assert_eq!(
        p[0].boundary,
        Dts14CarrierBoundary::FollowingCarrierSync { offset: 112 }
    );
}
fn refusal(data: &[u8]) -> Error {
    let (pool, mut r) = reader();
    for _ in 0..1000000 {
        match r.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Err(e) => return e,
            Ok(Step::NeedInput(q)) => {
                let at = q.offset as usize;
                if at == data.len() {
                    r.finish(q.source, q.token).unwrap();
                } else {
                    let end = data.len().min(at + 1);
                    r.provide(
                        &InputWindow::new(
                            q.source,
                            q.token,
                            q.offset,
                            Bytes::copy_from_slice(&pool, &data[at..end]).unwrap(),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                }
            }
            Ok(Step::Yield) => {}
            Ok(Step::Ready(_)) => panic!("unsupported boundary was published"),
            Ok(Step::End(_)) => panic!("unsupported boundary became empty EOF"),
        }
    }
    panic!("refusal not bounded");
}
#[test]
fn opaque_prefix_with_embedded_conventional_sync_is_explicitly_unsupported() {
    let mut input = vec![0x55; 112];
    input.extend_from_slice(&GOOD[..112]);
    assert!(matches!(refusal(&input), Error::Unsupported(_)));
}
#[test]
fn late_carrier_sync_does_not_guess_a_new_fsize_domain() {
    let mut input = GOOD[..112].to_vec();
    input.extend_from_slice(&[0x55, 0x55]);
    input.extend_from_slice(&GOOD[..112]);
    assert!(matches!(refusal(&input), Error::Unsupported(_)));
}

#[test]
fn consumption_view_uses_actual_admitted_windows_not_physical_boundary_alias() {
    use triergon_framing::Dts14D32ParserHints;
    let (_, one) = acquire(GOOD, DtsWordOrder::BigEndian, 1, 1);
    let (_, wide) = acquire(GOOD, DtsWordOrder::BigEndian, 31, 31);
    assert_eq!(
        (
            one[0].provision.last_window_start,
            one[0].provision.last_window_end,
            one[0].provision.consumed_through
        ),
        (117, 118, 118)
    );
    assert_eq!(
        (
            wide[0].provision.last_window_start,
            wide[0].provision.last_window_end,
            wide[0].provision.consumed_through
        ),
        (93, 124, 118)
    );
    assert_eq!(
        Dts14D32ParserHints::observe(&one[0])
            .unwrap()
            .consumed_offset,
        117
    );
    assert_eq!(
        Dts14D32ParserHints::observe(&wide[0])
            .unwrap()
            .consumed_offset,
        112
    );
    for p in [&one[0], &wide[0]] {
        assert_eq!(p.identity.offset, 0);
        assert_eq!(p.source_bytes().len(), 112);
        assert_eq!(
            Dts14D32ParserHints::observe(p).unwrap().opaque_next_offset,
            112
        );
    }
}
#[test]
fn explicit_flush_hint_is_signed_and_never_replaces_source_end() {
    use triergon_framing::{Dts14D32ParserHints, Dts14ParserHintOrigin};
    let (_, p) = acquire(&[0x55; 7], DtsWordOrder::BigEndian, 1, 1);
    let h = Dts14D32ParserHints::observe(&p[0]).unwrap();
    assert_eq!(
        (h.frame_offset, h.consumed_offset, h.opaque_next_offset),
        (0, 7, -93)
    );
    assert_eq!(h.origin, Dts14ParserHintOrigin::ExplicitFlushLookbehind);
    assert_eq!(p[0].provision.consumed_through, 7);
    assert_eq!(p[0].boundary, Dts14CarrierBoundary::ExplicitSourceEnd);
}
#[test]
fn parser_hint_refuses_inconsistent_provision_evidence() {
    use triergon_framing::Dts14D32ParserHints;
    let (_, p) = acquire(GOOD, DtsWordOrder::BigEndian, 31, 1);
    let mut q = p[0].clone();
    q.provision.consumed_through -= 1;
    assert!(Dts14D32ParserHints::observe(&q).is_err());
    let mut q = p[0].clone();
    q.provision.last_window_start = q.provision.last_window_end + 1;
    assert!(Dts14D32ParserHints::observe(&q).is_err());
}
#[test]
fn known_source_length_still_requires_real_finish_before_opaque_publication() {
    let pool = Budget::new(2_000_000).unwrap();
    let mut r = Dts14CarrierReader::new(
        &pool,
        SourceId(71),
        SourceCapabilities {
            length: Some(FIXED.len() as u64),
            random_access: false,
        },
        DtsWordOrder::BigEndian,
        limits(),
    )
    .unwrap();
    let q = r.request();
    r.provide(
        &InputWindow::new(
            q.source,
            q.token,
            q.offset,
            Bytes::copy_from_slice(&pool, FIXED).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    loop {
        match r
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::NeedInput(q) => {
                assert_eq!(q.offset, FIXED.len() as u64);
                r.finish(q.source, q.token).unwrap();
                break;
            }
            Step::Yield => {}
            _ => panic!("length advertisement became EOF"),
        }
    }
    loop {
        match r
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(p) => {
                assert_eq!(p.source_bytes().as_ref(), FIXED);
                break;
            }
            Step::Yield => {}
            _ => panic!("no explicit EOF packet"),
        }
    }
}

#[test]
fn checked_growth_backpressure_retries_without_losing_bytes_or_input_identity() {
    let pool = Budget::new(2000).unwrap();
    let mut blocker = Some(Bytes::copy_from_slice(&pool, &[0x33; 1600]).unwrap());
    let mut r = Dts14CarrierReader::new(
        &pool,
        SourceId(71),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        DtsWordOrder::BigEndian,
        limits(),
    )
    .unwrap();
    let generation = r.generation();
    let mut pressured = false;
    for _ in 0..100000 {
        match r.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Err(Error::Backpressure { .. }) => {
                assert!(!pressured);
                pressured = true;
                drop(blocker.take());
            }
            Err(e) => panic!("{e:?}"),
            Ok(Step::Yield) => {}
            Ok(Step::NeedInput(q)) => {
                let at = q.offset as usize;
                if at == FIXED.len() {
                    r.finish(q.source, q.token).unwrap();
                } else {
                    r.provide(
                        &InputWindow::new(
                            q.source,
                            q.token,
                            q.offset,
                            Bytes::copy_from_slice(&pool, &FIXED[at..at + 1]).unwrap(),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                }
            }
            Ok(Step::Ready(p)) => {
                assert!(pressured);
                assert_eq!(r.generation(), generation);
                assert_eq!(p.source_bytes().as_ref(), FIXED);
                assert_eq!(p.source_bytes().retained_capacity(), 256);
                drop(p);
                drop(r);
                assert_eq!(pool.used(), Budget::control_storage_bytes());
                return;
            }
            Ok(Step::End(_)) => panic!("growth pressure became empty EOF"),
        }
    }
    panic!("growth retry exhausted");
}
