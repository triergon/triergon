use super::*;
use crate::SourceEpochs;
use crate::stream::TransportStream;
use triergon_core::{InputWindow, SourceCapabilities, Step};
const BAD_DESC: &[u8] = include_bytes!("../../../../tests/data/pmt-descriptor-overrun.ts");
const BAD_NUMBER: &[u8] = include_bytes!("../../../../tests/data/pmt-multi-section-forbidden.ts");
const PARTIAL_PAT: &[u8] = include_bytes!("../../../../tests/data/pat-incomplete-version.ts");
struct Item {
    event: TraceEvent,
    context: SourceContext,
}
struct SourceRun {
    items: Vec<Item>,
    end: MediaEntityEnd,
    input: InputCursor,
}
fn source(data: &[u8], budget: &Budget) -> SourceRun {
    let mut input = InputCursor::new(
        budget,
        SourceId(1),
        SourceCapabilities {
            length: Some(data.len() as u64),
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
    let mut items = Vec::new();
    let mut end = None;
    for _ in 0..500_000 {
        match stream
            .poll(&mut input, &mut WorkBudget::new(17), &cancel)
            .unwrap()
        {
            Step::Ready(event) => {
                let context = SourceContext {
                    source_event_ordinal: items.len() as u64,
                    epochs: stream.source_epochs(),
                    byte_run_epoch: None,
                };
                let done = matches!(event, TraceEvent::Finished);
                items.push(Item { event, context });
                if done {
                    return SourceRun {
                        items,
                        end: end.unwrap(),
                        input,
                    };
                }
            }
            Step::Yield => {}
            Step::NeedInput(r) => {
                let at = usize::try_from(r.offset).unwrap();
                if at == data.len() {
                    input.finish(r.source, r.token).unwrap();
                } else {
                    let limit = data.len().min(at + r.maximum.min(7));
                    input
                        .provide(
                            &InputWindow::new(
                                r.source,
                                r.token,
                                r.offset,
                                Bytes::copy_from_slice(budget, &data[at..limit]).unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                }
            }
            Step::End(_) => {
                end = stream
                    .media_entity_end(&mut input, &mut WorkBudget::new(1), &cancel)
                    .unwrap();
                stream.end_of_source().unwrap();
            }
        }
    }
    panic!("source guard fixture did not terminate")
}
fn view(budget: &Budget, policies: PsiCompatibilityPolicies) -> Catalogue {
    let mut v = Catalogue::new(budget.clone(), ParseLimits::default(), 1).unwrap();
    v.enable_psi_compatibility(policies).unwrap();
    v
}
fn feed(v: &mut Catalogue, item: &Item) -> Result<bool> {
    let cost = v.required_psi_compatibility_work(&item.event)?;
    v.observe_psi_compatibility(
        &item.event,
        SourceId(1),
        item.context,
        &mut WorkBudget::new(cost),
        &AtomicBool::new(false),
    )
}
fn before_rejection(v: &mut Catalogue, run: &SourceRun) -> usize {
    let index = run
        .items
        .iter()
        .position(|i| matches!(&i.event, TraceEvent::Diagnostic(f) if f.kind == FaultKind::Psi))
        .unwrap();
    for item in &run.items[..index] {
        assert!(feed(v, item).unwrap());
    }
    index
}
#[test]
fn no_policy_cannot_activate_the_operation() {
    let b = Budget::new(1024 * 1024).unwrap();
    let mut v = Catalogue::new(b, ParseLimits::default(), 1).unwrap();
    assert!(
        v.enable_psi_compatibility(PsiCompatibilityPolicies::NONE)
            .is_err()
    );
}
#[test]
fn policies_are_explicit_and_independent() {
    let only = PsiCompatibilityPolicies::one(PsiCompatibility::PmtNonzeroSection);
    assert!(only.contains(PsiCompatibility::PmtNonzeroSection));
    assert!(!only.contains(PsiCompatibility::PmtDescriptorStopsAfterStreamIdentity));
    assert!(!only.contains(PsiCompatibility::CurrentPatSectionBeforeWholeTable));
}
#[test]
fn default_observe_still_refuses_the_rejected_pmt() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(BAD_DESC, &b);
    let mut v = Catalogue::new(b, ParseLimits::default(), 1).unwrap();
    let mut error = None;
    for item in &run.items {
        if let Err(e) = v.observe(
            &item.event,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(false),
        ) {
            error = Some(e);
            break;
        }
    }
    assert_eq!(
        error,
        Some(Error::Unsupported(
            "rejected PMT requires explicit compatibility review"
        ))
    );
}
#[test]
fn enabling_does_not_change_the_default_entry_point() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(BAD_DESC, &b);
    let mut v = view(&b, PsiCompatibilityPolicies::ALL);
    let mut error = None;
    for item in &run.items {
        if let Err(e) = v.observe(
            &item.event,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(false),
        ) {
            error = Some(e);
            break;
        }
    }
    assert!(error.is_some());
    assert!(v.psi_compatibility_evidence().is_empty());
}
#[test]
fn zero_work_does_not_consume_a_source_event() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(PARTIAL_PAT, &b);
    let mut v = view(&b, PsiCompatibilityPolicies::ALL);
    let item = &run.items[0];
    assert!(
        !v.observe_psi_compatibility(
            &item.event,
            SourceId(1),
            item.context,
            &mut WorkBudget::new(0),
            &AtomicBool::new(false)
        )
        .unwrap()
    );
    assert!(v.compatibility.as_ref().unwrap().history.is_empty());
    assert!(feed(&mut v, item).unwrap());
}
#[test]
fn cancelled_event_does_not_consume_source_or_allocate_evidence() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(PARTIAL_PAT, &b);
    let mut v = view(&b, PsiCompatibilityPolicies::ALL);
    let item = &run.items[0];
    assert_eq!(
        v.observe_psi_compatibility(
            &item.event,
            SourceId(1),
            item.context,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(true)
        ),
        Err(Error::Cancelled)
    );
    assert!(v.compatibility.as_ref().unwrap().history.is_empty());
    assert!(v.psi_compatibility_evidence().is_empty());
    assert!(feed(&mut v, item).unwrap());
}
#[test]
fn foreign_pool_actual_carrier_is_refused() {
    let a = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(PARTIAL_PAT, &a);
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut v = view(&b, PsiCompatibilityPolicies::ALL);
    assert_eq!(feed(&mut v, &run.items[0]), Err(Error::ForeignPool));
    assert!(v.fault().is_some());
}
#[test]
fn repeated_source_ordinal_is_sticky() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(PARTIAL_PAT, &b);
    let mut v = view(&b, PsiCompatibilityPolicies::ALL);
    assert!(feed(&mut v, &run.items[0]).unwrap());
    assert!(feed(&mut v, &run.items[0]).is_err());
    assert!(v.fault().is_some());
}
#[test]
fn changed_entity_cannot_reuse_a_source_context() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(PARTIAL_PAT, &b);
    let mut v = view(&b, PsiCompatibilityPolicies::ALL);
    let item = &run.items[0];
    assert!(
        v.observe_psi_compatibility(
            &item.event,
            SourceId(2),
            item.context,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(false)
        )
        .is_err()
    );
}
#[test]
fn actual_raw_carrier_cannot_have_its_decoded_pid_replaced() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(PARTIAL_PAT, &b);
    let mut v = view(&b, PsiCompatibilityPolicies::ALL);
    let TraceEvent::Transport(original) = &run.items[0].event else {
        panic!("carrier fixture")
    };
    let mut changed = original.clone();
    changed.pid = 4096;
    let event = TraceEvent::Transport(changed);
    assert!(
        v.observe_psi_compatibility(
            &event,
            SourceId(1),
            run.items[0].context,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(false)
        )
        .is_err()
    );
}
#[test]
fn a_different_real_source_error_is_not_the_enabled_policy() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(BAD_DESC, &b);
    let mut v = view(&b, PsiCompatibilityPolicies::ALL);
    let index = before_rejection(&mut v, &run);
    let TraceEvent::Diagnostic(f) = &run.items[index].event else {
        panic!("diagnostic")
    };
    let mut different = f.clone();
    different.error = Error::Invalid("descriptor header");
    let event = TraceEvent::Diagnostic(different);
    assert!(
        v.observe_psi_compatibility(
            &event,
            SourceId(1),
            run.items[index].context,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(false)
        )
        .is_err()
    );
    assert!(v.psi_compatibility_evidence().is_empty());
}
#[test]
fn a_rejection_from_another_carrier_is_not_accepted() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(BAD_DESC, &b);
    let mut v = view(&b, PsiCompatibilityPolicies::ALL);
    let index = before_rejection(&mut v, &run);
    let TraceEvent::Diagnostic(f) = &run.items[index].event else {
        panic!("diagnostic")
    };
    let mut changed = f.clone();
    changed.location.offset = 0;
    let event = TraceEvent::Diagnostic(changed);
    assert!(
        v.observe_psi_compatibility(
            &event,
            SourceId(1),
            run.items[index].context,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(false)
        )
        .is_err()
    );
}
#[test]
fn epoch_change_does_not_relabel_the_rejected_source_section() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(BAD_DESC, &b);
    let mut v = view(&b, PsiCompatibilityPolicies::ALL);
    let index = before_rejection(&mut v, &run);
    let mut changed = run.items[index].context;
    changed.epochs = SourceEpochs {
        clock: 9,
        configuration: changed.epochs.configuration,
    };
    assert!(
        v.observe_psi_compatibility(
            &run.items[index].event,
            SourceId(1),
            changed,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(false)
        )
        .is_err()
    );
}
#[test]
fn future_diagnostic_ordinal_is_not_an_actual_consecutive_rejection() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(BAD_DESC, &b);
    let mut v = view(&b, PsiCompatibilityPolicies::ALL);
    let index = before_rejection(&mut v, &run);
    let mut changed = run.items[index].context;
    changed.source_event_ordinal += 10;
    assert!(
        v.observe_psi_compatibility(
            &run.items[index].event,
            SourceId(1),
            changed,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(false)
        )
        .is_err()
    );
}
#[test]
fn unsupported_source_policy_remains_a_refusal() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(BAD_DESC, &b);
    let mut v = view(
        &b,
        PsiCompatibilityPolicies::one(PsiCompatibility::PmtNonzeroSection),
    );
    let index = before_rejection(&mut v, &run);
    assert!(feed(&mut v, &run.items[index]).is_err());
    assert!(v.psi_compatibility_evidence().is_empty());
}
#[test]
fn a_source_section_without_its_real_transports_cannot_be_admitted() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(PARTIAL_PAT, &b);
    let mut v = view(&b, PsiCompatibilityPolicies::ALL);
    let item = run
        .items
        .iter()
        .find(|i| matches!(i.event, TraceEvent::Section(_)))
        .unwrap();
    assert!(feed(&mut v, item).is_err());
    assert!(v.programs().is_empty());
}
#[test]
fn partial_pat_retains_source_incompleteness_and_real_demux_metadata() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(PARTIAL_PAT, &b);
    let mut v = view(&b, PsiCompatibilityPolicies::ALL);
    for item in &run.items {
        if !matches!(item.event, TraceEvent::Finished) {
            assert!(feed(&mut v, item).unwrap());
        }
    }
    assert!(v.has_pending_current_pat());
    assert_eq!(v.programs()[0].pcr_pid, 256);
    assert_eq!(v.streams()[0].pid, 256);
    assert!(v.psi_compatibility_evidence().iter().any(|e| matches!(
        e.source_status(),
        PsiCompatibilitySourceStatus::IncompleteCurrentPat {
            last_section_number: 1,
            ..
        }
    )));
    let cost = v.required_psi_compatibility_finish_work().unwrap();
    assert!(
        v.finish_psi_compatibility(
            &run.end,
            &run.input,
            &mut WorkBudget::new(cost),
            &AtomicBool::new(false)
        )
        .unwrap()
    );
    assert!(
        v.has_pending_current_pat(),
        "native EOF cannot turn a partial source table into a complete source table"
    );
}
#[test]
fn original_nonzero_section_numbers_are_not_rewritten() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(BAD_NUMBER, &b);
    let mut v = view(&b, PsiCompatibilityPolicies::ALL);
    for item in &run.items {
        if !matches!(item.event, TraceEvent::Finished) {
            assert!(feed(&mut v, item).unwrap());
        }
    }
    let e = &v.psi_compatibility_evidence()[0];
    assert_eq!(
        (
            e.wire_header().section_number,
            e.wire_header().last_section_number
        ),
        (1, 1)
    );
    assert!(matches!(
        e.source_status(),
        PsiCompatibilitySourceStatus::RejectedPmt {
            error: Error::Invalid("PMT single-section program identity"),
            ..
        }
    ));
}
#[test]
fn original_descriptor_tail_and_actual_error_location_are_retained() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(BAD_DESC, &b);
    let mut v = view(&b, PsiCompatibilityPolicies::ALL);
    for item in &run.items {
        if !matches!(item.event, TraceEvent::Finished) {
            assert!(feed(&mut v, item).unwrap());
        }
    }
    let e = &v.psi_compatibility_evidence()[0];
    assert_eq!(e.descriptor_stop_offset(), Some(17));
    assert_eq!(
        &e.section_bytes().as_ref()[17..23],
        &[0x0a, 8, b'e', b'n', b'g', 0]
    );
    assert!(matches!(
        e.source_status(),
        PsiCompatibilitySourceStatus::RejectedPmt {
            location: triergon_core::SourceLocation { offset: 188, .. },
            ..
        }
    ));
}
#[test]
fn end_with_unconfirmed_actual_source_diagnostic_is_not_eof() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(BAD_DESC, &b);
    let mut v = view(&b, PsiCompatibilityPolicies::ALL);
    let _ = before_rejection(&mut v, &run);
    let cost = v.required_psi_compatibility_finish_work().unwrap();
    assert!(
        v.finish_psi_compatibility(
            &run.end,
            &run.input,
            &mut WorkBudget::new(cost),
            &AtomicBool::new(false)
        )
        .is_err()
    );
    assert!(!v.is_ended());
}
#[test]
fn cancelled_end_cannot_publish_a_successful_object() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(BAD_DESC, &b);
    let mut v = view(&b, PsiCompatibilityPolicies::ALL);
    for item in &run.items {
        if !matches!(item.event, TraceEvent::Finished) {
            assert!(feed(&mut v, item).unwrap());
        }
    }
    assert_eq!(
        v.finish_psi_compatibility(
            &run.end,
            &run.input,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(true)
        ),
        Err(Error::Cancelled)
    );
    assert!(!v.is_ended());
}
#[test]
fn wrong_core_cursor_cannot_supply_the_demux_terminal_witness() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(BAD_DESC, &b);
    let mut v = view(&b, PsiCompatibilityPolicies::ALL);
    for item in &run.items {
        if !matches!(item.event, TraceEvent::Finished) {
            assert!(feed(&mut v, item).unwrap());
        }
    }
    let wrong = InputCursor::new(
        &b,
        SourceId(2),
        SourceCapabilities {
            length: Some(376),
            random_access: false,
        },
        4096,
    )
    .unwrap();
    assert!(
        v.finish_psi_compatibility(
            &run.end,
            &wrong,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(false)
        )
        .is_err()
    );
    assert!(!v.is_ended());
}
#[test]
fn exhausted_retained_history_is_explicit_and_releases_its_backings() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(BAD_DESC, &b);
    let before = b.used();
    let limits = ParseLimits {
        source_spans: 1,
        ..ParseLimits::default()
    };
    let mut v = Catalogue::new(b.clone(), limits, 1).unwrap();
    v.enable_psi_compatibility(PsiCompatibilityPolicies::ALL)
        .unwrap();
    let mut rejected = false;
    for item in &run.items {
        if feed(&mut v, item).is_err() {
            rejected = true;
            break;
        }
    }
    assert!(rejected);
    assert!(!v.is_ended());
    drop(v);
    assert_eq!(b.used(), before);
}
#[test]
fn invalid_crc_never_activates_a_compatibility_fact() {
    let mut bytes = BAD_DESC.to_vec();
    *bytes.last_mut().unwrap() ^= 1;
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(&bytes, &b);
    let mut v = view(&b, PsiCompatibilityPolicies::ALL);
    for item in &run.items {
        if !matches!(item.event, TraceEvent::Finished) {
            assert!(feed(&mut v, item).unwrap());
        }
    }
    assert!(v.psi_compatibility_evidence().is_empty());
    assert!(v.streams().is_empty());
    assert!(run.items.iter().any(|item| matches!(&item.event, TraceEvent::Diagnostic(f) if f.error == Error::Invalid("PSI table identity/CRC"))));
}
#[test]
fn zero_terminal_work_keeps_the_witness_retryable() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(PARTIAL_PAT, &b);
    let mut v = view(&b, PsiCompatibilityPolicies::ALL);
    for item in &run.items {
        if !matches!(item.event, TraceEvent::Finished) {
            assert!(feed(&mut v, item).unwrap());
        }
    }
    assert!(
        !v.finish_psi_compatibility(
            &run.end,
            &run.input,
            &mut WorkBudget::new(0),
            &AtomicBool::new(false)
        )
        .unwrap()
    );
    assert!(!v.is_ended());
    let cost = v.required_psi_compatibility_finish_work().unwrap();
    assert!(
        v.finish_psi_compatibility(
            &run.end,
            &run.input,
            &mut WorkBudget::new(cost),
            &AtomicBool::new(false)
        )
        .unwrap()
    );
}
#[test]
fn compatibility_cannot_be_enabled_after_a_source_fact() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let run = source(PARTIAL_PAT, &b);
    let mut v = Catalogue::new(b, ParseLimits::default(), 1).unwrap();
    let section = run
        .items
        .iter()
        .find(|item| matches!(item.event, TraceEvent::Section(_)))
        .unwrap();
    assert!(
        v.observe(
            &section.event,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(false)
        )
        .unwrap()
    );
    assert!(
        v.enable_psi_compatibility(PsiCompatibilityPolicies::ALL)
            .is_err()
    );
}
