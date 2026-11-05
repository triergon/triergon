//! Original sequence bytes, production adapter, real shared input continuations.
use super::*;

const A: &[u8] = include_bytes!("../data/hls-contiguous-part-a.ts");
const B: &[u8] = include_bytes!("../data/hls-contiguous-part-b.ts");
const MAP: &[u8] = include_bytes!("../data/hls-map-init.ts");
const MEDIA: &[u8] = include_bytes!("../data/hls-map-media.ts");
const BASIC: &[u8] = include_bytes!("../data/ts188-basic.ts");
const NEW: &[u8] = include_bytes!("../data/hls-new-timeline.ts");

struct Observed {
    source: Vec<(
        Vec<u8>,
        Option<u64>,
        SourceEpochs,
        Vec<triergon_mpegts::SourceRange>,
    )>,
    native: String,
    packets: u64,
    discontinuities: usize,
}
fn run(entities: &[(&[u8], bool)], grant: u64, chunk: usize) -> Observed {
    run_format(entities, grant, chunk, PacketFormat::Ts188, false)
}
fn run_format(
    entities: &[(&[u8], bool)],
    grant: u64,
    chunk: usize,
    format: PacketFormat,
    bound: bool,
) -> Observed {
    let budget = Budget::new(8 * 1024 * 1024).unwrap();
    let mut input = InputCursor::new(
        &budget,
        SourceId(1),
        SourceCapabilities {
            length: Some(entities[0].0.len() as u64),
            random_access: false,
        },
        4096,
    )
    .unwrap();
    let mut source =
        TransportStream::new(budget.clone(), format, ParseLimits::default(), 1).unwrap();
    let mut native = NativeAdapter::new(budget.clone(), 1).unwrap();
    if bound {
        bind_finite_startup(
            &mut native,
            &mut input,
            &budget,
            entities[0].0,
            format,
            source.source_epochs(),
        );
    }
    let cancel = AtomicBool::new(false);
    let mut current = 0;
    let mut ordinal = 0;
    let mut raw = Vec::new();
    let mut source_packets = Vec::new();
    let mut discontinuities = 0;
    for _ in 0..500_000 {
        match source
            .poll(&mut input, &mut WorkBudget::new(grant), &cancel)
            .unwrap()
        {
            Step::Ready(event) => {
                let cause = Cause::event(&event, ordinal, input.source(), source.source_epochs());
                ordinal += 1;
                if let TraceEvent::Pes(pes) = &event {
                    source_packets.push((
                        pes.payload().flatten().copied().collect(),
                        pes.header.pts_raw33,
                        pes.epochs,
                        pes.source_ranges().collect(),
                    ));
                }
                if matches!(event, TraceEvent::Discontinuity { .. }) {
                    discontinuities += 1;
                }
                assert!(
                    !matches!(event, TraceEvent::Diagnostic(_)),
                    "source fixture must stay valid: {event:?}"
                );
                if matches!(event, TraceEvent::Finished) {
                    native
                        .logical_end_with_input(&mut raw, cause, &cancel, Some(&input))
                        .unwrap();
                    assert!(!native.failed, "{}", String::from_utf8_lossy(&raw));
                    return Observed {
                        source: source_packets,
                        native: String::from_utf8(raw).unwrap(),
                        packets: native.packets,
                        discontinuities,
                    };
                }
                native.observe_with_input(
                    &event,
                    cause,
                    source.clock_is_quarantined(),
                    &cancel,
                    &input,
                );
                native
                    .drain_with_input(&mut raw, cause, &cancel, Some(&input))
                    .unwrap();
                assert!(!native.failed, "{}", String::from_utf8_lossy(&raw));
            }
            Step::Yield => {}
            Step::NeedInput(request) => {
                let data = entities[current].0;
                let start = usize::try_from(request.offset).unwrap();
                if start == data.len() {
                    input.finish(request.source, request.token).unwrap();
                } else {
                    let end = data.len().min(start + request.maximum.min(chunk));
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
            Step::End(location) => {
                current += 1;
                if current == entities.len() {
                    source.end_of_source().unwrap();
                } else {
                    let end = source
                        .media_entity_end_v1(&mut input, &mut WorkBudget::new(1), &cancel)
                        .unwrap()
                        .unwrap();
                    assert_eq!(end.location(), location);
                    let discontinuity = entities[current].1;
                    input
                        .continue_source(
                            SourceId(current as u64 + 1),
                            SourceCapabilities {
                                length: Some(entities[current].0.len() as u64),
                                random_access: false,
                            },
                        )
                        .unwrap();
                    assert!(
                        native
                            .media_entity_boundary_v1(
                                &end,
                                &mut input,
                                discontinuity,
                                &mut WorkBudget::new(1),
                                &cancel
                            )
                            .expect("RED: explicit media-entity transaction")
                    );
                    if discontinuity {
                        source.discontinuity().unwrap();
                        if bound {
                            bind_finite_startup(
                                &mut native,
                                &mut input,
                                &budget,
                                entities[current].0,
                                format,
                                source.source_epochs(),
                            );
                        }
                    }
                }
            }
        }
    }
    panic!("bounded source/native operation did not finish");
}

#[test]
fn original_contiguous_pes_keeps_all512_bytes_and_both_physical_entities() {
    let o = run(&[(A, false), (B, false)], 1, 1);
    assert_eq!(o.packets, 1);
    assert_eq!(o.source.len(), 1);
    assert_eq!(
        o.source[0].0,
        (0_u8..=255).cycle().take(512).collect::<Vec<_>>()
    );
    assert_eq!(o.source[0].1, Some(90000));
    assert_eq!(
        o.source[0].2,
        SourceEpochs {
            clock: 0,
            configuration: 2
        }
    );
    assert!(o.source[0].3.iter().any(|r| r.entity == SourceId(1)));
    assert!(o.source[0].3.iter().any(|r| r.entity == SourceId(2)));
    assert!(o.native.contains("\"source\":2,\"offset\":4"));
    assert!(
        o.native
            .contains("\"source_origin\":{\"source\":1,\"offset\":376}")
    );
    assert!(o.native.contains("\"native_flags\":0"));
    assert_eq!(o.discontinuities, 0);
}

#[test]
fn original_external_map_retains_validated_configuration_without_invented_psi() {
    let o = run(&[(MAP, false), (MEDIA, false)], 7, 5);
    assert_eq!(o.packets, 1);
    assert_eq!(o.source.len(), 1);
    assert_eq!(o.source[0].0, b"lane-ts-original-payload\0\x47\xff");
    assert_eq!(
        o.source[0].2,
        SourceEpochs {
            clock: 0,
            configuration: 2
        }
    );
    assert!(o.source[0].3.iter().all(|r| r.entity == SourceId(2)));
    assert!(
        o.native
            .contains("\"source_origin\":{\"source\":2,\"offset\":0}")
    );
    assert!(o.native.contains("\"native_position\":376"));
    assert_eq!(o.discontinuities, 0);
}

#[test]
fn original_explicit_discontinuity_reestablishes_signalling_and_clock_config() {
    let o = run(&[(BASIC, false), (NEW, true)], 1, 3);
    assert_eq!(o.packets, 2);
    assert_eq!(o.source.len(), 2);
    assert_eq!(o.source[0].1, Some(90000));
    assert_eq!(o.source[1].1, Some(10));
    assert_eq!(
        o.source[1].2,
        SourceEpochs {
            clock: 1,
            configuration: 5
        }
    );
    assert_eq!(o.discontinuities, 1);
    assert!(
        o.native
            .contains("\"source_clock_epoch\":1,\"source_configuration_epoch\":5")
    );
}

struct AtEnd {
    budget: Budget,
    source: TransportStream,
    input: InputCursor,
    native: NativeAdapter,
    end: triergon_mpegts::MediaEntityEndV1,
}
fn at_end(bytes: &[u8]) -> AtEnd {
    let budget = Budget::new(1 << 20).unwrap();
    let mut input = InputCursor::new(
        &budget,
        SourceId(1),
        SourceCapabilities {
            length: Some(bytes.len() as u64),
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
    let mut native = NativeAdapter::new(budget.clone(), 1).unwrap();
    let cancel = AtomicBool::new(false);
    let mut ordinal = 0;
    let mut raw = Vec::new();
    for _ in 0..100_000 {
        match source
            .poll(&mut input, &mut WorkBudget::new(1), &cancel)
            .unwrap()
        {
            Step::Ready(event) => {
                assert!(!matches!(
                    event,
                    TraceEvent::Diagnostic(_) | TraceEvent::Finished
                ));
                let cause = Cause::event(&event, ordinal, input.source(), source.source_epochs());
                native.observe_with_input(
                    &event,
                    cause,
                    source.clock_is_quarantined(),
                    &cancel,
                    &input,
                );
                native
                    .drain_with_input(&mut raw, cause, &cancel, Some(&input))
                    .unwrap();
                assert!(!native.failed, "{}", String::from_utf8_lossy(&raw));
                ordinal += 1;
            }
            Step::Yield => {}
            Step::NeedInput(r) => {
                let start = usize::try_from(r.offset).unwrap();
                if start == bytes.len() {
                    input.finish(r.source, r.token).unwrap();
                } else {
                    let end = bytes.len().min(start + r.maximum.min(7));
                    input
                        .provide(
                            &InputWindow::new(
                                r.source,
                                r.token,
                                r.offset,
                                Bytes::copy_from_slice(&budget, &bytes[start..end]).unwrap(),
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
                return AtEnd {
                    budget,
                    source,
                    input,
                    native,
                    end,
                };
            }
        }
    }
    panic!("finite map boundary did not drain");
}
fn next(state: &mut AtEnd, id: u64, length: Option<u64>) {
    state
        .input
        .continue_source(
            SourceId(id),
            SourceCapabilities {
                length,
                random_access: false,
            },
        )
        .unwrap();
}
fn transition(
    state: &mut AtEnd,
    discontinuity: bool,
    units: u64,
    cancel: &AtomicBool,
) -> triergon_core::Result<bool> {
    state.native.media_entity_boundary_v1(
        &state.end,
        &mut state.input,
        discontinuity,
        &mut WorkBudget::new(units),
        cancel,
    )
}
fn cancelled() -> AtomicBool {
    AtomicBool::new(true)
}
fn live() -> AtomicBool {
    AtomicBool::new(false)
}

#[test]
fn entity_witness_work_zero_does_not_consume_or_seal_input() {
    let mut s = at_end(MAP);
    let token = s.input.token();
    let used = s.budget.used();
    assert!(
        s.source
            .media_entity_end_v1(&mut s.input, &mut WorkBudget::new(0), &live())
            .unwrap()
            .is_none()
    );
    assert_eq!(s.input.token(), token);
    assert_eq!(s.input.position(), MAP.len() as u64);
    assert_eq!(s.budget.used(), used);
}
#[test]
fn entity_witness_cancellation_is_retryable_and_does_not_cancel_core_input() {
    let mut s = at_end(MAP);
    assert!(matches!(
        s.source
            .media_entity_end_v1(&mut s.input, &mut WorkBudget::new(1), &cancelled()),
        Err(Error::Cancelled)
    ));
    assert!(
        s.source
            .media_entity_end_v1(&mut s.input, &mut WorkBudget::new(1), &live())
            .unwrap()
            .is_some()
    );
}
#[test]
fn advertised_length_without_a_real_source_end_cannot_create_a_witness() {
    let b = Budget::new(4096).unwrap();
    let mut i = InputCursor::new(
        &b,
        SourceId(1),
        SourceCapabilities {
            length: Some(0),
            random_access: false,
        },
        4096,
    )
    .unwrap();
    let source = TransportStream::new(b, PacketFormat::Ts188, ParseLimits::default(), 1).unwrap();
    assert!(
        source
            .media_entity_end_v1(&mut i, &mut WorkBudget::new(1), &live())
            .is_err()
    );
}
#[test]
fn a_different_core_cursor_cannot_borrow_an_observed_source_end() {
    let s = at_end(MAP);
    let mut other = InputCursor::new(
        &s.budget,
        SourceId(1),
        SourceCapabilities {
            length: Some(0),
            random_access: false,
        },
        4096,
    )
    .unwrap();
    other.finish(other.source(), other.token()).unwrap();
    assert!(
        s.source
            .media_entity_end_v1(&mut other, &mut WorkBudget::new(1), &live())
            .is_err()
    );
}
#[test]
fn logical_eof_cannot_be_relabelled_as_a_media_boundary() {
    let mut s = at_end(MAP);
    s.source.end_of_source().unwrap();
    assert!(
        s.source
            .media_entity_end_v1(&mut s.input, &mut WorkBudget::new(1), &live())
            .is_err()
    );
}
#[test]
fn transport_failure_after_end_remains_a_transport_failure() {
    let mut s = at_end(MAP);
    s.input
        .fail(s.input.source(), s.input.token(), SourceFailure::Transport)
        .unwrap();
    assert!(matches!(
        s.source
            .media_entity_end_v1(&mut s.input, &mut WorkBudget::new(1), &live()),
        Err(Error::Transport)
    ));
}
#[test]
fn contiguous_transaction_work_zero_preserves_scope_and_charged_owners() {
    let mut s = at_end(A);
    next(&mut s, 2, Some(B.len() as u64));
    let old = s.native.scope.unwrap().scope;
    let used = s.budget.used();
    assert!(!transition(&mut s, false, 0, &live()).unwrap());
    assert_eq!(s.native.scope.unwrap().scope, old);
    assert_eq!(s.budget.used(), used);
    assert!(transition(&mut s, false, 1, &live()).unwrap());
}
#[test]
fn contiguous_transaction_cancel_preserves_partial_pes_for_retry() {
    let mut s = at_end(A);
    next(&mut s, 2, Some(B.len() as u64));
    let old = s.native.scope.unwrap().scope;
    let used = s.budget.used();
    assert_eq!(
        transition(&mut s, false, 1, &cancelled()),
        Err(Error::Cancelled)
    );
    assert_eq!(s.native.scope.unwrap().scope, old);
    assert_eq!(s.budget.used(), used);
    assert!(transition(&mut s, false, 1, &live()).unwrap());
}
#[test]
fn reset_work_zero_does_not_drop_partial_pes_or_signalling() {
    let mut s = at_end(A);
    next(&mut s, 2, Some(NEW.len() as u64));
    let used = s.budget.used();
    assert!(!transition(&mut s, true, 0, &live()).unwrap());
    assert!(s.native.observer.is_some());
    assert!(s.native.scope.is_some());
    assert_eq!(s.budget.used(), used);
}
#[test]
fn reset_cancel_does_not_drop_partial_pes_or_signalling() {
    let mut s = at_end(A);
    next(&mut s, 2, Some(NEW.len() as u64));
    let used = s.budget.used();
    assert_eq!(
        transition(&mut s, true, 1, &cancelled()),
        Err(Error::Cancelled)
    );
    assert!(s.native.observer.is_some());
    assert!(s.native.scope.is_some());
    assert_eq!(s.budget.used(), used);
}
#[test]
fn unrelated_pool_cannot_continue_an_observer() {
    let mut s = at_end(MAP);
    let b = Budget::new(4096).unwrap();
    let mut other = InputCursor::new(
        &b,
        SourceId(2),
        SourceCapabilities {
            length: Some(0),
            random_access: false,
        },
        4096,
    )
    .unwrap();
    assert_eq!(
        s.native.media_entity_boundary_v1(
            &s.end,
            &mut other,
            false,
            &mut WorkBudget::new(1),
            &live()
        ),
        Err(Error::ForeignPool)
    );
    assert_eq!(s.native.scope.unwrap().scope.entity, SourceId(1));
}
#[test]
fn a_generation_reset_is_not_entity_continuation() {
    let mut s = at_end(MAP);
    s.input.reset(0).unwrap();
    assert!(matches!(
        transition(&mut s, false, 1, &live()),
        Err(Error::StaleGeneration { .. })
    ));
}
#[test]
fn skipping_a_request_cannot_reuse_an_old_end_witness() {
    let mut s = at_end(MAP);
    next(&mut s, 2, Some(0));
    s.input.finish(s.input.source(), s.input.token()).unwrap();
    next(&mut s, 3, Some(0));
    assert!(matches!(
        transition(&mut s, false, 1, &live()),
        Err(Error::StaleRequest { .. })
    ));
}
#[test]
fn source_identity_must_distinguish_entities() {
    let mut s = at_end(MAP);
    next(&mut s, 1, Some(188));
    assert_eq!(
        transition(&mut s, false, 1, &live()),
        Err(Error::SourceMismatch)
    );
}
#[test]
fn replayed_boundary_is_rejected_after_the_first_commit() {
    let mut s = at_end(MAP);
    next(&mut s, 2, Some(188));
    assert!(transition(&mut s, false, 1, &live()).unwrap());
    assert!(transition(&mut s, false, 1, &live()).is_err());
    assert_eq!(s.native.scope.unwrap().scope.entity, SourceId(2));
}
#[test]
fn already_consumed_new_entity_cannot_be_rebound_retroactively() {
    let mut s = at_end(MAP);
    next(&mut s, 2, Some(188));
    s.input
        .provide(
            &InputWindow::new(
                s.input.source(),
                s.input.token(),
                0,
                Bytes::copy_from_slice(&s.budget, &[0x47]).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(matches!(
        s.input
            .read_into(&mut [0_u8; 1], &mut WorkBudget::new(1), &live())
            .unwrap(),
        Step::Ready(1)
    ));
    assert!(transition(&mut s, false, 1, &live()).is_err());
}
#[test]
fn a_failed_new_entity_cannot_be_committed_as_contiguous_media() {
    let mut s = at_end(A);
    next(&mut s, 2, Some(B.len() as u64));
    s.input
        .fail(s.input.source(), s.input.token(), SourceFailure::Transport)
        .unwrap();
    assert_eq!(transition(&mut s, false, 1, &live()), Err(Error::Transport));
    assert_eq!(s.native.scope.unwrap().scope.entity, SourceId(1));
}
#[test]
fn a_failed_new_entity_cannot_discard_old_state_as_a_reset() {
    let mut s = at_end(A);
    next(&mut s, 2, Some(NEW.len() as u64));
    let used = s.budget.used();
    s.input
        .fail(s.input.source(), s.input.token(), SourceFailure::Transport)
        .unwrap();
    assert_eq!(transition(&mut s, true, 1, &live()), Err(Error::Transport));
    assert!(s.native.observer.is_some());
    assert_eq!(s.budget.used(), used);
}
#[test]
fn unknown_entity_length_does_not_imply_eof_or_reset_the_source_clock() {
    let mut s = at_end(A);
    next(&mut s, 2, None);
    assert!(transition(&mut s, false, 1, &live()).unwrap());
    assert_eq!(
        s.native.scope.unwrap().scope.epochs,
        SourceEpochs {
            clock: 0,
            configuration: 2
        }
    );
}

// Independent byte construction from H.222.0 tables2-2/2-6/2-21. No media
// implementation, reference output or decoder fixture is translated here.
fn carrier(pid: u16, counter: u8, pusi: bool, payload: &[u8]) -> Vec<u8> {
    assert!(!payload.is_empty() && payload.len() <= 184);
    let mut b = vec![0xff; 188];
    b[0] = 0x47;
    b[1] = ((pid >> 8) as u8 & 31) | if pusi { 64 } else { 0 };
    b[2] = pid as u8;
    if payload.len() == 184 {
        b[3] = 0x10 | counter;
        b[4..].copy_from_slice(payload);
    } else {
        b[3] = 0x30 | counter;
        let length = 183 - payload.len();
        b[4] = length as u8;
        if length != 0 {
            b[5] = 0;
        }
        b[5 + length..].copy_from_slice(payload);
    }
    b
}
fn pes(pts: u64, body: &[u8], unbounded: bool) -> Vec<u8> {
    let n = if unbounded { 0 } else { body.len() + 8 };
    let mut b = vec![
        0,
        0,
        1,
        0xe0,
        (n >> 8) as u8,
        n as u8,
        0x80,
        0x80,
        5,
        0x21 | (((pts >> 30) as u8 & 7) << 1),
        (pts >> 22) as u8,
        (((pts >> 15) as u8 & 127) << 1) | 1,
        (pts >> 7) as u8,
        ((pts as u8 & 127) << 1) | 1,
    ];
    b.extend_from_slice(body);
    b
}
fn mapped_packet(pts: u64, counter: u8, body: &[u8]) -> Vec<u8> {
    let mut b = MAP.to_vec();
    b.extend_from_slice(&carrier(256, counter, true, &pes(pts, body, false)));
    b
}
#[test]
fn a_pes_header_can_cross_an_entity_boundary_without_reparse_or_timestamp_fill() {
    let p = pes(12345, b"original boundary header", false);
    let mut a = MAP.to_vec();
    a.extend_from_slice(&carrier(256, 0, true, &p[..5]));
    let b = carrier(256, 1, false, &p[5..]);
    let o = run(&[(&a, false), (&b, false)], 1, 1);
    assert_eq!(o.packets, 1);
    assert_eq!(o.source[0].0, b"original boundary header");
    assert_eq!(o.source[0].1, Some(12345));
    assert!(o.native.contains("\"header_source_ranges\":[{\"source\":1"));
    assert!(
        o.native
            .contains("\"source_pts_raw33\":12345,\"source_dts_raw33\":null")
    );
}
#[test]
fn unbounded_video_pes_is_flushed_only_at_the_final_logical_eof() {
    let p = pes(88, b"original unbounded media", true);
    let mut a = MAP.to_vec();
    a.extend_from_slice(&carrier(256, 0, true, &p[..18]));
    let b = carrier(256, 1, false, &p[18..]);
    let o = run(&[(&a, false), (&b, false)], 1, 1);
    assert_eq!(o.packets, 1);
    assert_eq!(o.source[0].0, b"original unbounded media");
    assert!(
        o.native
            .contains("\"boundary\":\"LogicalEof\",\"payload_kind\":\"Unbounded\"")
    );
}
#[test]
fn explicit_reset_discards_old_partial_pes_without_emitting_a_fabricated_packet() {
    let o = run(&[(A, false), (NEW, true)], 1, 1);
    assert_eq!(o.packets, 1);
    assert_eq!(o.source.len(), 1);
    assert_eq!(o.source[0].0, b"lane-ts-original-payload\0\x47\xff");
    assert_eq!(o.source[0].1, Some(10));
    assert!(!o.native.contains("\"payload_bytes\":170"));
}
#[test]
fn explicit_reset_discards_partial_psi_and_partial_pes_together() {
    let mut a = A.to_vec();
    let pat_fragment = [0, 0, 0xb0, 13, 0, 1];
    a.extend_from_slice(&carrier(0, 1, true, &pat_fragment));
    let o = run(&[(&a, false), (NEW, true)], 1, 1);
    assert_eq!(o.packets, 1);
    assert_eq!(o.discontinuities, 1);
    assert_eq!(
        o.source[0].2,
        SourceEpochs {
            clock: 1,
            configuration: 5
        }
    );
}
#[test]
fn explicit_reset_allows_fresh_hevc_signalling_without_stale_avc_scope() {
    let hevc = include_bytes!("../data/carriage-hevc-direct.ts");
    let o = run(&[(A, false), (hevc, true)], 1, 7);
    assert_eq!(o.packets, 1);
    assert_eq!(o.source.len(), 1);
    assert!(o.native.contains("\"stream_type\":36"));
    assert_eq!(
        o.source[0].2,
        SourceEpochs {
            clock: 1,
            configuration: 5
        }
    );
}
#[test]
fn contiguous_entities_preserve_the_independent_native_clock_interval() {
    let first = mapped_packet((1_u64 << 32) + 100, 0, b"first clock context");
    let second = carrier(256, 1, true, &pes(10, b"same clock context", false));
    let o = run(&[(&first, false), (&second, false)], 1, 7);
    assert_eq!(o.packets, 2);
    assert_eq!(o.source[1].1, Some(10));
    assert!(
        o.native
            .contains("\"pts90\":8589934602,\"dts90\":8589934602")
    );
    assert_eq!(
        o.native
            .matches("\"first_reference_raw90\":4294967396")
            .count(),
        2
    );
}
#[test]
fn explicit_reset_creates_a_new_native_clock_context_without_changing_raw_source_time() {
    let first = mapped_packet((1_u64 << 32) + 100, 0, b"first clock context");
    let second = mapped_packet(10, 0, b"new clock context");
    let o = run(&[(&first, false), (&second, true)], 1, 7);
    assert_eq!(o.packets, 2);
    assert_eq!(o.source[1].1, Some(10));
    assert!(o.native.contains("\"pts90\":10,\"dts90\":10"));
    assert!(o.native.contains("\"first_reference_raw90\":10"));
}
#[test]
fn empty_contiguous_entity_neither_flushes_nor_duplicates_a_partial_pes() {
    let o = run(&[(A, false), (&[], false), (B, false)], 1, 3);
    assert_eq!(o.packets, 1);
    assert_eq!(o.source[0].0.len(), 512);
    assert!(o.source[0].3.iter().any(|r| r.entity == SourceId(3)));
    assert!(!o.source[0].3.iter().any(|r| r.entity == SourceId(2)));
}
#[test]
fn repartitioning_at_packet_boundaries_preserves_original_payload_and_source_clock() {
    let joined = [A, B].concat();
    let one = run(&[(&joined, false)], 4096, 4096);
    let many = run(
        &[
            (MAP, false),
            (&A[376..], false),
            (&B[..188], false),
            (&B[188..], false),
        ],
        1,
        1,
    );
    assert_eq!(one.packets, many.packets);
    assert_eq!(one.source[0].0, many.source[0].0);
    assert_eq!(one.source[0].1, many.source[0].1);
    assert_eq!(one.source[0].2, many.source[0].2);
}

fn bind_finite_startup(
    native: &mut NativeAdapter,
    input: &mut InputCursor,
    budget: &Budget,
    bytes: &[u8],
    format: PacketFormat,
    epochs: SourceEpochs,
) {
    if format == PacketFormat::M2ts192 {
        let mut reader = std::io::Cursor::new(bytes);
        let mut buffer = [0_u8; 16 * 1024];
        let mut startup = prepare_native_startup_v1(
            &mut reader,
            input,
            budget,
            format,
            true,
            &mut buffer,
            &live(),
        )
        .unwrap();
        apply_native_startup_v1(native, &mut startup, input, epochs);
        assert!(!native.failed && startup.eof && !startup.failure && startup.byte.is_none());
        input.finish(input.source(), input.token()).unwrap();
        return;
    }
    let window = InputWindow::new(
        input.source(),
        input.token(),
        0,
        Bytes::copy_from_slice(budget, bytes).unwrap(),
    )
    .unwrap();
    input.provide(&window).unwrap();
    input.finish(input.source(), input.token()).unwrap();
    let mut job = NativeInputBootstrap::new(input, &window, format).unwrap();
    job.observe_entity_eof(input, window.source, &window.token, bytes.len() as u64)
        .unwrap();
    for _ in 0..100_000 {
        match job.poll(input, &mut WorkBudget::new(1), &live()).unwrap() {
            NativeInputStep::Ready(context) => {
                native.initial_input = Some(context);
                return;
            }
            NativeInputStep::Yield => {}
            NativeInputStep::NeedEntityEof => panic!("test supplied a real complete finite entity"),
        }
    }
    panic!("bounded finite startup did not finish");
}
fn m2ts(bytes: &[u8]) -> Vec<u8> {
    bytes
        .chunks_exact(188)
        .flat_map(|p| [b"\xa5\x5a\x47\x00".as_slice(), p].concat())
        .collect()
}
#[test]
fn continuation_commits_without_allocating_when_the_shared_budget_is_full() {
    let mut s = at_end(A);
    next(&mut s, 2, Some(B.len() as u64));
    let blocker = s.budget.reserve(s.budget.available()).unwrap();
    assert_eq!(s.budget.available(), 0);
    assert!(transition(&mut s, false, 1, &live()).unwrap());
    assert_eq!(s.budget.available(), 0);
    drop(blocker);
}
#[test]
fn preprovided_unconsumed_bytes_survive_the_entity_transaction() {
    let mut s = at_end(MAP);
    next(&mut s, 2, Some(MEDIA.len() as u64));
    let window = InputWindow::new(
        s.input.source(),
        s.input.token(),
        0,
        Bytes::copy_from_slice(&s.budget, MEDIA).unwrap(),
    )
    .unwrap();
    s.input.provide(&window).unwrap();
    assert!(transition(&mut s, false, 1, &live()).unwrap());
    assert_eq!(s.input.position(), 0);
    let mut read = [0_u8; 188];
    assert!(matches!(
        s.input
            .read_into(&mut read, &mut WorkBudget::new(188), &live())
            .unwrap(),
        Step::Ready(188)
    ));
    assert_eq!(&read, MEDIA);
}
#[test]
fn reset_requires_the_actual_source_discontinuity_event() {
    let mut s = at_end(A);
    next(&mut s, 2, Some(NEW.len() as u64));
    assert!(transition(&mut s, true, 1, &live()).unwrap());
    let mut event = None;
    for _ in 0..20000 {
        match s
            .source
            .poll(&mut s.input, &mut WorkBudget::new(1), &live())
            .unwrap()
        {
            Step::Ready(value) => {
                event = Some(value);
                break;
            }
            Step::NeedInput(r) => {
                s.input
                    .provide(
                        &InputWindow::new(
                            r.source,
                            r.token,
                            r.offset,
                            Bytes::copy_from_slice(&s.budget, NEW).unwrap(),
                        )
                        .unwrap(),
                    )
                    .unwrap();
            }
            Step::Yield => {}
            Step::End(_) => panic!("new entity has bytes"),
        }
    }
    let event = event.unwrap();
    let cause = Cause::event(&event, 100, s.input.source(), s.source.source_epochs());
    s.native
        .observe_with_input(&event, cause, false, &live(), &s.input);
    assert!(s.native.failed);
    assert_eq!(
        s.native.failure.as_ref().unwrap().error,
        Error::Invalid("native media reset lacks actual source discontinuity")
    );
}
#[test]
fn an_existing_native_nonpass_cannot_be_hidden_by_a_later_reset() {
    let mut s = at_end(MAP);
    s.native.fail(
        Error::Unsupported("retained earlier native context"),
        Cause::boundary("test_prior_fault", s.end.location(), s.end.epochs()),
    );
    next(&mut s, 2, Some(NEW.len() as u64));
    assert!(transition(&mut s, true, 1, &live()).is_err());
    assert_eq!(
        s.native.failure.as_ref().unwrap().error,
        Error::Unsupported("retained earlier native context")
    );
}
#[test]
fn input_bound_continuation_preserves_the_existing_startup_basis_and_pes() {
    let o = run_format(&[(A, false), (B, false)], 1, 1, PacketFormat::Ts188, true);
    assert_eq!(o.packets, 1);
    assert_eq!(o.source[0].0.len(), 512);
    assert!(
        o.native
            .contains("\"source_origin\":{\"source\":1,\"offset\":376}")
    );
    assert!(o.native.contains("\"native_position\":376"));
}
#[test]
fn input_bound_m2ts_entities_keep_opaque_prefixes_and_physical_source_coordinates() {
    let a = m2ts(A);
    let b = m2ts(B);
    let o = run_format(
        &[(&a, false), (&b, false)],
        1,
        1,
        PacketFormat::M2ts192,
        true,
    );
    assert_eq!(o.packets, 1);
    assert_eq!(o.source[0].0.len(), 512);
    assert!(
        o.native
            .contains("\"source_origin\":{\"source\":1,\"offset\":384}")
    );
    assert!(o.native.contains("\"native_position\":388"));
    assert!(
        o.source[0]
            .3
            .iter()
            .any(|r| r.entity == SourceId(2) && r.offset == 8)
    );
}

#[test]
fn input_bound_m2ts_reset_restarts_the_real_startup_evidence_for_each_context() {
    let first = m2ts(BASIC);
    let second = include_bytes!("../data/native-input-v1/original-valid-rebind-192.m2ts");
    let o = run_format(
        &[(&first, false), (second, true)],
        1,
        1,
        PacketFormat::M2ts192,
        true,
    );
    assert_eq!(o.packets, 3);
    assert!(
        o.native
            .contains("\"source_origin\":{\"source\":2,\"offset\":384}")
    );
    assert!(o.native.contains("\"native_position\":384"));
    assert!(o.native.contains("\"native_position\":1152"));
}

#[test]
fn input_bound_m2ts_map_applies_the_same_basis_to_a_pusi_in_the_next_entity() {
    let map = m2ts(MAP);
    let media = m2ts(MEDIA);
    let o = run_format(
        &[(&map, false), (&media, false)],
        1,
        1,
        PacketFormat::M2ts192,
        true,
    );
    assert_eq!(o.packets, 1);
    assert!(
        o.native
            .contains("\"source_origin\":{\"source\":2,\"offset\":0}")
    );
    assert!(o.native.contains("\"native_position\":388"));
    assert_eq!(o.source[0].1, Some(90000));
}
#[test]
fn short_contiguous_m2ts_entity_does_not_reseed_a_carrier_start_context() {
    let first = include_bytes!("../data/native-input-v1/original-valid-rebind-192.m2ts");
    let mut second = m2ts(MEDIA);
    // The pinned original stream finishes PID256 at counter5.
    // This new derived continuation advances that counter without a reset.
    second[7] = (second[7] & 0xf0) | 6;
    let o = run_format(
        &[(first, false), (&second, false)],
        1,
        1,
        PacketFormat::M2ts192,
        true,
    );
    assert_eq!(o.packets, 3);
    assert!(
        o.native
            .contains("\"source_origin\":{\"source\":2,\"offset\":0}")
    );
    assert!(o.native.contains("\"native_position\":1728"));
    assert!(!o.native.contains("\"native_position\":1732"));
    assert_eq!(o.source[2].2.clock, 0);
}
