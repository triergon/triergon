mod common;

use common::{child, fixtures::CASES};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error, TimeBase, WorkBudget};
use triergon_isobmff::{
    BoxView, BuildStatus, EditList, Limits, MovieHeader, Sample, SampleCursor, SampleStep,
    TrackHeader,
    compat::mov::{
        edits::{EditCursor, EditLimits},
        packets::{CursorStatus, DemuxProfile, DemuxTrack, PacketCursor, ResumePoint},
        seek::GlobalSeekJob,
    },
};

fn word_box(kind: &[u8; 4], rows: &[u32], version: u8) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&u32::try_from(16 + 4 * rows.len()).unwrap().to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(&[version, 0, 0, 0]);
    out.extend_from_slice(&u32::try_from(rows.len()).unwrap().to_be_bytes());
    for row in rows {
        out.extend_from_slice(&row.to_be_bytes());
    }
    out
}

// The unchanged six-sample authored fixture supplies descriptions, sizes, time
// and chunk tables. Only the independently authored stss/stps rows below differ.
fn authored_stbl(
    budget: &Budget,
    sync: Option<&[u32]>,
    partial: Option<&[u32]>,
    partial_version: u8,
) -> BoxView {
    let roots = common::roots(budget, include_bytes!("fixtures/r-front-stco.mov")).unwrap();
    let movie = roots
        .iter()
        .find(|root| root.header().kind == *b"moov")
        .unwrap();
    let track = child(movie, b"trak").unwrap();
    let media = child(&track, b"mdia").unwrap();
    let original = child(&child(&media, b"minf").unwrap(), b"stbl").unwrap();
    let mut body = Vec::new();
    for part in common::children(&original, 0).unwrap() {
        if part.header().kind != *b"stss" && part.header().kind != *b"stps" {
            body.extend_from_slice(part.bytes().as_ref());
        }
    }
    if let Some(rows) = sync {
        body.extend_from_slice(&word_box(b"stss", rows, 0));
    }
    if let Some(rows) = partial {
        body.extend_from_slice(&word_box(b"stps", rows, partial_version));
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&u32::try_from(body.len() + 8).unwrap().to_be_bytes());
    bytes.extend_from_slice(b"stbl");
    bytes.extend_from_slice(&body);
    BoxView::new(common::bytes(budget, &bytes), original.header().start).unwrap()
}

fn next(cursor: &mut SampleCursor<'_>) -> Option<Sample> {
    let mut work = WorkBudget::new(1);
    match cursor.poll(&mut work).unwrap() {
        SampleStep::Sample(sample) => {
            assert_eq!(work.spent(), 1, "one existing sample unit");
            Some(sample)
        }
        SampleStep::End => {
            assert_eq!(work.spent(), 0, "End does not consume a sample unit");
            None
        }
        SampleStep::Yield => panic!("one sample unit must suffice"),
    }
}

fn drain(mut cursor: SampleCursor<'_>) -> Vec<Sample> {
    let mut out = Vec::new();
    for _ in 0..7 {
        match next(&mut cursor) {
            Some(sample) => out.push(sample),
            None => return out,
        }
    }
    panic!("six-sample fixture must reach End");
}

#[test]
fn late_regular_seek_restarts_from_a_checked_sample_table_checkpoint() {
    check_late_regular_seek(None);
    check_late_regular_seek(Some(&[1, 19_999]));
}

fn check_late_regular_seek(sync: Option<&[u32]>) {
    let budget = Budget::new(2 * 1024 * 1024).unwrap();
    let original = authored_stbl(&budget, None, None, 0);
    let samples = 20_000;
    let mut bytes = vec![0; 4];
    bytes.extend_from_slice(b"stbl");
    bytes.extend_from_slice(child(&original, b"stsd").unwrap().bytes().as_ref());
    for (kind, rows) in [
        (b"stts", &[samples, 40][..]),
        (b"stsc", &[1, samples, 1][..]),
    ] {
        let mut part = word_box(kind, rows, 0);
        part[12..16].copy_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&part);
    }
    bytes.extend_from_slice(&word_box(b"stsz", &[samples], 0));
    bytes.extend_from_slice(&word_box(b"stco", &[1000], 0));
    if let Some(sync) = sync {
        bytes.extend_from_slice(&word_box(b"stss", sync, 0));
    }
    let size = bytes.len() as u32;
    bytes[..4].copy_from_slice(&size.to_be_bytes());
    let table = common::table(
        BoxView::new(common::bytes(&budget, &bytes), 0).unwrap(),
        &budget,
        1,
    )
    .unwrap();
    let track = DemuxTrack {
        track_id: 1,
        table: &table,
        fragments: &[],
        media_base: TimeBase::new(1, 1000).unwrap(),
        edits: None,
    };
    let extent = 1000..1000 + u64::from(samples);
    let media = std::slice::from_ref(&extent);
    let cancel = AtomicBool::new(false);
    let profile = DemuxProfile::NoParseNoFillin;
    let mut cursor = PacketCursor::new_complete_source(
        &budget,
        &[track],
        media,
        triergon_isobmff::compat::mov::packets::PacketLimits::default(),
        profile,
    )
    .unwrap();
    cursor.enable_range_seek_checkpoints();
    exhaust_and_rewind(&mut cursor, &cancel);

    let target = (16_384 + 7) * 40 + 1;
    let mut checked = cursor
        .prepare_checked_range_seek(
            1,
            target,
            triergon_isobmff::compat::mov::seek::SeekLimits::default(),
        )
        .unwrap()
        .expect("regular track supplies checkpoints");
    assert_eq!(
        checked.poll(&mut WorkBudget::new(128), &cancel).unwrap(),
        BuildStatus::Ready
    );
    let (mut replay, landing) = checked.finish().unwrap();

    let mut full = GlobalSeekJob::new_complete_source(
        &budget,
        &[track],
        media,
        1,
        target,
        triergon_isobmff::compat::mov::seek::SeekLimits::default(),
        profile,
    )
    .unwrap();
    while full.poll(&mut WorkBudget::new(1024), &cancel).unwrap() == BuildStatus::Yield {}
    let full = full.finish().unwrap();
    assert_eq!(landing.selected.ordinal, full.landing().selected.ordinal);
    assert_eq!(
        landing.selected.occurrence.source,
        full.landing().selected.occurrence.source
    );
    assert_eq!(
        landing.selected.occurrence.demux,
        full.landing().selected.occurrence.demux
    );
    assert_eq!(landing.scanned_entries, u64::from(samples));
    assert_eq!(
        landing.selected.ordinal,
        if sync.is_some() { 0 } else { 16_391 }
    );
    for ordinal in landing.selected.ordinal + 1..=landing.selected.ordinal + 5 {
        assert_eq!(
            replay.poll(&mut WorkBudget::new(128), &cancel).unwrap(),
            CursorStatus::Ready
        );
        let packet = replay.take_occurrence().unwrap();
        assert_eq!(packet.source.ordinal, ordinal);
        assert_eq!(packet.source.range, 999 + ordinal..1000 + ordinal);
        assert_eq!(packet.demux.dts, Some((ordinal as i64 - 1) * 40));
    }
    drop(replay);
    let before = budget.used();
    assert!(cursor.discard_seek_checkpoints());
    assert!(budget.used() < before);

    let mut resumed = PacketCursor::new_complete_source_at(
        &budget,
        &[track],
        media,
        triergon_isobmff::compat::mov::packets::PacketLimits::default(),
        profile,
        &[ResumePoint {
            track_id: 1,
            occurrence: 1,
        }],
    )
    .unwrap();
    resumed.enable_range_seek_checkpoints();
    exhaust_and_rewind(&mut resumed, &cancel);
    assert!(
        resumed
            .prepare_checked_range_seek(
                1,
                0,
                triergon_isobmff::compat::mov::seek::SeekLimits::default()
            )
            .unwrap()
            .is_none()
    );
}

fn exhaust_and_rewind(cursor: &mut PacketCursor<'_>, cancel: &AtomicBool) {
    loop {
        match cursor.poll(&mut WorkBudget::new(64), cancel).unwrap() {
            CursorStatus::Ready => {
                cursor.take_occurrence().unwrap();
            }
            CursorStatus::Yield => {}
            CursorStatus::End => break,
        }
    }
    cursor.rewind_at_end().unwrap();
}

fn check_membership(sync: Option<&[u32]>, partial: Option<&[u32]>) {
    let budget = Budget::new(2 * 1024 * 1024).unwrap();
    let stbl = authored_stbl(&budget, sync, partial, 0);
    let original = stbl.bytes().as_ref().to_vec();
    let table = common::table(stbl, &budget, 1).unwrap();
    assert_eq!(table.sample_count(), 6);
    assert_eq!(table.sync_entry_count(), sync.map(|rows| rows.len() as u32));
    let baseline = budget.used();
    let samples = drain(table.cursor());
    assert_eq!(
        budget.used(),
        baseline,
        "no new charged sample/bitmap storage"
    );
    let fixture = CASES
        .iter()
        .find(|case| case.name == "r-front-stco")
        .unwrap();
    for (index, (sample, expected)) in samples.iter().zip(fixture.samples).enumerate() {
        let ordinal = index as u64 + 1;
        assert_eq!(sample.ordinal, ordinal);
        assert_eq!(sample.range.start, expected.offset);
        assert_eq!(
            sample.range.end - sample.range.start,
            expected.payload.len() as u64
        );
        assert_eq!(sample.dts_ticks, Some(expected.dts));
        assert_eq!(sample.cts_ticks, Some(expected.cts));
        assert_eq!(sample.duration_ticks, expected.delta);
        assert_eq!(sample.description_index, expected.description);
        assert_eq!(
            sample.is_container_sync(),
            sync.is_none_or(|rows| rows.contains(&(ordinal as u32)))
        );
        assert_eq!(
            sample.partial_sync,
            partial.is_some_and(|rows| rows.contains(&(ordinal as u32)))
        );
        let previous = sync.map_or(Some(ordinal), |rows| {
            rows.iter()
                .copied()
                .filter(|row| u64::from(*row) <= ordinal)
                .max()
                .map(u64::from)
        });
        assert_eq!(table.previous_sync(ordinal).unwrap(), previous);
    }
    assert_eq!(samples.len(), 6);
    // Random access remains an independent binary-membership path, including
    // backward probes after the sequential cursor consumed both row arrays.
    for ordinal in [6, 1, 4, 2, 5, 3] {
        let mut locator = table.locate(ordinal).unwrap();
        let located = loop {
            match locator.poll(&mut WorkBudget::new(1)).unwrap() {
                SampleStep::Sample(value) => break value,
                SampleStep::Yield => {}
                SampleStep::End => panic!("missing random source sample"),
            }
        };
        assert_eq!(located, samples[ordinal as usize - 1]);
    }
    assert_eq!(table.raw().bytes().as_ref(), original);
}

#[test]
fn absent_sync_tables_keep_default_access_and_source_presence() {
    check_membership(None, None);
    check_membership(None, Some(&[2, 6]));
}

#[test]
fn present_empty_tables_are_retained_without_all_sync_fallback() {
    check_membership(Some(&[]), None);
    check_membership(Some(&[]), Some(&[]));
    let budget = Budget::new(2 * 1024 * 1024).unwrap();
    let stbl = authored_stbl(&budget, Some(&[]), Some(&[]), 0);
    for kind in [b"stss", b"stps"] {
        let raw = child(&stbl, kind).unwrap();
        assert_eq!(
            raw.payload(),
            &[0; 8],
            "authored present-empty table is retained"
        );
    }
}

#[test]
fn sparse_rows_include_last_sample_and_match_unchanged_random_access() {
    check_membership(Some(&[2, 6]), Some(&[1, 3, 6]));
    check_membership(Some(&[1, 4]), Some(&[2, 5]));
}

#[test]
fn all_samples_and_last_only_rows_cross_every_boundary() {
    check_membership(Some(&[1, 2, 3, 4, 5, 6]), Some(&[1, 2, 3, 4, 5, 6]));
    check_membership(Some(&[6]), Some(&[6]));
    check_membership(None, Some(&[1, 2, 3, 4, 5, 6]));
    check_membership(Some(&[]), Some(&[6]));
}

#[test]
fn zero_work_clone_and_restart_preserve_independent_row_positions() {
    let budget = Budget::new(2 * 1024 * 1024).unwrap();
    let table = common::table(
        authored_stbl(&budget, Some(&[2, 4, 6]), Some(&[1, 5]), 0),
        &budget,
        1,
    )
    .unwrap();
    let expected = drain(table.cursor());
    let mut cursor = table.cursor();
    for sample in &expected[..2] {
        let mut empty = WorkBudget::new(0);
        assert!(matches!(
            cursor.poll(&mut empty).unwrap(),
            SampleStep::Yield
        ));
        assert_eq!(empty.spent(), 0);
        assert_eq!(next(&mut cursor).as_ref(), Some(sample));
    }
    let fork = cursor;
    assert_eq!(drain(cursor), expected[2..]);
    assert_eq!(drain(fork), expected[2..]);
    assert_eq!(drain(table.cursor()), expected);
}

#[test]
fn repeated_edit_occurrences_restart_sequential_membership() {
    let fixtures: &[(&[u8], &[u64])] = &[
        (
            include_bytes!("fixtures/edit-two-repeat-two.mov"),
            &[1, 2, 3, 4, 1, 2, 3, 4],
        ),
        (
            include_bytes!("fixtures/edit-three-repeat-one.mov"),
            &[1, 2, 3, 4, 4, 5, 6, 1],
        ),
    ];
    for (bytes, wanted) in fixtures {
        let budget = Budget::new(2 * 1024 * 1024).unwrap();
        let roots = common::roots(&budget, bytes).unwrap();
        let movie = roots
            .iter()
            .find(|root| root.header().kind == *b"moov")
            .unwrap();
        let movie_header = MovieHeader::parse(&child(movie, b"mvhd").unwrap()).unwrap();
        let track = child(movie, b"trak").unwrap();
        let media = child(&track, b"mdia").unwrap();
        let header = TrackHeader::parse(
            &child(&track, b"tkhd").unwrap(),
            &child(&media, b"mdhd").unwrap(),
            &child(&media, b"hdlr").unwrap(),
        )
        .unwrap();
        let table = common::table(
            child(&child(&media, b"minf").unwrap(), b"stbl").unwrap(),
            &budget,
            1,
        )
        .unwrap();
        let source = drain(table.cursor());
        let edits = EditList::parse(
            child(&child(&track, b"edts").unwrap(), b"elst").unwrap(),
            Limits::default(),
        )
        .unwrap();
        for quantum in [1, 4096] {
            let mut cursor = EditCursor::new(
                &table,
                &edits,
                header.time_base().unwrap(),
                movie_header.time_base().unwrap(),
                EditLimits::default(),
            )
            .unwrap();
            let mut seen = Vec::new();
            let mut ended = false;
            for _ in 0..100_000 {
                match cursor.poll(&mut WorkBudget::new(quantum)).unwrap() {
                    CursorStatus::Yield => {}
                    CursorStatus::Ready => {
                        let occurrence = cursor.take_occurrence().unwrap();
                        seen.push(occurrence.source.ordinal);
                        assert_eq!(
                            occurrence.source,
                            source[occurrence.source.ordinal as usize - 1]
                        );
                    }
                    CursorStatus::End => {
                        ended = true;
                        break;
                    }
                }
            }
            assert!(ended);
            assert_eq!(seen, *wanted);
        }
    }
}

#[test]
fn malformed_sync_and_partial_rows_keep_error_kind_at_each_quantum() {
    for malformed in [&[0][..], &[1, 1], &[4, 2], &[7]] {
        for partial in [false, true] {
            for quantum in [1, 4096] {
                let budget = Budget::new(2 * 1024 * 1024).unwrap();
                let stbl = if partial {
                    authored_stbl(&budget, Some(&[1, 6]), Some(malformed), 0)
                } else {
                    authored_stbl(&budget, Some(malformed), Some(&[1, 6]), 0)
                };
                let error = common::table(stbl, &budget, quantum).unwrap_err();
                assert_eq!(error, Error::Invalid("sync sample order/range"));
            }
        }
    }
}

#[test]
fn syntax_errors_still_precede_semantic_sync_order_validation() {
    for quantum in [1, 4096] {
        let budget = Budget::new(2 * 1024 * 1024).unwrap();
        // stss row0 is semantically invalid, but the structurally unsupported
        // stps version is rejected at the original earlier table-parse stage.
        let stbl = authored_stbl(&budget, Some(&[0]), Some(&[1]), 1);
        assert!(matches!(
            common::table(stbl, &budget, quantum),
            Err(Error::Unsupported(_))
        ));
    }
}

#[test]
fn membership_polling_does_not_change_owned_capacity_or_source_bytes() {
    let budget = Budget::new(2 * 1024 * 1024).unwrap();
    let table = common::table(
        authored_stbl(&budget, Some(&[1, 6]), Some(&[2, 4, 6]), 0),
        &budget,
        1,
    )
    .unwrap();
    let baseline = budget.used();
    let original = table.raw().bytes().as_ref().to_vec();
    for _ in 0..3 {
        let mut cursor = table.cursor();
        for _ in 0..6 {
            assert!(next(&mut cursor).is_some());
            assert_eq!(budget.used(), baseline);
        }
        let mut empty = WorkBudget::new(0);
        assert!(matches!(cursor.poll(&mut empty).unwrap(), SampleStep::End));
        assert_eq!(empty.spent(), 0);
    }
    assert_eq!(table.raw().bytes().as_ref(), original);
}
