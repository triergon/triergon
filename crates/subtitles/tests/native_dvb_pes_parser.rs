use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::dvb::{
    NativePesFraming, NativePesPacket, NativePesParser, NativePesProgress,
};

fn project(
    parser: &mut NativePesParser,
    raw: &Bytes,
    pts: Option<i128>,
    quantum: u64,
) -> NativePesPacket {
    let cancel = AtomicBool::new(false);
    parser.provide(raw, pts).unwrap();
    assert!(matches!(
        parser.step(&mut WorkBudget::new(0), &cancel).unwrap(),
        NativePesProgress::Yield
    ));
    for _ in 0..200_000 {
        match parser.step(&mut WorkBudget::new(quantum), &cancel).unwrap() {
            NativePesProgress::Yield => {}
            NativePesProgress::Packet(packet) => {
                assert!(packet.pes().shares_storage_with(raw));
                assert_eq!(packet.source_pts(), pts);
                return packet;
            }
            NativePesProgress::NeedPacket => panic!("pending DVB packet disappeared"),
        }
    }
    panic!("DVB parser stalled");
}

#[test]
fn pinned_ts_payloads_project_complete_segments_and_keep_original_bytes() {
    let ordinary = include_bytes!("../../mpegts/tests/data/dvb/dvb2-literals-fields.pes-data");
    let truncated = include_bytes!("../../mpegts/tests/data/dvb/dvb-truncated-segment.pes-data");
    let prefix = include_bytes!("native-dvb-fixtures/input/prefix101.dvb");
    for quantum in [1, 7, 4096] {
        let pool = Budget::new(4096).unwrap();
        let raw = Bytes::copy_from_slice(&pool, ordinary).unwrap();
        let mut parser = NativePesParser::new(&pool);
        let unknown = project(&mut parser, &raw, None, quantum);
        assert_eq!(unknown.framing(), NativePesFraming::Passthrough);
        assert_eq!(&**unknown.codec(), ordinary);
        assert_eq!(unknown.codec_pts(), None);
        let first = project(&mut parser, &raw, Some(90_000), quantum);
        assert_eq!(first.framing(), NativePesFraming::CompleteSegments);
        assert_eq!(&**first.codec(), &ordinary[2..ordinary.len() - 1]);
        assert!(first.codec().shares_storage_with(&raw));
        let repeat = project(&mut parser, &raw, Some(90_000), quantum);
        assert_eq!(repeat.framing(), NativePesFraming::Passthrough);
        assert_eq!(&**repeat.codec(), ordinary);
        let raw = Bytes::copy_from_slice(&pool, truncated).unwrap();
        let partial = project(&mut parser, &raw, Some(180_000), quantum);
        assert_eq!(&**partial.codec(), prefix);
        assert!(partial.codec().shares_storage_with(&raw));
        assert_eq!(&**partial.pes(), truncated);
        parser.reset();
        assert_eq!(
            project(&mut parser, &raw, Some(180_000), quantum).framing(),
            NativePesFraming::CompleteSegments,
        );
    }
}

#[test]
fn split_segment_reassembles_with_bounded_work_and_inherits_timestamp() {
    let pool = Budget::new(4096).unwrap();
    let mut parser = NativePesParser::new(&pool);
    let first = Bytes::copy_from_slice(&pool, &[0x20, 0, 0x0f, 0x13, 0, 1, 0, 3, 0xa1]).unwrap();
    let next = Bytes::copy_from_slice(&pool, &[0xa2, 0xa3, 0xff]).unwrap();
    let a = project(&mut parser, &first, Some(7), 1);
    assert_eq!(a.framing(), NativePesFraming::Passthrough);
    assert_eq!(&**a.codec(), &*first);
    let b = project(&mut parser, &next, None, 1);
    assert_eq!(b.framing(), NativePesFraming::CompleteSegments);
    assert_eq!(&**b.codec(), &[0x0f, 0x13, 0, 1, 0, 3, 0xa1, 0xa2, 0xa3]);
    assert_eq!(b.codec_pts(), Some(7));
    assert!(b.prior_tail().unwrap().shares_storage_with(&first));
    assert!(!b.codec().shares_storage_with(&next));
    let after_end = project(&mut parser, &next, None, 1);
    assert_eq!(after_end.codec_pts(), None);
    assert!(after_end.prior_tail().is_none());
}

#[test]
fn restart_disposes_prior_tail_and_allocation_pressure_preserves_the_pending_join() {
    let pool = Budget::new(1024).unwrap();
    let mut parser = NativePesParser::new(&pool);
    let first = Bytes::copy_from_slice(&pool, &[0x20, 0, 0x0f, 0x13, 0, 1, 0, 3, 0xa1]).unwrap();
    let next = Bytes::copy_from_slice(&pool, &[0xa2, 0xa3, 0xff]).unwrap();
    project(&mut parser, &first, Some(7), 1);
    let pressure = pool.reserve(pool.limit() - pool.used()).unwrap();
    let cancel = AtomicBool::new(false);
    parser.provide(&next, Some(7)).unwrap();
    assert!(matches!(
        parser.step(&mut WorkBudget::new(100), &cancel),
        Err(Error::Backpressure { .. })
    ));
    assert!(parser.provide(&next, Some(8)).is_err());
    drop(pressure);
    let NativePesProgress::Packet(output) =
        parser.step(&mut WorkBudget::new(100), &cancel).unwrap()
    else {
        panic!("the pending join did not resume");
    };
    assert_eq!(output.codec().len(), 9);
    project(&mut parser, &first, Some(8), 1);
    let fresh = project(&mut parser, &first, Some(9), 1);
    assert!(fresh.prior_tail().is_none());
    parser.provide(&next, Some(9)).unwrap();
    cancel.store(true, Ordering::Relaxed);
    assert!(matches!(
        parser.step(&mut WorkBudget::new(100), &cancel),
        Err(Error::Cancelled)
    ));
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        project(&mut parser, &next, None, 1).framing(),
        NativePesFraming::Passthrough
    );
}

#[test]
fn pinned_parser_split_controls_cover_headers_bodies_and_timestamp_continuation() {
    let pes = include_bytes!("../../mpegts/tests/data/dvb/dvb2-literals-fields.pes-data");
    let end = pes.len() - 1;
    for (cut, complete) in [(4, 0), (25, 11), (99, 79)] {
        for pts in [None, Some(90_000)] {
            let pool = Budget::new(4096).unwrap();
            let first = Bytes::copy_from_slice(&pool, &pes[..cut]).unwrap();
            let next = Bytes::copy_from_slice(&pool, &pes[cut..]).unwrap();
            let mut parser = NativePesParser::new(&pool);
            let a = project(&mut parser, &first, Some(90_000), 1);
            let expected = if complete == 0 {
                &pes[..cut]
            } else {
                &pes[2..2 + complete]
            };
            assert_eq!(a.codec().as_ref(), expected);
            let b = project(&mut parser, &next, pts, 1);
            assert_eq!(b.codec().as_ref(), &pes[2 + complete..end]);
            assert_eq!(b.codec_pts(), Some(90_000));
            assert_eq!(b.prior_tail().unwrap().as_ref(), &pes[2 + complete..cut]);
        }
    }
}

#[test]
fn native_group_ceiling_preserves_oversized_pes_and_allows_next_stamp() {
    let pool = Budget::new(256 * 1024).unwrap();
    let mut parser = NativePesParser::new(&pool);
    let mut data = vec![0x20, 0, 0x0f, 0x13, 0, 1, 0xff, 0xfa];
    data.resize(2 + 65_536, 0x5a);
    let exact = Bytes::copy_from_slice(&pool, &data).unwrap();
    let output = project(&mut parser, &exact, Some(1), 1);
    assert_eq!(output.codec().len(), 65_536);
    assert_eq!(output.framing(), NativePesFraming::CompleteSegments);
    assert!(output.codec().shares_storage_with(&exact));
    data.push(0xff);
    let over = Bytes::copy_from_slice(&pool, &data).unwrap();
    let output = project(&mut parser, &over, Some(2), 1);
    assert_eq!(output.framing(), NativePesFraming::Passthrough);
    assert_eq!(output.codec().as_ref(), data);
    assert_eq!(
        project(&mut parser, &exact, Some(3), 1).framing(),
        NativePesFraming::CompleteSegments
    );
}

#[test]
fn byte_fragmented_segment_copies_each_input_byte_once() {
    let pool = Budget::new(64 * 1024).unwrap();
    let mut parser = NativePesParser::new(&pool);
    let header = Bytes::copy_from_slice(&pool, &[0x20, 0, 0x0f, 0x13, 0, 1, 0x10, 0]).unwrap();
    let body_byte = Bytes::copy_from_slice(&pool, &[0x6a]).unwrap();
    let cancel = AtomicBool::new(false);
    project(&mut parser, &header, Some(9), 1);

    let mut charged = 0;
    for index in 0..4096 {
        parser.provide(&body_byte, None).unwrap();
        let mut work = WorkBudget::new(65_536);
        let NativePesProgress::Packet(packet) = parser.step(&mut work, &cancel).unwrap() else {
            panic!("funded DVB fragment stalled");
        };
        charged += 65_536 - work.remaining();
        assert!(
            charged < 16_384,
            "DVB continuation recopy grew with the accumulated prefix"
        );
        if index == 4095 {
            assert_eq!(packet.framing(), NativePesFraming::CompleteSegments);
            assert_eq!(&packet.codec()[..6], &header[2..]);
            assert_eq!(packet.codec()[6..], vec![0x6a; 4096]);
            assert_eq!(packet.codec_pts(), Some(9));
            assert_eq!(packet.prior_tail().unwrap().len(), 4101);
        } else {
            assert_eq!(packet.framing(), NativePesFraming::Passthrough);
            assert!(packet.codec().shares_storage_with(&body_byte));
        }
    }
}

#[test]
fn fragmented_group_growth_respects_the_group_capacity() {
    let pool = Budget::new(100_000).unwrap();
    let mut parser = NativePesParser::new(&pool);
    let header = Bytes::copy_from_slice(&pool, &[0x20, 0, 0x0f, 0x13, 0, 1, 0xff, 0xfa]).unwrap();
    let fragment = Bytes::copy_from_slice(&pool, &[0x6a; 512]).unwrap();
    project(&mut parser, &header, Some(9), 4096);

    let mut remaining = 65_530;
    while remaining > fragment.len() {
        let output = project(&mut parser, &fragment, None, 4096);
        assert_eq!(output.framing(), NativePesFraming::Passthrough);
        remaining -= fragment.len();
    }
    let last = fragment.slice(0..remaining).unwrap();
    let output = project(&mut parser, &last, None, 4096);
    assert_eq!(output.codec().len(), 65_536);
    assert_eq!(output.codec().retained_capacity(), 65_536);
    assert_eq!(output.prior_tail().unwrap().len(), 65_536 - remaining);
    assert_eq!(&output.codec()[..6], &header[2..]);
    assert!(output.codec()[6..].iter().all(|&byte| byte == 0x6a));
}
