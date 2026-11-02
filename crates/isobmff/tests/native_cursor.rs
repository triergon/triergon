mod common;
use common::child;
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, Error, TimeBase, WorkBudget};
use triergon_isobmff::{
    Mov9CursorStatus, Mov9PacketCursor, Mov9PacketLimits, Mov9Profile, Mov9Track, SampleTable,
    TrackHeader,
};

type Table = (u32, TimeBase, SampleTable);

fn parse(budget: &Budget, bytes: &[u8]) -> (Vec<Table>, Vec<Range<u64>>) {
    let roots = common::roots(budget, bytes).unwrap();
    let media = roots
        .iter()
        .filter(|root| root.header().kind == *b"mdat")
        .map(|root| root.header().payload_range().unwrap())
        .collect();
    let movie = roots
        .iter()
        .find(|root| root.header().kind == *b"moov")
        .unwrap();
    let tables = common::children(movie, 0)
        .unwrap()
        .iter()
        .filter(|track| track.header().kind == *b"trak")
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
            (header.track_id, header.time_base().unwrap(), table)
        })
        .collect();
    (tables, media)
}
fn tracks(tables: &[Table]) -> Vec<Mov9Track<'_>> {
    tables
        .iter()
        .map(|(id, base, table)| Mov9Track {
            track_id: *id,
            table,
            fragments: &[],
            media_base: *base,
            edits: None,
        })
        .collect()
}

#[test]
fn native_cursor_exact_global_sequence_survives_tiny_work_and_held_ready() {
    let budget = Budget::new(2 * 1024 * 1024).unwrap();
    let (tables, media) = parse(&budget, include_bytes!("fixtures/r-multitrack.mov"));
    let sources = tracks(&tables);
    let baseline = budget.used();
    let raw: Vec<_> = tables
        .iter()
        .map(|(_, _, table)| table.raw().bytes().as_ref().to_vec())
        .collect();
    let expected = [
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
    for quantum in [1, 7] {
        let mut cursor = Mov9PacketCursor::new(
            &budget,
            &sources,
            &media,
            Mov9PacketLimits::default(),
            Mov9Profile::PinnedNoParseNoFillin,
        )
        .unwrap();
        assert_eq!(
            cursor
                .poll(&mut WorkBudget::new(0), &AtomicBool::new(false))
                .unwrap(),
            Mov9CursorStatus::Yield
        );
        let mut actual = Vec::new();
        let mut ended = false;
        for _ in 0..100_000 {
            match cursor
                .poll(&mut WorkBudget::new(quantum), &AtomicBool::new(false))
                .unwrap()
            {
                Mov9CursorStatus::Yield => {}
                Mov9CursorStatus::End => {
                    ended = true;
                    break;
                }
                Mov9CursorStatus::Ready => {
                    let held = budget.used();
                    assert_eq!(
                        cursor
                            .poll(&mut WorkBudget::new(0), &AtomicBool::new(false))
                            .unwrap(),
                        Mov9CursorStatus::Ready
                    );
                    assert_eq!(budget.used(), held);
                    let occurrence = cursor.take_occurrence().unwrap();
                    assert_eq!(occurrence.source.range, occurrence.native.source_range);
                    assert_eq!(
                        occurrence.source.dts_ticks,
                        Some(i128::from(occurrence.native.dts.unwrap()))
                    );
                    assert!(occurrence.edit.is_none());
                    actual.push((occurrence.track_id, occurrence.source.ordinal));
                }
            }
        }
        assert!(ended, "bounded progress");
        assert_eq!(actual, expected);
        assert_eq!(cursor.validated_samples(), 12);
        drop(cursor);
        assert_eq!(budget.used(), baseline, "all slot/head storage released");
    }
    for ((_, _, table), original) in tables.iter().zip(raw) {
        assert_eq!(table.raw().bytes().as_ref(), original);
    }
}

#[test]
fn native_cursor_clock_context_preserves_unsigned_source_and_wire_run_distinctions() {
    for (bytes, shift, source_offset, version) in [
        (
            include_bytes!("fixtures/native-followup/follow-clock-uniform-one.mov").as_slice(),
            0i64,
            -300i128,
            1u8,
        ),
        (
            include_bytes!("fixtures/native-followup/follow-clock-uniform-three.mov").as_slice(),
            -300,
            -300,
            1,
        ),
        (
            include_bytes!("fixtures/native-followup/follow-clock-v0-high-uniform-three.mov")
                .as_slice(),
            -2147483616,
            2147483680,
            0,
        ),
    ] {
        let budget = Budget::new(2 * 1024 * 1024).unwrap();
        let (tables, media) = parse(&budget, bytes);
        let sources = tracks(&tables);
        let mut cursor = Mov9PacketCursor::new(
            &budget,
            &sources,
            &media,
            Mov9PacketLimits::default(),
            Mov9Profile::PinnedNoParseNoFillin,
        )
        .unwrap();
        let mut generated = 0;
        let mut ended = false;
        for _ in 0..100_000 {
            match cursor
                .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                Mov9CursorStatus::Yield => {}
                Mov9CursorStatus::End => {
                    ended = true;
                    break;
                }
                Mov9CursorStatus::Ready => {
                    let occurrence = cursor.take_occurrence().unwrap();
                    let source_dts = i128::from(generated) * 1000;
                    assert_eq!(occurrence.source.dts_ticks, Some(source_dts));
                    assert_eq!(
                        occurrence.source.cts_ticks,
                        Some(source_dts + source_offset)
                    );
                    let offset = occurrence.source.composition_offset.unwrap();
                    assert_eq!(offset.version(), version);
                    assert_eq!(offset.ticks(), source_offset);
                    assert_eq!(
                        occurrence.native.dts,
                        Some(i64::try_from(source_dts).unwrap() + shift)
                    );
                    generated += 1;
                }
            }
        }
        assert!(ended);
        assert_eq!(generated, 6);
    }
}

#[test]
fn native_cursor_cancellation_and_budget_failure_release_all_owned_storage() {
    let budget = Budget::new(2 * 1024 * 1024).unwrap();
    let (tables, media) = parse(&budget, include_bytes!("fixtures/r-front-stco.mov"));
    let sources = tracks(&tables);
    let baseline = budget.used();
    let held = budget.reserve(budget.available()).unwrap();
    assert!(matches!(
        Mov9PacketCursor::new(
            &budget,
            &sources,
            &media,
            Mov9PacketLimits::default(),
            Mov9Profile::PinnedNoParseNoFillin
        ),
        Err(Error::Backpressure { .. })
    ));
    drop(held);
    assert_eq!(budget.used(), baseline);
    let mut cursor = Mov9PacketCursor::new(
        &budget,
        &sources,
        &media,
        Mov9PacketLimits::default(),
        Mov9Profile::PinnedNoParseNoFillin,
    )
    .unwrap();
    let cancel = AtomicBool::new(true);
    assert_eq!(
        cursor.poll(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    );
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        cursor.poll(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    );
    assert!(matches!(cursor.take_occurrence(), Err(Error::Cancelled)));
    drop(cursor);
    assert_eq!(budget.used(), baseline);
}

#[test]
fn native_cursor_bounds_and_source_occurrence_limits_fail_before_any_packet() {
    let budget = Budget::new(2 * 1024 * 1024).unwrap();
    let (tables, media) = parse(
        &budget,
        include_bytes!("fixtures/bad-chunk-offset-outside-source.mov"),
    );
    let sources = tracks(&tables);
    assert!(matches!(
        Mov9PacketCursor::new(
            &budget,
            &sources,
            &media,
            Mov9PacketLimits {
                max_source_samples: 1,
                ..Mov9PacketLimits::default()
            },
            Mov9Profile::PinnedNoParseNoFillin
        ),
        Err(Error::Limit { .. })
    ));
    let mut cursor = Mov9PacketCursor::new(
        &budget,
        &sources,
        &media,
        Mov9PacketLimits {
            max_occurrences: 0,
            ..Mov9PacketLimits::default()
        },
        Mov9Profile::PinnedNoParseNoFillin,
    )
    .unwrap();
    let mut failed = false;
    for _ in 0..100_000 {
        match cursor.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(Mov9CursorStatus::Yield) => {}
            Err(error @ Error::Invalid(_)) => {
                assert_eq!(
                    cursor.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
                    Err(error)
                );
                assert!(matches!(cursor.take_occurrence(), Err(Error::Invalid(_))));
                failed = true;
                break;
            }
            other => panic!("outside-MDAT must precede native policy/packet output: {other:?}"),
        }
    }
    assert!(failed);
    drop(cursor);
    let (valid_tables, valid_media) = parse(&budget, include_bytes!("fixtures/r-front-stco.mov"));
    let valid_sources = tracks(&valid_tables);
    let mut cursor = Mov9PacketCursor::new(
        &budget,
        &valid_sources,
        &valid_media,
        Mov9PacketLimits {
            max_occurrences: 1,
            ..Mov9PacketLimits::default()
        },
        Mov9Profile::PinnedNoParseNoFillin,
    )
    .unwrap();
    let mut emitted = 0;
    let mut limited = false;
    for _ in 0..100_000 {
        match cursor.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(Mov9CursorStatus::Yield) => {}
            Ok(Mov9CursorStatus::Ready) => {
                cursor.take_occurrence().unwrap();
                emitted += 1;
            }
            Err(Error::Limit { .. }) => {
                limited = true;
                break;
            }
            other => panic!("occurrence limit must be explicit: {other:?}"),
        }
    }
    assert!(limited);
    assert_eq!(emitted, 1);
}

#[test]
fn native_cursor_keeps_sparse_co64_source_addresses_above_four_gibibytes() {
    let budget = Budget::new(2 * 1024 * 1024).unwrap();
    let mut bytes = include_bytes!("fixtures/r-end-co64.mov").to_vec();
    let roots = common::roots(&budget, &bytes).unwrap();
    let movie = roots
        .iter()
        .find(|root| root.header().kind == *b"moov")
        .unwrap();
    let table = child(
        &child(
            &child(&child(movie, b"trak").unwrap(), b"mdia").unwrap(),
            b"minf",
        )
        .unwrap(),
        b"stbl",
    )
    .unwrap();
    let offsets = child(&table, b"co64").unwrap();
    let count = u32::from_be_bytes(offsets.payload()[4..8].try_into().unwrap());
    let start = usize::try_from(offsets.header().start).unwrap()
        + usize::from(offsets.header().header_size)
        + 8;
    let shift = 1u64 << 34;
    for row in 0..usize::try_from(count).unwrap() {
        let at = start + row * 8;
        let value = u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap());
        bytes[at..at + 8].copy_from_slice(&(value + shift).to_be_bytes());
    }
    // Metadata-only sparse source model; no huge file allocation or payload I/O.
    let (tables, media) = parse(&budget, &bytes);
    let media: Vec<_> = media
        .into_iter()
        .map(|range| range.start + shift..range.end + shift)
        .collect();
    let sources = tracks(&tables);
    let mut cursor = Mov9PacketCursor::new(
        &budget,
        &sources,
        &media,
        Mov9PacketLimits::default(),
        Mov9Profile::PinnedNoParseNoFillin,
    )
    .unwrap();
    let mut samples = 0;
    let mut ended = false;
    for _ in 0..100_000 {
        match cursor
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Mov9CursorStatus::Yield => {}
            Mov9CursorStatus::End => {
                ended = true;
                break;
            }
            Mov9CursorStatus::Ready => {
                let occurrence = cursor.take_occurrence().unwrap();
                assert!(occurrence.source.range.start >= shift);
                assert_eq!(occurrence.native.source_range, occurrence.source.range);
                samples += 1;
            }
        }
    }
    assert!(ended);
    assert_eq!(samples, 6);
}
