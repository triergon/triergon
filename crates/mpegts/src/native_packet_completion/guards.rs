use super::*;
use crate::{SourceEpochs, TransportStream};
use triergon_core::{InputWindow, SourceCapabilities, SourceFailure};
const BAD_CRC: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/data/psi-crc-invalid.ts"
));
const BASIC: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/data/ts188-basic.ts"
));
const INACTIVE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/data/psi-pending-not-current.ts"
));
#[allow(
    clippy::large_enum_variant,
    reason = "test recorder retains the original inline owned source event; no extra boxed allocation"
)]
enum Item {
    Event(TraceEvent, NativeSourceContext),
    End(MediaEntityEndV1),
}
struct Run {
    items: Vec<Item>,
    input: InputCursor,
}
fn source(data: &[u8], budget: &Budget) -> Run {
    let mut input = InputCursor::new(
        budget,
        SourceId(1),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        4096,
    )
    .unwrap();
    let mut stream = TransportStream::new(
        budget.clone(),
        PacketFormat::Ts188,
        ParseLimits::default(),
        1,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut ordinal = 0;
    let mut items = Vec::new();
    for _ in 0..500_000 {
        match stream
            .poll(&mut input, &mut WorkBudget::new(17), &cancel)
            .unwrap()
        {
            Step::Ready(event) => {
                let finished = matches!(event, TraceEvent::Finished);
                let context = NativeSourceContext {
                    source_event_ordinal: ordinal,
                    epochs: stream.source_epochs(),
                    byte_run_epoch: None,
                };
                ordinal += 1;
                items.push(Item::Event(event, context));
                if finished {
                    return Run { items, input };
                }
            }
            Step::Yield => {}
            Step::NeedInput(r) => {
                let at = usize::try_from(r.offset).unwrap();
                if at == data.len() {
                    input.finish(r.source, r.token).unwrap();
                } else {
                    let end = data.len().min(at + r.maximum.min(7));
                    input
                        .provide(
                            &InputWindow::new(
                                r.source,
                                r.token,
                                r.offset,
                                Bytes::copy_from_slice(budget, &data[at..end]).unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                }
            }
            Step::End(_) => {
                let end = stream
                    .media_entity_end_v1(&mut input, &mut WorkBudget::new(1), &cancel)
                    .unwrap()
                    .unwrap();
                items.push(Item::End(end));
                stream.end_of_source().unwrap();
            }
        }
    }
    panic!("bounded source guard did not finish")
}
fn view(b: &Budget) -> NativePacketCompletionV1 {
    NativePacketCompletionV1::new(b.clone(), ParseLimits::default(), 1).unwrap()
}
fn feed(v: &mut NativePacketCompletionV1, item: &Item, input: &mut InputCursor) -> Result<bool> {
    let cancel = AtomicBool::new(false);
    match item {
        Item::Event(e, c) => {
            let work = v.required_work_v1(e)?;
            v.observe_v1(e, SourceId(1), *c, &mut WorkBudget::new(work), &cancel)
        }
        Item::End(end) => {
            let work = v.required_end_work_v1()?;
            v.seal_physical_end_v1(end, input, &mut WorkBudget::new(work), &cancel)
        }
    }
}
fn all(v: &mut NativePacketCompletionV1, run: &mut Run) -> Result<()> {
    for item in &run.items {
        assert!(feed(v, item, &mut run.input)?);
    }
    Ok(())
}
fn finish(
    v: &mut NativePacketCompletionV1,
    input: &mut InputCursor,
) -> Result<Option<NativeNoMediaReceiptV1>> {
    let work = v.required_end_work_v1()?;
    v.confirm_no_media_v1(input, &mut WorkBudget::new(work), &AtomicBool::new(false))
}
fn first(run: &Run) -> (&TransportPacket, NativeSourceContext) {
    match &run.items[0] {
        Item::Event(TraceEvent::Transport(p), c) => (p, *c),
        _ => panic!("first carrier fixture"),
    }
}
#[test]
fn compact_null_runs_keep_physical_bytes_order_and_completion() {
    use crate::CompactTraceEvent;

    let mut data = BAD_CRC.to_vec();
    for cc in 0..19 {
        let mut carrier = [cc; 188];
        carrier[..6].copy_from_slice(&[0x47, 0x1f, 0xff, 0x30 | (cc & 15), 1, 0]);
        data.extend(carrier);
    }

    for window in [97, 1013, 4096] {
        let budget = Budget::new(8 * 1024 * 1024).unwrap();
        let literal = source(&data, &budget);
        let mut expected = literal.items.iter().filter_map(|item| match item {
            Item::Event(TraceEvent::Transport(packet), context) => Some((packet, context)),
            _ => None,
        });
        let mut input = InputCursor::new(
            &budget,
            SourceId(1),
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            window,
        )
        .unwrap();
        let mut stream = TransportStream::new(
            budget.clone(),
            PacketFormat::Ts188,
            ParseLimits::default(),
            1,
        )
        .unwrap();
        let mut completion = view(&budget);
        let cancel = AtomicBool::new(false);
        let mut ordinal = 0;
        for _ in 0..10_000 {
            match stream
                .poll_compact(&mut input, &mut WorkBudget::new(4096), &cancel)
                .unwrap()
            {
                Step::Ready(CompactTraceEvent::Event(event)) => {
                    let context = NativeSourceContext {
                        source_event_ordinal: ordinal,
                        epochs: stream.source_epochs(),
                        byte_run_epoch: None,
                    };
                    ordinal += 1;
                    if let TraceEvent::Transport(packet) = &event {
                        let (want, want_context) = expected.next().unwrap();
                        assert_eq!(
                            (packet.source_offset, packet.source.as_ref(), &context),
                            (want.source_offset, want.source.as_ref(), want_context)
                        );
                    }
                    let done = matches!(event, TraceEvent::Finished);
                    assert!(
                        feed(&mut completion, &Item::Event(event, context), &mut input).unwrap()
                    );
                    if done {
                        break;
                    }
                }
                Step::Ready(CompactTraceEvent::NullRun(run)) => {
                    let context = NativeSourceContext {
                        source_event_ordinal: ordinal,
                        epochs: stream.source_epochs(),
                        byte_run_epoch: None,
                    };
                    assert!(
                        !completion
                            .observe_null_run(&run, context, &mut WorkBudget::new(0), &cancel)
                            .unwrap()
                    );
                    assert!(
                        completion
                            .observe_null_run(&run, context, &mut WorkBudget::new(1), &cancel)
                            .unwrap()
                    );
                    for index in 0..run.len() {
                        let packet = run.packet(index).unwrap();
                        let (want, want_context) = expected.next().unwrap();
                        assert_eq!(
                            (
                                packet.source_offset,
                                packet.source.as_ref(),
                                packet.source_id
                            ),
                            (want.source_offset, want.source.as_ref(), want.source_id)
                        );
                        assert_eq!(want_context.source_event_ordinal, ordinal);
                        ordinal += 1;
                    }
                }
                Step::NeedInput(r) => {
                    let at = r.offset as usize;
                    if at == data.len() {
                        input.finish(r.source, r.token).unwrap();
                    } else {
                        let end = data.len().min(at + r.maximum);
                        input
                            .provide_owned(
                                InputWindow::new(
                                    r.source,
                                    r.token,
                                    r.offset,
                                    Bytes::copy_from_slice(&budget, &data[at..end]).unwrap(),
                                )
                                .unwrap(),
                            )
                            .unwrap();
                    }
                }
                Step::End(_) => {
                    let end = stream
                        .media_entity_end_v1(&mut input, &mut WorkBudget::new(1), &cancel)
                        .unwrap()
                        .unwrap();
                    assert!(feed(&mut completion, &Item::End(end), &mut input).unwrap());
                    stream.end_of_source().unwrap();
                }
                Step::Yield => {}
            }
        }
        let receipt = finish(&mut completion, &mut input).unwrap().unwrap();
        assert!(expected.next().is_none());
        assert_eq!(receipt.physical_carriers(), data.len() / 188);
        assert_eq!(completion.physical_null_runs().is_empty(), window < 188);
    }
}

#[test]
fn original_control_only_bytes_complete_without_a_pes_scope() {
    for data in [
        BAD_CRC,
        INACTIVE,
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/psi-pointer-overrun.ts"
        ))
        .as_slice(),
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/psi-length-over-policy.ts"
        ))
        .as_slice(),
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/pat-incomplete-version.ts"
        ))
        .as_slice(),
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/pat-network-pid-not-track.ts"
        ))
        .as_slice(),
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/psi-pointer-finishes-old-starts-new.ts"
        ))
        .as_slice(),
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/psi-two-sections-one-payload.ts"
        ))
        .as_slice(),
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/pmt-descriptor-overrun.ts"
        ))
        .as_slice(),
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/pmt-multi-section-forbidden.ts"
        ))
        .as_slice(),
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/psi-stuffing-tail-invalid.ts"
        ))
        .as_slice(),
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/psi-truncated-eof.ts"
        ))
        .as_slice(),
    ] {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(data, &b);
        let mut v = view(&b);
        all(&mut v, &mut run).unwrap();
        let receipt = finish(&mut v, &mut run.input).unwrap().unwrap();
        assert_eq!(receipt.source_bytes(), data.len() as u64);
        assert_eq!(receipt.physical_carriers(), data.len() / 188);
        for evidence in v.physical_carriers() {
            let p = evidence.packet();
            assert_eq!(
                p.source.as_ref(),
                &data[p.source_offset as usize..p.source_offset as usize + 188]
            );
            assert_ne!(
                evidence.kind(),
                NativeControlCarrierKindV1::RequiresScopedPacketObserver
            );
        }
    }
}
#[test]
fn inactive_pat_association_stays_inactive_and_raw() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(INACTIVE, &b);
    let mut v = view(&b);
    all(&mut v, &mut run).unwrap();
    finish(&mut v, &mut run.input).unwrap().unwrap();
    assert!(!v.raw_pat_facts()[0].section().header.current_next);
    assert_eq!(
        v.physical_carriers()[1].kind(),
        NativeControlCarrierKindV1::PmtSection { association: 0 }
    );
    assert_eq!(
        v.raw_pat_facts()[0].section().source.as_ref(),
        &INACTIVE[172..188]
    );
}
#[test]
fn zero_event_work_is_unconsumed_and_retryable() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(BAD_CRC, &b);
    let mut v = view(&b);
    let Item::Event(e, c) = &run.items[0] else {
        panic!("carrier")
    };
    assert!(
        !v.observe_v1(
            e,
            SourceId(1),
            *c,
            &mut WorkBudget::new(0),
            &AtomicBool::new(false)
        )
        .unwrap()
    );
    assert!(v.physical_carriers().is_empty());
    let cost = v.required_work_v1(e).unwrap();
    assert!(
        v.observe_v1(
            e,
            SourceId(1),
            *c,
            &mut WorkBudget::new(cost),
            &AtomicBool::new(false)
        )
        .unwrap()
    );
}
#[test]
fn cancellation_before_event_cannot_allocate_or_consume() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(BAD_CRC, &b);
    let mut v = view(&b);
    let Item::Event(e, c) = &run.items[0] else {
        panic!("carrier")
    };
    let before = b.used();
    assert_eq!(
        v.observe_v1(
            e,
            SourceId(1),
            *c,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(true)
        ),
        Err(Error::Cancelled)
    );
    assert_eq!(b.used(), before);
    assert!(v.physical_carriers().is_empty());
}
#[test]
fn foreign_pool_carrier_cannot_be_a_source_witness() {
    let a = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(BAD_CRC, &a);
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut v = view(&b);
    let Item::Event(e, c) = &run.items[0] else {
        panic!("carrier")
    };
    assert_eq!(
        v.observe_v1(
            e,
            SourceId(1),
            *c,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(false)
        ),
        Err(Error::ForeignPool)
    );
}
#[test]
fn decoded_pid_cannot_replace_actual_wire_pid() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(BAD_CRC, &b);
    let (p, c) = first(&run);
    let mut p = p.clone();
    p.pid = 4096;
    let mut v = view(&b);
    assert!(
        v.observe_v1(
            &TraceEvent::Transport(p),
            SourceId(1),
            c,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(false)
        )
        .is_err()
    );
}
#[test]
fn decoded_payload_cannot_replace_physical_bytes() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(BAD_CRC, &b);
    let (p, c) = first(&run);
    let mut p = p.clone();
    p.payload = Some(Bytes::copy_from_slice(&b, b"different").unwrap());
    let mut v = view(&b);
    assert!(
        v.observe_v1(
            &TraceEvent::Transport(p),
            SourceId(1),
            c,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(false)
        )
        .is_err()
    );
}
#[test]
fn repeated_source_ordinal_is_sticky() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(BAD_CRC, &b);
    let mut v = view(&b);
    feed(&mut v, &run.items[0], &mut run.input).unwrap();
    assert!(feed(&mut v, &run.items[0], &mut run.input).is_err());
    assert!(v.fault().is_some());
}
#[test]
fn foreign_entity_is_not_the_original_source() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(BAD_CRC, &b);
    let mut v = view(&b);
    let Item::Event(e, c) = &run.items[0] else {
        panic!("carrier")
    };
    assert!(
        v.observe_v1(
            e,
            SourceId(2),
            *c,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(false)
        )
        .is_err()
    );
}
#[test]
fn missing_physical_carrier_cannot_be_empty_completion() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(BAD_CRC, &b);
    let mut v = view(&b);
    let (p, c) = first(&run);
    let mut shifted = p.clone();
    shifted.source_offset += 188;
    shifted.payload_source_offset += 188;
    v.observe_v1(
        &TraceEvent::Transport(shifted),
        SourceId(1),
        c,
        &mut WorkBudget::new(1_000_000),
        &AtomicBool::new(false),
    )
    .unwrap();
    for item in &run.items {
        if matches!(item, Item::End(_) | Item::Event(TraceEvent::Finished, _)) {
            feed(&mut v, item, &mut run.input).unwrap();
        }
    }
    assert!(finish(&mut v, &mut run.input).is_err());
    assert!(v.receipt().is_none());
}
#[test]
fn actual_nonempty_source_pes_requires_real_scoped_observer() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(BASIC, &b);
    let mut v = view(&b);
    all(&mut v, &mut run).unwrap();
    assert!(v.empty_refusal().is_some());
    assert!(finish(&mut v, &mut run.input).is_err());
}
#[test]
fn unknown_pes_pid_does_not_select_empty_fallback() {
    let data = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/packet-completion-v1/unknown-pid-pes-after-pat-control.ts"
    ));
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(data, &b);
    let mut v = view(&b);
    all(&mut v, &mut run).unwrap();
    assert!(finish(&mut v, &mut run.input).is_err());
}
#[test]
fn unknown_continuation_without_pusi_is_still_uncertainty() {
    let data = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/packet-completion-v1/unknown-pid-continuation-after-control.ts"
    ));
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(data, &b);
    let mut v = view(&b);
    all(&mut v, &mut run).unwrap();
    assert!(finish(&mut v, &mut run.input).is_err());
}
#[test]
fn later_media_after_inactive_pat_is_not_dropped_as_control() {
    let data = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/packet-completion-v1/inactive-pat-then-unrouted-pes.ts"
    ));
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(data, &b);
    let mut v = view(&b);
    all(&mut v, &mut run).unwrap();
    assert!(finish(&mut v, &mut run.input).is_err());
}
#[test]
fn pes_signature_on_control_pid_is_an_explicit_refusal() {
    let data = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/packet-completion-v1/pes-on-declared-pmt-pid.ts"
    ));
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(data, &b);
    let mut v = view(&b);
    assert!(all(&mut v, &mut run).is_err());
    assert!(v.receipt().is_none());
}
#[test]
fn pmt_continuation_requires_its_actual_start() {
    let data = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/packet-completion-v1/unanchored-pmt-control.ts"
    ));
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(data, &b);
    let mut v = view(&b);
    assert!(all(&mut v, &mut run).is_err());
}
#[test]
fn incomplete_pmt_control_section_is_not_complete_empty() {
    let data = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/packet-completion-v1/pmt-control-truncated-section.ts"
    ));
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(data, &b);
    let mut v = view(&b);
    all(&mut v, &mut run).unwrap();
    assert!(finish(&mut v, &mut run.input).is_err());
}
#[test]
fn physical_truncated_tail_is_not_eof_coverage() {
    let data = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/packet-completion-v1/physical-truncated-tail.ts"
    ));
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(data, &b);
    let mut v = view(&b);
    all(&mut v, &mut run).unwrap();
    assert!(finish(&mut v, &mut run.input).is_err());
}
#[test]
fn rejected_unreadable_transport_is_not_control_only() {
    let data = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/packet-completion-v1/tei-rejected-control.ts"
    ));
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(data, &b);
    let mut v = view(&b);
    let admission = all(&mut v, &mut run);
    assert!(admission.is_err() || finish(&mut v, &mut run.input).is_err());
    assert!(v.receipt().is_none());
}
#[test]
fn later_transport_failure_with_same_eof_token_remains_failure() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(BAD_CRC, &b);
    let mut v = view(&b);
    all(&mut v, &mut run).unwrap();
    let token = run.input.token();
    let position = run.input.position();
    run.input
        .fail(SourceId(1), token.clone(), SourceFailure::Transport)
        .unwrap();
    assert_eq!(run.input.token(), token);
    assert_eq!(run.input.position(), position);
    assert_eq!(finish(&mut v, &mut run.input), Err(Error::Transport));
    assert!(v.receipt().is_none());
}
#[test]
fn cancellation_after_end_does_not_publish_a_receipt() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(BAD_CRC, &b);
    let mut v = view(&b);
    all(&mut v, &mut run).unwrap();
    assert_eq!(
        v.confirm_no_media_v1(
            &mut run.input,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(true)
        ),
        Err(Error::Cancelled)
    );
    assert!(v.receipt().is_none());
    assert!(finish(&mut v, &mut run.input).unwrap().is_some());
}
#[test]
fn actual_other_core_cursor_cannot_supply_end() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(BAD_CRC, &b);
    let mut v = view(&b);
    all(&mut v, &mut run).unwrap();
    let mut other = InputCursor::new(
        &b,
        SourceId(2),
        SourceCapabilities {
            length: Some(188),
            random_access: false,
        },
        4096,
    )
    .unwrap();
    assert!(finish(&mut v, &mut other).is_err());
}
#[test]
fn source_finished_without_actual_physical_end_is_refused() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(BAD_CRC, &b);
    let mut v = view(&b);
    let item = run
        .items
        .iter()
        .find(|i| matches!(i, Item::Event(TraceEvent::Finished, _)))
        .unwrap();
    assert!(feed(&mut v, item, &mut run.input).is_err());
}
#[test]
fn advertised_length_and_drained_bytes_are_not_eof() {
    for known in [false, true] {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut input = InputCursor::new(
            &b,
            SourceId(1),
            SourceCapabilities {
                length: known.then_some(188),
                random_access: false,
            },
            4096,
        )
        .unwrap();
        let mut stream =
            TransportStream::new(b.clone(), PacketFormat::Ts188, ParseLimits::default(), 1)
                .unwrap();
        let mut v = view(&b);
        let mut ordinal = 0;
        let mut supplied = false;
        let mut waiting = false;
        for _ in 0..500_000 {
            match stream
                .poll(
                    &mut input,
                    &mut WorkBudget::new(17),
                    &AtomicBool::new(false),
                )
                .unwrap()
            {
                Step::Ready(event) => {
                    let c = NativeSourceContext {
                        source_event_ordinal: ordinal,
                        epochs: stream.source_epochs(),
                        byte_run_epoch: None,
                    };
                    ordinal += 1;
                    let cost = v.required_work_v1(&event).unwrap();
                    v.observe_v1(
                        &event,
                        SourceId(1),
                        c,
                        &mut WorkBudget::new(cost),
                        &AtomicBool::new(false),
                    )
                    .unwrap();
                }
                Step::Yield => {}
                Step::NeedInput(r) => {
                    if supplied {
                        waiting = true;
                        break;
                    }
                    input
                        .provide(
                            &InputWindow::new(
                                r.source,
                                r.token,
                                r.offset,
                                Bytes::copy_from_slice(&b, BAD_CRC).unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                    supplied = true;
                }
                Step::End(_) => {
                    panic!("advertised length cannot establish actual terminal state")
                }
            }
        }
        assert!(waiting);
        assert!(finish(&mut v, &mut input).is_err());
        assert!(v.receipt().is_none());
    }
}
#[test]
fn source_finished_is_required_after_sealed_physical_end() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(BAD_CRC, &b);
    let mut v = view(&b);
    for item in &run.items {
        if matches!(item, Item::Event(TraceEvent::Finished, _)) {
            break;
        }
        feed(&mut v, item, &mut run.input).unwrap();
    }
    assert!(finish(&mut v, &mut run.input).is_err());
}
#[test]
fn zero_end_and_terminal_work_is_retryable() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(BAD_CRC, &b);
    let mut v = view(&b);
    for item in &run.items {
        if let Item::End(end) = item {
            assert!(
                !v.seal_physical_end_v1(
                    end,
                    &mut run.input,
                    &mut WorkBudget::new(0),
                    &AtomicBool::new(false)
                )
                .unwrap()
            );
            assert!(v.physical_end.is_none());
        }
        feed(&mut v, item, &mut run.input).unwrap();
    }
    assert!(
        v.confirm_no_media_v1(
            &mut run.input,
            &mut WorkBudget::new(0),
            &AtomicBool::new(false)
        )
        .unwrap()
        .is_none()
    );
    assert!(v.receipt().is_none());
    assert!(finish(&mut v, &mut run.input).unwrap().is_some());
}
#[test]
fn duplicate_end_and_receipt_cannot_replay_completion() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(BAD_CRC, &b);
    let mut v = view(&b);
    all(&mut v, &mut run).unwrap();
    let end = run
        .items
        .iter()
        .find_map(|i| if let Item::End(e) = i { Some(e) } else { None })
        .unwrap();
    assert!(
        v.seal_physical_end_v1(
            end,
            &mut run.input,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(false)
        )
        .is_err()
    );
    finish(&mut v, &mut run.input).unwrap().unwrap();
    assert!(finish(&mut v, &mut run.input).is_err());
}
#[test]
fn bounded_retained_history_releases_all_new_owners() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(INACTIVE, &b);
    let before = b.used();
    let mut v = NativePacketCompletionV1::new(
        b.clone(),
        ParseLimits {
            source_spans: 1,
            ..ParseLimits::default()
        },
        1,
    )
    .unwrap();
    assert!(all(&mut v, &mut run).is_err());
    drop(v);
    assert_eq!(b.used(), before);
}
#[test]
fn foreign_epoch_context_cannot_relabel_real_end() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut run = source(BAD_CRC, &b);
    let (p, mut c) = first(&run);
    c.epochs = SourceEpochs {
        clock: 9,
        configuration: 9,
    };
    let mut v = view(&b);
    v.observe_v1(
        &TraceEvent::Transport(p.clone()),
        SourceId(1),
        c,
        &mut WorkBudget::new(1_000_000),
        &AtomicBool::new(false),
    )
    .unwrap();
    let end = run
        .items
        .iter()
        .find_map(|i| if let Item::End(e) = i { Some(e) } else { None })
        .unwrap();
    assert!(
        v.seal_physical_end_v1(
            end,
            &mut run.input,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(false)
        )
        .is_err()
    );
}
#[test]
fn real_transport_failure_before_core_end_is_not_eof() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut input = InputCursor::new(
        &b,
        SourceId(1),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        4096,
    )
    .unwrap();
    let mut stream =
        TransportStream::new(b.clone(), PacketFormat::Ts188, ParseLimits::default(), 1).unwrap();
    let mut v = view(&b);
    let mut ordinal = 0;
    let mut supplied = false;
    let mut failed = false;
    for _ in 0..500_000 {
        match stream
            .poll(
                &mut input,
                &mut WorkBudget::new(17),
                &AtomicBool::new(false),
            )
            .unwrap()
        {
            Step::Ready(event) => {
                let c = NativeSourceContext {
                    source_event_ordinal: ordinal,
                    epochs: stream.source_epochs(),
                    byte_run_epoch: None,
                };
                ordinal += 1;
                let cost = v.required_work_v1(&event).unwrap();
                v.observe_v1(
                    &event,
                    SourceId(1),
                    c,
                    &mut WorkBudget::new(cost),
                    &AtomicBool::new(false),
                )
                .unwrap();
            }
            Step::Yield => {}
            Step::NeedInput(r) => {
                if supplied {
                    input
                        .fail(r.source, r.token, SourceFailure::Transport)
                        .unwrap();
                    failed = true;
                    break;
                }
                input
                    .provide(
                        &InputWindow::new(
                            r.source,
                            r.token,
                            r.offset,
                            Bytes::copy_from_slice(&b, BAD_CRC).unwrap(),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                supplied = true;
            }
            Step::End(_) => panic!("transport failure never supplied an End"),
        }
    }
    assert!(failed);
    assert!(matches!(
        stream.poll(
            &mut input,
            &mut WorkBudget::new(4096),
            &AtomicBool::new(false)
        ),
        Err(Error::Transport)
    ));
    assert!(finish(&mut v, &mut input).is_err());
    assert!(v.receipt().is_none());
}
#[test]
fn rejected_carrier_from_foreign_pool_cannot_escape_ownership() {
    let a = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/transport-afc-reserved.ts"
        )),
        &a,
    );
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut v = view(&b);
    let item = run
        .items
        .iter()
        .find(|i| matches!(i,Item::Event(TraceEvent::Diagnostic(f),_) if f.rejected.is_some()))
        .unwrap();
    let Item::Event(event, context) = item else {
        panic!("actual rejection")
    };
    assert_eq!(
        v.observe_v1(
            event,
            SourceId(1),
            *context,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(false)
        ),
        Err(Error::ForeignPool)
    );
}
