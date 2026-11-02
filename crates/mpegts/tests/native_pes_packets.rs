//! Red-first distinct native packet operation, bound to actual archived19c data.
//! Physical input assignments are independent of candidate output. No native
//! worker/comparator executes here; strict source regressions remain separate.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, SourceId, WorkBudget};
use triergon_mpegts::clock::{NativeClockProfile, NativePesClockV1, NativeTimestampOrigin};
use triergon_mpegts::native_pes::{
    NativePesObserver, NativePesPacket, NativePesScope, NativeSourceContext,
};
use triergon_mpegts::{
    AssemblyStep, PacketFormat, ParseLimits, SourceEpochs, TransportPacket, parse_transport,
};

const EPOCHS: SourceEpochs = SourceEpochs {
    clock: 0,
    configuration: 2,
};

fn scope() -> NativePesScope {
    NativePesScope {
        program: 1,
        pid: 256,
        stream_type: 0x1b,
        stream_id: 0xe0,
        entity: SourceId(1),
        epochs: EPOCHS,
    }
}

fn context(ordinal: usize) -> NativeSourceContext {
    NativeSourceContext {
        source_event_ordinal: ordinal as u64,
        epochs: EPOCHS,
        byte_run_epoch: None,
    }
}

fn input(source: &[u8], format: PacketFormat, budget: &Budget) -> Vec<TransportPacket> {
    source
        .chunks_exact(format.bytes())
        .enumerate()
        .map(|(index, bytes)| {
            let bytes = Bytes::copy_from_slice(budget, bytes).unwrap();
            let mut packet =
                parse_transport(bytes, format, (index * format.bytes()) as u64).unwrap();
            packet.source_id = Some(SourceId(1));
            packet
        })
        .collect()
}

fn observer(budget: &Budget) -> NativePesObserver {
    NativePesObserver::new(
        NativeClockProfile::ArchivedPesV1,
        scope(),
        budget.clone(),
        ParseLimits::default(),
    )
    .unwrap()
}

fn drain(observer: &mut NativePesObserver, ended: bool) -> Vec<NativePesPacket> {
    let cancel = AtomicBool::new(false);
    let mut output = Vec::new();
    for _ in 0..100_000 {
        match observer
            .poll(&mut WorkBudget::new(1), &cancel)
            .expect("runtime red until real input-boundary native PES assembly is implemented")
        {
            AssemblyStep::Output(packet) => output.push(packet),
            AssemblyStep::Yield => {}
            AssemblyStep::NeedPayload => {
                assert!(!ended);
                return output;
            }
            AssemblyStep::Finished => {
                assert!(ended);
                return output;
            }
        }
    }
    panic!("native packet observer failed to make bounded progress");
}

#[derive(Clone, Copy)]
struct Expected {
    parts: &'static [usize],
    skip: usize,
    origin: usize,
    flags: u32,
    header_applies: bool,
    native_time: Option<i128>,
}

fn complete(part: usize) -> Expected {
    Expected {
        parts: &[],
        skip: 14,
        origin: part,
        flags: 0,
        header_applies: true,
        native_time: Some(90000),
    }
}

fn expected(
    parts: &'static [usize],
    skip: usize,
    origin: usize,
    flags: u32,
    header_applies: bool,
    native_time: Option<i128>,
) -> Expected {
    Expected {
        parts,
        skip,
        origin,
        flags,
        header_applies,
        native_time,
    }
}

fn chosen<'a>(
    packets: &'a [TransportPacket],
    spec: &Expected,
) -> Vec<(usize, &'a TransportPacket)> {
    if spec.parts.is_empty() {
        vec![(spec.origin, &packets[spec.origin])]
    } else {
        spec.parts
            .iter()
            .map(|&index| (index, &packets[index]))
            .collect()
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Extent {
    offset: u64,
    bytes: usize,
    transport_offset: u64,
    event_ordinal: u64,
}

struct Physical {
    payload: Vec<u8>,
    extents: Vec<Extent>,
}

fn physical(packets: &[TransportPacket], spec: &Expected) -> Physical {
    let mut result = Vec::new();
    let mut extents = Vec::new();
    let mut skip = spec.skip;
    for (index, packet) in chosen(packets, spec) {
        let bytes = packet.payload.as_ref().unwrap();
        let count = skip.min(bytes.len());
        skip -= count;
        if count < bytes.len() {
            result.extend_from_slice(&bytes.as_ref()[count..]);
            extents.push(Extent {
                offset: packet.payload_source_offset + count as u64,
                bytes: bytes.len() - count,
                transport_offset: packet.source_offset,
                event_ordinal: index as u64,
            });
        }
    }
    assert_eq!(skip, 0);
    Physical {
        payload: result,
        extents,
    }
}

fn check(source: &[u8], format: PacketFormat, specs: &[Expected]) {
    let budget = Budget::new(1 << 20).unwrap();
    let packets = input(source, format, &budget);
    let before: Vec<_> = packets
        .iter()
        .map(|packet| packet.source.as_ref().to_vec())
        .collect();
    let mut observer = observer(&budget);
    let mut output = Vec::new();
    for (index, packet) in packets
        .iter()
        .enumerate()
        .filter(|(_, packet)| packet.pid == 256)
    {
        observer.push(packet.clone(), context(index)).unwrap();
        output.extend(drain(&mut observer, false));
    }
    observer.end_of_source().unwrap();
    output.extend(drain(&mut observer, true));
    assert_eq!(
        output.len(),
        specs.len(),
        "exact native packet count/order; no coalescing"
    );
    let mut clock = NativePesClockV1::new(1).unwrap();
    for (packet, spec) in output.iter().zip(specs) {
        let physical = physical(&packets, spec);
        assert_eq!(packet.profile, NativeClockProfile::ArchivedPesV1);
        assert_eq!(packet.program, 1);
        assert_eq!(packet.pid, 256);
        assert_eq!(packet.source_origin.source, SourceId(1));
        assert_eq!(
            packet.source_origin.offset,
            packets[spec.origin].source_offset
        );
        assert_eq!(packet.source_epochs, EPOCHS);
        assert_eq!(
            packet.native_position,
            (packets[spec.origin].source_offset + format.prefix_bytes() as u64) as i64
        );
        assert_eq!(packet.native_flags, spec.flags);
        assert_eq!(packet.native_duration90_raw, 0);
        assert_eq!(packet.native_stream_id, 0xe0);
        assert_eq!(packet.payload_len, physical.payload.len());
        assert_eq!(
            packet.payload().flatten().copied().collect::<Vec<_>>(),
            physical.payload
        );
        let actual: Vec<_> = packet
            .spans
            .as_slice()
            .iter()
            .map(|span| {
                assert_eq!(span.source.entity, SourceId(1));
                assert_eq!(span.transport.entity, SourceId(1));
                assert_eq!(span.transport.bytes, format.bytes());
                assert_eq!(span.context.epochs, EPOCHS);
                assert_eq!(span.context.byte_run_epoch, None);
                Extent {
                    offset: span.source.offset,
                    bytes: span.source.bytes,
                    transport_offset: span.transport.offset,
                    event_ordinal: span.context.source_event_ordinal,
                }
            })
            .collect();
        assert_eq!(actual, physical.extents);
        let mut header = Vec::new();
        let mut header_ranges = Vec::new();
        for source in packets[spec.origin..].iter().filter(|p| p.pid == 256) {
            if let Some(bytes) = source.payload.as_ref() {
                let count = (14 - header.len()).min(bytes.len());
                if count != 0 {
                    header.extend_from_slice(&bytes.as_ref()[..count]);
                    header_ranges.push((source.payload_source_offset, count));
                }
            }
            if header.len() >= 14 {
                break;
            }
        }
        assert_eq!(packet.source_header.as_bytes(), &header[..14]);
        let actual_header_ranges: Vec<_> = packet
            .header_source_ranges
            .as_slice()
            .iter()
            .map(|range| {
                assert_eq!(range.entity, SourceId(1));
                (range.offset, range.bytes)
            })
            .collect();
        assert_eq!(actual_header_ranges, header_ranges);
        assert_eq!(packet.source_header.dts_raw33, None);
        assert_eq!(packet.time_header_applies, spec.header_applies);
        // A headerless native continuation still retains causal raw PTS90000.
        let raw_pts = if spec.native_time == Some(93000) {
            93000
        } else {
            90000
        };
        assert_eq!(packet.source_header.pts_raw33, Some(raw_pts));
        let time = clock
            .project_native_header(
                packet.pid,
                packet.source_epochs,
                packet.time_header(),
                &mut WorkBudget::new(1),
                &AtomicBool::new(false),
            )
            .expect("red until the separately named raw-header clock entrypoint is implemented")
            .unwrap();
        assert_eq!(time.pts90, spec.native_time);
        assert_eq!(time.dts90, spec.native_time);
        assert_eq!(time.source_dts_raw33, None);
        assert_eq!(
            time.source_pts_raw33,
            spec.header_applies.then_some(raw_pts)
        );
        assert_eq!(
            time.dts_origin,
            if spec.header_applies {
                NativeTimestampOrigin::PtsOnlyCompatibility
            } else {
                NativeTimestampOrigin::Absent
            }
        );
    }
    for (packet, bytes) in packets.iter().zip(before) {
        assert_eq!(packet.source.as_ref(), bytes);
    }
}

#[test]
fn archived_one_duplicate_emits_two_distinct_input_packets() {
    check(
        include_bytes!("data/cc-one-duplicate.ts"),
        PacketFormat::Ts188,
        &[complete(2), complete(3)],
    );
}
#[test]
fn archived_third_copy_is_not_a_source_validity_waiver() {
    check(
        include_bytes!("data/cc-third-copy.ts"),
        PacketFormat::Ts188,
        &[complete(2), complete(3), complete(4)],
    );
}
#[test]
fn archived_duplicate_pcr_emits_both_pes_without_changing_source_clocks() {
    check(
        include_bytes!("data/cc-duplicate-pcr-update.ts"),
        PacketFormat::Ts188,
        &[complete(2), complete(3)],
    );
}
#[test]
fn archived_loss_mid_pes_emits_exact_corrupt_prefix_at_eof() {
    check(
        include_bytes!("data/cc-loss-mid-pes.ts"),
        PacketFormat::Ts188,
        &[expected(&[2, 3], 14, 2, 2, true, Some(90000))],
    );
}
#[test]
fn observed_continuous_payload_flag_keeps_one_native_packet() {
    check(
        include_bytes!("data/continuity-v1/di-continuous-pes-payload.ts"),
        PacketFormat::Ts188,
        &[expected(&[2, 3], 14, 2, 0, true, Some(90000))],
    );
}
#[test]
fn observed_continuous_header_flag_retains_split_header_facts() {
    check(
        include_bytes!("data/continuity-v1/di-continuous-pes-header.ts"),
        PacketFormat::Ts188,
        &[expected(&[2, 3, 4], 14, 2, 0, true, Some(90000))],
    );
}
#[test]
fn observed_adaptation_only_flag_does_not_mark_native_corrupt() {
    check(
        include_bytes!("data/continuity-v1/di-continuous-adaptation-only.ts"),
        PacketFormat::Ts188,
        &[expected(&[2, 4], 14, 2, 0, true, Some(90000))],
    );
}
#[test]
fn observed_flagged_complete_duplicate_has_two_original_positions() {
    check(
        include_bytes!("data/continuity-v1/di-duplicate-complete.ts"),
        PacketFormat::Ts188,
        &[complete(2), complete(3)],
    );
}
#[test]
fn observed_flagged_start_repeat_flushes_prefix_and_restarts_at_new_input() {
    check(
        include_bytes!("data/continuity-v1/di-duplicate-start.ts"),
        PacketFormat::Ts188,
        &[
            expected(&[2], 14, 2, 2, true, Some(90000)),
            expected(&[3, 4], 14, 3, 0, true, Some(90000)),
        ],
    );
}
#[test]
fn observed_split_pat_context_preserves_native_packet_position() {
    check(
        include_bytes!("data/continuity-v1/di-continuous-pat.ts"),
        PacketFormat::Ts188,
        &[complete(3)],
    );
}
#[test]
fn observed_split_pmt_context_preserves_native_packet_position() {
    check(
        include_bytes!("data/continuity-v1/di-continuous-pmt.ts"),
        PacketFormat::Ts188,
        &[complete(3)],
    );
}
#[test]
fn observed_partial_start_repeat_keeps_corrupt_then_complete_boundaries() {
    check(
        include_bytes!("data/continuity-v1/partial-start-duplicate.ts"),
        PacketFormat::Ts188,
        &[
            expected(&[2], 14, 2, 2, true, Some(90000)),
            expected(&[3, 4, 5], 14, 3, 0, true, Some(90000)),
        ],
    );
}
#[test]
fn observed_middle_repeat_keeps_354_and_342_without_after_output_splitting() {
    check(
        include_bytes!("data/continuity-v1/partial-middle-duplicate.ts"),
        PacketFormat::Ts188,
        &[
            expected(&[2, 3], 14, 2, 2, true, Some(90000)),
            expected(&[4, 5], 0, 2, 0, false, None),
        ],
    );
}
#[test]
fn observed_other_pid_interleaving_keeps_two_native_packet_positions() {
    check(
        include_bytes!("data/continuity-v1/interleaved-other-pid-duplicate.ts"),
        PacketFormat::Ts188,
        &[complete(2), complete(4)],
    );
}
#[test]
fn observed_m2ts_native_position_is_sync_offset_not_source_prefix_offset() {
    check(
        include_bytes!("data/continuity-v1/m2ts-prefix-only-duplicate.m2ts"),
        PacketFormat::M2ts192,
        &[complete(2), complete(3)],
    );
}
#[test]
fn observed_incremented_identical_content_is_two_native_pes_packets() {
    check(
        include_bytes!("data/continuity-v1/same-content-new-counters.ts"),
        PacketFormat::Ts188,
        &[complete(2), complete(3)],
    );
}
#[test]
fn observed_counter_gap_retains_bytes_but_marks_only_affected_native_packet() {
    check(
        include_bytes!("data/continuity-v1/loss-then-clean-pusi.ts"),
        PacketFormat::Ts188,
        &[
            expected(&[2, 3, 4], 14, 2, 2, true, Some(90000)),
            expected(&[5], 14, 5, 0, true, Some(93000)),
        ],
    );
}
#[test]
fn observed_new_pusi_flushes_corrupt_prefix_before_clean_packet() {
    check(
        include_bytes!("data/continuity-v1/loss-at-clean-pusi.ts"),
        PacketFormat::Ts188,
        &[
            expected(&[2], 14, 2, 2, true, Some(90000)),
            expected(&[3], 14, 3, 0, true, Some(93000)),
        ],
    );
}
#[test]
fn observed_opcr_unequal_packet_still_has_two_unflagged_native_outputs() {
    check(
        include_bytes!("data/continuity-v1/opcr-change-is-not-duplicate.ts"),
        PacketFormat::Ts188,
        &[complete(2), complete(3)],
    );
}
#[test]
fn observed_third_middle_copy_keeps_all_three_native_outputs_and_unknowns() {
    check(
        include_bytes!("data/continuity-v1/third-middle-copy-then-clean.ts"),
        PacketFormat::Ts188,
        &[
            expected(&[2, 3], 14, 2, 2, true, Some(90000)),
            expected(&[4, 5, 6], 0, 2, 2, false, None),
            expected(&[7], 14, 7, 0, true, Some(93000)),
        ],
    );
}
#[test]
fn observed_invalid_opcr_draft_is_retained_without_source_acceptance() {
    check_rejected_original(include_bytes!(
        "data/native-pes-v1/opcr-without-pcr-invalid.ts"
    ));
}

#[test]
fn native_headerless_time_is_unknown_and_does_not_seed_a_clock() {
    let mut clock = NativePesClockV1::new(1).unwrap();
    let view = clock
        .project_native_header(
            256,
            EPOCHS,
            None,
            &mut WorkBudget::new(1),
            &AtomicBool::new(false),
        )
        .expect("runtime red until native header time adapter exists")
        .unwrap();
    assert_eq!(view.pts90, None);
    assert_eq!(view.dts90, None);
    assert_eq!(view.source_pts_raw33, None);
    assert_eq!(view.source_dts_raw33, None);
    assert_eq!(view.origin, None);
}

#[test]
fn observer_zero_work_and_cancel_preserve_pending_original_input() {
    let budget = Budget::new(16384).unwrap();
    let packets = input(
        include_bytes!("data/cc-one-duplicate.ts"),
        PacketFormat::Ts188,
        &budget,
    );
    let mut observer = observer(&budget);
    observer.push(packets[2].clone(), context(2)).unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        observer.poll(&mut WorkBudget::new(0), &cancel),
        Ok(AssemblyStep::Yield)
    ));
    cancel.store(true, Ordering::Relaxed);
    assert!(matches!(
        observer.poll(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    ));
    cancel.store(false, Ordering::Relaxed);
    let mut output = drain(&mut observer, false);
    observer.end_of_source().unwrap();
    output.extend(drain(&mut observer, true));
    assert_eq!(output.len(), 1);
    assert_eq!(output[0].source_origin.offset, 376);
}

#[test]
fn retained_output_keeps_shared_charge_after_observer_and_inputs_drop() {
    let budget = Budget::new(16384).unwrap();
    let initial = budget.used();
    let packets = input(
        include_bytes!("data/cc-one-duplicate.ts"),
        PacketFormat::Ts188,
        &budget,
    );
    let mut observer = observer(&budget);
    observer.push(packets[2].clone(), context(2)).unwrap();
    let mut output = drain(&mut observer, false);
    observer.end_of_source().unwrap();
    output.extend(drain(&mut observer, true));
    drop(observer);
    drop(packets);
    assert!(budget.used() > initial);
    assert_eq!(
        output[0].payload().flatten().copied().collect::<Vec<_>>(),
        b"lane-ts-original-payload\x00\x47\xff"
    );
    drop(output);
    assert_eq!(budget.used(), initial);
}

#[test]
fn configured_payload_limit_is_not_bypassed_by_native_view() {
    let budget = Budget::new(16384).unwrap();
    let packets = input(
        include_bytes!("data/continuity-v1/partial-start-duplicate.ts"),
        PacketFormat::Ts188,
        &budget,
    );
    let limits = ParseLimits {
        pes_bytes: 100,
        ..ParseLimits::default()
    };
    let mut observer = NativePesObserver::new(
        NativeClockProfile::ArchivedPesV1,
        scope(),
        budget.clone(),
        limits,
    )
    .unwrap();
    observer.push(packets[2].clone(), context(2)).unwrap();
    let mut found = false;
    for _ in 0..1000 {
        match observer.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Err(Error::Limit { maximum: 100, .. }) => {
                found = true;
                break;
            }
            Ok(AssemblyStep::Yield) => {}
            other => panic!("payload bound must be explicit, got {other:?}"),
        }
    }
    assert!(found);
}

#[test]
fn foreign_pool_rejection_recovers_original_charged_transport() {
    let budget = Budget::new(16384).unwrap();
    let foreign = Budget::new(16384).unwrap();
    let packets = input(
        include_bytes!("data/cc-one-duplicate.ts"),
        PacketFormat::Ts188,
        &foreign,
    );
    let mut observer = observer(&budget);
    let error = observer.push(packets[2].clone(), context(2)).unwrap_err();
    assert_eq!(error.error, Error::Invalid("foreign native PES byte pool"));
    assert_eq!(error.value.source.as_ref(), packets[2].source.as_ref());
    assert_eq!(error.value.source_offset, 376);
}

#[test]
fn undrained_mailbox_rejection_does_not_allocate_or_lose_input() {
    let budget = Budget::new(16384).unwrap();
    let packets = input(
        include_bytes!("data/cc-one-duplicate.ts"),
        PacketFormat::Ts188,
        &budget,
    );
    let mut observer = observer(&budget);
    observer.push(packets[2].clone(), context(2)).unwrap();
    let before = budget.used();
    let error = observer.push(packets[3].clone(), context(3)).unwrap_err();
    assert_eq!(error.error, Error::Invalid("native PES input not drained"));
    assert_eq!(error.value.source_offset, 564);
    assert_eq!(budget.used(), before);
}

#[test]
fn unproven_source_epoch_context_is_visible_and_recovers_input() {
    let budget = Budget::new(16384).unwrap();
    let packets = input(
        include_bytes!("data/cc-one-duplicate.ts"),
        PacketFormat::Ts188,
        &budget,
    );
    let mut observer = observer(&budget);
    let mut changed = context(2);
    changed.epochs.clock = 1;
    let error = observer.push(packets[2].clone(), changed).unwrap_err();
    assert_eq!(
        error.error,
        Error::Unsupported("native PES source context transition unobserved")
    );
    assert_eq!(error.value.source_offset, 376);
}

#[test]
fn unproven_entity_transition_does_not_masquerade_as_clock_reset_or_eof() {
    let budget = Budget::new(16384).unwrap();
    let mut packets = input(
        include_bytes!("data/cc-one-duplicate.ts"),
        PacketFormat::Ts188,
        &budget,
    );
    packets[2].source_id = Some(SourceId(2));
    let mut observer = observer(&budget);
    let error = observer.push(packets[2].clone(), context(2)).unwrap_err();
    assert_eq!(
        error.error,
        Error::Unsupported("native PES source context transition unobserved")
    );
    assert_eq!(error.value.source_id, Some(SourceId(2)));
}

#[test]
fn unproven_stream_profile_is_an_explicit_nonpass() {
    let budget = Budget::new(16384).unwrap();
    let mut unproven = scope();
    unproven.stream_type = 0x24;
    assert!(matches!(
        NativePesObserver::new(
            NativeClockProfile::ArchivedPesV1,
            unproven,
            budget,
            ParseLimits::default()
        ),
        Err(Error::Unsupported("native PES stream context unobserved"))
    ));
}

#[test]
fn native_profile_keeps_exact_original_reference_contract() {
    let budget = Budget::new(4096).unwrap();
    let observer = observer(&budget);
    let profile = observer.profile();
    assert_eq!(
        profile.worker_sha256(),
        "19c06e53c62852b409eec9771cb11d9d7f5164b4618545a449d7e993f99346df"
    );
    assert_eq!(
        profile.source_pin(),
        "d32b387f2b0a484599d4587d651891f0c63c4238"
    );
    assert_eq!(profile.flags(), 48);
    assert!(profile.default_overflow_correction());
    assert!(!profile.find_stream_info());
    assert_eq!(observer.scope(), scope());
    assert_eq!(
        observer.limits().pes_bytes,
        ParseLimits::default().pes_bytes
    );
}

// Isolated access-path correction for this original invalid fixture. No packet
// gold is changed: source rejection and native encoded-header evidence are distinct.
fn check_rejected_original(source: &[u8]) {
    use triergon_mpegts::native_pes::NativeRegisteredCarriage;
    use triergon_mpegts::native_recovery::{
        NativePesObservation, NativePesRecoveryObserver, NativeRejectedPolicy,
    };
    use triergon_mpegts::{TransportInspection, TransportInspector};
    let budget = Budget::new(1 << 20).unwrap();
    let owner = Bytes::copy_from_slice(&budget, source).unwrap();
    let mut observer = NativePesRecoveryObserver::new(
        NativeClockProfile::ArchivedPesV1,
        scope(),
        NativeRegisteredCarriage::Avc,
        NativeRejectedPolicy::OpcrWithoutPcrV1,
        budget.clone(),
        ParseLimits::default(),
    )
    .unwrap();
    let mut clock = NativePesClockV1::new(1).unwrap();
    let mut count = 0;
    for (index, offset) in [376_usize, 564].into_iter().enumerate() {
        let mut inspect = TransportInspector::new(
            owner.slice(offset..offset + 188).unwrap(),
            PacketFormat::Ts188,
            triergon_core::SourceLocation {
                source: SourceId(1),
                offset: offset as u64,
            },
            &budget,
        )
        .unwrap();
        let rejection = loop {
            match inspect
                .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                AssemblyStep::Yield => {}
                AssemblyStep::Output(TransportInspection::Rejected(rejection)) => break rejection,
                other => panic!("strict OPCR rejection required, got {other:?}"),
            }
        };
        assert_eq!(rejection.error(), Error::Invalid("OPCR without PCR"));
        assert!(rejection.backing_bytes().shares_storage_with(&owner));
        let ctx = NativeSourceContext {
            source_event_ordinal: 8 + index as u64,
            epochs: EPOCHS,
            byte_run_epoch: None,
        };
        observer.push_rejected(rejection, ctx).unwrap();
        let mut emitted = false;
        loop {
            match observer
                .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                AssemblyStep::Yield => {}
                AssemblyStep::Output(NativePesObservation::FromRejectedCarrier(packet)) => {
                    assert!(!emitted);
                    emitted = true;
                    count += 1;
                    let raw = &source[offset..offset + 188];
                    let start = 5 + usize::from(raw[4]);
                    assert_eq!(packet.profile, NativeClockProfile::ArchivedPesV1);
                    assert_eq!((packet.program, packet.pid), (1, 256));
                    assert_eq!(packet.source_epochs, EPOCHS);
                    assert_eq!(packet.source_origin.source, SourceId(1));
                    assert_eq!(packet.source_origin.offset, offset as u64);
                    assert_eq!(packet.native_position, offset as i64);
                    assert_eq!(
                        (
                            packet.native_flags,
                            packet.native_duration90_raw,
                            packet.native_stream_id
                        ),
                        (0, 0, 0xe0)
                    );
                    assert!(
                        packet
                            .payload()
                            .flatten()
                            .copied()
                            .eq(raw[start + 14..].iter().copied())
                    );
                    assert_eq!(packet.payload_len, raw.len() - start - 14);
                    assert_eq!(packet.encoded_header.as_bytes(), &raw[start..start + 14]);
                    assert_eq!(packet.encoded_header.encoded_pts_raw33(), 90000);
                    assert_eq!(packet.encoded_header.encoded_dts_raw33(), None);
                    assert_eq!(
                        packet.header_source_ranges.as_slice(),
                        &[triergon_mpegts::SourceRange {
                            entity: SourceId(1),
                            offset: (offset + start) as u64,
                            bytes: 14
                        }]
                    );
                    let spans = packet.spans.as_slice();
                    assert_eq!(spans.len(), 1);
                    assert_eq!(
                        spans[0].source,
                        triergon_mpegts::SourceRange {
                            entity: SourceId(1),
                            offset: (offset + start + 14) as u64,
                            bytes: raw.len() - start - 14
                        }
                    );
                    assert_eq!(spans[0].transport.offset, offset as u64);
                    assert_eq!(spans[0].transport.bytes, 188);
                    assert_eq!(spans[0].context, ctx);
                    let time = clock
                        .project_encoded_header(
                            packet.pid,
                            &packet.encoded_header,
                            &mut WorkBudget::new(1),
                            &AtomicBool::new(false),
                        )
                        .unwrap()
                        .unwrap();
                    assert_eq!((time.pts90, time.dts90), (90000, 90000));
                    assert_eq!(
                        (time.encoded_pts_raw33, time.encoded_dts_raw33),
                        (90000, None)
                    );
                }
                AssemblyStep::NeedPayload => {
                    assert!(emitted);
                    break;
                }
                other => panic!("original rejected-carrier native gold, got {other:?}"),
            }
        }
    }
    observer.end_of_source().unwrap();
    assert!(matches!(
        observer.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Ok(AssemblyStep::Finished)
    ));
    assert_eq!(count, 2);
    assert_eq!(owner.as_ref(), source);
}
