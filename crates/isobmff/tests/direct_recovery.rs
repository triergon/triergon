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
            DirectAssociation, DirectContext, DirectRead, DirectRecoveryProfile,
            RecoveryTrackHeader, RunRecoveryBuilder, RunRecoveryProfile, RunRecoveryTable,
        },
        seek::IndexLimits,
    },
};
const BAD_DESC: &[u8] = include_bytes!("fixtures/bad-stsc-description-zero.mov");
const BAD_OFFSET: &[u8] = include_bytes!("fixtures/bad-chunk-offset-outside-source.mov");
fn parts(bytes: &[u8], pool: &Budget) -> (BoxView, RecoveryTrackHeader, Vec<Range<u64>>) {
    let top = roots(pool, bytes).unwrap();
    let moov = top.iter().find(|v| v.header().kind == *b"moov").unwrap();
    let track = child(moov, b"trak").unwrap();
    let mdia = child(&track, b"mdia").unwrap();
    let header = RecoveryTrackHeader::parse(
        &child(&track, b"tkhd").unwrap(),
        &child(&mdia, b"mdhd").unwrap(),
        &child(&mdia, b"hdlr").unwrap(),
        RunRecoveryProfile,
    )
    .unwrap();
    (
        child(&child(&mdia, b"minf").unwrap(), b"stbl").unwrap(),
        header,
        top.iter()
            .filter(|v| v.header().kind == *b"mdat")
            .map(|v| v.header().payload_range().unwrap())
            .collect(),
    )
}
fn build(raw: BoxView, p: &Budget) -> Result<RunRecoveryTable, Error> {
    let mut b =
        RunRecoveryBuilder::new_direct(raw, p.clone(), Limits::default(), DirectRecoveryProfile)?;
    for _ in 0..100_000 {
        if b.poll(&mut WorkBudget::new(1), &AtomicBool::new(false))? == BuildStatus::Ready {
            return b.finish();
        }
    }
    panic!("bounded table builder did not progress");
}
fn positions(bytes: &[u8]) -> Vec<(u64, u64, i64)> {
    let p = Budget::new(1 << 20).unwrap();
    let (raw, h, m) = parts(bytes, &p);
    let t = build(raw, &p).unwrap();
    let mut c = t
        .direct_packet_cursor(
            &p,
            &h,
            &m,
            PacketLimits::default(),
            DirectContext {
                physical_length: bytes.len() as u64,
            },
            DirectRecoveryProfile,
        )
        .unwrap();
    let mut rows = Vec::new();
    loop {
        match c
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            CursorStatus::Yield => {}
            CursorStatus::End => break,
            CursorStatus::Ready => {
                let r = c.take_occurrence().unwrap();
                rows.push((
                    r.demux.source_range.start,
                    r.demux.source_range.end - r.demux.source_range.start,
                    r.demux.dts.unwrap(),
                ));
            }
        }
    }
    rows
}
#[test]
fn strict_and_run_description_zero_rejections_are_unchanged() {
    let p = Budget::new(1 << 20).unwrap();
    let (raw, _, _) = parts(BAD_DESC, &p);
    assert_eq!(
        common::table(raw.clone(), &p, 1).unwrap_err(),
        Error::Invalid("stsc order/count/description")
    );
    let mut b = RunRecoveryBuilder::new(raw, p, Limits::default(), RunRecoveryProfile).unwrap();
    assert_eq!(
        b.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
        Err(Error::Invalid("stsc order/count/description"))
    );
}
#[test]
fn raw_description_counts_ranges_and_configuration_are_not_repaired() {
    let p = Budget::new(1 << 20).unwrap();
    let (raw, h, m) = parts(BAD_DESC, &p);
    let original = raw.bytes().clone();
    let t = build(raw, &p).unwrap();
    let d = t.initial_description_diagnostic().unwrap();
    assert_eq!(
        (
            d.raw_description,
            d.declared_first_chunk_samples,
            d.observed_first_chunk_samples,
            d.demux_catalogue_entry
        ),
        (0, 2, 1, 1)
    );
    assert_eq!(
        &BAD_DESC[d.field_start as usize..d.field_end as usize],
        &[0; 4]
    );
    assert_eq!(t.raw().bytes().as_ref(), original.as_ref());
    let mut source = t.source_prefix_cursor();
    let mut raw_rows = Vec::new();
    loop {
        match source.poll(&mut WorkBudget::new(1)).unwrap() {
            SampleStep::Yield => {}
            SampleStep::End => break,
            SampleStep::Sample(s) => raw_rows.push(s),
        }
    }
    assert_eq!(raw_rows.len(), 6);
    assert_eq!(raw_rows[1].description_index, 0);
    assert_eq!(raw_rows[1].range, 687..699);
    assert_eq!(raw_rows[1].dts_ticks, Some(1000));
    let mut c = t
        .direct_packet_cursor(
            &p,
            &h,
            &m,
            PacketLimits::default(),
            DirectContext {
                physical_length: 747,
            },
            DirectRecoveryProfile,
        )
        .unwrap();
    let mut n = 0;
    loop {
        match c
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            CursorStatus::Yield => {}
            CursorStatus::End => break,
            CursorStatus::Ready => {
                let r = c.take_occurrence().unwrap();
                assert_eq!(r.association, DirectAssociation::RawSizeAndClockOrdinalOnly);
                assert_eq!(r.declaration, raw_rows[n]);
                if n == 1 {
                    assert_eq!(r.demux.source_range, 699..711);
                    assert_eq!(r.declaration.range, 687..699);
                }
                n += 1;
            }
        }
    }
    assert_eq!(n, 5);
}
#[test]
fn source_declared_six_demux_observed_five_whole_positions() {
    assert_eq!(
        positions(BAD_DESC),
        vec![
            (675, 12, 0),
            (699, 12, 1000),
            (711, 12, 2000),
            (723, 12, 3000),
            (735, 12, 4000)
        ]
    );
}
#[test]
fn following_row_count_controls_not_singleton_or_whole_chunk_drop() {
    for (b, want) in [
        (
            include_bytes!("fixtures/recovery-v2/counts-2,2,2.mov").as_slice(),
            vec![675, 687, 699, 711, 723, 735],
        ),
        (
            include_bytes!("fixtures/recovery-v2/counts-3,1,2.mov"),
            vec![675, 711, 723, 735],
        ),
        (
            include_bytes!("fixtures/recovery-v2/counts-1,3,2.mov"),
            vec![675, 687, 699, 687, 699, 711],
        ),
        (
            include_bytes!("fixtures/recovery-v2/counts-3,2,1.mov"),
            vec![675, 687, 711, 723, 735],
        ),
        (
            include_bytes!("fixtures/recovery-v2/counts-2,3,1.mov"),
            vec![675, 687, 699, 699, 711, 723],
        ),
        (
            include_bytes!("fixtures/recovery-v2/counts-4,1,1.mov"),
            vec![675, 723, 735],
        ),
    ] {
        assert_eq!(positions(b).iter().map(|r| r.0).collect::<Vec<_>>(), want);
    }
}
#[test]
fn variable_size_prefix_is_demux_ordinal_not_source_payload_association() {
    assert_eq!(
        positions(include_bytes!(
            "fixtures/recovery-v2/desc-zero-variable-sizes.mov"
        )),
        vec![
            (675, 5, 0),
            (699, 19, 1000),
            (706, 7, 2000),
            (713, 11, 3000),
            (724, 13, 4000)
        ]
    );
}
#[test]
fn vfr_clock_prefix_is_read_from_raw_runs() {
    assert_eq!(
        positions(include_bytes!("fixtures/recovery-v2/desc-zero-vfr.mov")),
        vec![
            (715, 12, 0),
            (739, 12, 333),
            (751, 12, 1000),
            (763, 12, 2001),
            (775, 12, 2002)
        ]
    );
}
#[test]
fn noninitial_multiple_or_other_invalid_descriptions_remain_errors() {
    let p = Budget::new(1 << 20).unwrap();
    for b in [
        include_bytes!("fixtures/recovery-v2/desc-1,0,1.mov").as_slice(),
        include_bytes!("fixtures/recovery-v2/desc-0,0,0.mov"),
        include_bytes!("fixtures/recovery-v2/desc-0,1,0.mov"),
        include_bytes!("fixtures/recovery-v2/desc-2,1,1.mov"),
        include_bytes!("fixtures/recovery-v2/desc-4294967295,1,1.mov"),
    ] {
        let (r, _, _) = parts(b, &p);
        assert!(build(r, &p).is_err());
    }
}
#[test]
fn second_fault_zero_next_count_and_source_coverage_not_recovered() {
    let p = Budget::new(1 << 20).unwrap();
    for offset in [12usize, 24] {
        let mut b = BAD_DESC.to_vec();
        let at = b.windows(4).position(|v| v == b"stsc").unwrap() + 4 + offset;
        b[at..at + 4].copy_from_slice(&0u32.to_be_bytes());
        let (r, _, _) = parts(&b, &p);
        assert!(build(r, &p).is_err());
    }
}
#[test]
fn outside_source_is_invalid_source_but_observed_forward_eof() {
    let p = Budget::new(1 << 20).unwrap();
    let (r, h, m) = parts(BAD_OFFSET, &p);
    let t = build(r, &p).unwrap();
    let mut c = t
        .direct_packet_cursor(
            &p,
            &h,
            &m,
            PacketLimits::default(),
            DirectContext {
                physical_length: 747,
            },
            DirectRecoveryProfile,
        )
        .unwrap();
    loop {
        if c.poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            == CursorStatus::Ready
        {
            break;
        }
    }
    let r = c.take_occurrence().unwrap();
    assert_eq!(r.declaration.range, 4843..4855);
    assert_eq!(r.demux.source_range, r.declaration.range);
    assert_eq!(r.read, DirectRead::ObservedForwardEof);
    assert!(r.range_error.is_some());
    assert!(r.declaration.require_within(m[0].clone()).is_err());
}
#[test]
fn invalid_index_positions_are_not_clipped_sorted_or_removed() {
    let p = Budget::new(1 << 20).unwrap();
    let (r, h, m) = parts(BAD_OFFSET, &p);
    let t = build(r, &p).unwrap();
    let mut c = t
        .direct_index_cursor(
            &p,
            &h,
            &m,
            IndexLimits::default(),
            DirectContext {
                physical_length: 747,
            },
            DirectRecoveryProfile,
        )
        .unwrap();
    let mut out = Vec::new();
    loop {
        match c
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            CursorStatus::Yield => {}
            CursorStatus::End => break,
            CursorStatus::Ready => {
                let e = c.take_entry().unwrap();
                out.push((
                    e.entry.position,
                    e.entry.size,
                    e.entry.timestamp,
                    e.range_error.is_some(),
                ));
            }
        }
    }
    assert_eq!(
        out,
        vec![
            (4843, 12, 0, true),
            (4855, 12, 1000, true),
            (699, 12, 2000, false),
            (711, 12, 3000, false),
            (723, 12, 4000, false),
            (735, 12, 5000, false)
        ]
    );
}
#[test]
fn far_seek_never_invents_clean_eof() {
    let b = include_bytes!("fixtures/recovery-v2/offset-4294967295.mov");
    let p = Budget::new(1 << 20).unwrap();
    let (r, h, m) = parts(b, &p);
    let t = build(r, &p).unwrap();
    let mut c = t
        .direct_packet_cursor(
            &p,
            &h,
            &m,
            PacketLimits::default(),
            DirectContext {
                physical_length: 747,
            },
            DirectRecoveryProfile,
        )
        .unwrap();
    loop {
        if c.poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            == CursorStatus::Ready
        {
            break;
        }
    }
    assert_eq!(
        c.take_occurrence().unwrap().read,
        DirectRead::UnreviewedSeekRefusal
    );
}
#[test]
fn zero_work_yields_cancel_is_sticky_even_ready() {
    let p = Budget::new(1 << 20).unwrap();
    let (r, h, m) = parts(BAD_DESC, &p);
    let t = build(r, &p).unwrap();
    let mut c = t
        .direct_packet_cursor(
            &p,
            &h,
            &m,
            PacketLimits::default(),
            DirectContext {
                physical_length: 747,
            },
            DirectRecoveryProfile,
        )
        .unwrap();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        c.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
        CursorStatus::Yield
    );
    assert_eq!(
        c.poll(&mut WorkBudget::new(1), &cancel).unwrap(),
        CursorStatus::Ready
    );
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        c.poll(&mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    );
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        c.poll(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    );
}
#[test]
fn source_limits_and_index_limits_are_not_waived() {
    let p = Budget::new(1 << 20).unwrap();
    let (r, h, m) = parts(BAD_DESC, &p);
    let t = build(r, &p).unwrap();
    let ctx = DirectContext {
        physical_length: 747,
    };
    assert!(matches!(
        t.direct_packet_cursor(
            &p,
            &h,
            &m,
            PacketLimits {
                max_source_samples: 5,
                ..PacketLimits::default()
            },
            ctx,
            DirectRecoveryProfile
        ),
        Err(Error::Limit { .. })
    ));
    let mut c = t
        .direct_index_cursor(
            &p,
            &h,
            &m,
            IndexLimits {
                max_entries: 1,
                ..IndexLimits::default()
            },
            ctx,
            DirectRecoveryProfile,
        )
        .unwrap();
    loop {
        if c.poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            == CursorStatus::Ready
        {
            c.take_entry().unwrap();
            break;
        }
    }
    let e = loop {
        match c.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(CursorStatus::Yield) => {}
            Err(e) => break e,
            other => panic!("{other:?}"),
        }
    };
    assert!(matches!(e, Error::Limit { .. }));
    assert_eq!(
        c.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(e)
    );
}
#[test]
fn direct_iteration_does_not_allocate_and_raw_backing_survives_close() {
    let p = Budget::new(1 << 20).unwrap();
    let initial = p.used();
    {
        let (r, h, m) = parts(BAD_DESC, &p);
        let t = build(r, &p).unwrap();
        let raw = t.raw().clone();
        let before = p.used();
        let mut c = t
            .direct_packet_cursor(
                &p,
                &h,
                &m,
                PacketLimits::default(),
                DirectContext {
                    physical_length: 747,
                },
                DirectRecoveryProfile,
            )
            .unwrap();
        loop {
            match c
                .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                CursorStatus::Yield => {}
                CursorStatus::End => break,
                CursorStatus::Ready => {
                    c.take_occurrence().unwrap();
                }
            }
        }
        assert_eq!(p.used(), before);
        drop(c);
        drop(t);
        assert!(raw.bytes().belongs_to(&p));
    }
    assert_eq!(p.used(), initial);
}

#[test]
fn direct_description_cannot_enter_the_run_cursor_surface() {
    let p = Budget::new(1 << 20).unwrap();
    let (raw, h, m) = parts(BAD_DESC, &p);
    let t = build(raw, &p).unwrap();
    let before = p.used();
    assert!(matches!(
        t.packet_cursor(&p, &h, &m, PacketLimits::default()),
        Err(Error::Unsupported(
            "initial-description recovery requires explicit v2 cursor"
        ))
    ));
    assert_eq!(p.used(), before);
    assert!(matches!(
        t.index_cursor(&p, &h, &m, IndexLimits::default()),
        Err(Error::Unsupported(
            "initial-description recovery requires explicit v2 cursor"
        ))
    ));
    assert_eq!(p.used(), before);
}
