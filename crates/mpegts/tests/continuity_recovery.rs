//! Original red-first continuity/PSI/PES regressions. Parent executes this target.
//! H.222.0 (08/2023) 2.4.3.3/5: a flag and an actual counter break differ.
//! These assert the strict source operation, never native replay/corrupt-packet policy.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_mpegts::{
    Continuity, ContinuityOutcome, FaultKind, PacketFormat, ParseLimits, Pes, TraceEvent,
    TransportStream, parse_transport,
};

const MARKER: &[u8] = b"lane-ts-original-payload\x00\x47\xff";
const CLEAN: &[u8] = b"after-loss-clean";
const SOURCE: SourceId = SourceId(77);

fn collect_with(
    source: &[u8],
    format: PacketFormat,
    short_read: usize,
    work_units: u64,
    check_cancel: bool,
) -> Vec<TraceEvent> {
    let budget = Budget::new(1 << 20).unwrap();
    let mut input = InputCursor::new(
        &budget,
        SOURCE,
        SourceCapabilities {
            length: Some(source.len() as u64),
            random_access: false,
        },
        4096,
    )
    .unwrap();
    let mut demux =
        TransportStream::new(budget.clone(), format, ParseLimits::default(), 1).unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        demux.poll(&mut input, &mut WorkBudget::new(0), &cancel),
        Ok(Step::Yield)
    ));
    let mut events = Vec::new();
    for _ in 0..200_000 {
        if check_cancel {
            cancel.store(true, Ordering::Relaxed);
            assert!(matches!(
                demux.poll(&mut input, &mut WorkBudget::new(work_units), &cancel),
                Err(Error::Cancelled)
            ));
            cancel.store(false, Ordering::Relaxed);
        }
        match demux
            .poll(&mut input, &mut WorkBudget::new(work_units), &cancel)
            .unwrap()
        {
            Step::Ready(TraceEvent::Finished) => {
                assert!(!demux.clock_is_quarantined());
                return events;
            }
            Step::Ready(event) => events.push(event),
            Step::Yield => {}
            Step::End(_) => demux.end_of_source().unwrap(),
            Step::NeedInput(request) => {
                let offset = request.offset as usize;
                if offset == source.len() {
                    input.finish(request.source, request.token).unwrap();
                } else {
                    let end = source.len().min(offset + request.maximum.min(short_read));
                    let bytes = Bytes::copy_from_slice(&budget, &source[offset..end]).unwrap();
                    input
                        .provide(
                            &InputWindow::new(request.source, request.token, request.offset, bytes)
                                .unwrap(),
                        )
                        .unwrap();
                }
            }
        }
    }
    panic!("continuity source operation failed to make bounded progress");
}

fn collect(source: &[u8]) -> Vec<TraceEvent> {
    collect_with(source, PacketFormat::Ts188, 7, 1, false)
}

fn packets(events: &[TraceEvent]) -> Vec<&Pes> {
    events
        .iter()
        .filter_map(|event| match event {
            TraceEvent::Pes(value) => Some(value),
            _ => None,
        })
        .collect()
}

fn payload(pes: &Pes) -> Vec<u8> {
    pes.payload().flatten().copied().collect()
}

fn stamp(pes: &Pes, pts: u64, byte_run: u64, elementary_offset: u64) {
    assert_eq!(pes.pid, 256);
    assert_eq!(pes.header.pts_raw33, Some(pts));
    assert_eq!(pes.header.dts_raw33, None);
    assert_eq!(pes.epochs.clock, 0);
    assert_eq!(pes.epochs.configuration, 2);
    assert_eq!(pes.byte_run_epoch, byte_run);
    assert_eq!(pes.elementary_offset, elementary_offset);
    assert!(pes.source_ranges().all(|range| range.entity == SOURCE));
    assert!(
        pes.header_source_ranges
            .as_slice()
            .iter()
            .all(|range| range.entity == SOURCE)
    );
}

fn clean(events: &[TraceEvent]) {
    assert!(!events.iter().any(|event| matches!(
        event,
        TraceEvent::Diagnostic(_) | TraceEvent::Timeline { .. }
    )));
}

fn fault(events: &[TraceEvent], offset: u64, error: &'static str) {
    let faults: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            TraceEvent::Diagnostic(value) => Some(value),
            _ => None,
        })
        .collect();
    assert_eq!(faults.len(), 1);
    assert_eq!(faults[0].kind, FaultKind::Continuity);
    assert_eq!(faults[0].pid, Some(256));
    assert_eq!(faults[0].location.source, SOURCE);
    assert_eq!(faults[0].location.offset, offset);
    assert_eq!(faults[0].error, Error::Invalid(error));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TraceEvent::Timeline { .. }))
    );
}

fn raw(events: &[TraceEvent], source: &[u8], format: PacketFormat) {
    let transport: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            TraceEvent::Transport(packet) => Some(packet),
            _ => None,
        })
        .collect();
    assert_eq!(transport.len(), source.len() / format.bytes());
    for (index, packet) in transport.iter().enumerate() {
        let start = index * format.bytes();
        assert_eq!(packet.source_offset, start as u64);
        assert_eq!(packet.source_id, Some(SOURCE));
        assert_eq!(
            packet.source.as_ref(),
            &source[start..start + format.bytes()]
        );
    }
}

fn one_original(events: &[TraceEvent], expected: &[u8]) {
    let pes = packets(events);
    assert_eq!(pes.len(), 1);
    assert_eq!(payload(pes[0]), expected);
    stamp(pes[0], 90000, 0, 0);
    clean(events);
}

#[test]
fn frozen_one_duplicate_keeps_one_source_pes_and_both_raw_packets() {
    let source = include_bytes!("data/cc-one-duplicate.ts");
    let events = collect(source);
    one_original(&events, MARKER);
    raw(&events, source, PacketFormat::Ts188);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                TraceEvent::Continuity {
                    pid: 256,
                    outcome: ContinuityOutcome::Duplicate
                }
            ))
            .count(),
        1
    );
    // Exact native19c emits two PES packets; this source assertion does not waive it.
}

#[test]
fn frozen_third_copy_keeps_the_invalid_source_diagnostic() {
    let source = include_bytes!("data/cc-third-copy.ts");
    let events = collect(source);
    let pes = packets(&events);
    assert_eq!(pes.len(), 1);
    assert_eq!(payload(pes[0]), MARKER);
    stamp(pes[0], 90000, 0, 0);
    fault(&events, 752, "TS excess duplicate");
    raw(&events, source, PacketFormat::Ts188);
    // Native19c emits three unflagged PES packets. Source error remains required.
}

#[test]
fn frozen_duplicate_pcr_update_keeps_two_raw_clocks_and_one_source_pes() {
    let source = include_bytes!("data/cc-duplicate-pcr-update.ts");
    let events = collect(source);
    one_original(&events, MARKER);
    let clocks: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            TraceEvent::Transport(packet) => packet
                .adaptation
                .as_ref()
                .and_then(|a| a.pcr)
                .map(|clock| (packet.source_offset, clock.base, clock.extension)),
            _ => None,
        })
        .collect();
    assert_eq!(clocks, [(376, 90000, 7), (564, 90100, 9)]);
    raw(&events, source, PacketFormat::Ts188);
    // No PCR field or repeated native PES is rewritten into a single source sample.
}

#[test]
fn frozen_mid_pes_loss_never_becomes_a_complete_source_packet() {
    let source = include_bytes!("data/cc-loss-mid-pes.ts");
    let events = collect(source);
    assert!(packets(&events).is_empty());
    fault(&events, 564, "TS continuity loss");
    raw(&events, source, PacketFormat::Ts188);
    // Native19c emits354 bytes with AV_PKT_FLAG_CORRUPT=2. Separate operation only.
}

#[test]
fn continuous_non_pcr_flag_preserves_in_progress_pes_payload() {
    let source = include_bytes!("data/continuity-v1/di-continuous-pes-payload.ts");
    let events = collect(source);
    one_original(&events, &(0..240).collect::<Vec<u8>>());
    raw(&events, source, PacketFormat::Ts188);
}

#[test]
fn continuous_non_pcr_flag_preserves_the_split_pes_header() {
    let source = include_bytes!("data/continuity-v1/di-continuous-pes-header.ts");
    let events = collect(source);
    one_original(&events, MARKER);
    let pes = packets(&events);
    assert_eq!(pes[0].header.as_bytes(), &hex_header());
    let offsets: Vec<_> = pes[0]
        .header_source_ranges
        .as_slice()
        .iter()
        .map(|range| (range.offset, range.bytes))
        .collect();
    assert_eq!(offsets, [(561, 3), (747, 5), (907, 6)]);
    raw(&events, source, PacketFormat::Ts188);
}

fn hex_header() -> [u8; 14] {
    [
        0, 0, 1, 0xe0, 0, 0x23, 0x80, 0x80, 5, 0x21, 0, 5, 0xbf, 0x21,
    ]
}

#[test]
fn continuous_non_pcr_adaptation_only_flag_does_not_drop_a_partial_pes() {
    let source = include_bytes!("data/continuity-v1/di-continuous-adaptation-only.ts");
    let events = collect(source);
    one_original(&events, &(0..240).collect::<Vec<u8>>());
    raw(&events, source, PacketFormat::Ts188);
}

#[test]
fn duplicate_classification_is_still_used_when_both_packets_carry_a_flag() {
    let source = include_bytes!("data/continuity-v1/di-duplicate-complete.ts");
    let budget = Budget::new(4096).unwrap();
    let a = parse_transport(
        Bytes::copy_from_slice(&budget, &source[376..564]).unwrap(),
        PacketFormat::Ts188,
        376,
    )
    .unwrap();
    let b = parse_transport(
        Bytes::copy_from_slice(&budget, &source[564..752]).unwrap(),
        PacketFormat::Ts188,
        564,
    )
    .unwrap();
    assert!(a.discontinuity_indicator() && b.discontinuity_indicator());
    let mut cc = Continuity::new(256).unwrap();
    assert_eq!(cc.observe(&a).unwrap(), ContinuityOutcome::First);
    assert_eq!(cc.observe(&b).unwrap(), ContinuityOutcome::Duplicate);
}

#[test]
fn flagged_complete_duplicate_is_one_source_pes_with_original_extents() {
    let source = include_bytes!("data/continuity-v1/di-duplicate-complete.ts");
    let events = collect(source);
    one_original(&events, MARKER);
    let pes = packets(&events);
    assert_eq!(pes[0].header_source_ranges.as_slice()[0].offset, 523);
    assert_eq!(pes[0].source_ranges().next().unwrap().offset, 537);
    raw(&events, source, PacketFormat::Ts188);
}

#[test]
fn flagged_start_duplicate_does_not_rebase_header_or_byte_run() {
    let source = include_bytes!("data/continuity-v1/di-duplicate-start.ts");
    let events = collect(source);
    one_original(&events, &(0..240).collect::<Vec<u8>>());
    let pes = packets(&events);
    assert_eq!(pes[0].header_source_ranges.as_slice()[0].offset, 436);
    assert_eq!(
        pes[0]
            .source_ranges()
            .map(|range| (range.offset, range.bytes))
            .collect::<Vec<_>>(),
        [(450, 114), (814, 126)]
    );
    raw(&events, source, PacketFormat::Ts188);
}

#[test]
fn continuous_pat_flag_does_not_abandon_the_same_version_section() {
    let source = include_bytes!("data/continuity-v1/di-continuous-pat.ts");
    let events = collect(source);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, TraceEvent::Pat(_)))
            .count(),
        1
    );
    one_original(&events, MARKER);
    raw(&events, source, PacketFormat::Ts188);
}

#[test]
fn continuous_pmt_flag_does_not_abandon_the_same_version_section() {
    let source = include_bytes!("data/continuity-v1/di-continuous-pmt.ts");
    let events = collect(source);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, TraceEvent::ProgramMap { active: true, .. }))
            .count(),
        1
    );
    one_original(&events, MARKER);
    raw(&events, source, PacketFormat::Ts188);
}

#[test]
fn repeated_content_at_incremented_counters_is_two_source_pes_packets() {
    let source = include_bytes!("data/continuity-v1/same-content-new-counters.ts");
    let events = collect(source);
    let pes = packets(&events);
    assert_eq!(pes.len(), 2);
    for (index, value) in pes.iter().enumerate() {
        assert_eq!(payload(value), MARKER);
        stamp(value, 90000, 0, index as u64 * MARKER.len() as u64);
    }
    clean(&events);
    raw(&events, source, PacketFormat::Ts188);
}

#[test]
fn duplicate_first_fragment_does_not_replay_or_rebase_partial_pes() {
    let source = include_bytes!("data/continuity-v1/partial-start-duplicate.ts");
    let events = collect(source);
    one_original(&events, &(0..=255).cycle().take(512).collect::<Vec<u8>>());
    let pes = packets(&events);
    assert_eq!(pes[0].header_source_ranges.as_slice()[0].offset, 380);
    assert_eq!(pes[0].source_ranges().next().unwrap().offset, 394);
    raw(&events, source, PacketFormat::Ts188);
}

#[test]
fn duplicate_middle_fragment_is_not_reassembled_as_extra_elementary_bytes() {
    let source = include_bytes!("data/continuity-v1/partial-middle-duplicate.ts");
    let events = collect(source);
    one_original(&events, &(0..=255).cycle().take(512).collect::<Vec<u8>>());
    let pes = packets(&events);
    let offsets: Vec<_> = pes[0]
        .source_ranges()
        .map(|range| (range.offset, range.bytes))
        .collect();
    assert_eq!(offsets, [(394, 170), (568, 184), (970, 158)]);
    raw(&events, source, PacketFormat::Ts188);
}

#[test]
fn other_pid_interleaving_does_not_break_per_pid_duplicate_detection() {
    let source = include_bytes!("data/continuity-v1/interleaved-other-pid-duplicate.ts");
    let events = collect(source);
    one_original(&events, MARKER);
    raw(&events, source, PacketFormat::Ts188);
}

#[test]
fn m2ts_opaque_prefix_changes_do_not_turn_a_duplicate_into_new_payload() {
    let source = include_bytes!("data/continuity-v1/m2ts-prefix-only-duplicate.m2ts");
    let events = collect_with(source, PacketFormat::M2ts192, 1, 1, false);
    one_original(&events, MARKER);
    let prefixes: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            TraceEvent::Transport(packet) => {
                Some(packet.prefix.as_ref().unwrap().as_ref().to_vec())
            }
            _ => None,
        })
        .collect();
    assert_ne!(prefixes[2], prefixes[3]);
    assert_eq!(
        packets(&events)[0].header_source_ranges.as_slice()[0].offset,
        535
    );
    raw(&events, source, PacketFormat::M2ts192);
}

fn recovered(source: &[u8], offset: u64, reason: &'static str) {
    let events = collect(source);
    let pes = packets(&events);
    assert_eq!(pes.len(), 1);
    assert_eq!(payload(pes[0]), CLEAN);
    stamp(pes[0], 93000, 1, 0);
    fault(&events, offset, reason);
    raw(&events, source, PacketFormat::Ts188);
}

#[test]
fn counter_loss_discards_partial_pes_and_recovers_only_at_later_pusi() {
    recovered(
        include_bytes!("data/continuity-v1/loss-then-clean-pusi.ts"),
        564,
        "TS continuity loss",
    );
}

#[test]
fn counter_loss_on_new_pusi_preserves_the_new_packet_after_diagnostic() {
    recovered(
        include_bytes!("data/continuity-v1/loss-at-clean-pusi.ts"),
        564,
        "TS continuity loss",
    );
}

#[test]
fn opcr_changes_are_not_covered_by_the_duplicate_pcr_exception() {
    let source = include_bytes!("data/continuity-v1/opcr-change-is-not-duplicate.ts");
    let events = collect(source);
    let pes = packets(&events);
    assert_eq!(pes.len(), 2);
    assert_eq!(payload(pes[0]), MARKER);
    assert_eq!(payload(pes[1]), MARKER);
    stamp(pes[0], 90000, 0, 0);
    stamp(pes[1], 90000, 1, 0);
    let opcr: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            TraceEvent::Transport(packet) => packet
                .adaptation
                .as_ref()
                .and_then(|a| a.opcr)
                .map(|clock| (packet.source_offset, clock.base, clock.extension)),
            _ => None,
        })
        .collect();
    assert_eq!(opcr, [(376, 90000, 0), (564, 90100, 0)]);
    fault(&events, 564, "TS continuity loss");
    raw(&events, source, PacketFormat::Ts188);
}

#[test]
fn third_middle_copy_remains_an_error_before_later_clean_recovery() {
    recovered(
        include_bytes!("data/continuity-v1/third-middle-copy-then-clean.ts"),
        940,
        "TS excess duplicate",
    );
}

#[derive(Debug, PartialEq, Eq)]
struct PesFacts {
    header: Vec<u8>,
    payload: Vec<u8>,
    ranges: Vec<(u64, usize)>,
    header_ranges: Vec<(u64, usize)>,
    clock: u64,
    configuration: u64,
    byte_run: u64,
    elementary_offset: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct Facts {
    raw: Vec<(u64, Vec<u8>)>,
    pes: Vec<PesFacts>,
    continuity: Vec<(u16, ContinuityOutcome)>,
}

fn facts(events: &[TraceEvent]) -> Facts {
    Facts {
        raw: events
            .iter()
            .filter_map(|event| match event {
                TraceEvent::Transport(packet) => {
                    Some((packet.source_offset, packet.source.as_ref().to_vec()))
                }
                _ => None,
            })
            .collect(),
        pes: packets(events)
            .iter()
            .map(|pes| PesFacts {
                header: pes.header.as_bytes().to_vec(),
                payload: payload(pes),
                ranges: pes
                    .source_ranges()
                    .map(|range| (range.offset, range.bytes))
                    .collect(),
                header_ranges: pes
                    .header_source_ranges
                    .as_slice()
                    .iter()
                    .map(|range| (range.offset, range.bytes))
                    .collect(),
                clock: pes.epochs.clock,
                configuration: pes.epochs.configuration,
                byte_run: pes.byte_run_epoch,
                elementary_offset: pes.elementary_offset,
            })
            .collect(),
        continuity: events
            .iter()
            .filter_map(|event| match event {
                TraceEvent::Continuity { pid, outcome } => Some((*pid, *outcome)),
                _ => None,
            })
            .collect(),
    }
}

#[test]
fn short_reads_work_yields_and_cancellation_preserve_the_same_source_facts() {
    let source = include_bytes!("data/continuity-v1/partial-middle-duplicate.ts");
    let baseline = collect_with(source, PacketFormat::Ts188, 4096, 4096, false);
    let resumed = collect_with(source, PacketFormat::Ts188, 1, 1, true);
    assert_eq!(facts(&baseline), facts(&resumed));
    one_original(&resumed, &(0..=255).cycle().take(512).collect::<Vec<u8>>());
    raw(&resumed, source, PacketFormat::Ts188);
}

#[test]
fn adaptation_only_packet_does_not_increment_the_payload_counter() {
    let events = collect(include_bytes!("data/cc-adaptation-only.ts"));
    let pes = packets(&events);
    assert_eq!(pes.len(), 2);
    assert_eq!(payload(pes[0]), MARKER);
    assert_eq!(payload(pes[1]), b"b");
    stamp(pes[0], 90000, 0, 0);
    stamp(pes[1], 90000, 0, MARKER.len() as u64);
    clean(&events);
}

#[test]
fn payload_counter_wrap_does_not_imply_a_clock_or_configuration_epoch() {
    let events = collect(include_bytes!("data/cc-wrap-15-to-zero.ts"));
    let pes = packets(&events);
    assert_eq!(pes.len(), 2);
    assert_eq!(payload(pes[0]), b"a");
    assert_eq!(payload(pes[1]), b"b");
    stamp(pes[0], 90000, 0, 0);
    stamp(pes[1], 93000, 0, 1);
    clean(&events);
}
