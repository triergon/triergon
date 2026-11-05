#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputWindow, SourceCapabilities, SourceFailure, SourceId,
    Step, WorkBudget,
};
use triergon_framing::{
    AacAdtsCarrier, AacAdtsCarrierBoundary, AacAdtsCarrierLimits, AacAdtsCarrierReader,
    AacAscParser, AacAscScopeParser, AacConfigLimits, AacDecoderChannelReason,
    AacDecoderConfigDecision, AacDecoderConfigPolicy, AacDecoderConfigRefusal, AacSource,
    BuildStep, HvcConfigLimits, HvcConfigParser, HvcConfigSource, HvcConfiguration,
    HvcDecoderConfigOutput, HvcDecoderConfigWriter, aac_decoder_channel_view,
    aac_decoder_configuration_view,
};

fn hvcc(kind: u8, empty: bool) -> Vec<u8> {
    let mut raw = vec![
        1,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0xf0,
        0,
        0xfc,
        0xfc,
        0xf8,
        0xf8,
        0,
        0,
        3,
        1,
        kind,
        0,
        u8::from(!empty),
    ];
    if !empty {
        raw.extend_from_slice(&[0, 2, kind << 1, 1]);
    }
    raw
}
fn config(pool: &Budget, raw: &[u8]) -> HvcConfiguration {
    let mut p = HvcConfigParser::new(
        pool,
        HvcConfigSource {
            source: SourceId(12),
            generation: 19,
            offset: 73,
        },
        Bytes::copy_from_slice(pool, raw).unwrap(),
        HvcConfigLimits {
            maximum_record_bytes: 10000,
            maximum_arrays: 255,
            maximum_nals: 1000,
            maximum_nal_bytes: 65535,
        },
    )
    .unwrap();
    for _ in 0..10000 {
        match p
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            BuildStep::Ready(o) => return o,
            BuildStep::Yield => {}
            _ => panic!(),
        }
    }
    panic!()
}
fn convert(pool: &Budget, raw: &[u8], work: u64) -> HvcDecoderConfigOutput {
    let mut w = HvcDecoderConfigWriter::new(pool, config(pool, raw), 10000).unwrap();
    for _ in 0..10000 {
        match w
            .step(&mut WorkBudget::new(work), &AtomicBool::new(false))
            .unwrap()
        {
            BuildStep::Ready(o) => return o,
            BuildStep::Yield => {}
            _ => panic!(),
        }
    }
    panic!()
}
#[test]
fn decoder_array_policy_is_exhaustive_and_never_filters_unknown_arrays() {
    let pool = Budget::new(100000).unwrap();
    for kind in 0..64 {
        for empty in [false, true] {
            let raw = hvcc(kind, empty);
            let o = convert(&pool, &raw, 1);
            let allowed = matches!(kind, 32 | 33 | 34 | 39 | 40);
            assert_eq!(o.refusal().is_none(), allowed, "type{kind} empty{empty}");
            assert_eq!(o.configuration().source_bytes().as_ref(), raw);
            if !allowed {
                assert_eq!(o.bytes().as_ref(), raw);
                assert_eq!(o.refusal().unwrap().declared_nal_type, kind);
            } else {
                assert_eq!(
                    o.bytes().as_ref(),
                    if empty {
                        vec![]
                    } else {
                        vec![0, 0, 0, 1, kind << 1, 1]
                    }
                );
            }
        }
    }
}
#[test]
fn decoder_refusal_keeps_the_first_array_index_and_all_later_source_bytes() {
    let pool = Budget::new(100000).unwrap();
    let mut raw = hvcc(32, false);
    raw[22] = 3;
    raw.extend_from_slice(&hvcc(55, false)[23..]);
    raw.extend_from_slice(&hvcc(33, false)[23..]);
    let o = convert(&pool, &raw, 3);
    assert_eq!(o.refusal().unwrap().array_index, 1);
    assert_eq!(o.bytes().as_ref(), raw);
    assert_eq!(o.configuration().arrays().len(), 3);
    assert_eq!(o.configuration().nals().len(), 3);
}
#[test]
fn decoder_zero_work_cancellation_and_completed_state_are_bounded() {
    let pool = Budget::new(100000).unwrap();
    let raw = hvcc(32, false);
    let mut w = HvcDecoderConfigWriter::new(&pool, config(&pool, &raw), 10000).unwrap();
    assert!(matches!(
        w.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(BuildStep::Yield)
    ));
    assert!(matches!(
        w.step(&mut WorkBudget::new(1), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        w.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
        Err(Error::Cancelled)
    ));
    drop(w);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
    let mut w = HvcDecoderConfigWriter::new(&pool, config(&pool, &raw), 10000).unwrap();
    assert!(matches!(
        w.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
        Ok(BuildStep::Ready(_))
    ));
    assert!(matches!(
        w.step(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Ok(BuildStep::Complete)
    ));
}

fn adts(payload: &[u8], blocks: u8) -> Vec<u8> {
    let n = 7 + payload.len();
    let mut v = vec![
        0xff,
        0xf1,
        0x4c,
        0x80 | ((n >> 11) as u8),
        (n >> 3) as u8,
        ((n & 7) as u8) << 5 | 31,
        0xfc | blocks,
    ];
    v.extend_from_slice(payload);
    v
}
fn reader(pool: &Budget, length: Option<u64>, maximum: usize) -> AacAdtsCarrierReader {
    AacAdtsCarrierReader::new(
        pool,
        SourceId(73),
        SourceCapabilities {
            length,
            random_access: true,
        },
        AacAdtsCarrierLimits {
            maximum_packet_bytes: maximum,
            maximum_packets: 100,
            maximum_input_window: 8192,
        },
    )
    .unwrap()
}
fn collect(
    raw: &[u8],
    known: bool,
    eof: bool,
    window: usize,
    work: u64,
) -> (Vec<AacAdtsCarrier>, bool) {
    let pool = Budget::new(100000).unwrap();
    let mut r = reader(&pool, known.then_some(raw.len() as u64), 10000);
    let mut out = Vec::new();
    let mut finished = false;
    for _ in 0..100000 {
        match r
            .step(&mut WorkBudget::new(work), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(o) => out.push(o),
            Step::Yield => {}
            Step::End(e) => {
                assert_eq!(e.offset, raw.len() as u64);
                return (out, true);
            }
            Step::NeedInput(q) => {
                let at = q.offset as usize;
                if at < raw.len() {
                    let end = (at + window).min(raw.len());
                    r.provide_owned(
                        InputWindow::new(
                            q.source,
                            q.token,
                            q.offset,
                            Bytes::copy_from_slice(&pool, &raw[at..end]).unwrap(),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                } else if eof && !finished {
                    r.finish(q.source, q.token).unwrap();
                    finished = true;
                } else {
                    return (out, false);
                }
            }
        }
    }
    panic!("carrier did not terminate")
}
#[test]
fn explicit_eof_preserves_every_trailing_byte_as_fragment_not_audio() {
    let a = adts(&[1, 2, 3], 0);
    for tail in [
        vec![0xff],
        vec![0xff, 0xf0, 0x4c],
        vec![0, 0xf1, 0x4c, 0x80, 1, 0x5f, 0xfc, 1, 2, 3],
    ] {
        let raw = [a.clone(), tail.clone()].concat();
        for known in [false, true] {
            for window in [1, 7, 4096] {
                for work in [1, 4096] {
                    let (o, end) = collect(&raw, known, true, window, work);
                    assert!(end);
                    assert_eq!(o.len(), 2);
                    assert_eq!(o[0].source_bytes().as_ref(), a);
                    assert_eq!(o[1].source_bytes().as_ref(), tail);
                    assert_eq!(o[1].boundary(), AacAdtsCarrierBoundary::ExplicitEofFragment);
                    assert!(o[1].selected_error().is_some());
                    assert_eq!(o[1].source().offset, a.len() as u64);
                }
            }
        }
    }
}
#[test]
fn complete_unselected_block_layout_is_carried_without_guessing_crc_or_payload() {
    for blocks in 1..=3 {
        let raw = adts(&[1, 2, 3, 4, 5], blocks);
        let (o, end) = collect(&raw, false, true, 1, 1);
        assert!(end);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].source_bytes().as_ref(), raw);
        assert_eq!(o[0].header().unwrap().raw_blocks_minus_one(), blocks);
        assert_eq!(
            o[0].boundary(),
            AacAdtsCarrierBoundary::DeclaredEnvelope { header_offset: 0 }
        );
        assert!(matches!(o[0].selected_error(), Some(Error::Unsupported(_))));
    }
}
#[test]
fn short_declared_body_waits_for_real_eof_even_at_known_length() {
    let mut raw = adts(&[1, 2, 3, 4, 5], 0);
    raw.pop();
    for known in [false, true] {
        let (o, end) = collect(&raw, known, false, 4096, 1);
        assert!(o.is_empty());
        assert!(!end);
        let (o, end) = collect(&raw, known, true, 4096, 1);
        assert!(end);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].source_bytes().as_ref(), raw);
        assert_eq!(o[0].selected_error(), Some(Error::Truncated));
    }
}
#[test]
fn garbage_prefix_resynchronizes_without_dropping_bytes_or_splitting_embedded_payload() {
    let embedded = adts(&[8, 9], 0);
    let a = adts(&embedded, 0);
    let raw = [vec![0, 1, 2, 3, 4], a.clone()].concat();
    let (o, end) = collect(&raw, false, true, 1, 1);
    assert!(end);
    assert_eq!(o.len(), 1);
    assert_eq!(o[0].source_bytes().as_ref(), raw);
    assert_eq!(
        o[0].boundary(),
        AacAdtsCarrierBoundary::DeclaredEnvelope { header_offset: 5 }
    );
    assert!(o[0].selected_error().is_some());
}
#[test]
fn failure_never_flushes_pending_fragment_and_reset_invalidates_old_callbacks() {
    let pool = Budget::new(10000).unwrap();
    let mut r = reader(&pool, None, 1000);
    let old = r.token();
    r.provide_inline(SourceId(73), old.clone(), 0, &[0xff, 0xf1, 0x4c])
        .unwrap();
    loop {
        match r
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::NeedInput(q) => {
                r.fail(q.source, q.token, SourceFailure::Transport).unwrap();
                break;
            }
            Step::Yield => {}
            _ => panic!(),
        }
    }
    assert!(matches!(
        r.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
        Err(Error::Transport)
    ));
    let q = r.reset(0).unwrap();
    assert_ne!(q.token.generation(), old.generation());
    assert!(matches!(
        r.provide_inline(SourceId(73), old, 0, &[]),
        Err(Error::StaleGeneration { .. })
    ));
    r.finish(q.source, q.token).unwrap();
    assert!(matches!(
        r.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Ok(Step::End(_))
    ));
    drop(r);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn pending_fragment_at_exact_byte_limit_still_needs_explicit_eof() {
    let pool = Budget::new(10000).unwrap();
    let mut r = reader(&pool, None, 3);
    let q = r.request();
    r.provide_inline(q.source, q.token, 0, &[0xff, 0xf1, 0x4c])
        .unwrap();
    loop {
        match r
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::NeedInput(q) => {
                assert_eq!(q.offset, 3);
                r.finish(q.source, q.token).unwrap();
                break;
            }
            Step::Yield => {}
            _ => panic!(),
        }
    }
    loop {
        match r
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(o) => {
                assert_eq!(o.source_bytes().as_ref(), [0xff, 0xf1, 0x4c]);
                break;
            }
            Step::Yield => {}
            _ => panic!(),
        }
    }
}
#[test]
fn carrier_capacity_tracks_actual_progress_and_retained_output_releases() {
    let pool = Budget::new(1024).unwrap();
    let raw = adts(&[3; 112], 0);
    let mut r = reader(&pool, None, 100000);
    let q = r.request();
    r.provide_owned(
        InputWindow::new(
            q.source,
            q.token,
            0,
            Bytes::copy_from_slice(&pool, &raw).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let o;
    loop {
        match r
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(v) => {
                o = v;
                break;
            }
            Step::Yield => {}
            _ => panic!(),
        }
    }
    assert_eq!(o.source_bytes().as_ref(), raw);
    assert!(o.source_bytes().retained_storage_bytes() < 512);
    drop(r);
    assert!(pool.used() > Budget::control_storage_bytes());
    drop(o);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn parser_packet_view_preserves_recovery_prefix_separately_before_incomplete_body() {
    let pool = Budget::new(10000).unwrap();
    let mut r = AacAdtsCarrierReader::for_parser_packets(
        &pool,
        SourceId(73),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        AacAdtsCarrierLimits {
            maximum_packet_bytes: 1000,
            maximum_packets: 100,
            maximum_input_window: 8192,
        },
    )
    .unwrap();
    let complete = adts(&[1, 2, 3], 0);
    let raw = [vec![0, 1, 2], complete[..7].to_vec()].concat();
    let q = r.request();
    r.provide_inline(q.source, q.token, 0, &raw).unwrap();
    let mut output = None;
    for _ in 0..1000 {
        match r
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(o) => {
                assert_eq!(o.source_bytes().as_ref(), [0, 1, 2]);
                assert_eq!(o.boundary(), AacAdtsCarrierBoundary::FollowingEnvelope);
                output = Some(o);
            }
            Step::NeedInput(q) => {
                assert!(output.is_some());
                assert_eq!(q.offset, 10);
                r.finish(q.source, q.token).unwrap();
                break;
            }
            Step::Yield => {}
            _ => panic!(),
        }
    }
    loop {
        match r
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(o) => {
                assert_eq!(o.source_bytes().as_ref(), &complete[..7]);
                assert_eq!(o.source().offset, 3);
                assert_eq!(o.boundary(), AacAdtsCarrierBoundary::ExplicitEofFragment);
                break;
            }
            Step::Yield => {}
            _ => panic!(),
        }
    }
}

fn source() -> AacSource {
    AacSource {
        source: SourceId(9),
        generation: 12,
        offset: 21,
    }
}
fn limits() -> AacConfigLimits {
    AacConfigLimits {
        maximum_record_bytes: 10000,
        maximum_pce_elements: 70,
        maximum_comment_bytes: 255,
    }
}
#[test]
fn dual_scope_keeps_all_truncated_declared_bits_and_parses_real_physical_bytes() {
    let pool = Budget::new(100000).unwrap();
    for bits in 0..16 {
        let mut p = AacAscScopeParser::new(
            &pool,
            source(),
            Bytes::copy_from_slice(&pool, &[0x12, 0x10]).unwrap(),
            bits,
            limits(),
        )
        .unwrap();
        loop {
            match p
                .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                BuildStep::Ready(o) => {
                    assert_eq!(o.declared().error(), Some(Error::Truncated));
                    assert!(o.physical().parsed().is_some());
                    assert!(!o.same_extent());
                    assert_eq!(o.source_bytes().as_ref(), [0x12, 0x10]);
                    break;
                }
                BuildStep::Yield => {}
                _ => panic!(),
            }
        }
    }
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn equal_scope_does_not_duplicate_pce_or_invent_an_unknown_bit_extent() {
    let pool = Budget::new(100000).unwrap();
    let mut p = AacAscScopeParser::new(
        &pool,
        source(),
        Bytes::copy_from_slice(&pool, &[0x12, 0x10]).unwrap(),
        16,
        limits(),
    )
    .unwrap();
    loop {
        match p
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            BuildStep::Ready(o) => {
                assert!(o.same_extent());
                assert!(std::ptr::eq(o.declared(), o.physical()));
                break;
            }
            BuildStep::Yield => {}
            _ => panic!(),
        }
    }
    assert!(matches!(
        AacAscScopeParser::new(
            &pool,
            source(),
            Bytes::copy_from_slice(&pool, &[0x12, 0x10]).unwrap(),
            17,
            limits()
        ),
        Err(Error::Invalid(_))
    ));
}
#[test]
fn decoder_channel_choice_keeps_signalled_mono_and_raw_bytes_unchanged() {
    let pool = Budget::new(100000).unwrap();
    let raw = [0x2c, 0x0a, 0x88, 0x00];
    let mut p = AacAscParser::new(
        &pool,
        source(),
        Bytes::copy_from_slice(&pool, &raw).unwrap(),
        limits(),
    )
    .unwrap();
    loop {
        match p
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            BuildStep::Ready(o) => {
                let f = o.facts();
                assert_eq!(f.core_channels, 1);
                assert_eq!(f.signalled_render_channels(), 1);
                let view = aac_decoder_channel_view(f);
                assert_eq!(view.channels, 2);
                assert_eq!(view.reason, AacDecoderChannelReason::SbrMonoDecoderChoice);
                assert_eq!(o.source_bytes().as_ref(), raw);
                break;
            }
            BuildStep::Yield => {}
            _ => panic!(),
        }
    }
}
#[test]
fn carrier_overflow_does_not_consume_the_byte_beyond_the_bound() {
    let pool = Budget::new(10000).unwrap();
    let mut r = reader(&pool, None, 3);
    let q = r.request();
    r.provide_inline(q.source, q.token, 0, &[0, 1, 2, 3])
        .unwrap();
    loop {
        match r.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Err(Error::Limit {
                requested: 4,
                maximum: 3,
            }) => {
                assert_eq!(r.position(), 3);
                break;
            }
            Ok(Step::Yield) => {}
            _ => panic!(),
        }
    }
    assert!(matches!(
        r.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
        Err(Error::Limit {
            requested: 4,
            maximum: 3
        })
    ));
    drop(r);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn carrier_cancel_after_real_finish_does_not_publish_pending_bytes() {
    let pool = Budget::new(10000).unwrap();
    let mut r = reader(&pool, None, 1000);
    let q = r.request();
    r.provide_inline(q.source, q.token.clone(), 0, &[0xff, 0xf1, 0x4c])
        .unwrap();
    r.finish(q.source, q.token).unwrap();
    assert!(matches!(
        r.step(&mut WorkBudget::new(1000), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        r.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
        Err(Error::Cancelled)
    ));
    drop(r);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn carrier_freeze_backpressure_keeps_real_work_and_position_for_retry() {
    let pool = Budget::new(2000).unwrap();
    let raw = adts(&[3; 112], 0);
    let mut r = reader(&pool, None, 10000);
    let q = r.request();
    r.provide_owned(
        InputWindow::new(
            q.source,
            q.token,
            0,
            Bytes::copy_from_slice(&pool, &raw).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    loop {
        match r
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Yield if r.position() == raw.len() as u64 => break,
            Step::Yield => {}
            _ => panic!(),
        }
    }
    let mut retained = BudgetVec::<u8>::new(&pool);
    retained.try_reserve_exact(pool.available() - 8).unwrap();
    assert!(matches!(
        r.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    let position = r.position();
    assert!(matches!(
        r.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(Step::Yield)
    ));
    assert_eq!(r.position(), position);
    drop(retained);
    match r
        .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
    {
        Step::Ready(o) => assert_eq!(o.source_bytes().as_ref(), raw),
        _ => panic!(),
    }
    drop(r);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn carrier_known_length_mismatch_is_failure_not_a_final_fragment() {
    let pool = Budget::new(10000).unwrap();
    let mut r = reader(&pool, Some(20), 1000);
    let q = r.request();
    r.provide_inline(q.source, q.token.clone(), 0, &[0xff, 0xf1, 0x4c])
        .unwrap();
    assert_eq!(r.finish(q.source, q.token), Err(Error::Truncated));
    assert!(matches!(
        r.step(&mut WorkBudget::new(100), &AtomicBool::new(false)),
        Err(Error::Truncated)
    ));
}
fn parse_asc(pool: &Budget, raw: &[u8]) -> triergon_framing::AacAudioSpecificConfig {
    let mut p = AacAscParser::new(
        pool,
        source(),
        Bytes::copy_from_slice(pool, raw).unwrap(),
        limits(),
    )
    .unwrap();
    loop {
        match p
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            BuildStep::Ready(o) => return o,
            BuildStep::Yield => {}
            _ => panic!(),
        }
    }
}
#[test]
fn explicit_decoder_limits_do_not_reject_or_rewrite_source_frequency_and_pce() {
    let pool = Budget::new(100000).unwrap();
    let p = AacDecoderConfigPolicy {
        allow_explicit_core_frequency: false,
        maximum_channels: 64,
    };
    let f = parse_asc(&pool, &[0x17, 0x80, 0x5d, 0xc0, 0x38]);
    assert_eq!(f.facts().core_frequency.hz, 48000);
    assert_eq!(
        aac_decoder_configuration_view(&f, p).unwrap(),
        AacDecoderConfigDecision::Refused(AacDecoderConfigRefusal::ExplicitFrequency)
    );
    assert!(matches!(
        aac_decoder_configuration_view(
            &f,
            AacDecoderConfigPolicy {
                allow_explicit_core_frequency: true,
                ..p
            }
        ),
        Ok(AacDecoderConfigDecision::Eligible(_))
    ));
    let c = parse_asc(&pool, include_bytes!("aac-fixtures/compat-pce-93.bin"));
    assert_eq!(c.facts().core_channels, 93);
    assert_eq!(
        aac_decoder_configuration_view(&c, p).unwrap(),
        AacDecoderConfigDecision::Refused(AacDecoderConfigRefusal::Channels {
            declared: 93,
            maximum: 64
        })
    );
    assert!(matches!(
        aac_decoder_configuration_view(
            &c,
            AacDecoderConfigPolicy {
                maximum_channels: 93,
                ..p
            }
        ),
        Ok(AacDecoderConfigDecision::Eligible(_))
    ));
}
#[test]
fn explicit_ps_false_and_lc_mono_do_not_acquire_the_sbr_decoder_choice() {
    let pool = Budget::new(100000).unwrap();
    for raw in [
        &[0x12, 0x08][..],
        &[0x11, 0x88, 0x56, 0xe5, 0x85, 0x48, 0x00][..],
    ] {
        let o = parse_asc(&pool, raw);
        let v = aac_decoder_channel_view(o.facts());
        assert_eq!(v.channels, 1);
        assert_eq!(v.reason, AacDecoderChannelReason::Signalled);
    }
}
#[test]
fn scope_cancellation_is_sticky_and_releases_both_parser_owners() {
    let pool = Budget::new(10000).unwrap();
    let mut p = AacAscScopeParser::new(
        &pool,
        source(),
        Bytes::copy_from_slice(&pool, &[0x12, 0x10]).unwrap(),
        1,
        limits(),
    )
    .unwrap();
    assert!(matches!(
        p.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(BuildStep::Yield)
    ));
    assert!(matches!(
        p.step(&mut WorkBudget::new(1000), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        p.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
        Err(Error::Cancelled)
    ));
    drop(p);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn two_scope_pce_live_set_that_cannot_fit_is_limit_not_permanent_backpressure() {
    let mut raw = include_bytes!("aac-fixtures/compat-pce-93.bin").to_vec();
    let bits = raw.len() * 8;
    raw.push(0);
    let metadata = 70 * std::mem::size_of::<triergon_framing::AacPceElement>();
    let maximum =
        Budget::control_storage_bytes() + Bytes::owner_overhead_bytes() + raw.len() + 2 * metadata
            - 1;
    let pool = Budget::new(maximum).unwrap();
    let mut p = AacAscScopeParser::new(
        &pool,
        source(),
        Bytes::copy_from_slice(&pool, &raw).unwrap(),
        bits,
        limits(),
    )
    .unwrap();
    for _ in 0..100000 {
        match p.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Err(Error::Limit { .. }) => {
                drop(p);
                assert_eq!(pool.used(), Budget::control_storage_bytes());
                return;
            }
            Ok(BuildStep::Yield) => {}
            _ => panic!("own impossible live set was not a typed Limit"),
        }
    }
    panic!("bounded scope operation stalled")
}
fn reserved_frequency_with_following_frame() -> Vec<u8> {
    let mut bad = adts(&[0, 1, 2, 3, 4, 5, 6, 7], 0);
    bad[2] = (bad[2] & 0xc3) | (13 << 2);
    [bad, adts(&[1, 2, 3], 0)].concat()
}
fn parser_reader(pool: &Budget, packets: usize) -> AacAdtsCarrierReader {
    AacAdtsCarrierReader::for_parser_packets(
        pool,
        SourceId(73),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        AacAdtsCarrierLimits {
            maximum_packet_bytes: 10000,
            maximum_packets: packets,
            maximum_input_window: 8192,
        },
    )
    .unwrap()
}
#[test]
fn parser_candidates_retain_invalid_layer_and_frequency_without_certifying_audio() {
    let raw = reserved_frequency_with_following_frame();
    for eof in [false, true] {
        let pool = Budget::new(100000).unwrap();
        let mut r = parser_reader(&pool, 100);
        let mut units = Vec::new();
        let mut finished = false;
        for _ in 0..10000 {
            match r
                .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                Step::Ready(o) => units.push(o),
                Step::Yield => {}
                Step::End(_) => break,
                Step::NeedInput(q) => {
                    let at = q.offset as usize;
                    if at < raw.len() {
                        let end = (at + 7).min(raw.len());
                        r.provide_owned(
                            InputWindow::new(
                                q.source,
                                q.token,
                                q.offset,
                                Bytes::copy_from_slice(&pool, &raw[at..end]).unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                    } else if eof && !finished {
                        r.finish(q.source, q.token).unwrap();
                        finished = true;
                    } else {
                        break;
                    }
                }
            }
        }
        assert_eq!(units[0].source_bytes().as_ref(), &raw[..5]);
        assert_eq!(
            units[0].boundary(),
            AacAdtsCarrierBoundary::FollowingEnvelope
        );
        assert!(units[0].selected_error().is_some());
        assert_eq!(units.len(), if eof { 2 } else { 1 });
        if eof {
            assert_eq!(units[1].source_bytes().as_ref(), &raw[5..]);
            assert_eq!(units[1].source().offset, 5);
            assert_eq!(units[1].header().unwrap().layer(), 2);
            assert_eq!(units[1].header().unwrap().frame_length(), 2064);
            assert!(units[1].selected_error().is_some());
            assert_eq!(
                units[1].boundary(),
                AacAdtsCarrierBoundary::ExplicitEofFragment
            );
        }
    }
}
#[test]
fn six_byte_parser_candidate_cannot_establish_a_recovery_boundary() {
    let raw = reserved_frequency_with_following_frame();
    let raw = &raw[..11];
    let pool = Budget::new(10000).unwrap();
    let mut r = parser_reader(&pool, 100);
    let q = r.request();
    r.provide_inline(q.source, q.token, 0, raw).unwrap();
    for _ in 0..1000 {
        match r
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Yield => {}
            Step::NeedInput(q) => {
                assert_eq!(q.offset, 11);
                r.finish(q.source, q.token).unwrap();
                break;
            }
            _ => panic!("partial candidate created a source boundary"),
        }
    }
    loop {
        match r
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(o) => {
                assert_eq!(o.source_bytes().as_ref(), raw);
                assert_eq!(o.boundary(), AacAdtsCarrierBoundary::ExplicitEofFragment);
                break;
            }
            Step::Yield => {}
            _ => panic!(),
        }
    }
}
#[test]
fn parser_packet_count_limit_preserves_the_published_prefix() {
    let raw = reserved_frequency_with_following_frame();
    let pool = Budget::new(10000).unwrap();
    let mut r = parser_reader(&pool, 1);
    let q = r.request();
    r.provide_owned(
        InputWindow::new(
            q.source,
            q.token,
            0,
            Bytes::copy_from_slice(&pool, &raw).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let mut prefix = None;
    for _ in 0..1000 {
        match r.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(Step::Ready(o)) => {
                assert_eq!(o.source_bytes().as_ref(), &raw[..5]);
                prefix = Some(o)
            }
            Ok(Step::Yield) => {}
            Err(Error::Limit {
                requested: 2,
                maximum: 1,
            }) => {
                assert!(prefix.is_some());
                assert_eq!(r.completed_packets(), 1);
                return;
            }
            _ => panic!(),
        }
    }
    panic!()
}
#[test]
fn invalid_layer_candidates_expose_inspection_extent_and_keep_source_error() {
    let pool = Budget::new(100000).unwrap();
    for layer in 1..=3 {
        let mut raw = adts(&[1, 2, 3], 0);
        raw[1] = (raw[1] & !6) | (layer << 1);
        let mut r = parser_reader(&pool, 100);
        let q = r.request();
        r.provide_inline(q.source, q.token, 0, &raw).unwrap();
        loop {
            match r
                .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                Step::Ready(o) => {
                    assert_eq!(o.source_bytes().as_ref(), raw);
                    assert_eq!(o.header().unwrap().layer(), layer);
                    assert_eq!(o.inspection_end(), raw.len() as u64);
                    assert_eq!(o.boundary(), AacAdtsCarrierBoundary::ParserLengthCandidate);
                    assert!(matches!(o.selected_error(), Some(Error::Invalid(_))));
                    break;
                }
                Step::Yield => {}
                _ => panic!(),
            }
        }
    }
    let raw = reserved_frequency_with_following_frame();
    let mut r = parser_reader(&pool, 100);
    let q = r.request();
    r.provide_owned(
        InputWindow::new(
            q.source,
            q.token,
            0,
            Bytes::copy_from_slice(&pool, &raw).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    loop {
        match r
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(o) => {
                assert_eq!(o.source_bytes().len(), 5);
                assert_eq!(o.inspection_end(), 12);
                assert!(o.inspection_end() > o.source().offset + o.source_bytes().len() as u64);
                break;
            }
            Step::Yield => {}
            _ => panic!(),
        }
    }
}
