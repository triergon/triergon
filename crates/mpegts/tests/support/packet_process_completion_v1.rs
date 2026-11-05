//! Literal original packet-view/process regressions. No filename dispatch in production.
use super::*;
fn adapter(b: Budget) -> NativeAdapter {
    NativeAdapter::new_packet_completion_v1(b, 1).unwrap()
}
struct Seen {
    failed: bool,
    finished: bool,
    packets: u64,
    validated_scope: bool,
    no_media_receipt: bool,
    source: Vec<u8>,
    native: Vec<u8>,
    object: Option<String>,
}
fn observe(data: &[u8], active: bool) -> Seen {
    observe_with_late_failure(data, active, false)
}
fn observe_with_late_failure(data: &[u8], active: bool, late_failure: bool) -> Seen {
    let budget = Budget::new(8 * 1024 * 1024).unwrap();
    let mut input = InputCursor::new(
        &budget,
        SourceId(1),
        SourceCapabilities {
            length: None,
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
    let mut native = if active {
        adapter(budget.clone())
    } else {
        NativeAdapter::new_psi_compatibility_v1(
            budget.clone(),
            1,
            triergon_mpegts::native_catalogue::NativePsiCompatibilityPoliciesV1::ALL_OBSERVED,
        )
        .unwrap()
    };
    let cancel = AtomicBool::new(false);
    let mut strict = Vec::new();
    let mut raw = Vec::new();
    let mut ordinal = 0;
    for _ in 0..500_000 {
        match source
            .poll(&mut input, &mut WorkBudget::new(31), &cancel)
            .unwrap()
        {
            Step::Ready(event) => {
                let cause = Cause::event(&event, ordinal, input.source(), source.source_epochs());
                ordinal += 1;
                native.observe_with_cancel(&event, cause, source.clock_is_quarantined(), &cancel);
                let done = source_trace::event(
                    &mut strict,
                    event,
                    None,
                    source.clock_is_quarantined(),
                    &cancel,
                )
                .unwrap();
                native.drain(&mut raw, cause, &cancel).unwrap();
                if done {
                    if late_failure {
                        let request = input.request();
                        input
                            .fail(request.source, request.token, SourceFailure::Transport)
                            .unwrap();
                    }
                    if active {
                        native
                            .packet_logical_end_v1(&mut raw, cause, &cancel, &mut input)
                            .unwrap();
                    } else {
                        native.logical_end(&mut raw, cause, &cancel).unwrap();
                    }
                    let mut object = Vec::new();
                    let present = native.object_json(&mut object).unwrap();
                    return Seen {
                        failed: native.failed,
                        finished: native.finished,
                        packets: native.packets,
                        validated_scope: native.scope.is_some(),
                        no_media_receipt: native
                            .packet_completion
                            .as_ref()
                            .is_some_and(|c| c.receipt().is_some()),
                        source: strict,
                        native: raw,
                        object: present.then(|| String::from_utf8(object).unwrap()),
                    };
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
                if active {
                    let _ = native.packet_physical_end_v1(&end, &mut input, &cancel);
                } else {
                    let _ = native.object_end_with_cancel_v1(&end, &input, &cancel);
                }
                source.end_of_source().unwrap();
            }
        }
    }
    panic!("bounded packet operation did not finish")
}
fn check(data: &[u8], literal: &str) {
    let old = observe(data, false);
    let new = observe(data, true);
    assert!(
        old.failed,
        "original packet-view refusal remains visible in old profile"
    );
    assert_eq!(new.source, old.source, "Strict facts must stay literal");
    assert!(!new.failed, "{}", String::from_utf8_lossy(&new.native));
    assert!(new.finished);
    assert_eq!(new.packets, 0);
    assert!(!new.validated_scope);
    assert!(new.no_media_receipt);
    assert!(!String::from_utf8_lossy(&new.native).contains("\"kind\":\"native_pes_nonpass\""));
    assert_eq!(new.object.as_deref(), Some(literal));
}

#[test]
fn original_psi_crc_invalid_full_packet_process() {
    check(
        include_bytes!("../data/psi-crc-invalid.ts"),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[],"programs":[],"read_result":-541478725,"confirmed_eof":true}
"###,
    );
}

#[test]
fn original_psi_pointer_overrun_full_packet_process() {
    check(
        include_bytes!("../data/psi-pointer-overrun.ts"),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[],"programs":[],"read_result":-541478725,"confirmed_eof":true}
"###,
    );
}

#[test]
fn original_psi_length_over_policy_full_packet_process() {
    check(
        include_bytes!("../data/psi-length-over-policy.ts"),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[],"programs":[],"read_result":-541478725,"confirmed_eof":true}
"###,
    );
}

#[test]
fn original_psi_pending_not_current_full_packet_process() {
    check(
        include_bytes!("../data/psi-pending-not-current.ts"),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[],"programs":[],"read_result":-541478725,"confirmed_eof":true}
"###,
    );
}

#[test]
fn original_pat_incomplete_version_full_packet_process() {
    check(
        include_bytes!("../data/pat-incomplete-version.ts"),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###,
    );
}

#[test]
fn original_pat_network_pid_not_track_full_packet_process() {
    check(
        include_bytes!("../data/pat-network-pid-not-track.ts"),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[],"programs":[],"read_result":-541478725,"confirmed_eof":true}
"###,
    );
}

#[test]
fn original_psi_pointer_finishes_old_starts_new_full_packet_process() {
    check(
        include_bytes!("../data/psi-pointer-finishes-old-starts-new.ts"),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":0,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###,
    );
}

#[test]
fn original_psi_two_sections_one_payload_full_packet_process() {
    check(
        include_bytes!("../data/psi-two-sections-one-payload.ts"),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":0,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###,
    );
}

#[test]
fn original_pmt_descriptor_overrun_full_packet_process() {
    check(
        include_bytes!("../data/pmt-descriptor-overrun.ts"),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###,
    );
}

#[test]
fn original_pmt_multi_section_forbidden_full_packet_process() {
    check(
        include_bytes!("../data/pmt-multi-section-forbidden.ts"),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###,
    );
}

#[test]
fn original_psi_stuffing_tail_invalid_full_packet_process() {
    check(
        include_bytes!("../data/psi-stuffing-tail-invalid.ts"),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":0,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###,
    );
}

#[test]
fn original_psi_truncated_eof_full_packet_process() {
    check(
        include_bytes!("../data/psi-truncated-eof.ts"),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[],"programs":[],"read_result":-541478725,"confirmed_eof":true}
"###,
    );
}

#[test]
fn actual_nonempty_validated_scope_packet_and_full_object_are_preserved() {
    let data = include_bytes!("../data/ts188-basic.ts");
    let old = observe(data, false);
    let new = observe(data, true);
    assert!(!new.failed);
    assert!(new.finished);
    assert!(new.validated_scope);
    assert!(!new.no_media_receipt);
    assert_eq!(new.packets, 1);
    assert_eq!(new.source, old.source);
    assert_eq!(
        new.object.as_deref(),
        Some(
            r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[{"stream":0,"pid":256,"pts":90000,"dts":90000,"duration":0,"position":376,"flags":0,"payload":"6c616e652d74732d6f726967696e616c2d7061796c6f61640047ff","side_data":[{"type":19,"bytes":"e0"}]}],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
        )
    );
}

#[test]
fn later_real_pes_after_bad_control_is_not_dropped() {
    let data = include_bytes!("../data/packet-completion-v1/crc-control-then-valid-pes.ts");
    let new = observe(data, true);
    assert!(!new.failed, "{}", String::from_utf8_lossy(&new.native));
    assert!(new.finished);
    assert!(new.validated_scope);
    assert!(!new.no_media_receipt);
    assert_eq!(new.packets, 1);
    assert!(
        new.object
            .as_deref()
            .unwrap()
            .contains("6c616e652d74732d6f726967696e616c2d7061796c6f61640047ff")
    );
}
#[test]
fn unvalidated_pes_after_control_remains_refused_not_empty() {
    for data in [
        include_bytes!("../data/packet-completion-v1/inactive-pat-then-unrouted-pes.ts").as_slice(),
        include_bytes!("../data/packet-completion-v1/unknown-pid-pes-after-pat-control.ts")
            .as_slice(),
        include_bytes!("../data/packet-completion-v1/unknown-pid-continuation-after-control.ts")
            .as_slice(),
    ] {
        let new = observe(data, true);
        assert!(new.failed);
        assert!(!new.no_media_receipt);
        assert!(new.object.is_none());
    }
}
#[test]
fn failed_core_after_real_end_cannot_emit_confirmed_eof_object() {
    let new = observe_with_late_failure(include_bytes!("../data/psi-crc-invalid.ts"), true, true);
    assert!(new.failed);
    assert!(!new.finished);
    assert!(!new.no_media_receipt);
    assert!(new.object.is_none());
}
#[test]
fn incomplete_or_rejected_physical_control_is_not_complete_empty() {
    for data in [
        include_bytes!("../data/packet-completion-v1/physical-truncated-tail.ts").as_slice(),
        include_bytes!("../data/packet-completion-v1/tei-rejected-control.ts").as_slice(),
        include_bytes!("../data/packet-completion-v1/pmt-control-truncated-section.ts").as_slice(),
        include_bytes!("../data/packet-completion-v1/pes-on-declared-pmt-pid.ts").as_slice(),
        include_bytes!("../data/packet-completion-v1/unanchored-pmt-control.ts").as_slice(),
    ] {
        let new = observe(data, true);
        assert!(new.failed);
        assert!(!new.finished);
        assert!(!new.no_media_receipt);
        assert!(new.object.is_none());
    }
}
