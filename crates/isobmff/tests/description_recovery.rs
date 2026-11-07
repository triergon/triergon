mod common;
use common::{child, roots};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, Error, WorkBudget};
use triergon_isobmff::{
    BoxView, BuildStatus, Limits, SampleStep,
    compat::mov::{
        packets::{CursorStatus, PacketLimits},
        recovery::{
            DescriptionAssociation, DescriptionContext, DescriptionEmission, DescriptionEvent,
            DescriptionRead, DescriptionRecoveryBuilder, DescriptionRecoveryProfile,
            DescriptionRecoveryTable, RecoveryTrackHeader, RunRecoveryBuilder, RunRecoveryProfile,
        },
        seek::IndexLimits,
    },
};

const ZERO: &[u8] = include_bytes!("fixtures/recovery-v3/desc-0,0,0.mov");
const POSITIVE: &[u8] = include_bytes!("fixtures/recovery-v3/desc-2,1,1.mov");

fn parts(bytes: &[u8], p: &Budget) -> (BoxView, RecoveryTrackHeader, Vec<Range<u64>>) {
    let top = roots(p, bytes).unwrap();
    let moov = top.iter().find(|b| b.header().kind == *b"moov").unwrap();
    let track = child(moov, b"trak").unwrap();
    let media = child(&track, b"mdia").unwrap();
    let header = RecoveryTrackHeader::parse(
        &child(&track, b"tkhd").unwrap(),
        &child(&media, b"mdhd").unwrap(),
        &child(&media, b"hdlr").unwrap(),
        RunRecoveryProfile,
    )
    .unwrap();
    (
        child(&child(&media, b"minf").unwrap(), b"stbl").unwrap(),
        header,
        top.iter()
            .filter(|b| b.header().kind == *b"mdat")
            .map(|b| b.header().payload_range().unwrap())
            .collect(),
    )
}
fn build(raw: BoxView, p: &Budget, q: u64) -> Result<DescriptionRecoveryTable, Error> {
    let mut job = DescriptionRecoveryBuilder::new(
        raw,
        p.clone(),
        Limits::default(),
        DescriptionRecoveryProfile,
    )?;
    for _ in 0..100_000 {
        if job.poll(&mut WorkBudget::new(q), &AtomicBool::new(false))? == BuildStatus::Ready {
            return job.finish();
        }
    }
    panic!("bounded v3 build made no progress");
}
fn events(bytes: &[u8], q: u64) -> Vec<DescriptionEvent> {
    let p = Budget::new(1 << 20).unwrap();
    let (raw, header, media) = parts(bytes, &p);
    let table = build(raw, &p, q).unwrap();
    let mut cursor = table
        .description_packet_cursor(
            &p,
            &header,
            &media,
            PacketLimits::default(),
            DescriptionContext {
                physical_length: bytes.len() as u64,
                custom_read_maximum: 65536,
            },
            DescriptionRecoveryProfile,
        )
        .unwrap();
    assert_eq!(
        cursor
            .poll(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        CursorStatus::Yield
    );
    let mut result = Vec::new();
    for _ in 0..100_000 {
        match cursor
            .poll(&mut WorkBudget::new(q), &AtomicBool::new(false))
            .unwrap()
        {
            CursorStatus::Yield => {}
            CursorStatus::Ready => result.push(cursor.take_event().unwrap()),
            CursorStatus::End => return result,
        }
    }
    panic!("bounded v3 cursor made no progress");
}
fn positions(bytes: &[u8]) -> Vec<u64> {
    events(bytes, 1)
        .into_iter()
        .filter_map(|r| r.packet.map(|p| p.source_range.start))
        .collect()
}

#[test]
fn strict_and_run_description_errors_remain_exact() {
    let p = Budget::new(1 << 20).unwrap();
    let (raw, _, _) = parts(ZERO, &p);
    assert_eq!(
        common::table(raw.clone(), &p, 1).unwrap_err(),
        Error::Invalid("stsc order/count/description")
    );
    let mut old = RunRecoveryBuilder::new(raw, p, Limits::default(), RunRecoveryProfile).unwrap();
    assert_eq!(
        old.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
        Err(Error::Invalid("stsc order/count/description"))
    );
}
#[test]
fn all_zero_rows_retain_raw_configuration_and_all_source_ordinals() {
    let p = Budget::new(1 << 20).unwrap();
    let (raw, _, _) = parts(ZERO, &p);
    let original = raw.bytes().clone();
    let table = build(raw, &p, 1).unwrap();
    assert_eq!(table.invalid_description_count(), 3);
    assert_eq!(table.raw().bytes().as_ref(), original.as_ref());
    for row in 0..3 {
        let d = table.description_diagnostic(row).unwrap().unwrap();
        assert_eq!(d.raw_description, 0);
        assert_eq!(&ZERO[d.field_start as usize..d.field_end as usize], &[0; 4]);
    }
    let mut cursor = table.source_prefix_cursor();
    let mut raw_rows = Vec::new();
    loop {
        match cursor.poll(&mut WorkBudget::new(1)).unwrap() {
            SampleStep::Yield => {}
            SampleStep::Sample(s) => raw_rows.push(s),
            SampleStep::End => break,
        }
    }
    assert_eq!(raw_rows.len(), 6);
    assert!(raw_rows.iter().all(|s| s.description_index == 0));
    assert_eq!(raw_rows[3].range, 711..723);
    assert_eq!(raw_rows[3].dts_ticks, Some(3000));
    let projected = events(ZERO, 1);
    assert_eq!(projected[3].packet.as_ref().unwrap().source_range, 699..711);
    assert_eq!(projected[3].declaration, raw_rows[3]);
    assert!(projected.iter().all(|r| r.association
        == DescriptionAssociation::RawSizeAndClockOrdinalOnly
        && r.source_error.is_some()));
}
#[test]
fn multiple_and_noninitial_zero_rows_use_observed_successor_counts_without_source_repair() {
    for (bytes, expected) in [
        (
            include_bytes!("fixtures/recovery-v3/desc-1,0,1.mov").as_slice(),
            vec![675, 687, 699, 711, 723, 711],
        ),
        (
            include_bytes!("fixtures/recovery-v3/desc-1,1,0.mov"),
            vec![675, 687, 699, 711, 723, 735],
        ),
        (ZERO, vec![675, 687, 699, 699, 711, 723]),
        (
            include_bytes!("fixtures/recovery-v3/desc-0,1,0.mov"),
            vec![675, 699, 711, 723, 735],
        ),
        (
            include_bytes!("fixtures/recovery-v3/desc-1,0,0.mov"),
            vec![675, 687, 699, 711, 723, 711],
        ),
        (
            include_bytes!("fixtures/recovery-v3/desc-0,0,1.mov"),
            vec![675, 687, 699, 699, 711, 723],
        ),
    ] {
        assert_eq!(positions(bytes), expected);
    }
}
#[test]
fn unsigned_all_ones_is_only_a_labelled_observation_not_a_source_description_fix() {
    let bytes = include_bytes!("fixtures/recovery-v3/desc-4294967295,1,1.mov");
    let rows = events(bytes, 1);
    assert_eq!(rows[0].declaration.description_index, u32::MAX);
    assert_eq!(rows[0].raw_chunk_description, u32::MAX);
    assert_eq!(positions(bytes), vec![675, 699, 711, 723, 735]);
    assert_eq!(rows[1].declaration.description_index, u32::MAX);
    assert!(
        rows[1].source_error.is_some(),
        "invalid raw clock-ordinal configuration cannot disappear"
    );
}
#[test]
fn missing_positive_catalogue_has_typed_suppression_events_and_keeps_clock_prefix() {
    let rows = events(POSITIVE, 1);
    assert_eq!(rows.len(), 6);
    for (n, row) in rows[..2].iter().enumerate() {
        assert_eq!(
            row.emission,
            DescriptionEmission::SuppressedMissingCatalogueEntry
        );
        assert!(row.packet.is_none());
        assert_eq!(row.declaration.ordinal, n as u64 + 1);
        assert_eq!(row.declaration.description_index, 2);
        assert_eq!(row.declaration.dts_ticks, Some(n as i128 * 1000));
        assert!(row.source_error.is_some());
    }
    assert_eq!(rows[2].declaration.ordinal, 3);
    assert_eq!(rows[2].packet.as_ref().unwrap().dts, Some(2000));
    assert_eq!(rows[2].demux_index_distance, 2);
    assert_eq!(positions(POSITIVE), vec![699, 711, 723, 735]);
}
#[test]
fn demux_index_compacts_only_its_ordinals_and_carries_suppressed_sync_distance() {
    let p = Budget::new(1 << 20).unwrap();
    let (raw, header, media) = parts(POSITIVE, &p);
    let table = build(raw, &p, 1).unwrap();
    let mut index = table
        .description_index_cursor(
            &p,
            &header,
            &media,
            IndexLimits::default(),
            DescriptionContext {
                physical_length: POSITIVE.len() as u64,
                custom_read_maximum: 65536,
            },
            DescriptionRecoveryProfile,
        )
        .unwrap();
    let (mut suppressed, mut entries) = (0, Vec::new());
    for _ in 0..100_000 {
        match index
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            CursorStatus::Yield => {}
            CursorStatus::Ready => {
                let event = index.take_event().unwrap();
                if let Some(entry) = event.demux_entry {
                    entries.push(entry);
                } else {
                    suppressed += 1;
                    assert_eq!(event.source_event.declaration.description_index, 2);
                }
            }
            CursorStatus::End => break,
        }
    }
    assert_eq!(suppressed, 2);
    assert_eq!(
        entries
            .iter()
            .map(|r| (r.ordinal, r.position, r.timestamp, r.min_distance))
            .collect::<Vec<_>>(),
        vec![
            (0, 699, 2000, 2),
            (1, 711, 3000, 0),
            (2, 723, 4000, 1),
            (3, 735, 5000, 2)
        ]
    );
}
#[test]
fn physical_availability_does_not_establish_demux_eof_from_read_configuration() {
    for (length, start) in [(747, 675), (847, 775), (4843, 4771)] {
        for maximum in [0, 1, 2, 3, 4, 7, 64, 65536] {
            let context = DescriptionContext {
                physical_length: length,
                custom_read_maximum: maximum,
            };
            assert_eq!(
                context.read_disposition(start, start..length).unwrap(),
                DescriptionRead::Available
            );
            for position in [
                length,
                length + 1,
                start + 32768,
                start + 32769,
                length + 32768,
                length + 32769,
            ] {
                assert_eq!(
                    context.read_disposition(position, start..length).unwrap(),
                    DescriptionRead::UnavailablePhysicalStart
                );
            }
        }
    }
}
#[test]
fn unseen_geometry_and_nonterminal_context_cannot_invent_demux_eof() {
    let context = DescriptionContext {
        physical_length: 1000,
        custom_read_maximum: 65536,
    };
    assert_eq!(
        context.read_disposition(33500, 750..1000).unwrap(),
        DescriptionRead::UnavailablePhysicalStart
    );
    assert!(context.read_disposition(33500, 750..1100).is_err());
    let maximum = DescriptionContext {
        physical_length: u64::MAX,
        custom_read_maximum: 65536,
    };
    assert_eq!(
        maximum
            .read_disposition(u64::MAX, u64::MAX - 1..u64::MAX)
            .unwrap(),
        DescriptionRead::UnavailablePhysicalStart
    );
}
#[test]
fn unobserved_description_values_and_noninitial_specials_are_refused() {
    for (row, value) in [(0, 3), (0, u32::MAX - 1), (1, 2), (1, u32::MAX)] {
        let mut bytes = include_bytes!("fixtures/r-front-stco.mov").to_vec();
        let at = bytes.windows(4).position(|v| v == b"stsc").unwrap();
        bytes[at + 20 + row * 12..at + 24 + row * 12].copy_from_slice(&value.to_be_bytes());
        let p = Budget::new(1 << 20).unwrap();
        let (raw, _, _) = parts(&bytes, &p);
        assert!(matches!(build(raw, &p, 1), Err(Error::Unsupported(_))));
    }
}
#[test]
fn malformed_zero_chunk_count_and_nonincreasing_chunk_rows_are_not_recovered() {
    for (relative, value) in [(16, 0u32), (24, 1u32)] {
        let mut bytes = ZERO.to_vec();
        let at = bytes.windows(4).position(|v| v == b"stsc").unwrap();
        bytes[at + relative..at + relative + 4].copy_from_slice(&value.to_be_bytes());
        let p = Budget::new(1 << 20).unwrap();
        let (raw, _, _) = parts(&bytes, &p);
        assert!(build(raw, &p, 1).is_err());
    }
}
#[test]
fn builder_and_cursors_refuse_foreign_pool_before_allocation() {
    let p = Budget::new(1 << 20).unwrap();
    let other = Budget::new(1 << 20).unwrap();
    let (raw, header, media) = parts(ZERO, &p);
    let before = other.used();
    assert!(matches!(
        DescriptionRecoveryBuilder::new(
            raw.clone(),
            other.clone(),
            Limits::default(),
            DescriptionRecoveryProfile
        ),
        Err(Error::ForeignPool)
    ));
    let table = build(raw, &p, 1).unwrap();
    assert!(matches!(
        table.description_packet_cursor(
            &other,
            &header,
            &media,
            PacketLimits::default(),
            DescriptionContext {
                physical_length: ZERO.len() as u64,
                custom_read_maximum: 65536
            },
            DescriptionRecoveryProfile
        ),
        Err(Error::ForeignPool)
    ));
    assert!(matches!(
        table.description_index_cursor(
            &other,
            &header,
            &media,
            IndexLimits::default(),
            DescriptionContext {
                physical_length: ZERO.len() as u64,
                custom_read_maximum: 65536
            },
            DescriptionRecoveryProfile
        ),
        Err(Error::ForeignPool)
    ));
    assert_eq!(other.used(), before);
}
#[test]
fn builder_zero_work_and_cancel_are_sticky_without_ready_escape() {
    let p = Budget::new(1 << 20).unwrap();
    let (raw, _, _) = parts(ZERO, &p);
    let mut builder =
        DescriptionRecoveryBuilder::new(raw, p, Limits::default(), DescriptionRecoveryProfile)
            .unwrap();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        builder.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
        BuildStatus::Yield
    );
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        builder.poll(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    );
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        builder.poll(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    );
    assert!(matches!(builder.finish(), Err(Error::Cancelled)));
}
#[test]
fn cancellation_clears_ready_packet_and_index_outputs_and_stays_sticky() {
    let p = Budget::new(1 << 20).unwrap();
    let (raw, h, m) = parts(ZERO, &p);
    let t = build(raw, &p, 1).unwrap();
    let context = DescriptionContext {
        physical_length: ZERO.len() as u64,
        custom_read_maximum: 65536,
    };
    let mut packet = t
        .description_packet_cursor(
            &p,
            &h,
            &m,
            PacketLimits::default(),
            context,
            DescriptionRecoveryProfile,
        )
        .unwrap();
    let mut index = t
        .description_index_cursor(
            &p,
            &h,
            &m,
            IndexLimits::default(),
            context,
            DescriptionRecoveryProfile,
        )
        .unwrap();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        packet.poll(&mut WorkBudget::new(4096), &cancel).unwrap(),
        CursorStatus::Ready
    );
    assert_eq!(
        index.poll(&mut WorkBudget::new(4096), &cancel).unwrap(),
        CursorStatus::Ready
    );
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        packet.poll(&mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    );
    assert_eq!(
        index.poll(&mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    );
    assert!(matches!(packet.take_event(), Err(Error::Cancelled)));
    assert!(matches!(index.take_event(), Err(Error::Cancelled)));
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        packet.poll(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    );
    assert_eq!(
        index.poll(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    );
}
#[test]
fn metadata_source_sample_and_index_limits_are_not_waived() {
    let p = Budget::new(1 << 20).unwrap();
    let (raw, h, m) = parts(ZERO, &p);
    let mut limited = DescriptionRecoveryBuilder::new(
        raw.clone(),
        p.clone(),
        Limits {
            max_records: 0,
            ..Limits::default()
        },
        DescriptionRecoveryProfile,
    )
    .unwrap();
    assert!(
        limited
            .poll(&mut WorkBudget::new(4096), &AtomicBool::new(false))
            .is_err()
    );
    let t = build(raw, &p, 1).unwrap();
    let context = DescriptionContext {
        physical_length: ZERO.len() as u64,
        custom_read_maximum: 65536,
    };
    assert!(matches!(
        t.description_packet_cursor(
            &p,
            &h,
            &m,
            PacketLimits {
                max_source_samples: 5,
                ..PacketLimits::default()
            },
            context,
            DescriptionRecoveryProfile
        ),
        Err(Error::Limit { .. })
    ));
    let mut index = t
        .description_index_cursor(
            &p,
            &h,
            &m,
            IndexLimits {
                max_entries: 0,
                ..IndexLimits::default()
            },
            context,
            DescriptionRecoveryProfile,
        )
        .unwrap();
    assert!(matches!(
        index.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
        Err(Error::Limit { .. })
    ));
    assert!(matches!(
        index.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Limit { .. })
    ));
}
#[test]
fn backpressure_can_resume_builder_without_mutating_source_bytes() {
    let p = Budget::new(1 << 20).unwrap();
    let (raw, _, _) = parts(ZERO, &p);
    let original = raw.bytes().clone();
    let held = p.reserve(p.available()).unwrap();
    let mut job = DescriptionRecoveryBuilder::new(
        raw,
        p.clone(),
        Limits::default(),
        DescriptionRecoveryProfile,
    )
    .unwrap();
    assert!(matches!(
        job.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    drop(held);
    for _ in 0..100_000 {
        if job
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            == BuildStatus::Ready
        {
            let table = job.finish().unwrap();
            assert_eq!(table.raw().bytes().as_ref(), original.as_ref());
            return;
        }
    }
    panic!("backpressure did not resume");
}
#[test]
fn yielded_schedules_do_not_change_events_or_allocate_per_sample_storage() {
    let baseline = events(ZERO, 4096);
    for q in [1, 3, 7] {
        let actual = events(ZERO, q);
        assert_eq!(
            actual
                .iter()
                .map(|r| (
                    &r.declaration,
                    &r.packet,
                    r.emission,
                    r.demux_index_distance
                ))
                .collect::<Vec<_>>(),
            baseline
                .iter()
                .map(|r| (
                    &r.declaration,
                    &r.packet,
                    r.emission,
                    r.demux_index_distance
                ))
                .collect::<Vec<_>>()
        );
    }
    let p = Budget::new(1 << 20).unwrap();
    let (raw, h, m) = parts(ZERO, &p);
    let t = build(raw, &p, 1).unwrap();
    let before = p.used();
    let mut c = t
        .description_packet_cursor(
            &p,
            &h,
            &m,
            PacketLimits::default(),
            DescriptionContext {
                physical_length: ZERO.len() as u64,
                custom_read_maximum: 65536,
            },
            DescriptionRecoveryProfile,
        )
        .unwrap();
    for _ in 0..1000 {
        match c
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            CursorStatus::Ready => {
                c.take_event().unwrap();
            }
            CursorStatus::Yield => {}
            CursorStatus::End => break,
        }
        assert_eq!(p.used(), before);
    }
    assert_eq!(p.used(), before);
}
#[test]
fn retained_source_raw_and_actual_reader_payload_survive_table_and_input_close() {
    use triergon_core::{InputCursor, SourceCapabilities, SourceId, Step};
    use triergon_isobmff::compat::mov::recovery::PartialSampleReader;
    let p = Budget::new(1 << 20).unwrap();
    let baseline = p.used();
    {
        let data = ZERO.to_vec();
        let (raw, header, media) = parts(&data, &p);
        let table = build(raw, &p, 1).unwrap();
        let raw_backing = table.raw().clone();
        let mut cursor = table
            .description_packet_cursor(
                &p,
                &header,
                &media,
                PacketLimits::default(),
                DescriptionContext {
                    physical_length: data.len() as u64,
                    custom_read_maximum: 65536,
                },
                DescriptionRecoveryProfile,
            )
            .unwrap();
        let event = loop {
            match cursor
                .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                CursorStatus::Ready => {
                    let row = cursor.take_event().unwrap();
                    if row.declaration.ordinal == 4 {
                        break row;
                    }
                }
                CursorStatus::Yield => {}
                CursorStatus::End => panic!("missing source ordinal"),
            }
        };
        assert_eq!(
            event.association,
            DescriptionAssociation::RawSizeAndClockOrdinalOnly
        );
        let original_declaration = event.declaration.clone();
        let mut physical = event.declaration;
        physical.range = event.packet.unwrap().source_range;
        physical.description_index = event.raw_chunk_description;
        physical.dts_ticks = None;
        physical.cts_ticks = None;
        physical.duration_ticks = 0;
        physical.composition_offset = None;
        let mut input = InputCursor::new(
            &p,
            SourceId(71),
            SourceCapabilities {
                length: Some(data.len() as u64),
                random_access: true,
            },
            4,
        )
        .unwrap();
        let mut reader = PartialSampleReader::new(
            &mut input,
            physical,
            media[0].clone(),
            12,
            RunRecoveryProfile,
        )
        .unwrap();
        let packet = loop {
            match reader
                .poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                Step::Ready(packet) => break packet,
                Step::Yield => {}
                Step::NeedInput(q) => {
                    let at = q.offset as usize;
                    input
                        .provide_inline(q.source, q.token, q.offset, &data[at..at + 1])
                        .unwrap();
                }
                Step::End(_) => panic!("unexpected source end"),
            }
        };
        assert_eq!(packet.source, SourceId(71));
        assert_eq!(packet.source_sample.dts_ticks, None);
        assert_eq!(original_declaration.range, 711..723);
        assert_eq!(packet.bounds.declared, 699..711);
        let retained = packet.payload.clone();
        assert!(retained.shares_storage_with(&packet.payload));
        drop(packet);
        drop(reader);
        drop(input);
        drop(cursor);
        drop(table);
        drop(header);
        drop(data);
        assert_eq!(retained.as_ref(), &ZERO[699..711]);
        assert!(retained.belongs_to(&p));
        assert_eq!(raw_backing.header().kind, *b"stbl");
        assert!(p.used() > baseline);
    }
    assert_eq!(p.used(), baseline);
}
#[test]
fn source_zero_scale_stays_invalid_while_only_demux_projection_has_fallback() {
    let bytes = include_bytes!("fixtures/bad-zero-timescale.mov");
    let p = Budget::new(1 << 20).unwrap();
    let (raw, h, m) = parts(bytes, &p);
    assert_eq!(h.source().timescale, 0);
    assert!(!h.source_clock_valid());
    assert!(h.source().time_base().is_err());
    let t = build(raw, &p, 1).unwrap();
    let mut c = t
        .description_packet_cursor(
            &p,
            &h,
            &m,
            PacketLimits::default(),
            DescriptionContext {
                physical_length: bytes.len() as u64,
                custom_read_maximum: 65536,
            },
            DescriptionRecoveryProfile,
        )
        .unwrap();
    for _ in 0..100_000 {
        if c.poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            == CursorStatus::Ready
        {
            let r = c.take_event().unwrap();
            assert_eq!(r.source_timescale, 0);
            assert_eq!(r.declaration.dts_ticks, Some(0));
            return;
        }
    }
    panic!("zero scale observation did not progress");
}
