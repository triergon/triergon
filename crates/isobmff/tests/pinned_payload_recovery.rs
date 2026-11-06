use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{
    Budget, Error, InputCursor, SourceCapabilities, SourceFailure, SourceId, Step, WorkBudget,
};
use triergon_isobmff::{
    Access, BoxEvent, BoxHeader, BoxReader, MediaDataPolicy, Mov9PartialSampleReader,
    Mov9PayloadBounds, PinnedMovD32RegularRecoveryV1, Sample, SampleReader,
};
fn sample(range: Range<u64>) -> Sample {
    Sample {
        ordinal: 6,
        description_index: 1,
        range,
        dts_ticks: Some(5000),
        cts_ticks: Some(5001),
        duration_ticks: 1000,
        composition_offset: None,
        access: Access::NonSync,
        partial_sync: false,
        dependency: None,
        fragment_flags: None,
    }
}
fn input(p: &Budget, length: Option<u64>) -> InputCursor {
    InputCursor::new(
        p,
        SourceId(71),
        SourceCapabilities {
            length,
            random_access: true,
        },
        4,
    )
    .unwrap()
}
#[test]
fn strict_range_and_header_rejections_are_not_relaxed() {
    let p = Budget::new(4096).unwrap();
    let mut i = input(&p, Some(14));
    assert!(matches!(
        SampleReader::new(&mut i, sample(8..20), 8..20, 12),
        Err(Error::Invalid("sample exceeds source length"))
    ));
    assert_eq!(
        BoxHeader::parse(&[0, 0, 0, 20, b'm', b'd', b'a', b't'], 0, Some(14), true),
        Err(Error::Truncated)
    );
}
fn header(recovery: bool, kind: [u8; 4]) -> Result<BoxHeader, Error> {
    let p = Budget::new(4096).unwrap();
    let mut i = input(&p, Some(14));
    let generation = i.generation();
    let mut r = if recovery {
        BoxReader::with_media_data_policy(&i, 1024, MediaDataPolicy::PreserveTerminalDeclaration)?
    } else {
        BoxReader::new(&i, 1024)?
    };
    let bytes = [
        0, 0, 0, 20, kind[0], kind[1], kind[2], kind[3], 9, 8, 7, 6, 5, 4,
    ];
    for _ in 0..64 {
        match r.poll(&mut i, &mut WorkBudget::new(1), &AtomicBool::new(false))? {
            Step::NeedInput(q) => {
                let at = q.offset as usize;
                assert!(at < bytes.len());
                i.provide_inline(q.source, q.token, q.offset, &bytes[at..at + 1])?;
            }
            Step::Yield => {}
            Step::Ready(BoxEvent::Header(h)) => {
                assert_eq!(i.generation(), generation);
                return Ok(h);
            }
            other => panic!("{other:?}"),
        }
    }
    panic!("bounded header progress");
}
#[test]
fn recovery_header_preserves_declared_end_beyond_physical_without_issuing_invalid_range() {
    let h = header(true, *b"mdat").unwrap();
    assert_eq!(h.size, Some(20));
    assert_eq!(h.payload_range().unwrap(), 8..20);
    assert_eq!(header(false, *b"mdat"), Err(Error::Truncated));
}
#[test]
fn oversized_metadata_is_never_an_eof_media_recovery() {
    assert_eq!(header(true, *b"moov"), Err(Error::Truncated));
}
#[test]
fn physical_bounds_do_not_rewrite_declared_range_or_create_missing_bytes() {
    let s = sample(8..20);
    let b =
        Mov9PayloadBounds::from_source(&s, 8..20, 14, 12, PinnedMovD32RegularRecoveryV1).unwrap();
    assert_eq!(b.declared, 8..20);
    assert_eq!(b.available, Some(8..14));
    assert!(b.incomplete_at_known_eof);
    assert_eq!(s.range, 8..20);
    let b =
        Mov9PayloadBounds::from_source(&s, 8..20, 8, 12, PinnedMovD32RegularRecoveryV1).unwrap();
    assert_eq!(b.available, None);
}
#[test]
fn bounds_keep_declared_limit_and_media_validation_before_clamping() {
    let s = sample(8..20);
    assert!(matches!(
        Mov9PayloadBounds::from_source(&s, 8..20, 9, 11, PinnedMovD32RegularRecoveryV1),
        Err(Error::Limit {
            requested: 12,
            maximum: 11
        })
    ));
    assert!(
        Mov9PayloadBounds::from_source(&s, 9..20, 14, 12, PinnedMovD32RegularRecoveryV1).is_err()
    );
    let p = Budget::new(4096).unwrap();
    let mut i = input(&p, None);
    assert!(matches!(
        Mov9PartialSampleReader::new(&mut i, s, 8..20, 12, PinnedMovD32RegularRecoveryV1),
        Err(Error::Unsupported(_))
    ));
}
#[test]
fn partial_bytes_use_exact_owner_and_source_fields_survive_input_and_reader_drop() {
    let p = Budget::new(4096).unwrap();
    let initial = p.used();
    let mut i = input(&p, Some(14));
    let source = sample(8..20);
    let mut r = Mov9PartialSampleReader::new(
        &mut i,
        source.clone(),
        8..20,
        12,
        PinnedMovD32RegularRecoveryV1,
    )
    .unwrap();
    let packet = loop {
        match r
            .poll(&mut i, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::NeedInput(q) => i
                .provide_inline(q.source, q.token, q.offset, &[q.offset as u8])
                .unwrap(),
            Step::Yield => {}
            Step::Ready(p) => break p,
            Step::End(_) => panic!("not packet completion"),
        }
    };
    assert_eq!(packet.source_sample, source);
    assert_eq!(packet.payload.as_ref(), &[8, 9, 10, 11, 12, 13]);
    assert_eq!(packet.bounds.available, Some(8..14));
    assert!(packet.payload.belongs_to(&p));
    drop(r);
    drop(i);
    let retained = packet.payload.clone();
    assert!(retained.shares_storage_with(&packet.payload));
    drop(packet);
    assert_eq!(retained.as_ref(), &[8, 9, 10, 11, 12, 13]);
    drop(retained);
    assert_eq!(p.used(), initial);
}
#[test]
fn cancellation_and_transport_failure_remain_sticky_errors() {
    let p = Budget::new(4096).unwrap();
    let mut i = input(&p, Some(14));
    let mut r = Mov9PartialSampleReader::new(
        &mut i,
        sample(8..20),
        8..20,
        12,
        PinnedMovD32RegularRecoveryV1,
    )
    .unwrap();
    assert!(matches!(
        r.poll(&mut i, &mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        r.poll(&mut i, &mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Cancelled)
    ));
    let mut i = input(&p, Some(14)); // independent transport-failure control
    let mut r = Mov9PartialSampleReader::new(
        &mut i,
        sample(8..20),
        8..20,
        12,
        PinnedMovD32RegularRecoveryV1,
    )
    .unwrap();
    let q = i.request();
    i.fail(q.source, q.token, SourceFailure::Transport).unwrap();
    assert!(matches!(
        r.poll(&mut i, &mut WorkBudget::new(10), &AtomicBool::new(false)),
        Err(Error::Transport)
    ));
}
#[test]
fn partial_freeze_backpressure_retains_same_transaction_and_real_bytes() {
    let p = Budget::new(4096).unwrap();
    let mut i = input(&p, Some(14));
    let mut r = Mov9PartialSampleReader::new(
        &mut i,
        sample(8..20),
        8..20,
        12,
        PinnedMovD32RegularRecoveryV1,
    )
    .unwrap();
    let q = i.request();
    i.provide_inline(q.source, q.token, q.offset, &[8, 9, 10, 11])
        .unwrap();
    loop {
        match r
            .poll(&mut i, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::NeedInput(q) => {
                i.provide_inline(q.source, q.token, q.offset, &[12, 13])
                    .unwrap();
                break;
            }
            Step::Yield => {}
            other => panic!("{other:?}"),
        }
    }
    assert!(matches!(
        r.poll(&mut i, &mut WorkBudget::new(2), &AtomicBool::new(false))
            .unwrap(),
        Step::Yield
    ));
    assert_eq!(i.position(), 14);
    let token = i.token();
    let held = p.reserve(p.available()).unwrap();
    assert!(matches!(
        r.poll(&mut i, &mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    drop(held);
    assert_eq!(i.token(), token);
    let Step::Ready(packet) = r
        .poll(&mut i, &mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
    else {
        panic!("pending real bytes lost")
    };
    assert_eq!(packet.payload.as_ref(), &[8, 9, 10, 11, 12, 13]);
}
