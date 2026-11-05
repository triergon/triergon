//! Original PSI memberships: literal complete native objects, unchanged Strict trace.
use super::*;
fn compatible_adapter(budget: Budget) -> NativeAdapter {
    NativeAdapter::new_psi_compatibility_v1(
        budget,
        1,
        triergon_mpegts::native_catalogue::NativePsiCompatibilityPoliciesV1::ALL_OBSERVED,
    )
    .unwrap()
}
struct Observed {
    object: Option<String>,
    source: Vec<u8>,
    native: String,
}
fn observe(data: &[u8], compatibility: bool, chunk: usize) -> Observed {
    let budget = Budget::new(8 * 1024 * 1024).unwrap();
    let mut input = InputCursor::new(
        &budget,
        SourceId(1),
        SourceCapabilities {
            length: Some(data.len() as u64),
            random_access: false,
        },
        4096,
    )
    .unwrap();
    let mut source = TransportStream::new(
        budget.clone(),
        PacketFormat::Ts188,
        ParseLimits::default(),
        1,
    )
    .unwrap();
    let mut native = if compatibility {
        compatible_adapter(budget.clone())
    } else {
        NativeAdapter::new_object_v1(budget.clone(), 1).unwrap()
    };
    let cancel = AtomicBool::new(false);
    let mut ordinal = 0;
    let mut raw = Vec::new();
    let mut strict = Vec::new();
    for _ in 0..500_000 {
        match source
            .poll(&mut input, &mut WorkBudget::new(31), &cancel)
            .unwrap()
        {
            Step::Ready(event) => {
                let cause = Cause::event(&event, ordinal, input.source(), source.source_epochs());
                ordinal += 1;
                native.observe_with_cancel(&event, cause, source.clock_is_quarantined(), &cancel);
                let finished = source_trace::event(
                    &mut strict,
                    event,
                    None,
                    source.clock_is_quarantined(),
                    &cancel,
                )
                .unwrap();
                native.drain(&mut raw, cause, &cancel).unwrap();
                if finished {
                    native.logical_end(&mut raw, cause, &cancel).unwrap();
                    if let Some(object) = native.object.as_ref() {
                        object.evidence_json(&mut raw).unwrap();
                    }
                    let mut object = Vec::new();
                    let complete = native.object_json(&mut object).unwrap();
                    return Observed {
                        object: complete.then(|| String::from_utf8(object).unwrap()),
                        source: strict,
                        native: String::from_utf8(raw).unwrap(),
                    };
                }
            }
            Step::Yield => {}
            Step::NeedInput(r) => {
                let at = usize::try_from(r.offset).unwrap();
                if at == data.len() {
                    input.finish(r.source, r.token).unwrap();
                } else {
                    let end = data.len().min(at + r.maximum.min(chunk));
                    input
                        .provide(
                            &InputWindow::new(
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
                let end = source
                    .media_entity_end_v1(&mut input, &mut WorkBudget::new(1), &cancel)
                    .unwrap()
                    .unwrap();
                let _ = native.object_end_with_cancel_v1(&end, &input, &cancel);
                source.end_of_source().unwrap();
            }
        }
    }
    panic!("bounded PSI source did not finish")
}

#[test]
fn original_pat_incomplete_version_literal_object_and_source() {
    let input = include_bytes!("../data/pat-incomplete-version.ts");
    let old = observe(input, false, 7);
    let new = observe(input, true, 7);
    assert!(
        old.object.is_none(),
        "the default profile must retain its original refusal"
    );
    assert_eq!(
        new.source, old.source,
        "Strict/source facts and physical ranges cannot change"
    );
    assert_eq!(
        new.object.as_deref(),
        Some(
            r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
        ),
        "{}",
        new.native
    );
    assert!(new.native.contains("IncompleteCurrentPat"));
    assert!(new.native.contains("CurrentPatSectionBeforeWholeTableV1"));
}

#[test]
fn original_pmt_descriptor_overrun_literal_object_and_source() {
    let input = include_bytes!("../data/pmt-descriptor-overrun.ts");
    let old = observe(input, false, 7);
    let new = observe(input, true, 7);
    assert!(
        old.object.is_none(),
        "the default profile must retain its original refusal"
    );
    assert_eq!(
        new.source, old.source,
        "Strict/source facts and physical ranges cannot change"
    );
    assert_eq!(
        new.object.as_deref(),
        Some(
            r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
        ),
        "{}",
        new.native
    );
    assert!(new.native.contains("RejectedPmt"));
    assert!(
        new.native
            .contains("PmtDescriptorStopsAfterStreamIdentityV1")
    );
}

#[test]
fn original_pmt_multi_section_forbidden_literal_object_and_source() {
    let input = include_bytes!("../data/pmt-multi-section-forbidden.ts");
    let old = observe(input, false, 7);
    let new = observe(input, true, 7);
    assert!(
        old.object.is_none(),
        "the default profile must retain its original refusal"
    );
    assert_eq!(
        new.source, old.source,
        "Strict/source facts and physical ranges cannot change"
    );
    assert_eq!(
        new.object.as_deref(),
        Some(
            r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
        ),
        "{}",
        new.native
    );
    assert!(new.native.contains("RejectedPmt"));
    assert!(new.native.contains("PmtNonzeroSectionV1"));
}
