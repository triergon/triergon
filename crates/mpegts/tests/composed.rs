//! Composed-operation regressions.
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_mpegts::{FaultKind, PacketFormat, ParseLimits, TraceEvent, TransportStream};

fn collect(resources: &[&[u8]], format: PacketFormat) -> Vec<TraceEvent> {
    let budget = Budget::new(16 * 1024 * 1024).unwrap();
    let mut input = InputCursor::new(
        &budget,
        SourceId(1),
        SourceCapabilities {
            length: Some(resources[0].len() as u64),
            random_access: false,
        },
        4096,
    )
    .unwrap();
    let mut demux =
        TransportStream::new(budget.clone(), format, ParseLimits::default(), 1).unwrap();
    let cancel = AtomicBool::new(false);
    let mut entity = 0;
    let mut result = Vec::new();
    for _ in 0..200_000 {
        match demux
            .poll(&mut input, &mut WorkBudget::new(7), &cancel)
            .unwrap()
        {
            Step::Ready(TraceEvent::Finished) => return result,
            Step::Ready(event) => result.push(event),
            Step::Yield => {}
            Step::NeedInput(request) => {
                let source = resources[entity];
                let offset = request.offset as usize;
                if offset == source.len() {
                    input.finish(request.source, request.token).unwrap();
                } else {
                    let end = source.len().min(offset + request.maximum.min(7));
                    let data = Bytes::copy_from_slice(&budget, &source[offset..end]).unwrap();
                    input
                        .provide(
                            &InputWindow::new(request.source, request.token, request.offset, data)
                                .unwrap(),
                        )
                        .unwrap();
                }
            }
            Step::End(_) => {
                entity += 1;
                if entity == resources.len() {
                    demux.end_of_source().unwrap();
                } else {
                    input
                        .continue_source(
                            SourceId(entity as u64 + 1),
                            SourceCapabilities {
                                length: Some(resources[entity].len() as u64),
                                random_access: false,
                            },
                        )
                        .unwrap();
                }
            }
        }
    }
    panic!("composed operation made no bounded progress");
}

#[test]
fn composed_pat_pmt_and_pes_keep_original_payload_header_and_source_ranges() {
    let events = collect(
        &[include_bytes!("data/ts188-basic.ts")],
        PacketFormat::Ts188,
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, TraceEvent::Pat(_)))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, TraceEvent::ProgramMap { active: true, .. }))
            .count(),
        1
    );
    let packets: Vec<_> = events
        .iter()
        .filter_map(|event| {
            if let TraceEvent::Pes(pes) = event {
                Some(pes)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(packets.len(), 1);
    assert_eq!(
        packets[0].payload().flatten().copied().collect::<Vec<_>>(),
        b"lane-ts-original-payload\x00\x47\xff"
    );
    assert_eq!(packets[0].header.pts_raw33, Some(90000));
    assert_eq!(packets[0].header.dts_raw33, None);
    assert_eq!(
        packets[0]
            .source_ranges()
            .map(|range| range.bytes)
            .sum::<usize>(),
        27
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TraceEvent::Diagnostic(_)))
    );
}

#[test]
fn m2ts_and_ts_produce_the_same_pes_without_treating_prefix_as_time() {
    let events = collect(
        &[include_bytes!("data/m2ts192-opaque-prefix.m2ts")],
        PacketFormat::M2ts192,
    );
    let pes = events
        .iter()
        .find_map(|event| {
            if let TraceEvent::Pes(value) = event {
                Some(value)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(pes.header.pts_raw33, Some(90000));
    assert_eq!(pes.header.dts_raw33, None);
    assert_eq!(pes.source_ranges().next().unwrap().offset, 549);
}

#[test]
fn full_pat_version_is_required_and_next_tables_never_activate() {
    for source in [
        include_bytes!("data/pat-incomplete-version.ts").as_slice(),
        include_bytes!("data/psi-pending-not-current.ts").as_slice(),
    ] {
        let events = collect(&[source], PacketFormat::Ts188);
        assert!(!events.iter().any(|event| matches!(
            event,
            TraceEvent::Pat(_) | TraceEvent::ProgramMap { active: true, .. } | TraceEvent::Pes(_)
        )));
    }
    let events = collect(
        &[include_bytes!("data/pat-atomic-two-sections.ts")],
        PacketFormat::Ts188,
    );
    let table = events
        .iter()
        .find_map(|event| {
            if let TraceEvent::Pat(value) = event {
                Some(value)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(table.programs.as_slice().len(), 2);
    assert_eq!(table.source_sections.len(), 2);
}

#[test]
fn duplicate_payload_is_ignored_and_counter_loss_does_not_join_partial_pes() {
    let events = collect(
        &[include_bytes!("data/cc-one-duplicate.ts")],
        PacketFormat::Ts188,
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, TraceEvent::Pes(_)))
            .count(),
        1
    );
    let events = collect(
        &[include_bytes!("data/cc-loss-mid-pes.ts")],
        PacketFormat::Ts188,
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TraceEvent::Pes(_)))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, TraceEvent::Diagnostic(_)))
    );
}

fn assert_pointer_recovery(source: &[u8], pid: u16, offset: u64) {
    let events = collect(&[source], PacketFormat::Ts188);
    let faults: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            TraceEvent::Diagnostic(fault) => Some(fault),
            _ => None,
        })
        .collect();
    assert_eq!(faults.len(), 1);
    assert_eq!(faults[0].kind, FaultKind::Psi);
    assert_eq!(faults[0].pid, Some(pid));
    assert_eq!(faults[0].location.source, SourceId(1));
    assert_eq!(faults[0].location.offset, offset);
    assert_eq!(faults[0].error, Error::Invalid("PSI pointer"));
    let packets: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            TraceEvent::Pes(pes) => Some(pes),
            _ => None,
        })
        .collect();
    assert_eq!(packets.len(), 1);
    assert_eq!(
        packets[0].payload().flatten().copied().collect::<Vec<_>>(),
        b"lane-ts-original-payload\x00\x47\xff"
    );
    assert_eq!(packets[0].header.pts_raw33, Some(90000));
    assert_eq!(packets[0].header.dts_raw33, None);
    assert_eq!(packets[0].source_ranges().next().unwrap().offset, 725);
}

#[test]
fn rejected_pat_pointer_is_a_source_diagnostic_and_a_new_pusi_can_recover() {
    // Original malformed source followed by an unchanged valid section/PES run,
    // except its PAT counter advances from the rejected packet's counter zero.
    let mut source = include_bytes!("data/psi-pointer-overrun.ts").to_vec();
    let mut following = include_bytes!("data/ts188-basic.ts").to_vec();
    following[3] = (following[3] & 0xf0) | 1;
    source.extend_from_slice(&following);
    assert_pointer_recovery(&source, 0, 0);
}

#[test]
fn rejected_pmt_pointer_does_not_prevent_a_later_complete_program_map() {
    let original = include_bytes!("data/ts188-basic.ts");
    let mut rejected = include_bytes!("data/psi-pointer-overrun.ts").to_vec();
    // Route the malformed pointer to the PMT PID announced by the valid PAT.
    rejected[1] = (rejected[1] & 0xe0) | 0x10;
    rejected[2] = 0;
    let mut following = original[188..376].to_vec();
    following[3] = (following[3] & 0xf0) | 1;
    let mut source = original[..188].to_vec();
    source.extend_from_slice(&rejected);
    source.extend_from_slice(&following);
    source.extend_from_slice(&original[376..]);
    assert_pointer_recovery(&source, 4096, 188);
}

#[test]
fn contiguous_source_entity_end_carries_the_selected_pes() {
    let events = collect(
        &[
            include_bytes!("data/hls-contiguous-part-a.ts"),
            include_bytes!("data/hls-contiguous-part-b.ts"),
        ],
        PacketFormat::Ts188,
    );
    let pes = events
        .iter()
        .find_map(|event| {
            if let TraceEvent::Pes(value) = event {
                Some(value)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(pes.payload_len, 512);
    assert_eq!(
        pes.payload().flatten().copied().collect::<Vec<_>>(),
        (0_u8..=255).cycle().take(512).collect::<Vec<_>>()
    );
    assert!(pes.source_ranges().any(|range| range.entity == SourceId(2)));
}

#[test]
fn raw_header_timestamp_and_optional_dts_remain_independent_of_demux_inference() {
    let events = collect(
        &[include_bytes!("data/pts-dts-common-epoch.ts")],
        PacketFormat::Ts188,
    );
    let packets: Vec<_> = events
        .iter()
        .filter_map(|event| {
            if let TraceEvent::Pes(value) = event {
                Some(value)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0].header.dts_raw33, Some((1 << 33) - 6));
    assert_eq!(packets[0].header.pts_raw33, Some(2));
    assert_eq!(packets[1].header.dts_raw33, Some(3));
}

#[test]
fn continuity_loss_changes_byte_run_identity_without_inventing_a_clock_reset() {
    let events = collect(
        &[include_bytes!("data/cc-loss-clean-pusi.ts")],
        PacketFormat::Ts188,
    );
    let packets: Vec<_> = events
        .iter()
        .filter_map(|event| {
            if let TraceEvent::Pes(value) = event {
                Some(value)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0].epochs.clock, packets[1].epochs.clock);
    assert!(packets[1].byte_run_epoch > packets[0].byte_run_epoch);
    assert_eq!(packets[1].elementary_offset, 0);
}

#[test]
fn truncated_final_transport_packet_reports_its_actual_start() {
    let events = collect(
        &[include_bytes!("data/transport-partial-final-packet.ts")],
        PacketFormat::Ts188,
    );
    let fault = events
        .iter()
        .find_map(|event| {
            if let TraceEvent::Diagnostic(value) = event {
                if value.kind == FaultKind::Transport {
                    Some(value)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(fault.location.offset, 564);
}
