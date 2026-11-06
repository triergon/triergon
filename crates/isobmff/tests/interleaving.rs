mod common;
use common::{child, fixtures::CASES};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, Error, InputCursor, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_isobmff::{
    BoxView, BuildStatus, FragmentBuilder, FragmentContext, FragmentIndex, ImplicitBase,
    InterleaveStatus, Limits, PhysicalInterleaveLimits, PhysicalSampleCursor, SampleReader,
    SampleStep, SampleTable, SourceTrack, TrackHeader, TrackSampleCursor, Trex,
};

fn regular_tables(budget: &Budget, name: &str) -> Vec<(u32, SampleTable)> {
    let case = CASES.iter().find(|c| c.name == name).unwrap();
    let roots = common::roots(budget, case.bytes).unwrap();
    let movie = roots.iter().find(|b| b.header().kind == *b"moov").unwrap();
    common::children(movie, 0)
        .unwrap()
        .iter()
        .filter(|b| b.header().kind == *b"trak")
        .map(|track| {
            let mdia = child(track, b"mdia").unwrap();
            let header = TrackHeader::parse(
                &child(track, b"tkhd").unwrap(),
                &child(&mdia, b"mdhd").unwrap(),
                &child(&mdia, b"hdlr").unwrap(),
            )
            .unwrap();
            let table = common::table(
                child(&child(&mdia, b"minf").unwrap(), b"stbl").unwrap(),
                budget,
                1,
            )
            .unwrap();
            (header.track_id, table)
        })
        .collect()
}

// Original Adobe v0 fragment construction, independent of production field readers.
// Two one-sample runs allow sparse, equal and backward source addresses.
fn fragment(budget: &Budget, id: u32, offsets: &[i32], size: u32) -> FragmentIndex {
    fragment_from_ordinal(budget, id, offsets, size, 1)
}
fn fragment_from_ordinal(
    budget: &Budget,
    id: u32,
    offsets: &[i32],
    size: u32,
    first: u64,
) -> FragmentIndex {
    let mut children = Vec::new();
    children.extend_from_slice(&16u32.to_be_bytes());
    children.extend_from_slice(b"tfhd");
    children.extend_from_slice(&0u32.to_be_bytes());
    children.extend_from_slice(&id.to_be_bytes());
    for offset in offsets {
        children.extend_from_slice(&20u32.to_be_bytes());
        children.extend_from_slice(b"trun");
        children.extend_from_slice(&1u32.to_be_bytes());
        children.extend_from_slice(&1u32.to_be_bytes());
        children.extend_from_slice(&offset.to_be_bytes());
    }
    let mut bytes = u32::try_from(children.len() + 8)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    bytes.extend_from_slice(b"traf");
    bytes.extend_from_slice(&children);
    let mut builder = FragmentBuilder::new(
        BoxView::new(common::bytes(budget, &bytes), 0).unwrap(),
        Trex {
            track_id: id,
            description_index: 1,
            duration: 2,
            size,
            flags: 0x02000000,
        },
        FragmentContext {
            moof_start: 1u64 << 32,
            implicit_base: ImplicitBase::FirstTraf,
            decode_origin: None,
            first_ordinal: first,
            description_count: 1,
        },
        budget.clone(),
        Limits::default(),
    )
    .unwrap();
    while builder.poll(&mut WorkBudget::new(1)).unwrap() != BuildStatus::Ready {}
    builder.finish().unwrap()
}

#[test]
fn physical_regular_interleaving_matches_independent_source_ranges_in_both_schedules() {
    let case = CASES.iter().find(|c| c.name == "r-multitrack").unwrap();
    let mut expected: Vec<_> = case.samples.iter().collect();
    // Only the independent source expectation is ordered by physical byte address.
    // Native comparison never sorts either actual operation's packet stream.
    expected.sort_by_key(|s| s.offset);
    for quantum in [1, 4096] {
        let budget = Budget::new(1024 * 1024).unwrap();
        let tables = regular_tables(&budget, case.name);
        let tracks: Vec<_> = tables
            .iter()
            .map(|(id, t)| SourceTrack::new(*id, Some(t), &[]).unwrap())
            .collect();
        let retained = budget.used();
        let mut cursor =
            PhysicalSampleCursor::new(&budget, &tracks, PhysicalInterleaveLimits::default())
                .unwrap();
        let fixed_storage = budget.used();
        assert!(fixed_storage > retained);
        let cancel = AtomicBool::new(false);
        let mut actual = Vec::new();
        for _ in 0..50_000 {
            match cursor.poll(&mut WorkBudget::new(quantum), &cancel).unwrap() {
                InterleaveStatus::Yield => {}
                InterleaveStatus::End => break,
                InterleaveStatus::Ready => {
                    // A waiting output cannot consume the next source sample.
                    assert_eq!(
                        cursor.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
                        InterleaveStatus::Ready
                    );
                    let next = cursor.take_sample().unwrap();
                    let source = expected[actual.len()];
                    assert_eq!(
                        (next.track_id, next.sample.ordinal),
                        (source.track, source.ordinal)
                    );
                    assert_eq!(
                        next.sample.range,
                        source.offset..source.offset + source.payload.len() as u64
                    );
                    assert_eq!(next.sample.dts_ticks, Some(source.dts));
                    assert_eq!(next.sample.cts_ticks, Some(source.cts));
                    assert_eq!(next.sample.description_index, source.description);
                    actual.push(next.track_id);
                }
            }
            assert_eq!(budget.used(), fixed_storage);
        }
        assert_eq!(actual, [1, 1, 2, 2, 1, 2, 1, 1, 1, 2, 2, 2]);
        assert_eq!(cursor.validated_samples(), 12);
        drop(cursor);
        assert_eq!(budget.used(), retained);
    }
}

#[test]
fn pinned_default_native_order_remains_distinct_from_source_physical_order() {
    // Exact transcription of the already captured r-multitrack native operation.
    // Input SHA256 221d72b05e73706c99dc97228b15aa4aa7c8761350f642a1e9776d97df7c378c.
    // Pinned worker SHA256 d97ce08126efe876cf92dfaac3655073da87fd8c590793a7d37ffd9e1fbea2d2.
    // This is a disagreement-preservation regression, not a native-parity pass.
    let native = [
        (1, 1),
        (1, 2),
        (2, 1),
        (2, 2),
        (1, 3),
        (2, 3),
        (1, 4),
        (1, 5),
        (2, 4),
        (1, 6),
        (2, 5),
        (2, 6),
    ];
    let budget = Budget::new(1024 * 1024).unwrap();
    let tables = regular_tables(&budget, "r-multitrack");
    let tracks: Vec<_> = tables
        .iter()
        .map(|(id, t)| SourceTrack::new(*id, Some(t), &[]).unwrap())
        .collect();
    let mut cursor =
        PhysicalSampleCursor::new(&budget, &tracks, PhysicalInterleaveLimits::default()).unwrap();
    let mut physical = Vec::new();
    for _ in 0..10_000 {
        match cursor
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            InterleaveStatus::Yield => {}
            InterleaveStatus::Ready => {
                let next = cursor.take_sample().unwrap();
                physical.push((next.track_id, next.sample.ordinal));
            }
            InterleaveStatus::End => break,
        }
    }
    assert_eq!(physical.len(), native.len());
    assert_eq!(&physical[..8], &native[..8]);
    assert_eq!(physical[8], (1, 6));
    assert_eq!(native[8], (2, 4));
    assert_ne!(physical, native);
    for id in [1, 2] {
        let left: Vec<_> = physical.iter().filter(|(track, _)| *track == id).collect();
        let right: Vec<_> = native.iter().filter(|(track, _)| *track == id).collect();
        assert_eq!(left, right);
    }
}

#[test]
fn sparse_legacy_tracks_merge_without_narrowing_or_inventing_missing_clocks() {
    let budget = Budget::new(32 * 1024).unwrap();
    let fragments = [
        fragment(&budget, 7, &[200, 400], 4),
        fragment(&budget, 3, &[100, 300], 4),
    ];
    let tracks = [
        SourceTrack::new(7, None, &fragments).unwrap(),
        SourceTrack::new(3, None, &fragments).unwrap(),
    ];
    let mut cursor =
        PhysicalSampleCursor::new(&budget, &tracks, PhysicalInterleaveLimits::default()).unwrap();
    let mut actual = Vec::new();
    for _ in 0..1000 {
        match cursor
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            InterleaveStatus::Ready => {
                let next = cursor.take_sample().unwrap();
                assert_eq!(next.sample.dts_ticks, None);
                assert_eq!(next.sample.cts_ticks, None);
                assert_eq!(next.sample.range.end - next.sample.range.start, 4);
                actual.push((next.track_id, next.sample.range.start));
            }
            InterleaveStatus::Yield => {}
            InterleaveStatus::End => break,
        }
    }
    assert_eq!(
        actual,
        [
            (3, (1u64 << 32) + 100),
            (7, (1u64 << 32) + 200),
            (3, (1u64 << 32) + 300),
            (7, (1u64 << 32) + 400)
        ]
    );
}

#[test]
fn regular_samples_then_legacy_fragments_keep_source_identity_and_unknown_fragment_origin() {
    let budget = Budget::new(1024 * 1024).unwrap();
    let tables = regular_tables(&budget, "r-front-stco");
    let fragments = [fragment_from_ordinal(&budget, 1, &[100, 200], 4, 7)];
    let source = SourceTrack::new(1, Some(&tables[0].1), &fragments).unwrap();
    let mut cursor =
        PhysicalSampleCursor::new(&budget, &[source], PhysicalInterleaveLimits::default()).unwrap();
    let mut samples = Vec::new();
    for _ in 0..2000 {
        match cursor
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            InterleaveStatus::Ready => samples.push(cursor.take_sample().unwrap()),
            InterleaveStatus::Yield => {}
            InterleaveStatus::End => break,
        }
    }
    assert_eq!(samples.len(), 8);
    for (index, item) in samples.iter().enumerate() {
        assert_eq!(item.track_id, 1);
        assert_eq!(item.sample.ordinal, index as u64 + 1);
        assert_eq!(item.sample.description_index, 1);
        if index < 6 {
            assert_eq!(item.sample.dts_ticks, Some(index as i128 * 1000));
            assert_eq!(item.sample.fragment_flags, None);
        } else {
            assert!(item.sample.range.start > u64::from(u32::MAX));
            assert_eq!(item.sample.dts_ticks, None);
            assert_eq!(item.sample.cts_ticks, None);
            assert_eq!(item.sample.fragment_flags, Some(0x02000000));
        }
    }
}

#[test]
fn backward_track_offsets_fail_before_any_physical_output_and_remain_source_accessible() {
    let budget = Budget::new(16 * 1024).unwrap();
    let fragments = [fragment(&budget, 1, &[200, 100], 4)];
    let source = SourceTrack::new(1, None, &fragments).unwrap();
    let mut cursor =
        PhysicalSampleCursor::new(&budget, &[source], PhysicalInterleaveLimits::default()).unwrap();
    let mut failure = None;
    for _ in 0..1000 {
        match cursor.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(InterleaveStatus::Yield) => {}
            result => {
                failure = Some(result.unwrap_err());
                break;
            }
        }
    }
    let error = failure.expect("bounded rejection before a physical output");
    assert!(matches!(error, Error::Unsupported(_)));
    assert!(matches!(cursor.take_sample(), Err(Error::Unsupported(_))));
    assert_eq!(
        cursor.poll(&mut WorkBudget::new(100), &AtomicBool::new(false)),
        Err(error)
    );
    let mut original = TrackSampleCursor::new(source);
    let mut addresses = Vec::new();
    for _ in 0..100 {
        match original.poll(&mut WorkBudget::new(1)).unwrap() {
            SampleStep::Sample(sample) => addresses.push(sample.range.start),
            SampleStep::Yield => {}
            SampleStep::End => break,
        }
    }
    assert_eq!(addresses, [(1u64 << 32) + 200, (1u64 << 32) + 100]);
}

#[test]
fn equal_addresses_and_zero_sizes_preserve_declared_track_order() {
    let budget = Budget::new(16 * 1024).unwrap();
    let fragments = [
        fragment(&budget, 9, &[100], 0),
        fragment(&budget, 1, &[100], 0),
    ];
    let tracks = [
        SourceTrack::new(9, None, &fragments).unwrap(),
        SourceTrack::new(1, None, &fragments).unwrap(),
    ];
    let mut cursor =
        PhysicalSampleCursor::new(&budget, &tracks, PhysicalInterleaveLimits::default()).unwrap();
    let mut ids = Vec::new();
    for _ in 0..1000 {
        match cursor
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            InterleaveStatus::Ready => {
                let next = cursor.take_sample().unwrap();
                assert!(next.sample.range.is_empty());
                ids.push(next.track_id);
            }
            InterleaveStatus::Yield => {}
            InterleaveStatus::End => break,
        }
    }
    assert_eq!(ids, [9, 1]);
}

#[test]
fn track_and_sample_limits_reject_without_output_or_leaking_charges() {
    let budget = Budget::new(32 * 1024).unwrap();
    let fragments = [fragment(&budget, 1, &[100, 200], 4)];
    let source = SourceTrack::new(1, None, &fragments).unwrap();
    assert!(SourceTrack::new(0, None, &fragments).is_err());
    let baseline = budget.used();
    assert!(matches!(
        PhysicalSampleCursor::new(
            &budget,
            &[source, source],
            PhysicalInterleaveLimits::default()
        ),
        Err(Error::Invalid(_))
    ));
    assert_eq!(budget.used(), baseline);
    let limits = PhysicalInterleaveLimits {
        max_tracks: 0,
        max_samples: 1,
    };
    assert!(matches!(
        PhysicalSampleCursor::new(&budget, &[source], limits),
        Err(Error::Limit { .. })
    ));
    assert_eq!(budget.used(), baseline);
    let mut cursor = PhysicalSampleCursor::new(
        &budget,
        &[source],
        PhysicalInterleaveLimits {
            max_tracks: 1,
            max_samples: 1,
        },
    )
    .unwrap();
    let mut failure = None;
    for _ in 0..1000 {
        match cursor.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(InterleaveStatus::Yield) => {}
            other => {
                failure = Some(other.unwrap_err());
                break;
            }
        }
    }
    let error = failure.expect("cumulative limit must reject in bounded work");
    assert_eq!(
        error,
        Error::Limit {
            requested: 2,
            maximum: 1
        }
    );
    assert_eq!(cursor.take_sample(), Err(error));
    drop(cursor);
    assert_eq!(budget.used(), baseline);
}

#[test]
fn zero_work_and_cancellation_do_not_advance_or_release_waiting_output() {
    let budget = Budget::new(16 * 1024).unwrap();
    let fragments = [fragment(&budget, 1, &[100, 200], 4)];
    let source = SourceTrack::new(1, None, &fragments).unwrap();
    let mut cursor =
        PhysicalSampleCursor::new(&budget, &[source], PhysicalInterleaveLimits::default()).unwrap();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        cursor.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
        InterleaveStatus::Yield
    );
    assert_eq!(cursor.validated_samples(), 0);
    let mut ready = false;
    for _ in 0..1000 {
        if cursor.poll(&mut WorkBudget::new(1), &cancel).unwrap() == InterleaveStatus::Ready {
            ready = true;
            break;
        }
    }
    assert!(ready);
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        cursor.poll(&mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    );
    assert_eq!(cursor.take_sample(), Err(Error::Cancelled));
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        cursor.poll(&mut WorkBudget::new(100), &cancel),
        Err(Error::Cancelled)
    );
}

#[test]
fn interleaved_source_samples_drive_real_core_range_reads_under_tiny_windows() {
    let case = CASES.iter().find(|c| c.name == "r-multitrack").unwrap();
    let budget = Budget::new(1024 * 1024).unwrap();
    let tables = regular_tables(&budget, case.name);
    let tracks: Vec<_> = tables
        .iter()
        .map(|(id, t)| SourceTrack::new(*id, Some(t), &[]).unwrap())
        .collect();
    let mut cursor =
        PhysicalSampleCursor::new(&budget, &tracks, PhysicalInterleaveLimits::default()).unwrap();
    let roots = common::roots(&budget, case.bytes).unwrap();
    let mdat = roots.iter().find(|b| b.header().kind == *b"mdat").unwrap();
    let extent = mdat.header().start + u64::from(mdat.header().header_size)
        ..mdat.header().end().unwrap().unwrap();
    let mut input = InputCursor::new(
        &budget,
        SourceId(82),
        SourceCapabilities {
            length: Some(case.bytes.len() as u64),
            random_access: true,
        },
        1,
    )
    .unwrap();
    let generation = input.generation();
    let cancel = AtomicBool::new(false);
    let mut count = 0;
    for _ in 0..50_000 {
        match cursor.poll(&mut WorkBudget::new(1), &cancel).unwrap() {
            InterleaveStatus::Yield => {}
            InterleaveStatus::End => break,
            InterleaveStatus::Ready => {
                let item = cursor.take_sample().unwrap();
                let source = case
                    .samples
                    .iter()
                    .find(|s| s.track == item.track_id && s.ordinal == item.sample.ordinal)
                    .unwrap();
                let mut reader =
                    SampleReader::new(&mut input, item.sample.clone(), extent.clone(), 1024)
                        .unwrap();
                loop {
                    match reader
                        .poll(&mut input, &mut WorkBudget::new(1), &cancel)
                        .unwrap()
                    {
                        Step::Ready(packet) => {
                            assert_eq!(packet.sample, item.sample);
                            assert_eq!(packet.payload.as_ref(), source.payload);
                            assert_eq!(packet.source, SourceId(82));
                            assert_eq!(packet.token.generation(), generation);
                            count += 1;
                            break;
                        }
                        Step::NeedInput(request) => {
                            let offset = usize::try_from(request.offset).unwrap();
                            let bytes = common::bytes(&budget, &case.bytes[offset..offset + 1]);
                            input
                                .provide(
                                    &InputWindow::new(
                                        request.source,
                                        request.token,
                                        request.offset,
                                        bytes,
                                    )
                                    .unwrap(),
                                )
                                .unwrap();
                        }
                        Step::Yield => {}
                        Step::End(_) => panic!("sample read is not logical EOF"),
                    }
                }
            }
        }
    }
    assert_eq!(count, 12);
}

#[test]
fn empty_track_set_finishes_without_allocating_sample_storage() {
    let budget = Budget::new(4096).unwrap();
    let baseline = budget.used();
    let mut cursor =
        PhysicalSampleCursor::new(&budget, &[], PhysicalInterleaveLimits::default()).unwrap();
    assert_eq!(
        cursor
            .poll(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        InterleaveStatus::End
    );
    assert_eq!(budget.used(), baseline);
    assert!(matches!(cursor.take_sample(), Err(Error::Invalid(_))));
}

fn poll_until_physical_decision(
    cursor: &mut PhysicalSampleCursor<'_>,
    quantum: u64,
) -> Result<InterleaveStatus, Error> {
    for _ in 0..20_000 {
        let mut work = WorkBudget::new(quantum);
        let result = cursor.poll(&mut work, &AtomicBool::new(false));
        assert!(work.spent() <= quantum);
        if result != Ok(InterleaveStatus::Yield) {
            return result;
        }
    }
    panic!("bounded physical preflight failed to make progress");
}

#[test]
fn checked_media_rejects_original_outside_mdat_before_layout_policy() {
    let case = CASES
        .iter()
        .find(|c| c.name == "bad-chunk-offset-outside-source")
        .unwrap();
    for quantum in [1, 4096] {
        let budget = Budget::new(1024 * 1024).unwrap();
        let tables = regular_tables(&budget, case.name);
        let roots = common::roots(&budget, case.bytes).unwrap();
        let media: Vec<_> = roots
            .iter()
            .filter(|b| b.header().kind == *b"mdat")
            .map(|b| b.header().payload_range().unwrap())
            .collect();
        let tracks: Vec<_> = tables
            .iter()
            .map(|(id, table)| SourceTrack::new(*id, Some(table), &[]).unwrap())
            .collect();
        let mut cursor = PhysicalSampleCursor::new_with_media_extents(
            &budget,
            &tracks,
            &media,
            PhysicalInterleaveLimits::default(),
        )
        .unwrap();
        let error = poll_until_physical_decision(&mut cursor, quantum).unwrap_err();
        assert!(matches!(error, Error::Invalid(_)));
        assert_eq!(cursor.take_sample(), Err(error));
        assert_eq!(
            cursor.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
            Err(error)
        );
    }
}

#[test]
fn checked_media_validates_later_tracks_before_rejecting_valid_backward_layout() {
    let budget = Budget::new(32 * 1024).unwrap();
    let base = 1u64 << 32;
    let extent = base + 90..base + 300;
    let media = std::slice::from_ref(&extent);
    let fragments = [
        fragment(&budget, 1, &[200, 100], 4),
        fragment(&budget, 2, &[500], 4),
    ];
    let tracks = [
        SourceTrack::new(1, None, &fragments).unwrap(),
        SourceTrack::new(2, None, &fragments).unwrap(),
    ];
    for quantum in [1, 4096] {
        let mut checked = PhysicalSampleCursor::new_with_media_extents(
            &budget,
            &tracks,
            media,
            PhysicalInterleaveLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            poll_until_physical_decision(&mut checked, quantum),
            Err(Error::Invalid(_))
        ));
        assert_eq!(checked.validated_samples(), 3);
        let mut metadata =
            PhysicalSampleCursor::new(&budget, &tracks, PhysicalInterleaveLimits::default())
                .unwrap();
        assert!(matches!(
            poll_until_physical_decision(&mut metadata, quantum),
            Err(Error::Unsupported(_))
        ));
        assert_eq!(metadata.validated_samples(), 2);
        let valid_tracks = [tracks[0]];
        let mut backward = PhysicalSampleCursor::new_with_media_extents(
            &budget,
            &valid_tracks,
            media,
            PhysicalInterleaveLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            poll_until_physical_decision(&mut backward, quantum),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(backward.take_sample(), Err(Error::Unsupported(_))));
    }
}

#[test]
fn checked_media_sparse_ranges_reject_holes_and_cross_extent_samples() {
    let budget = Budget::new(64 * 1024).unwrap();
    let base = 1u64 << 32;
    let media = [base + 100..base + 104, base + 200..base + 204];
    for offsets in [&[100, 150][..], &[100, 102][..]] {
        let fragments = [fragment(&budget, 1, offsets, 4)];
        let tracks = [SourceTrack::new(1, None, &fragments).unwrap()];
        let mut cursor = PhysicalSampleCursor::new_with_media_extents(
            &budget,
            &tracks,
            &media,
            PhysicalInterleaveLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            poll_until_physical_decision(&mut cursor, 1),
            Err(Error::Invalid(_))
        ));
    }
    let fragments = [fragment(&budget, 1, &[100, 200], 4)];
    let tracks = [SourceTrack::new(1, None, &fragments).unwrap()];
    let mut cursor = PhysicalSampleCursor::new_with_media_extents(
        &budget,
        &tracks,
        &media,
        PhysicalInterleaveLimits::default(),
    )
    .unwrap();
    for offset in [100, 200] {
        assert_eq!(
            poll_until_physical_decision(&mut cursor, 1).unwrap(),
            InterleaveStatus::Ready
        );
        let item = cursor.take_sample().unwrap();
        assert_eq!(item.sample.range, base + offset..base + offset + 4);
        assert_eq!(item.sample.dts_ticks, None);
    }
    assert_eq!(
        poll_until_physical_decision(&mut cursor, 1).unwrap(),
        InterleaveStatus::End
    );
}

#[test]
fn checked_media_rejects_invalid_extent_arrays_before_any_samples() {
    let budget = Budget::new(16 * 1024).unwrap();
    let base = 1u64 << 32;
    let invalid = std::ops::Range {
        start: base + 200,
        end: base + 100,
    };
    for media in [
        vec![invalid],
        vec![base + 200..base + 204, base + 100..base + 104],
        vec![base + 100..base + 200, base + 150..base + 204],
    ] {
        let mut cursor = PhysicalSampleCursor::new_with_media_extents(
            &budget,
            &[],
            &media,
            PhysicalInterleaveLimits::default(),
        )
        .unwrap();
        let error = poll_until_physical_decision(&mut cursor, 1).unwrap_err();
        assert!(matches!(error, Error::Invalid(_)));
        assert_eq!(cursor.validated_samples(), 0);
        assert_eq!(cursor.take_sample(), Err(error));
    }
}

#[test]
fn checked_media_work_yields_cancellation_and_borrowed_storage_are_bounded() {
    let budget = Budget::new(64 * 1024).unwrap();
    let base = 1u64 << 32;
    let fragments = [fragment(&budget, 1, &[100], 4)];
    let tracks = [SourceTrack::new(1, None, &fragments).unwrap()];
    let media: Vec<_> = (0u64..64)
        .map(|i| base + i * 10..base + i * 10 + 4)
        .collect();
    let baseline = budget.used();
    let mut cursor = PhysicalSampleCursor::new_with_media_extents(
        &budget,
        &tracks,
        &media,
        PhysicalInterleaveLimits::default(),
    )
    .unwrap();
    let storage = budget.used();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        cursor.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
        InterleaveStatus::Yield
    );
    for _ in 0..64 {
        assert_eq!(
            cursor.poll(&mut WorkBudget::new(1), &cancel).unwrap(),
            InterleaveStatus::Yield
        );
        assert_eq!(cursor.validated_samples(), 0);
        assert_eq!(budget.used(), storage);
    }
    // Enter the source pass and pause after reading its first metadata sample.
    for _ in 0..100 {
        assert_eq!(
            cursor.poll(&mut WorkBudget::new(1), &cancel).unwrap(),
            InterleaveStatus::Yield
        );
        if cursor.validated_samples() == 1 {
            break;
        }
    }
    assert_eq!(cursor.validated_samples(), 1);
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        cursor.poll(&mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    );
    assert_eq!(cursor.take_sample(), Err(Error::Cancelled));
    drop(cursor);
    assert_eq!(budget.used(), baseline);
    let mut retry = PhysicalSampleCursor::new_with_media_extents(
        &budget,
        &tracks,
        &media,
        PhysicalInterleaveLimits::default(),
    )
    .unwrap();
    let fixed = budget.used();
    assert_eq!(
        poll_until_physical_decision(&mut retry, 1).unwrap(),
        InterleaveStatus::Ready
    );
    assert_eq!(budget.used(), fixed);
    assert_eq!(
        retry
            .poll(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        InterleaveStatus::Ready
    );
    assert_eq!(
        retry.take_sample().unwrap().sample.range,
        base + 100..base + 104
    );
    assert_eq!(
        poll_until_physical_decision(&mut retry, 1).unwrap(),
        InterleaveStatus::End
    );
    assert_eq!(budget.used(), fixed);
    drop(retry);
    assert_eq!(budget.used(), baseline);
}

#[test]
fn checked_media_empty_sources_and_zero_byte_endpoints_are_explicit() {
    let budget = Budget::new(16 * 1024).unwrap();
    let base = 1u64 << 32;
    let mut empty = PhysicalSampleCursor::new_with_media_extents(
        &budget,
        &[],
        &[],
        PhysicalInterleaveLimits::default(),
    )
    .unwrap();
    assert_eq!(
        empty
            .poll(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        InterleaveStatus::End
    );
    let fragments = [fragment(&budget, 1, &[104], 0)];
    let tracks = [SourceTrack::new(1, None, &fragments).unwrap()];
    let mut missing = PhysicalSampleCursor::new_with_media_extents(
        &budget,
        &tracks,
        &[],
        PhysicalInterleaveLimits::default(),
    )
    .unwrap();
    assert!(matches!(
        poll_until_physical_decision(&mut missing, 1),
        Err(Error::Invalid(_))
    ));
    for extent in [base + 100..base + 104, base + 104..base + 104] {
        let media = std::slice::from_ref(&extent);
        let mut cursor = PhysicalSampleCursor::new_with_media_extents(
            &budget,
            &tracks,
            media,
            PhysicalInterleaveLimits::default(),
        )
        .unwrap();
        assert_eq!(
            poll_until_physical_decision(&mut cursor, 1).unwrap(),
            InterleaveStatus::Ready
        );
        assert_eq!(
            cursor.take_sample().unwrap().sample.range,
            base + 104..base + 104
        );
        assert_eq!(
            poll_until_physical_decision(&mut cursor, 1).unwrap(),
            InterleaveStatus::End
        );
    }
}
