//! New public indexed output controls. Baseline has no such API: run this exact
//! test source against frozen Source8 before applying the production overlay.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, TimeBase, WorkBudget};
use triergon_subtitles::pgs::{
    DisplayConfig, DisplayDecoder, DisplayLimits, DisplaySet, DisplayStatus, Envelope,
    IndexedDisplayDecoder, IndexedDisplaySet, IndexedNativeRecoveryDecoder, IndexedRecoveredPacket,
    NativeArgbView, NativeColourOrigin, NativeRecoveryDecoder, PacketClock, RecoveredPacket,
    RecoveryStatus,
};

fn clock() -> PacketClock {
    PacketClock {
        pts: None,
        dts: Some(-19),
        timebase: TimeBase::new(1, 90_000).unwrap(),
    }
}

fn segment(packet: &mut Vec<u8>, kind: u8, payload: &[u8]) {
    packet.push(kind);
    packet.extend_from_slice(&u16::try_from(payload.len()).unwrap().to_be_bytes());
    packet.extend_from_slice(payload);
}

fn packet(indices: &[u8], full_palette: bool, epoch: bool) -> Vec<u8> {
    let width = u16::try_from(indices.len()).unwrap();
    let [hi, lo] = width.to_be_bytes();
    let mut packet = Vec::new();
    segment(
        &mut packet,
        0x16,
        &[
            hi,
            lo,
            0,
            1,
            0x10,
            0,
            1,
            if epoch { 0x80 } else { 0 },
            0,
            0,
            1,
            0,
            7,
            0,
            0,
            0,
            0,
            0,
            0,
        ],
    );
    let mut palette = vec![0, 0];
    if full_palette {
        for i in 0..=255u8 {
            palette.extend_from_slice(&[
                i,
                i.wrapping_mul(13),
                i.wrapping_mul(17),
                i.wrapping_mul(23),
                i ^ 0x95,
            ]);
        }
    }
    segment(&mut packet, 0x14, &palette);
    let mut encoded = Vec::new();
    if indices.len() > 1024 && indices.iter().all(|i| *i == indices[0]) {
        encoded.extend_from_slice(&[0, 0xc0 | (hi & 0x3f), lo, indices[0]]);
    } else {
        for index in indices {
            if *index == 0 {
                encoded.extend_from_slice(&[0, 0x81, 0]);
            } else {
                encoded.push(*index);
            }
        }
    }
    encoded.extend_from_slice(&[0, 0]);
    let size = u32::try_from(encoded.len() + 4).unwrap().to_be_bytes();
    let mut object = vec![0, 7, 0, 0xc0, size[1], size[2], size[3], hi, lo, 0, 1];
    object.extend_from_slice(&encoded);
    segment(&mut packet, 0x15, &object);
    segment(&mut packet, 0x80, &[]);
    packet
}

fn indexed(budget: &Budget, raw: &[u8], config: DisplayConfig, quantum: u64) -> IndexedDisplaySet {
    let cancel = AtomicBool::new(false);
    let mut decoder = IndexedDisplayDecoder::new(budget, DisplayLimits::default(), config).unwrap();
    decoder
        .push(
            Bytes::copy_from_slice(budget, raw).unwrap(),
            clock(),
            Envelope::Packet,
            &cancel,
        )
        .unwrap();
    for _ in 0..1_000_000 {
        if decoder
            .poll(&mut WorkBudget::new(quantum), &cancel)
            .unwrap()
            == DisplayStatus::OutputReady
        {
            return decoder.take_output(&cancel).unwrap();
        }
    }
    panic!("indexed decoder progress bound");
}

fn materialized(budget: &Budget, raw: &[u8], config: DisplayConfig) -> DisplaySet {
    let cancel = AtomicBool::new(false);
    let mut decoder = DisplayDecoder::new(budget, DisplayLimits::default(), config).unwrap();
    decoder
        .push(
            Bytes::copy_from_slice(budget, raw).unwrap(),
            clock(),
            Envelope::Packet,
            &cancel,
        )
        .unwrap();
    for _ in 0..1_000_000 {
        if decoder.poll(&mut WorkBudget::new(4096), &cancel).unwrap() == DisplayStatus::OutputReady
        {
            return decoder.take_output(&cancel).unwrap();
        }
    }
    panic!("materialized decoder progress bound");
}

fn all_bytes(view: NativeArgbView, quantum: u64, target_length: usize) -> Vec<u8> {
    let length = view.len();
    let mut writer = view.writer(length);
    let mut output = Vec::new();
    for _ in 0..1_000_000 {
        let mut target = vec![0xa7; target_length];
        let mut work = WorkBudget::new(quantum);
        let step = writer
            .write(&mut target, &mut work, &AtomicBool::new(false))
            .unwrap();
        assert!(step.written <= 256);
        assert_eq!(work.spent(), step.written as u64);
        assert!(target[step.written..].iter().all(|byte| *byte == 0xa7));
        output.extend_from_slice(&target[..step.written]);
        if step.complete {
            assert_eq!(output.len(), length);
            assert_eq!(writer.work_spent(), length);
            return output;
        }
        assert!(step.written > 0);
    }
    panic!("writer progress bound");
}

#[test]
fn all_source_facts_and_materialized_argb_remain_literal() {
    let raw = packet(&(0..=255).collect::<Vec<_>>(), true, true);
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let old = materialized(&budget, &raw, DisplayConfig::default());
    let new = indexed(&budget, &raw, DisplayConfig::default(), 1);
    assert_eq!(new.generation, old.generation);
    assert_eq!(new.epoch, old.epoch);
    assert_eq!(new.presentation, old.presentation);
    assert_eq!(new.end_stamp, old.end_stamp);
    assert_eq!(new.native_pts_microseconds, old.native_pts_microseconds);
    assert_eq!(new.native_time_error, old.native_time_error);
    assert_eq!(new.native_clock_source, old.native_clock_source);
    assert_eq!(new.source_end, old.source_end);
    assert_eq!(new.objects().len(), old.objects().len());
    assert_eq!(new.segments().len(), old.segments().len());
    for (new, old) in new.segments().iter().zip(old.segments()) {
        assert_eq!(new.kind, old.kind);
        assert_eq!(new.stamp, old.stamp);
        assert_eq!(new.raw.as_ref(), old.raw.as_ref());
    }
    for (new, old) in new.objects().iter().zip(old.objects()) {
        assert_eq!(new.composition, old.composition);
        assert_eq!(new.window, old.window);
        assert_eq!(new.object_version, old.object_version);
        assert_eq!(new.object_width, old.object_width);
        assert_eq!(new.object_height, old.object_height);
        assert_eq!(new.object_first_stamp, old.object_first_stamp);
        assert_eq!(new.object_last_stamp, old.object_last_stamp);
        assert_eq!(new.visible, old.visible);
        assert_eq!(new.visibility_window_known, old.visibility_window_known);
        assert_eq!(new.native_x, old.native_x);
        assert_eq!(new.native_y, old.native_y);
        assert_eq!(
            new.native_forced_only_suppressed,
            old.native_forced_only_suppressed
        );
        assert_eq!(new.pixels().as_ref(), old.pixels().as_ref());
        assert_eq!(new.visible_pixels().as_ref(), old.visible_pixels().as_ref());
        for i in 0..=255 {
            assert_eq!(new.palette().source(i), old.palette().source(i));
            assert_eq!(new.palette().native_argb(i), old.palette().native_argb(i));
            assert_eq!(
                new.palette().native_origin(i),
                old.palette().native_origin(i)
            );
        }
        assert_eq!(
            all_bytes(new.native_argb_view().unwrap(), 7, 257),
            old.native_argb_pixels().as_ref()
        );
    }
}

#[test]
fn every_index_channel_and_partial_pixel_matches_old_plane_at_all_quanta() {
    let raw = packet(&(0..=255).rev().collect::<Vec<_>>(), true, true);
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let old = materialized(&budget, &raw, DisplayConfig::default());
    let new = indexed(&budget, &raw, DisplayConfig::default(), 3);
    for quantum in [1, 2, 3, 4, 7, 63, 64, 65, 255, 256, 4096] {
        for target_length in [1, 3, 7, 256, 321] {
            assert_eq!(
                all_bytes(
                    new.objects()[0].native_argb_view().unwrap(),
                    quantum,
                    target_length
                ),
                old.objects()[0].native_argb_pixels().as_ref()
            );
        }
    }
}

#[test]
fn run16383_owned_indexed_output_fits_below_old_argb_plane_alone() {
    let budget = Budget::new(4 * 16383 - 1).unwrap();
    let baseline = budget.used();
    let new = indexed(
        &budget,
        &packet(&vec![177; 16383], true, true),
        DisplayConfig::default(),
        4096,
    );
    assert_eq!(new.objects()[0].pixels().len(), 16383);
    let view = new.objects()[0].native_argb_view().unwrap();
    assert_eq!(view.len(), 65532);
    let word = new.objects()[0].native_argb(177).to_be_bytes();
    let mut writer = view.writer(65532);
    let mut count = 0;
    for _ in 0..1000 {
        let mut target = [0xa5; 256];
        let step = writer
            .write(
                &mut target,
                &mut WorkBudget::new(255),
                &AtomicBool::new(false),
            )
            .unwrap();
        for (at, byte) in target[..step.written].iter().enumerate() {
            assert_eq!(*byte, word[(count + at) % 4]);
        }
        count += step.written;
        if step.complete {
            break;
        }
    }
    assert_eq!(count, 65532);
    drop(writer);
    drop(new);
    assert_eq!(budget.used(), baseline);
}

#[test]
fn zero_work_and_empty_target_do_not_mutate_or_allocate() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let new = indexed(
        &budget,
        &packet(&[1, 2, 3], true, true),
        DisplayConfig::default(),
        1,
    );
    let used = budget.used();
    let mut writer = new.objects()[0].native_argb_view().unwrap().writer(12);
    let mut target = [0xa5; 20];
    for _ in 0..4 {
        let step = writer
            .write(
                &mut target,
                &mut WorkBudget::new(0),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(step.written, 0);
        assert!(!step.complete);
        assert_eq!(target, [0xa5; 20]);
        assert_eq!(writer.offset(), 0);
        assert_eq!(writer.work_spent(), 0);
        assert_eq!(budget.used(), used);
    }
    assert_eq!(
        writer
            .write(&mut [], &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            .written,
        0
    );
    assert_eq!(writer.offset(), 0);
    assert_eq!(budget.used(), used);
}

#[test]
fn one_work_unit_writes_one_byte_without_hidden_prefill() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let new = indexed(
        &budget,
        &packet(&[1, 2], true, true),
        DisplayConfig::default(),
        1,
    );
    let expected = all_bytes(new.objects()[0].native_argb_view().unwrap(), 4096, 256);
    let mut writer = new.objects()[0].native_argb_view().unwrap().writer(8);
    for byte in expected {
        let mut target = [0xf9; 4];
        let step = writer
            .write(
                &mut target,
                &mut WorkBudget::new(1),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(step.written, 1);
        assert_eq!(target, [byte, 0xf9, 0xf9, 0xf9]);
    }
    assert_eq!(writer.offset(), 8);
    assert!(
        writer
            .write(
                &mut [0; 1],
                &mut WorkBudget::new(0),
                &AtomicBool::new(false)
            )
            .unwrap()
            .complete
    );
}

#[test]
fn cumulative_work_limit_is_exact_sticky_and_precedes_writes() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let new = indexed(
        &budget,
        &packet(&[1, 2], true, true),
        DisplayConfig::default(),
        1,
    );
    let mut writer = new.objects()[0].native_argb_view().unwrap().writer(3);
    assert_eq!(
        writer
            .write(
                &mut [0; 8],
                &mut WorkBudget::new(8),
                &AtomicBool::new(false)
            )
            .unwrap()
            .written,
        3
    );
    let mut target = [0xa5; 4];
    let error = Error::Limit {
        requested: 4,
        maximum: 3,
    };
    for quantum in [1, 0, 4096] {
        assert_eq!(
            writer.write(
                &mut target,
                &mut WorkBudget::new(quantum),
                &AtomicBool::new(false)
            ),
            Err(error)
        );
        assert_eq!(target, [0xa5; 4]);
        assert_eq!(writer.offset(), 3);
        assert_eq!(writer.work_spent(), 3);
    }
}

#[test]
fn cancellation_is_sticky_even_at_zero_work_and_releases_its_owners() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = budget.used();
    let new = indexed(
        &budget,
        &packet(&[1, 2], true, true),
        DisplayConfig::default(),
        1,
    );
    let mut writer = new.objects()[0].native_argb_view().unwrap().writer(8);
    drop(new);
    assert!(budget.used() > baseline);
    let cancel = AtomicBool::new(true);
    let mut target = [0xa5; 4];
    assert_eq!(
        writer.write(&mut target, &mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    );
    assert_eq!(budget.used(), baseline);
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        writer.write(&mut target, &mut WorkBudget::new(4096), &cancel),
        Err(Error::Cancelled)
    );
    assert_eq!(target, [0xa5; 4]);
}

#[test]
fn retained_view_survives_output_drop_and_sibling_cancel() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = budget.used();
    let new = indexed(
        &budget,
        &packet(&[1, 2], true, true),
        DisplayConfig::default(),
        1,
    );
    let view = new.objects()[0].native_argb_view().unwrap();
    let expected = all_bytes(view.clone(), 1, 3);
    let mut cancelled = view.clone().writer(8);
    drop(new);
    assert_eq!(
        cancelled.write(&mut [0; 1], &mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    );
    assert_eq!(all_bytes(view.clone(), 7, 7), expected);
    assert!(budget.used() > baseline);
    drop(cancelled);
    drop(view);
    assert_eq!(budget.used(), baseline);
}

#[test]
fn forced_receiver_zero_preserves_all_literal_source_colours() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let config = DisplayConfig {
        native_forced_only: true,
    };
    let new = indexed(&budget, &packet(&[1, 2], true, true), config, 1);
    let object = &new.objects()[0];
    assert!(object.native_forced_only_suppressed);
    assert!(object.palette().source(1).is_some());
    assert_ne!(object.palette().native_argb(1), 0);
    assert_eq!(object.pixels().as_ref(), [1, 2]);
    assert_eq!(all_bytes(object.native_argb_view().unwrap(), 1, 7), [0; 8]);
}

#[test]
fn foreign_packet_rejection_keeps_the_exact_caller_owner() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let foreign = Budget::new(4 * 1024 * 1024).unwrap();
    let raw = Bytes::copy_from_slice(&foreign, &packet(&[1, 2], true, true)).unwrap();
    let address = raw.as_ptr();
    let mut decoder =
        IndexedDisplayDecoder::new(&budget, DisplayLimits::default(), DisplayConfig::default())
            .unwrap();
    let error = decoder
        .push(raw, clock(), Envelope::Packet, &AtomicBool::new(false))
        .unwrap_err();
    assert_eq!(error.error, Error::ForeignPool);
    assert_eq!(error.packet.as_ptr(), address);
    assert_eq!(error.clock, clock());
}

#[test]
fn decoder_pressure_retry_and_zero_work_keep_real_ownership_and_work() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let mut decoder =
        IndexedDisplayDecoder::new(&budget, DisplayLimits::default(), DisplayConfig::default())
            .unwrap();
    decoder
        .push(
            Bytes::copy_from_slice(&budget, &packet(&[1, 2], true, true)).unwrap(),
            clock(),
            Envelope::Packet,
            &AtomicBool::new(false),
        )
        .unwrap();
    let pressure = budget.reserve(budget.available()).unwrap();
    assert!(matches!(
        decoder.poll(&mut WorkBudget::new(100), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    let used = budget.used();
    let spent = decoder.work_spent();
    for _ in 0..3 {
        assert_eq!(
            decoder
                .poll(&mut WorkBudget::new(0), &AtomicBool::new(false))
                .unwrap(),
            DisplayStatus::Yield
        );
        assert_eq!(budget.used(), used);
        assert_eq!(decoder.work_spent(), spent);
    }
    drop(pressure);
    for _ in 0..100_000 {
        if decoder
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            == DisplayStatus::OutputReady
        {
            let out = decoder.take_output(&AtomicBool::new(false)).unwrap();
            assert_eq!(out.objects()[0].pixels().as_ref(), [1, 2]);
            return;
        }
    }
    panic!("pressure retry progress bound");
}

fn try_recover_indexed(
    decoder: &mut IndexedNativeRecoveryDecoder,
    budget: &Budget,
    raw: &[u8],
) -> Result<IndexedRecoveredPacket, Error> {
    let cancel = AtomicBool::new(false);
    let mut owner = Some(Bytes::copy_from_slice(budget, raw).unwrap());
    decoder.push(&mut owner, clock(), &cancel)?;
    for _ in 0..1_000_000 {
        if decoder.poll(&mut WorkBudget::new(1), &cancel)? == RecoveryStatus::OutputReady {
            return decoder.take_output(&cancel);
        }
    }
    panic!("indexed receiver progress bound");
}

fn recover_default(budget: &Budget, raw: &[u8]) -> Result<RecoveredPacket, Error> {
    let cancel = AtomicBool::new(false);
    let mut decoder =
        NativeRecoveryDecoder::new(budget, DisplayLimits::default(), DisplayConfig::default())
            .unwrap();
    let mut owner = Some(Bytes::copy_from_slice(budget, raw).unwrap());
    decoder.push(&mut owner, clock(), &cancel)?;
    for _ in 0..1_000_000 {
        if decoder.poll(&mut WorkBudget::new(4096), &cancel)? == RecoveryStatus::OutputReady {
            return decoder.take_output(&cancel);
        }
    }
    panic!("default receiver progress bound");
}

fn recover_indexed(
    decoder: &mut IndexedNativeRecoveryDecoder,
    budget: &Budget,
    raw: &[u8],
) -> IndexedRecoveredPacket {
    try_recover_indexed(decoder, budget, raw).unwrap()
}

#[test]
fn receiver_full_events_and_source_failures_are_unchanged_on_negative_grammar() {
    let mut good = packet(&[1, 2], true, true);
    let mut missing_end = good.clone();
    missing_end.truncate(missing_end.len() - 3);
    let mut unknown = good.clone();
    segment(&mut unknown, 0x99, &[1, 2, 3]);
    let mut tail = good.clone();
    tail.extend_from_slice(&[0x14, 0, 7, 0, 0, 1]);
    good.extend_from_slice(&[0x80, 0]);
    let mut complete = 0;
    let mut refusals = 0;
    for raw in [missing_end, unknown, tail, good] {
        let budget = Budget::new(4 * 1024 * 1024).unwrap();
        let old = recover_default(&budget, &raw);
        let mut decoder = IndexedNativeRecoveryDecoder::new(
            &budget,
            DisplayLimits::default(),
            DisplayConfig::default(),
        )
        .unwrap();
        let new = try_recover_indexed(&mut decoder, &budget, &raw);
        let (new, old) = match (new, old) {
            (Ok(new), Ok(old)) => (new, old),
            (Err(new), Err(old)) => {
                assert_eq!(new, old, "literal pre-output refusal differs");
                refusals += 1;
                continue;
            }
            (new, old) => {
                panic!("whole operation return differs: indexed={new:?} materialized={old:?}")
            }
        };
        complete += 1;
        assert_eq!(new.raw_packet().as_ref(), old.raw_packet().as_ref());
        assert_eq!(new.clock, old.clock);
        assert_eq!(new.source_error(), old.source_error());
        assert_eq!(new.source_sets().len(), old.source_sets().len());
        for (new_set, old_set) in new.source_sets().iter().zip(old.source_sets()) {
            assert_eq!(new_set.generation, old_set.generation);
            assert_eq!(new_set.epoch, old_set.epoch);
            assert_eq!(new_set.presentation, old_set.presentation);
            assert_eq!(new_set.end_stamp, old_set.end_stamp);
            assert_eq!(
                new_set.native_pts_microseconds,
                old_set.native_pts_microseconds
            );
            assert_eq!(new_set.native_time_error, old_set.native_time_error);
            assert_eq!(new_set.native_clock_source, old_set.native_clock_source);
            assert_eq!(new_set.source_end, old_set.source_end);
            assert_eq!(new_set.segments().len(), old_set.segments().len());
            for (new, old) in new_set.segments().iter().zip(old_set.segments()) {
                assert_eq!(new.kind, old.kind);
                assert_eq!(new.stamp, old.stamp);
                assert_eq!(new.raw.as_ref(), old.raw.as_ref());
            }
            assert_eq!(new_set.objects().len(), old_set.objects().len());
            for (new, old) in new_set.objects().iter().zip(old_set.objects()) {
                assert_eq!(new.composition, old.composition);
                assert_eq!(new.window, old.window);
                assert_eq!(
                    (new.object_version, new.object_width, new.object_height),
                    (old.object_version, old.object_width, old.object_height)
                );
                assert_eq!(new.object_first_stamp, old.object_first_stamp);
                assert_eq!(new.object_last_stamp, old.object_last_stamp);
                assert_eq!(new.visible, old.visible);
                assert_eq!(new.visibility_window_known, old.visibility_window_known);
                assert_eq!(new.native_x, old.native_x);
                assert_eq!(new.native_y, old.native_y);
                assert_eq!(
                    new.native_forced_only_suppressed,
                    old.native_forced_only_suppressed
                );
                assert_eq!(new.pixels().as_ref(), old.pixels().as_ref());
                assert_eq!(new.visible_pixels().as_ref(), old.visible_pixels().as_ref());
                for i in 0..=255 {
                    assert_eq!(new.palette().source(i), old.palette().source(i));
                    assert_eq!(
                        new.palette().native_origin(i),
                        old.palette().native_origin(i)
                    );
                    assert_eq!(new.palette().native_argb(i), old.palette().native_argb(i));
                }
                assert_eq!(
                    all_bytes(new.native_argb_view().unwrap(), 1, 7),
                    old.native_argb_pixels().as_ref()
                );
            }
        }
        assert_eq!(new.receiver_observations, old.receiver_observations);
        assert_eq!(new.context_canvas, old.context_canvas);
        assert_eq!(new.receiver_sets().len(), old.receiver_sets().len());
        assert_eq!(new.omitted_receiver_events(), old.omitted_receiver_events());
        for (new, old) in new.receiver_sets().iter().zip(old.receiver_sets()) {
            assert_eq!(new.pts_microseconds, old.pts_microseconds);
            assert_eq!(new.rectangles().len(), old.rectangles().len());
            for (new, old) in new.rectangles().iter().zip(old.rectangles()) {
                assert_eq!(new.reference, old.reference);
                assert_eq!(
                    (new.x, new.y, new.width, new.height, new.stride, new.forced),
                    (old.x, old.y, old.width, old.height, old.stride, old.forced)
                );
                assert_eq!(new.pixels().as_ref(), old.pixels().as_ref());
                assert_eq!(new.palette(), old.palette());
            }
        }
    }
    assert_eq!(
        complete + refusals,
        4,
        "all original malformed transports remain required"
    );
    assert!(
        complete > 0 && refusals > 0,
        "both full output and literal refusal controls must execute"
    );
}

#[test]
fn receiver_epoch_carry_never_promotes_missing_source_colour() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let mut decoder = IndexedNativeRecoveryDecoder::new(
        &budget,
        DisplayLimits::default(),
        DisplayConfig::default(),
    )
    .unwrap();
    let old = recover_indexed(&mut decoder, &budget, &packet(&[1, 2], true, true));
    let old_view = old.source_sets()[0].objects()[0]
        .native_argb_view()
        .unwrap();
    let fresh = recover_indexed(&mut decoder, &budget, &packet(&[1, 2], false, true));
    let object = &fresh.source_sets()[0].objects()[0];
    assert_eq!(object.palette().source(1), None);
    assert_eq!(
        object.palette().native_origin(1),
        NativeColourOrigin::PreviousEpochSlot
    );
    assert_eq!(
        all_bytes(object.native_argb_view().unwrap(), 3, 7),
        all_bytes(old_view, 1, 3)
    );
    assert_eq!(object.object_first_stamp.packet_sequence, 1);
    assert_eq!(
        old.source_sets()[0].objects()[0]
            .object_first_stamp
            .packet_sequence,
        0
    );
}

#[test]
fn retained_indexed_source_generations_survive_cancel_reset() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = IndexedNativeRecoveryDecoder::new(
        &budget,
        DisplayLimits::default(),
        DisplayConfig::default(),
    )
    .unwrap();
    let old = recover_indexed(&mut decoder, &budget, &packet(&[1, 2], true, true));
    let expected = all_bytes(
        old.source_sets()[0].objects()[0]
            .native_argb_view()
            .unwrap(),
        1,
        7,
    );
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        decoder.poll(&mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    );
    cancel.store(false, Ordering::Relaxed);
    decoder.reset(&cancel).unwrap();
    let fresh = recover_indexed(&mut decoder, &budget, &packet(&[3, 4], true, true));
    assert_eq!(
        fresh.source_sets()[0].generation,
        old.source_sets()[0].generation + 1
    );
    assert_eq!(
        all_bytes(
            old.source_sets()[0].objects()[0]
                .native_argb_view()
                .unwrap(),
            3,
            7
        ),
        expected
    );
    assert_eq!(
        fresh.source_sets()[0].objects()[0].pixels().as_ref(),
        [3, 4]
    );
}

#[test]
fn operation_writer_charges_decoder_and_caller_exactly_once_per_byte() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let mut decoder = IndexedNativeRecoveryDecoder::new(
        &budget,
        DisplayLimits::default(),
        DisplayConfig::default(),
    )
    .unwrap();
    let out = recover_indexed(&mut decoder, &budget, &packet(&[1, 2], true, true));
    let mut writer = out.source_sets()[0].objects()[0]
        .native_argb_view()
        .unwrap()
        .writer(8);
    let before = decoder.work_spent();
    let used = budget.used();
    for offset in 0..8 {
        let mut work = WorkBudget::new(1);
        let mut target = [0xa5; 4];
        let step = decoder
            .write_source_argb(&mut writer, &mut target, &mut work, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(step.written, 1);
        assert_eq!(work.spent(), 1);
        assert_eq!(writer.work_spent(), offset + 1);
        assert_eq!(decoder.work_spent(), before + offset + 1);
        assert_eq!(&target[1..], &[0xa5; 3]);
        assert_eq!(budget.used(), used);
    }
    let step = decoder
        .write_source_argb(
            &mut writer,
            &mut [0xa5; 4],
            &mut WorkBudget::new(0),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert!(step.complete);
    assert_eq!(step.written, 0);
    assert_eq!(decoder.work_spent(), before + 8);
}

#[test]
fn full_operation_work_limit_includes_source_argb_and_stops_before_next_write() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let raw = packet(&[1, 2], true, true);
    let mut calibration = IndexedNativeRecoveryDecoder::new(
        &budget,
        DisplayLimits::default(),
        DisplayConfig::default(),
    )
    .unwrap();
    let first = recover_indexed(&mut calibration, &budget, &raw);
    let parse_work = calibration.work_spent();
    drop(first);
    drop(calibration);
    let mut decoder = IndexedNativeRecoveryDecoder::new(
        &budget,
        DisplayLimits {
            max_work: parse_work + 3,
            ..DisplayLimits::default()
        },
        DisplayConfig::default(),
    )
    .unwrap();
    let out = recover_indexed(&mut decoder, &budget, &raw);
    assert_eq!(decoder.work_spent(), parse_work);
    let mut writer = out.source_sets()[0].objects()[0]
        .native_argb_view()
        .unwrap()
        .writer(8);
    let mut work = WorkBudget::new(8);
    assert_eq!(
        decoder
            .write_source_argb(&mut writer, &mut [0; 8], &mut work, &AtomicBool::new(false))
            .unwrap()
            .written,
        3
    );
    assert_eq!(work.spent(), 3);
    let spent = parse_work + 3;
    assert_eq!(decoder.work_spent(), spent);
    let error = Error::Limit {
        requested: spent + 1,
        maximum: spent,
    };
    let mut target = [0xa5; 4];
    assert_eq!(
        decoder.write_source_argb(
            &mut writer,
            &mut target,
            &mut WorkBudget::new(1),
            &AtomicBool::new(false)
        ),
        Err(error)
    );
    assert_eq!(target, [0xa5; 4]);
    assert_eq!(writer.offset(), 3);
    assert_eq!(
        decoder.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Err(error)
    );
    assert_eq!(out.source_sets()[0].objects()[0].pixels().as_ref(), &[1, 2]);
}

#[test]
fn foreign_operation_writer_is_refused_without_consuming_target_or_work() {
    let own = Budget::new(4 * 1024 * 1024).unwrap();
    let foreign = Budget::new(4 * 1024 * 1024).unwrap();
    let out = indexed(
        &foreign,
        &packet(&[1, 2], true, true),
        DisplayConfig::default(),
        1,
    );
    let mut writer = out.objects()[0].native_argb_view().unwrap().writer(8);
    let mut decoder =
        IndexedNativeRecoveryDecoder::new(&own, DisplayLimits::default(), DisplayConfig::default())
            .unwrap();
    let mut target = [0xa5; 8];
    let mut work = WorkBudget::new(8);
    assert_eq!(
        decoder.write_source_argb(&mut writer, &mut target, &mut work, &AtomicBool::new(false)),
        Err(Error::ForeignPool)
    );
    assert_eq!(target, [0xa5; 8]);
    assert_eq!(work.spent(), 0);
    assert_eq!(writer.offset(), 0);
    assert_eq!(decoder.work_spent(), 0);
    assert_eq!(
        writer
            .write(&mut target, &mut work, &AtomicBool::new(false))
            .unwrap()
            .written,
        8
    );
}
