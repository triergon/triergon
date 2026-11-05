//! Actual composed adapter guards, compiled only as example unit tests.
use super::*;
#[path = "selected_lifecycle_fixture.rs"]
mod fixtures;
use fixtures::Case;

fn drive(case: Case) -> (NativeAdapter, String) {
    let b = Budget::new(1 << 20).unwrap();
    let bytes = fixtures::bytes(case);
    let owner = Bytes::copy_from_slice(&b, &bytes).unwrap();
    let format = if fixtures::width(case) == 192 {
        PacketFormat::M2ts192
    } else {
        PacketFormat::Ts188
    };
    let mut source = TransportStream::new(
        b.clone(),
        format,
        ParseLimits::default(),
        fixtures::selected(case),
    )
    .unwrap();
    let mut cursor = InputCursor::new(
        &b,
        SourceId(1),
        SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: false,
        },
        4096,
    )
    .unwrap();
    let mut native = NativeAdapter::new(b, fixtures::selected(case)).unwrap();
    let mut out = Vec::new();
    let mut n = 0;
    let cancel = AtomicBool::new(false);
    for _ in 0..100_000 {
        match source
            .poll(&mut cursor, &mut WorkBudget::new(1), &cancel)
            .unwrap()
        {
            Step::Ready(e) => {
                let cause = Cause::event(&e, n, SourceId(1), source.source_epochs());
                native.observe(&e, cause, source.clock_is_quarantined());
                native.drain(&mut out, cause, &cancel).unwrap();
                n += 1;
                if matches!(e, TraceEvent::Finished) {
                    native
                        .logical_end(
                            &mut out,
                            Cause::boundary(
                                "logical_eof",
                                SourceLocation {
                                    source: SourceId(1),
                                    offset: bytes.len() as u64,
                                },
                                source.source_epochs(),
                            ),
                            &cancel,
                        )
                        .unwrap();
                    return (native, String::from_utf8(out).unwrap());
                }
            }
            Step::Yield => {}
            Step::NeedInput(r) => {
                let start = r.offset as usize;
                if start == bytes.len() {
                    cursor.finish(r.source, r.token).unwrap();
                } else {
                    let end = (start + r.maximum).min(bytes.len());
                    cursor
                        .provide(
                            &InputWindow::new(
                                r.source,
                                r.token,
                                r.offset,
                                owner.slice(start..end).unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                }
            }
            Step::End(_) => source.end_of_source().unwrap(),
        }
    }
    panic!("composed adapter {} did not finish", fixtures::id(case));
}
fn complete(case: Case, count: u64) -> (NativeAdapter, String) {
    let (n, text) = drive(case);
    assert!(
        !n.failed,
        "selected-program adapter runtime RED until implemented: {text}"
    );
    assert_eq!(n.packets, count);
    assert!(n.finished);
    assert!(!text.contains("native_pes_nonpass"));
    (n, text)
}

#[test]
fn lifecycle_adapter_selected_one_among_programs() {
    let (n, s) = complete(Case::MultiOne, 1);
    assert_eq!(n.scope.unwrap().scope.program, 1);
    assert!(s.contains("\"payload_hex\":\"41564330\""));
}
#[test]
fn lifecycle_adapter_selected_two_is_not_first() {
    let (n, s) = complete(Case::MultiTwo, 1);
    assert_eq!(n.scope.unwrap().scope.program, 2);
    assert!(s.contains("\"payload_hex\":\"53454c32\""));
}
#[test]
fn lifecycle_adapter_reversed_sections_wait_for_complete_pat() {
    let (n, s) = complete(Case::MultiSections, 1);
    assert_eq!(n.scope.unwrap().scope.program, 2);
    assert!(s.contains("\"native_position\":564"));
}
#[test]
fn lifecycle_adapter_rebind_real_pes_before_after() {
    let (n, s) = complete(Case::Retype188, 2);
    assert_eq!(n.scope.unwrap().scope.stream_type, 0x24);
    assert_eq!(n.scope.unwrap().scope.epochs.configuration, 3);
    assert!(s.contains("\"kind\":\"native_pes_scope_update\""));
    assert!(s.contains("\"source_event_ordinal\":14"));
    assert!(s.contains("\"payload_hex\":\"41564330\""));
    assert!(s.contains("\"payload_hex\":\"48455631\""));
}
#[test]
fn lifecycle_adapter_rebind_192_uses_real_sync_offsets() {
    let (_, s) = complete(Case::Retype192, 2);
    assert!(s.contains("\"native_position\":388"));
    assert!(s.contains("\"native_position\":772"));
}
#[test]
fn lifecycle_adapter_rebind_keeps_one_native_wrap_interval() {
    let (_, s) = complete(Case::RetypeWrap, 2);
    assert!(s.contains("\"pts90\":-90000"));
    assert!(s.contains("\"pts90\":45000"));
    assert!(s.contains("\"source_dts_raw33\":null"));
}
#[test]
fn lifecycle_adapter_repeat_and_next_map_leave_binding_unchanged() {
    for c in [Case::Repeat, Case::NextMap] {
        let (n, s) = complete(c, 2);
        assert_eq!(n.scope.unwrap().scope.stream_type, 0x1b);
        assert_eq!(n.scope.unwrap().scope.epochs.configuration, 2);
        assert!(!s.contains("native_pes_scope_update"));
    }
}
#[test]
fn lifecycle_adapter_partial_pes_still_refuses() {
    for c in [Case::PendingBody, Case::PendingHeader] {
        let (n, s) = drive(c);
        assert!(n.failed);
        assert_eq!(n.packets, 0);
        assert!(s.contains("native_pes_nonpass"));
    }
}
#[test]
fn lifecycle_adapter_error_and_missing_scope_stay_nonpass() {
    for c in [
        Case::BadMapCrc,
        Case::IncompletePat,
        Case::NextPat,
        Case::NetworkOnly,
    ] {
        let (n, s) = drive(c);
        assert!(n.failed);
        assert!(s.contains("native_pes_nonpass"));
        assert!(!s.contains("native_pes_scope_update"));
    }
}
