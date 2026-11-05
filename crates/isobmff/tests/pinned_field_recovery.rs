mod common;
use common::{child, roots};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, Error, WorkBudget};
use triergon_isobmff::{
    Access, BoxView, BuildStatus, Limits, Mov9CursorStatus, Mov9NativeIndexLimits,
    Mov9PacketLimits, Mov9RecoveryTrackHeader, Mov9RunRecoveryBuilder, Mov9RunRecoveryTable,
    PinnedMovD32RegularRecoveryV1, SampleStep, TrackHeader,
};
fn source(input: &[u8], pool: &Budget) -> (BoxView, Mov9RecoveryTrackHeader, Vec<Range<u64>>) {
    let top = roots(pool, input).unwrap();
    let moov = top.iter().find(|v| v.header().kind == *b"moov").unwrap();
    let track = child(moov, b"trak").unwrap();
    let mdia = child(&track, b"mdia").unwrap();
    let h = Mov9RecoveryTrackHeader::parse(
        &child(&track, b"tkhd").unwrap(),
        &child(&mdia, b"mdhd").unwrap(),
        &child(&mdia, b"hdlr").unwrap(),
        PinnedMovD32RegularRecoveryV1,
    )
    .unwrap();
    let stbl = child(&child(&mdia, b"minf").unwrap(), b"stbl").unwrap();
    let media = top
        .iter()
        .filter(|b| b.header().kind == *b"mdat")
        .map(|b| b.header().payload_range().unwrap())
        .collect();
    (stbl, h, media)
}
fn table(stbl: BoxView, p: &Budget) -> Mov9RunRecoveryTable {
    let mut b = Mov9RunRecoveryBuilder::new(
        stbl,
        p.clone(),
        Limits::default(),
        PinnedMovD32RegularRecoveryV1,
    )
    .unwrap();
    while b
        .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
        != BuildStatus::Ready
    {}
    b.finish().unwrap()
}
#[test]
fn zero_scale_stays_zero_and_source_clock_cannot_be_constructed() {
    let p = Budget::new(1 << 20).unwrap();
    let (_, h, _) = source(
        include_bytes!("fixtures/pinned-field-recovery/mdhd-zero-baseline.mov"),
        &p,
    );
    assert_eq!(h.source().timescale, 0);
    assert!(!h.source_clock_valid());
    assert!(h.source().time_base().is_err());
    assert_eq!(h.native_projection_base().unwrap().denominator(), 1);
    assert_eq!(
        h.source_clock_error(),
        Some(Error::Invalid("zero media/movie timescale"))
    );
    assert_eq!(
        TrackHeader::parse(
            &h.source().raw_tkhd,
            &h.source().raw_mdhd,
            &h.source().raw_hdlr
        )
        .unwrap_err(),
        Error::Invalid("zero media/movie timescale")
    );
    assert_eq!(&h.source().raw_mdhd.payload()[12..16], &[0; 4]);
}
#[test]
fn vfr_zero_source_scale_retains_raw_ticks_and_native_base_only() {
    let p = Budget::new(1 << 20).unwrap();
    let (raw, h, media) = source(
        include_bytes!("fixtures/pinned-field-recovery/mdhd-zero-vfr.mov"),
        &p,
    );
    let t = table(raw, &p);
    let mut c = t
        .packet_cursor(&p, &h, &media, Mov9PacketLimits::default())
        .unwrap();
    let mut clocks = Vec::new();
    loop {
        match c
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Mov9CursorStatus::Yield => {}
            Mov9CursorStatus::End => break,
            Mov9CursorStatus::Ready => {
                let r = c.take_occurrence().unwrap();
                assert_eq!(r.source_timescale, 0);
                clocks.push((
                    r.source.dts_ticks.unwrap(),
                    r.native.dts.unwrap(),
                    r.source.duration_ticks,
                ));
            }
        }
    }
    assert_eq!(
        clocks,
        vec![
            (0, 0, 333),
            (333, 333, 667),
            (1000, 1000, 1001),
            (2001, 2001, 1),
            (2002, 2002, 2000),
            (4002, 4002, 17)
        ]
    );
}
#[test]
fn unknown_media_header_version_is_not_a_zero_scale_recovery() {
    let mut input =
        include_bytes!("fixtures/pinned-field-recovery/mdhd-zero-baseline.mov").to_vec();
    let at = input.windows(4).position(|b| b == b"mdhd").unwrap();
    input[at + 4] = 2;
    let p = Budget::new(1 << 20).unwrap();
    let top = roots(&p, &input).unwrap();
    let track = child(
        top.iter().find(|v| v.header().kind == *b"moov").unwrap(),
        b"trak",
    )
    .unwrap();
    let mdia = child(&track, b"mdia").unwrap();
    assert!(matches!(
        Mov9RecoveryTrackHeader::parse(
            &child(&track, b"tkhd").unwrap(),
            &child(&mdia, b"mdhd").unwrap(),
            &child(&mdia, b"hdlr").unwrap(),
            PinnedMovD32RegularRecoveryV1
        ),
        Err(Error::Unsupported("clock header version"))
    ));
}
#[test]
fn malformed_positive_stss_keeps_raw_order_and_source_access_unknown() {
    let p = Budget::new(1 << 20).unwrap();
    let (raw, _, _) = source(include_bytes!("fixtures/bad-stss-out-of-range.mov"), &p);
    let retained = raw.clone();
    let t = table(raw, &p);
    let d = t.sync_diagnostic().unwrap();
    assert_eq!((d.first_bad_row, d.source_value), (0, 100));
    assert_eq!(t.raw().bytes().as_ref(), retained.bytes().as_ref());
    let mut c = t.source_prefix_cursor();
    let mut n = 0;
    loop {
        match c.poll(&mut WorkBudget::new(1)).unwrap() {
            SampleStep::Yield => {}
            SampleStep::End => break,
            SampleStep::Sample(s) => {
                assert_eq!(s.access, Access::Unknown);
                n += 1;
            }
        }
    }
    assert_eq!(n, 6);
}
#[test]
fn ordered_walk_never_sorts_filters_or_rewinds_positive_sync_rows() {
    let p = Budget::new(1 << 20).unwrap();
    for (input, expected) in [
        (
            include_bytes!("fixtures/pinned-field-recovery/stss-100-4.mov").as_slice(),
            vec![0, 0, 0, 0, 0, 0],
        ),
        (
            include_bytes!("fixtures/pinned-field-recovery/stss-7-4.mov"),
            vec![0, 0, 0, 0, 0, 0],
        ),
        (
            include_bytes!("fixtures/pinned-field-recovery/stss-4-1.mov"),
            vec![0, 0, 0, 1, 0, 0],
        ),
        (
            include_bytes!("fixtures/pinned-field-recovery/stss-4-4.mov"),
            vec![0, 0, 0, 1, 0, 0],
        ),
        (
            include_bytes!("fixtures/pinned-field-recovery/stss-1-100.mov"),
            vec![1, 0, 0, 0, 0, 0],
        ),
    ] {
        let (raw, h, media) = source(input, &p);
        let t = table(raw, &p);
        let mut c = t
            .packet_cursor(&p, &h, &media, Mov9PacketLimits::default())
            .unwrap();
        let mut flags = Vec::new();
        loop {
            match c
                .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                Mov9CursorStatus::Yield => {}
                Mov9CursorStatus::End => break,
                Mov9CursorStatus::Ready => flags.push(c.take_occurrence().unwrap().native.flags),
            }
        }
        assert_eq!(flags, expected);
    }
}
#[test]
fn no_key_start_index_is_only_native_projection_and_has_zero_initial_distance() {
    let p = Budget::new(1 << 20).unwrap();
    let (raw, h, media) = source(include_bytes!("fixtures/bad-stss-out-of-range.mov"), &p);
    let t = table(raw, &p);
    let mut c = t
        .index_cursor(&p, &h, &media, Mov9NativeIndexLimits::default())
        .unwrap();
    let mut n = 0;
    loop {
        match c
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Mov9CursorStatus::Yield => {}
            Mov9CursorStatus::End => break,
            Mov9CursorStatus::Ready => {
                let e = c.take_entry().unwrap();
                assert_eq!(e.flags, 0);
                assert_eq!(e.min_distance, n);
                assert_eq!(e.source.access, Access::Unknown);
                n += 1;
            }
        }
    }
    assert_eq!(n, 6);
}
#[test]
fn recovery_native_index_limit_is_sticky() {
    let p = Budget::new(1 << 20).unwrap();
    let (raw, h, media) = source(include_bytes!("fixtures/bad-stss-out-of-range.mov"), &p);
    let t = table(raw, &p);
    let mut c = t
        .index_cursor(
            &p,
            &h,
            &media,
            Mov9NativeIndexLimits {
                max_entries: 1,
                ..Mov9NativeIndexLimits::default()
            },
        )
        .unwrap();
    loop {
        if c.poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            == Mov9CursorStatus::Ready
        {
            c.take_entry().unwrap();
            break;
        }
    }
    let err = loop {
        match c.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(Mov9CursorStatus::Yield) => {}
            Err(e) => break e,
            other => panic!("{other:?}"),
        }
    };
    assert!(matches!(err, Error::Limit { .. }));
    assert_eq!(
        c.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(err)
    );
}
#[test]
fn source_scale_one_is_not_diagnosed_as_native_fallback() {
    let p = Budget::new(1 << 20).unwrap();
    let (_, h, _) = source(
        include_bytes!("fixtures/pinned-field-recovery/mdhd-one-baseline.mov"),
        &p,
    );
    assert!(h.source_clock_valid());
    assert!(h.source_clock_error().is_none());
    assert_eq!(h.source().timescale, 1);
}
