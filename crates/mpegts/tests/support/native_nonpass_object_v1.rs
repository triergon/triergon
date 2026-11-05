//! Whole archived native objects, original physical bytes, shared core input.
use super::*;
fn run(data: &[u8]) -> String {
    try_run(data).expect("complete native object required")
}
fn try_run(data: &[u8]) -> Option<String> {
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
    let mut native = NativeAdapter::new_object_v1(budget.clone(), 1).unwrap();
    let cancel = AtomicBool::new(false);
    let mut ordinal = 0;
    let mut raw = Vec::new();
    for _ in 0..500_000 {
        match source
            .poll(&mut input, &mut WorkBudget::new(31), &cancel)
            .unwrap()
        {
            Step::Ready(event) => {
                let cause = Cause::event(&event, ordinal, input.source(), source.source_epochs());
                ordinal += 1;
                if matches!(event, TraceEvent::Finished) {
                    native.logical_end(&mut raw, cause, &cancel).unwrap();
                    let mut object = Vec::new();
                    return native
                        .object_json(&mut object)
                        .unwrap()
                        .then(|| String::from_utf8(object).unwrap());
                }
                native.observe_with_cancel(&event, cause, source.clock_is_quarantined(), &cancel);
                native.drain(&mut raw, cause, &cancel).unwrap();
            }
            Step::Yield => {}
            Step::NeedInput(request) => {
                let start = usize::try_from(request.offset).unwrap();
                if start == data.len() {
                    input.finish(request.source, request.token).unwrap();
                } else {
                    let end = data.len().min(start + request.maximum.min(7));
                    input
                        .provide(
                            &InputWindow::new(
                                request.source,
                                request.token,
                                request.offset,
                                Bytes::copy_from_slice(&budget, &data[start..end]).unwrap(),
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
                let _ = native.object_end_v1(&end, &input);
                source.end_of_source().unwrap();
            }
        }
    }
    panic!("bounded source trace did not terminate")
}

#[test]
fn transport_afc_reserved() {
    assert_eq!(
        run(include_bytes!("../data/transport-afc-reserved.ts")),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn transport_adaptation_overrun() {
    assert_eq!(
        run(include_bytes!("../data/transport-adaptation-overrun.ts")),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn transport_partial_final_packet() {
    assert_eq!(
        run(include_bytes!("../data/transport-partial-final-packet.ts")),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[{"stream":0,"pid":256,"pts":90000,"dts":90000,"duration":0,"position":376,"flags":0,"payload":"6c616e652d74732d6f726967696e616c2d7061796c6f61640047ff","side_data":[{"type":19,"bytes":"e0"}]}],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn transport_sync_loss() {
    assert_eq!(
        run(include_bytes!("../data/transport-sync-loss.ts")),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[{"stream":0,"pid":256,"pts":90000,"dts":90000,"duration":0,"position":376,"flags":0,"payload":"6c616e652d74732d6f726967696e616c2d7061796c6f61640047ff","side_data":[{"type":19,"bytes":"e0"}]}],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn psi_crc_invalid() {
    assert_eq!(
        run(include_bytes!("../data/psi-crc-invalid.ts")),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[],"programs":[],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn psi_pointer_overrun() {
    assert_eq!(
        run(include_bytes!("../data/psi-pointer-overrun.ts")),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[],"programs":[],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn psi_length_over_policy() {
    assert_eq!(
        run(include_bytes!("../data/psi-length-over-policy.ts")),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[],"programs":[],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn psi_pending_not_current() {
    assert_eq!(
        run(include_bytes!("../data/psi-pending-not-current.ts")),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[],"programs":[],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn pat_atomic_two_sections() {
    assert_eq!(
        run(include_bytes!("../data/pat-atomic-two-sections.ts")),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]},{"id":2,"program":2,"pmt_pid":4097,"pcr_pid":0,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn pat_network_pid_not_track() {
    assert_eq!(
        run(include_bytes!("../data/pat-network-pid-not-track.ts")),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[],"programs":[],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn psi_pointer_finishes_old_starts_new() {
    assert_eq!(
        run(include_bytes!(
            "../data/psi-pointer-finishes-old-starts-new.ts"
        )),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":0,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn psi_two_sections_one_payload() {
    assert_eq!(
        run(include_bytes!("../data/psi-two-sections-one-payload.ts")),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":0,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn pmt_version_wrap_retypes_pid() {
    assert_eq!(
        run(include_bytes!("../data/pmt-version-wrap-retypes-pid.ts")),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[{"stream":0,"pid":256,"codec":"hevc","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn pmt_opaque_language_descriptors() {
    assert_eq!(
        run(include_bytes!("../data/pmt-opaque-language-descriptors.ts")),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[{"key_hex":"6c616e6775616765","value_hex":"656e672c667261"}]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn psi_stuffing_tail_invalid() {
    assert_eq!(
        run(include_bytes!("../data/psi-stuffing-tail-invalid.ts")),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":0,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn psi_truncated_eof() {
    assert_eq!(
        run(include_bytes!("../data/psi-truncated-eof.ts")),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[],"programs":[],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn pes_optional_header_overrun() {
    assert_eq!(
        run(include_bytes!("../data/pes-optional-header-overrun.ts")),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn pes_start_prefix_invalid() {
    assert_eq!(
        run(include_bytes!("../data/pes-start-prefix-invalid.ts")),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn neighbor_pes_start_prefix_invalid_then_valid() {
    assert_eq!(
        run(include_bytes!(
            "../data/native-nonpass-v1/pes-start-prefix-invalid-then-valid.ts"
        )),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[{"stream":0,"pid":256,"pts":90000,"dts":90000,"duration":0,"position":564,"flags":0,"payload":"6c616e652d74732d6f726967696e616c2d7061796c6f61640047fe","side_data":[{"type":19,"bytes":"e0"}]}],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn neighbor_valid_pes_start_prefix_invalid_then_valid() {
    assert_eq!(
        run(include_bytes!(
            "../data/native-nonpass-v1/valid-pes-start-prefix-invalid-then-valid.ts"
        )),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[{"stream":0,"pid":256,"pts":90000,"dts":90000,"duration":0,"position":376,"flags":0,"payload":"6c616e652d74732d6f726967696e616c2d7061796c6f61640047ff","side_data":[{"type":19,"bytes":"e0"}]},{"stream":0,"pid":256,"pts":90000,"dts":90000,"duration":0,"position":752,"flags":0,"payload":"6c616e652d74732d6f726967696e616c2d7061796c6f61640047fe","side_data":[{"type":19,"bytes":"e0"}]}],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn neighbor_pes_optional_header_overrun_then_valid() {
    assert_eq!(
        run(include_bytes!(
            "../data/native-nonpass-v1/pes-optional-header-overrun-then-valid.ts"
        )),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[{"stream":0,"pid":256,"pts":90000,"dts":90000,"duration":0,"position":564,"flags":0,"payload":"6c616e652d74732d6f726967696e616c2d7061796c6f61640047fe","side_data":[{"type":19,"bytes":"e0"}]}],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn neighbor_valid_pes_optional_header_overrun_then_valid() {
    assert_eq!(
        run(include_bytes!(
            "../data/native-nonpass-v1/valid-pes-optional-header-overrun-then-valid.ts"
        )),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[{"stream":0,"pid":256,"pts":90000,"dts":90000,"duration":0,"position":376,"flags":0,"payload":"6c616e652d74732d6f726967696e616c2d7061796c6f61640047ff","side_data":[{"type":19,"bytes":"e0"}]},{"stream":0,"pid":256,"pts":90000,"dts":90000,"duration":0,"position":752,"flags":0,"payload":"6c616e652d74732d6f726967696e616c2d7061796c6f61640047fe","side_data":[{"type":19,"bytes":"e0"}]}],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn neighbor_valid_transport_afc_reserved_then_valid() {
    assert_eq!(
        run(include_bytes!(
            "../data/native-nonpass-v1/valid-transport-afc-reserved-then-valid.ts"
        )),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[{"stream":0,"pid":256,"pts":90000,"dts":90000,"duration":0,"position":376,"flags":0,"payload":"6c616e652d74732d6f726967696e616c2d7061796c6f61640047ff","side_data":[{"type":19,"bytes":"e0"}]},{"stream":0,"pid":256,"pts":90000,"dts":90000,"duration":0,"position":752,"flags":0,"payload":"6c616e652d74732d6f726967696e616c2d7061796c6f61640047fe","side_data":[{"type":19,"bytes":"e0"}]}],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn neighbor_valid_transport_adaptation_overrun_then_valid() {
    assert_eq!(
        run(include_bytes!(
            "../data/native-nonpass-v1/valid-transport-adaptation-overrun-then-valid.ts"
        )),
        r###"{"schema":1,"operation":"mpegts-native-pes-observation-v1","program":1,"packets":[{"stream":0,"pid":256,"pts":90000,"dts":90000,"duration":0,"position":376,"flags":0,"payload":"6c616e652d74732d6f726967696e616c2d7061796c6f61640047ff","side_data":[{"type":19,"bytes":"e0"}]},{"stream":0,"pid":256,"pts":90000,"dts":90000,"duration":0,"position":752,"flags":0,"payload":"6c616e652d74732d6f726967696e616c2d7061796c6f61640047fe","side_data":[{"type":19,"bytes":"e0"}]}],"streams":[{"stream":0,"pid":256,"codec":"h264","type":0,"time_base":[1,90000],"extradata":"","tags":[]}],"programs":[{"id":1,"program":1,"pmt_pid":4096,"pcr_pid":256,"tags":[]}],"read_result":-541478725,"confirmed_eof":true}
"###
    );
}

#[test]
fn guard_valid_transport_sync_loss_then_valid() {
    assert!(
        try_run(include_bytes!(
            "../data/native-nonpass-v1/valid-transport-sync-loss-then-valid.ts"
        ))
        .is_none(),
        "post-sync native resynchronization is an explicit unobserved context"
    );
}

#[test]
fn guard_pending_review_pat_incomplete_version() {
    assert!(
        try_run(include_bytes!("../data/pat-incomplete-version.ts")).is_none(),
        "contradictory PSI policy remains inactive pending review"
    );
}

#[test]
fn guard_pending_review_pmt_descriptor_overrun() {
    assert!(
        try_run(include_bytes!("../data/pmt-descriptor-overrun.ts")).is_none(),
        "contradictory PSI policy remains inactive pending review"
    );
}

#[test]
fn guard_pending_review_pmt_multi_section_forbidden() {
    assert!(
        try_run(include_bytes!("../data/pmt-multi-section-forbidden.ts")).is_none(),
        "contradictory PSI policy remains inactive pending review"
    );
}

#[test]
fn guard_early_pmt_cannot_be_reported_as_an_empty_catalogue() {
    let original = include_bytes!("../data/pat-atomic-two-sections.ts");
    assert_eq!(original.len(), 3 * 188);
    // Authored single-TS mutation: PMT physically precedes the final PAT section.
    // It is a separate source-only refusal guard, with no native parity claim.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&original[..188]);
    bytes.extend_from_slice(&original[376..]);
    bytes.extend_from_slice(&original[188..376]);
    assert!(try_run(&bytes).is_none());
}

#[test]
fn guard_other_program_pmt_is_not_an_absent_native_programme() {
    let original = include_bytes!("../data/pat-atomic-two-sections.ts");
    let mut input = original.to_vec();
    let mut other = original[376..564].to_vec();
    other[1] = (other[1] & 0xe0) | 0x10;
    other[2] = 1;
    input.extend_from_slice(&other);
    assert!(try_run(&input).is_none());
}
#[test]
fn guard_broadcast_si_is_not_silently_an_empty_tag_list() {
    let original = include_bytes!("../data/ts188-basic.ts");
    let mut input = original.to_vec();
    let mut other = original[188..376].to_vec();
    other[1] &= 0xe0;
    other[2] = 0x11;
    input.extend_from_slice(&other);
    assert!(try_run(&input).is_none());
}
