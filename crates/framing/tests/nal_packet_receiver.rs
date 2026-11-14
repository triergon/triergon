#![forbid(unsafe_code)]
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, SourceId, SourceLocation, WorkBudget};
use triergon_framing::compat::nal::*;
use triergon_framing::*;

fn bytes(p: &Budget, b: &[u8]) -> Bytes {
    Bytes::copy_from_slice(p, b).unwrap()
}
fn limits() -> NalPacketLimits {
    NalPacketLimits {
        maximum_packet_bytes: 65536,
        maximum_output_bytes: 131072,
        maximum_nals: 64,
        maximum_segments: 256,
    }
}
fn source() -> AvcConfigSource {
    AvcConfigSource {
        source: SourceId(7),
        generation: 29,
        offset: 5100,
    }
}
fn avcc(width: u8) -> Vec<u8> {
    vec![
        1,
        66,
        0,
        10,
        0xfc | (width - 1),
        225,
        0,
        5,
        0x67,
        66,
        0,
        10,
        128,
        1,
        0,
        4,
        0x68,
        0xce,
        6,
        0xe2,
    ]
}
fn hvcc(width: u8, unknown: bool) -> Vec<u8> {
    let mut b = vec![
        1,
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
        120,
        240,
        0,
        252,
        253,
        248,
        248,
        0,
        0,
        0x0c | (width - 1),
        3,
    ];
    for k in [32, 33, 34] {
        b.extend_from_slice(&[128 | k, 0, 1, 0, 3, k << 1, 1, 128]);
    }
    if unknown {
        b[22] = 4;
        b.extend_from_slice(&[55, 0, 0]);
    }
    b
}
fn alimits() -> AvcConfigLimits {
    AvcConfigLimits {
        maximum_record_bytes: 4096,
        maximum_nals: 64,
        maximum_nal_bytes: 4096,
    }
}
fn hlimits() -> HvcConfigLimits {
    HvcConfigLimits {
        maximum_record_bytes: 4096,
        maximum_arrays: 255,
        maximum_nals: 64,
        maximum_nal_bytes: 4096,
    }
}
fn avc(p: &Budget) -> AvcConfiguration {
    let mut parser = AvcConfigParser::new(p, source(), bytes(p, &avcc(4)), alimits()).unwrap();
    for _ in 0..10000 {
        if let BuildStep::Ready(v) = parser
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            return v;
        }
    }
    panic!("bounded AVC fixture");
}
fn observed_avc(p: &Budget, width: u8) -> ReceiverAvcConfiguration {
    let mut parser =
        ReceiverAvcConfigParser::new(p, source(), bytes(p, &avcc(width)), alimits()).unwrap();
    for _ in 0..10000 {
        if let BuildStep::Ready(v) = parser
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            return v;
        }
    }
    panic!("bounded observed AVC fixture");
}
fn hevc_source() -> HvcConfigSource {
    HvcConfigSource {
        source: SourceId(7),
        generation: 29,
        offset: 5100,
    }
}
fn hevc(p: &Budget, unknown: bool) -> HvcConfiguration {
    let mut parser =
        HvcConfigParser::new(p, hevc_source(), bytes(p, &hvcc(4, unknown)), hlimits()).unwrap();
    for _ in 0..10000 {
        if let BuildStep::Ready(v) = parser
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            return v;
        }
    }
    panic!("bounded HEVC fixture");
}
fn packet(p: &Budget, raw: &[u8]) -> NalPacket {
    NalPacket {
        bytes: bytes(p, raw),
        location: SourceLocation {
            source: SourceId(99),
            offset: 1 << 40,
        },
        generation: 81,
        fields: NalPacketFields {
            pts: i64::MIN,
            dts: -7,
            duration: 31,
            position: i64::MAX,
            flags: u32::MAX,
            stream_index: -3,
        },
        new_extradata: None,
    }
}
fn session(p: &Budget) -> NalPacketSession {
    NalPacketSession::new(p, NalPacketConfiguration::Avc(avc(p)), limits()).unwrap()
}
fn error<M>(job: &mut NalPacketJob<'_, M>) -> Error {
    for _ in 0..10000 {
        match job.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Err(e) => return e,
            Ok(BuildStep::Yield) => (),
            _ => panic!("expected bounded failure"),
        }
    }
    panic!("bounded failure stalled");
}
fn output<M>(job: &mut NalPacketJob<'_, M>) -> NalPacketOutput<M> {
    for _ in 0..10000 {
        match job
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            BuildStep::Ready(v) => return v,
            BuildStep::Yield => (),
            BuildStep::Complete => panic!("missing publication"),
        }
    }
    panic!("bounded publication stalled");
}
const IDR: &[u8] = &[0, 0, 0, 3, 0x65, 0xb8, 128];

#[test]
fn physical_prefix_rejection_moves_nonclone_metadata_once_and_outlives_session() {
    struct Caller {
        stamp: Option<triergon_core::Time>,
        owner: Box<u8>,
    }
    let p = Budget::new(100000).unwrap();
    let mut s = session(&p);
    let source = NalPacket {
        bytes: bytes(&p, &[0, 0]),
        location: SourceLocation {
            source: SourceId(99),
            offset: 1 << 40,
        },
        generation: 81,
        fields: Caller {
            stamp: None,
            owner: Box::new(42),
        },
        new_extradata: None,
    };
    let pointer = std::ptr::from_ref(source.fields.owner.as_ref());
    let mut job = s.packet(source, None).unwrap();
    assert!(job.take_receiver_rejection().is_none());
    assert_eq!(error(&mut job), Error::Truncated);
    let rejected = job.take_receiver_rejection().unwrap();
    assert!(job.take_receiver_rejection().is_none());
    assert!(job.source().is_none());
    assert!(matches!(
        job.step(&mut WorkBudget::new(99), &AtomicBool::new(false)),
        Err(Error::Truncated)
    ));
    drop(job);
    drop(s);
    assert_eq!(
        rejected.kind(),
        NalPacketReceiverErrorKind::IncompleteLengthPrefix
    );
    #[cfg(feature = "reference-trace")]
    assert_eq!(rejected.native_receive_result(), -1_094_995_529);
    assert_eq!(rejected.source_error(), Error::Truncated);
    assert_eq!(rejected.source().bytes.as_ref(), [0, 0]);
    assert_eq!(rejected.source().generation, 81);
    assert!(rejected.source().fields.stamp.is_none());
    assert_eq!(
        std::ptr::from_ref(rejected.source().fields.owner.as_ref()),
        pointer
    );
    drop(rejected);
    assert_eq!(p.used(), Budget::control_storage_bytes());
}
#[test]
fn physical_payload_rejection_keeps_raw_lengths_flags_and_source_identity() {
    let p = Budget::new(100000).unwrap();
    let mut s = session(&p);
    let mut job = s.packet(packet(&p, &[0, 0, 0, 5, 0x65]), None).unwrap();
    assert_eq!(error(&mut job), Error::Truncated);
    let rejected = job.take_receiver_rejection().unwrap();
    assert_eq!(
        rejected.kind(),
        NalPacketReceiverErrorKind::IncompleteNalPayload
    );
    assert_eq!(rejected.source().bytes.as_ref(), [0, 0, 0, 5, 0x65]);
    assert_eq!(rejected.source().fields.flags, u32::MAX);
    assert_eq!(rejected.source().fields.pts, i64::MIN);
    assert_eq!(
        rejected.configuration_source_identity(),
        Some((
            SourceLocation {
                source: SourceId(7),
                offset: 5100
            },
            29
        ))
    );
}
#[test]
fn empty_avc_is_a_receive_error_without_an_output_packet() {
    let p = Budget::new(100000).unwrap();
    let mut s = session(&p);
    let mut job = s.packet(packet(&p, &[]), None).unwrap();
    assert!(matches!(error(&mut job), Error::Invalid(_)));
    let r = job.take_receiver_rejection().unwrap();
    assert_eq!(r.kind(), NalPacketReceiverErrorKind::EmptyAvcPacket);
    assert!(r.source().bytes.is_empty());
}
#[test]
fn zero_hevc_nal_is_a_receive_error_with_original_prefix_retained() {
    let p = Budget::new(100000).unwrap();
    let mut s =
        NalPacketSession::new(&p, NalPacketConfiguration::Hevc(hevc(&p, false)), limits()).unwrap();
    let mut job = s.packet(packet(&p, &[0, 0, 0, 0]), None).unwrap();
    assert!(matches!(error(&mut job), Error::Invalid(_)));
    let r = job.take_receiver_rejection().unwrap();
    assert_eq!(r.kind(), NalPacketReceiverErrorKind::EmptyHevcNal);
    assert_eq!(r.source().bytes.as_ref(), [0, 0, 0, 0]);
}
#[test]
fn fixed_header_refusal_is_not_relabelled_as_native_codec_error() {
    let p = Budget::new(100000).unwrap();
    let mut s = session(&p);
    let mut job = s
        .packet(packet(&p, &[0, 0, 0, 2, 0xe5, 128]), None)
        .unwrap();
    assert!(matches!(error(&mut job), Error::Invalid(_)));
    assert!(job.take_receiver_rejection().is_none());
    assert_eq!(
        job.source().unwrap().bytes.as_ref(),
        [0, 0, 0, 2, 0xe5, 128]
    );
}
#[test]
fn short_opaque_idr_body_truncation_has_no_inferred_native_result() {
    let p = Budget::new(100000).unwrap();
    let mut s = session(&p);
    let mut job = s.packet(packet(&p, &[0, 0, 0, 1, 0x65]), None).unwrap();
    assert_eq!(error(&mut job), Error::Truncated);
    assert!(job.take_receiver_rejection().is_none());
}
#[test]
fn cancellation_stays_source_cancellation_after_flag_clears() {
    let p = Budget::new(100000).unwrap();
    let mut s = session(&p);
    let mut job = s.packet(packet(&p, IDR), None).unwrap();
    let c = AtomicBool::new(true);
    assert!(matches!(
        job.step(&mut WorkBudget::new(0), &c),
        Err(Error::Cancelled)
    ));
    c.store(false, Ordering::Relaxed);
    assert!(matches!(
        job.step(&mut WorkBudget::new(100), &c),
        Err(Error::Cancelled)
    ));
    assert!(job.take_receiver_rejection().is_none());
    assert!(job.source().is_some());
}
#[test]
fn output_policy_failure_is_not_a_native_codec_error() {
    let p = Budget::new(100000).unwrap();
    let mut l = limits();
    l.maximum_output_bytes = 4;
    let mut s = NalPacketSession::new(&p, NalPacketConfiguration::Avc(avc(&p)), l).unwrap();
    let mut job = s.packet(packet(&p, IDR), None).unwrap();
    assert!(matches!(error(&mut job), Error::Limit { .. }));
    assert!(job.take_receiver_rejection().is_none());
}
#[test]
fn external_pressure_reports_no_error_and_resumes_the_same_packet() {
    let p = Budget::new(100000).unwrap();
    let mut s = session(&p);
    let source = packet(&p, IDR);
    let blocker = p.reserve(p.available() - 1).unwrap();
    let mut job = s.packet(source, None).unwrap();
    assert!(matches!(error(&mut job), Error::Backpressure { .. }));
    assert!(job.take_receiver_rejection().is_none());
    drop(blocker);
    let out = output(&mut job);
    assert_eq!(out.source().bytes.as_ref(), IDR);
}
#[test]
fn failed_partial_inband_packet_does_not_commit_availability() {
    let p = Budget::new(100000).unwrap();
    let mut s = session(&p);
    let mut raw = vec![0, 0, 0, 5, 0x67, 66, 0, 10, 128];
    raw.extend_from_slice(&[0, 0]);
    let mut job = s.packet(packet(&p, &raw), None).unwrap();
    assert_eq!(error(&mut job), Error::Truncated);
    let rejected = job.take_receiver_rejection().unwrap();
    drop(job);
    let mut next = s.packet(packet(&p, IDR), None).unwrap();
    let out = output(&mut next);
    assert!(matches!(
        out.segments()[0].origin(),
        NalPacketOrigin::Configuration { .. }
    ));
    assert_eq!(rejected.source().bytes.as_ref(), raw);
}
#[test]
fn source_avc_width_three_refusal_and_observed_bytes_remain_distinct() {
    let p = Budget::new(100000).unwrap();
    let raw = avcc(3);
    let mut strict = AvcConfigParser::new(&p, source(), bytes(&p, &raw), alimits()).unwrap();
    assert!(matches!(
        strict.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Invalid("AVC three-byte sample length"))
    ));
    let cfg = observed_avc(&p, 3);
    assert_eq!(cfg.source_bytes().as_ref(), raw);
    assert_eq!(cfg.fields().length_size_minus_one(), 2);
    assert_eq!(
        cfg.strict_source_refusal(),
        Some(Error::Invalid("AVC three-byte sample length"))
    );
    let mut s =
        NalPacketSession::new(&p, NalPacketConfiguration::ReceiverAvc(cfg), limits()).unwrap();
    let mut job = s
        .receiver_packet(packet(&p, &[0, 0, 3, 0x65, 0xb8, 128]), None)
        .unwrap();
    let out = output(&mut job);
    assert_eq!(out.source().bytes.as_ref(), [0, 0, 3, 0x65, 0xb8, 128]);
    assert_eq!(
        out.configuration_source_refusal(),
        Some(Error::Invalid("AVC three-byte sample length"))
    );
    assert_eq!(out.configuration_bytes().as_ref(), raw);
}

#[test]
fn avc_receiver_preserves_short_profile_extension_and_frames_complete_parameter_sets() {
    let pool = Budget::new(100_000).unwrap();
    let cancel = AtomicBool::new(false);
    let mut base = avcc(4);
    base[1] = 100;
    base[9] = 100;
    let extension = [0xfd, 0xf8, 0xf8, 0];
    let idr = [0, 0, 0, 3, 0x65, 0xb8, 128];
    let mut complete = base.clone();
    complete.extend_from_slice(&extension);
    let mut strict =
        AvcConfigParser::new(&pool, source(), bytes(&pool, &complete), alimits()).unwrap();
    let BuildStep::Ready(complete) = strict.step(&mut WorkBudget::new(4096), &cancel).unwrap()
    else {
        panic!("complete AVC record must publish");
    };
    let mut complete_session =
        NalPacketSession::new(&pool, NalPacketConfiguration::Avc(complete), limits()).unwrap();
    let expected = output(&mut complete_session.packet(packet(&pool, &idr), None).unwrap());

    for length in 0..extension.len() {
        let suffix = &extension[..length];
        let mut raw = base.clone();
        raw.extend_from_slice(suffix);
        let raw_bytes = bytes(&pool, &raw);
        let mut strict =
            AvcConfigParser::new(&pool, source(), raw_bytes.clone(), alimits()).unwrap();
        assert!(matches!(
            strict.step(&mut WorkBudget::new(4096), &cancel),
            Err(Error::Truncated)
        ));
        for budget in [1, 4096] {
            let mut receiver =
                ReceiverAvcConfigParser::new(&pool, source(), raw_bytes.clone(), alimits())
                    .unwrap();
            let config = (0..128)
                .find_map(|_| {
                    match receiver
                        .step(&mut WorkBudget::new(budget), &cancel)
                        .unwrap()
                    {
                        BuildStep::Ready(value) => Some(value),
                        BuildStep::Yield => None,
                        BuildStep::Complete => panic!("AVC receiver finished without a record"),
                    }
                })
                .expect("AVC receiver must publish with bounded work");
            assert_eq!(config.source_bytes().as_ref(), raw);
            assert_eq!(config.fields().raw_extension(), None);
            assert_eq!(config.trailing_bytes(), suffix);
            assert_eq!(config.strict_source_refusal(), Some(Error::Truncated));
            assert_eq!(
                receiver.strict_source_refusal(),
                config.strict_source_refusal()
            );
            let mut session =
                NalPacketSession::new(&pool, NalPacketConfiguration::ReceiverAvc(config), limits())
                    .unwrap();
            let framed = output(&mut session.packet(packet(&pool, &idr), None).unwrap());
            assert_eq!(framed.bytes().as_ref(), expected.bytes().as_ref());
            assert_eq!(framed.configuration_bytes().as_ref(), raw);
            assert_eq!(
                framed.configuration_source_refusal(),
                Some(Error::Truncated)
            );
        }
    }
}

#[test]
fn avc_receiver_refuses_missing_parameter_set_in_complete_extension() {
    let pool = Budget::new(100_000).unwrap();
    let cancel = AtomicBool::new(false);
    let mut raw = avcc(4);
    raw[1] = 100;
    raw.extend_from_slice(&[0xfd, 0xf8, 0xf8, 1]);
    let raw = bytes(&pool, &raw);
    let mut strict = AvcConfigParser::new(&pool, source(), raw.clone(), alimits()).unwrap();
    let mut receiver = ReceiverAvcConfigParser::new(&pool, source(), raw, alimits()).unwrap();
    assert!(matches!(
        strict.step(&mut WorkBudget::new(4096), &cancel),
        Err(Error::Truncated)
    ));
    assert!(matches!(
        receiver.step(&mut WorkBudget::new(4096), &cancel),
        Err(Error::Truncated)
    ));
}

#[test]
fn source_hevc_width_three_refusal_and_observed_header_bits_remain_distinct() {
    let p = Budget::new(100000).unwrap();
    let raw = hvcc(3, false);
    let mut strict = HvcConfigParser::new(&p, hevc_source(), bytes(&p, &raw), hlimits()).unwrap();
    assert!(matches!(
        strict.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Unsupported("hvcC three-byte sample length"))
    ));
    let mut parser =
        ReceiverHevcConfigParser::new(&p, hevc_source(), bytes(&p, &raw), hlimits()).unwrap();
    let cfg = loop {
        if let BuildStep::Ready(v) = parser
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            break v;
        }
    };
    assert_eq!(cfg.fields().length_size_minus_one(), 2);
    assert_ne!(cfg.fields().syntax_issues() & 32, 0);
    let mut s =
        NalPacketSession::new(&p, NalPacketConfiguration::ReceiverHevc(cfg), limits()).unwrap();
    let mut job = s
        .receiver_packet(packet(&p, &[0, 0, 3, 38, 1, 128]), None)
        .unwrap();
    let out = output(&mut job);
    assert_eq!(
        out.configuration_source_refusal(),
        Some(Error::Unsupported("hvcC three-byte sample length"))
    );
    assert_eq!(out.configuration_bytes().as_ref(), raw);
}
#[test]
fn buffering_period_uses_later_in_band_sets_and_keeps_the_original_nals() {
    // The receiver inserts a copy of the following SPS and PPS before the
    // buffering-period SEI. The original sets stay at their source positions.
    let p = Budget::new(100000).unwrap();
    let mut s = session(&p);
    let raw = [
        0, 0, 0, 4, 0x06, 0x00, 0x01, 0x80, // buffering period
        0, 0, 0, 3, 0x67, 0x21, 0x80, // SPS
        0, 0, 0, 3, 0x68, 0x25, 0x80, // PPS
        0, 0, 0, 2, 0x65, 0x80, // first IDR slice
    ];
    let mut job = s.receiver_packet(packet(&p, &raw), None).unwrap();
    let out = output(&mut job);
    assert_eq!(
        out.bytes().as_ref(),
        [
            0, 0, 0, 1, 0x67, 0x21, 0x80, 0, 0, 0, 1, 0x68, 0x25, 0x80, 0, 0, 1, 0x06, 0x00, 0x01,
            0x80, 0, 0, 0, 1, 0x67, 0x21, 0x80, 0, 0, 0, 1, 0x68, 0x25, 0x80, 0, 0, 1, 0x65, 0x80,
        ],
    );
    assert_eq!(
        out.segments()[0].origin(),
        NalPacketOrigin::Packet {
            source_range: 12..15
        },
    );
    assert_eq!(
        out.segments()[1].origin(),
        NalPacketOrigin::Packet {
            source_range: 19..22
        },
    );
    assert_eq!(out.configuration_bytes().as_ref(), avcc(4));
}

#[test]
fn buffering_period_insertion_survives_packet_boundaries_and_flush_rearms_it() {
    let p = Budget::new(100000).unwrap();
    let mut s = session(&p);
    let sei = [0, 0, 0, 4, 0x06, 0x00, 0x01, 0x80];
    let idr = [0, 0, 0, 2, 0x65, 0x80];
    let cancelled = AtomicBool::new(true);
    let initial = {
        let mut job = s.receiver_packet(packet(&p, &sei), None).unwrap();
        assert!(matches!(
            job.step(&mut WorkBudget::new(0), &AtomicBool::new(false))
                .unwrap(),
            BuildStep::Yield
        ));
        output(&mut job)
    };
    let subsequent = output(&mut s.receiver_packet(packet(&p, &idr), None).unwrap());
    assert_eq!(subsequent.bytes().as_ref(), [0, 0, 0, 1, 0x65, 0x80]);
    assert_eq!(initial.segments().len(), 3);
    s.flush();
    {
        let mut job = s.receiver_packet(packet(&p, &sei), None).unwrap();
        assert!(matches!(
            job.step(&mut WorkBudget::new(0), &cancelled),
            Err(Error::Cancelled)
        ));
    }
    let restarted = output(&mut s.receiver_packet(packet(&p, &sei), None).unwrap());
    assert_eq!(restarted.bytes().as_ref(), initial.bytes().as_ref());
}

#[test]
fn receiver_retains_header_only_idr_with_end_of_packet_padding() {
    let p = Budget::new(100000).unwrap();
    let mut s = session(&p);
    let idr = [0, 0, 0, 1, 0x65];
    {
        let mut job = s.packet(packet(&p, &idr), None).unwrap();
        assert_eq!(error(&mut job), Error::Truncated);
    }
    let out = output(&mut s.receiver_packet(packet(&p, &idr), None).unwrap());
    assert_eq!(out.segments().len(), 3);
    assert_eq!(out.segment_bytes(2).unwrap(), [0x65]);
    assert!(out.bytes().ends_with(&[0, 0, 1, 0x65]));
}

#[test]
fn observed_avc_update_changes_native_width_but_keeps_source_refusal_and_epoch() {
    let p = Budget::new(100000).unwrap();
    let mut s = session(&p);
    let raw = avcc(3);
    let mut src = packet(&p, &[0, 0, 3, 0x65, 0xb8, 128]);
    src.new_extradata = Some(bytes(&p, &raw));
    let mut job = s.receiver_packet(src, Some(observed_avc(&p, 3))).unwrap();
    let out = output(&mut job);
    assert_eq!(out.receiver_epoch(), 1);
    assert_eq!(out.source().new_extradata.as_ref().unwrap().as_ref(), raw);
    assert_eq!(
        out.configuration_source_refusal(),
        Some(Error::Invalid("AVC three-byte sample length"))
    );
    drop(job);
    assert_eq!(s.receiver_epoch(), 1);
    assert!(matches!(
        s.configuration(),
        NalPacketConfiguration::ReceiverAvc(_)
    ));
}
#[test]
fn observed_record_work_zero_and_cancellation_do_not_publish_or_invent_errors() {
    let p = Budget::new(100000).unwrap();
    let mut parser =
        ReceiverAvcConfigParser::new_update(&p, source(), bytes(&p, &avcc(3)), alimits()).unwrap();
    let c = AtomicBool::new(false);
    for _ in 0..8 {
        let mut w = WorkBudget::new(0);
        assert!(matches!(parser.step(&mut w, &c).unwrap(), BuildStep::Yield));
        assert_eq!(w.spent(), 0);
    }
    c.store(true, Ordering::Relaxed);
    assert!(matches!(
        parser.step(&mut WorkBudget::new(0), &c),
        Err(Error::Cancelled)
    ));
    assert!(parser.take_update_rejection().is_none());
}
#[test]
fn observed_record_parser_rejects_foreign_pool_before_any_result() {
    let p = Budget::new(100000).unwrap();
    let q = Budget::new(100000).unwrap();
    assert!(matches!(
        ReceiverAvcConfigParser::new(&p, source(), bytes(&q, &avcc(3)), alimits()),
        Err(Error::ForeignPool)
    ));
}
#[test]
fn short_avc_update_rejection_is_bound_to_update_role_and_retains_exact_raw_header() {
    let p = Budget::new(100000).unwrap();
    let raw = bytes(&p, &[1]);
    let mut initial = ReceiverAvcConfigParser::new(&p, source(), raw.clone(), alimits()).unwrap();
    assert!(matches!(
        initial.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Truncated)
    ));
    assert!(initial.take_update_rejection().is_none());
    let mut update = ReceiverAvcConfigParser::new_update(&p, source(), raw, alimits()).unwrap();
    assert!(update.take_update_rejection().is_none());
    assert!(matches!(
        update.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Truncated)
    ));
    let rejected = update.take_update_rejection().unwrap();
    assert!(update.take_update_rejection().is_none());
    drop(update);
    drop(initial);
    assert_eq!(rejected.source_bytes().as_ref(), [1]);
    assert_eq!(rejected.source(), source());
    #[cfg(feature = "reference-trace")]
    assert_eq!(rejected.native_receive_result(), -1_094_995_529);
}
#[test]
fn unsupported_update_version_does_not_get_a_predicted_native_error() {
    let p = Budget::new(100000).unwrap();
    let mut parser =
        ReceiverAvcConfigParser::new_update(&p, source(), bytes(&p, &[2]), alimits()).unwrap();
    assert!(matches!(
        parser.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Unsupported(_))
    ));
    assert!(parser.take_update_rejection().is_none());
}
#[test]
fn unsupported_hevc_initial_array_retains_raw_source_and_has_no_fake_output() {
    let p = Budget::new(100000).unwrap();
    let cfg = NalPacketConfiguration::Hevc(hevc(&p, true));
    let mut writer = NalPacketConfigWriter::new(&p, &cfg, 65536).unwrap();
    assert!(writer.take_receiver_rejection().is_none());
    let cancel = AtomicBool::new(false);
    let count = u64::from(hvcc(4, true)[22]);
    let mut before = WorkBudget::new(count - 1);
    assert!(matches!(
        writer.step(&mut before, &cancel),
        Ok(BuildStep::Yield)
    ));
    assert_eq!(before.remaining(), 0);
    assert!(writer.take_receiver_rejection().is_none());
    let mut refusal = WorkBudget::new(1);
    assert!(matches!(
        writer.step(&mut refusal, &cancel),
        Err(Error::Unsupported("pinned HEVC decoder array type"))
    ));
    assert_eq!(refusal.remaining(), 0);
    let r = writer.take_receiver_rejection().unwrap();
    assert!(writer.take_receiver_rejection().is_none());
    drop(writer);
    drop(cfg);
    assert_eq!(r.source_bytes().as_ref(), hvcc(4, true));
    assert_eq!(r.declared_array_type(), 55);
    #[cfg(feature = "reference-trace")]
    assert_eq!(r.native_initialize_result(), -1_094_995_529);
}
#[test]
fn observed_initialization_retains_width_three_refusal_after_config_drop() {
    let p = Budget::new(100000).unwrap();
    let cfg = NalPacketConfiguration::ReceiverAvc(observed_avc(&p, 3));
    let mut writer = NalPacketConfigWriter::new(&p, &cfg, 65536).unwrap();
    let output = loop {
        if let BuildStep::Ready(v) = writer
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            break v;
        }
    };
    drop(writer);
    drop(cfg);
    assert_eq!(output.source_bytes().as_ref(), avcc(3));
    assert_eq!(
        output.strict_source_refusal(),
        Some(Error::Invalid("AVC three-byte sample length"))
    );
}
#[test]
fn physical_error_after_observed_update_keeps_new_config_without_committing_inband_state() {
    let p = Budget::new(100000).unwrap();
    let mut s = session(&p);
    let mut source = packet(&p, &[0, 0]);
    source.new_extradata = Some(bytes(&p, &avcc(3)));
    let mut job = s
        .receiver_packet(source, Some(observed_avc(&p, 3)))
        .unwrap();
    assert_eq!(error(&mut job), Error::Truncated);
    let r = job.take_receiver_rejection().unwrap();
    drop(job);
    assert!(matches!(
        s.configuration(),
        NalPacketConfiguration::ReceiverAvc(_)
    ));
    drop(s);
    assert_eq!(r.receiver_epoch(), 1);
    assert_eq!(r.configuration_bytes().as_ref(), avcc(3));
    assert_eq!(
        r.configuration_source_refusal(),
        Some(Error::Invalid("AVC three-byte sample length"))
    );
}

#[test]
fn observed_hevc_short_header_retains_source_refusal_before_later_physical_failure() {
    let p = Budget::new(100000).unwrap();
    let raw = [0, 0, 0, 1, 38, 1, 128];
    let mut strict =
        NalPacketSession::new(&p, NalPacketConfiguration::Hevc(hevc(&p, false)), limits()).unwrap();
    let mut job = strict.packet(packet(&p, &raw), None).unwrap();
    assert_eq!(error(&mut job), Error::Truncated);
    assert!(job.take_receiver_rejection().is_none());
    drop(job);
    let mut native =
        NalPacketSession::new(&p, NalPacketConfiguration::Hevc(hevc(&p, false)), limits()).unwrap();
    let mut job = native.receiver_packet(packet(&p, &raw), None).unwrap();
    assert_eq!(error(&mut job), Error::Truncated);
    let rejected = job.take_receiver_rejection().unwrap();
    assert_eq!(
        rejected.kind(),
        NalPacketReceiverErrorKind::IncompleteLengthPrefix
    );
    assert_eq!(rejected.packet_source_refusal(), Some(Error::Truncated));
    assert_eq!(rejected.source().bytes.as_ref(), raw);
}

#[test]
fn short_observed_update_retains_prior_width_three_source_refusal_separately() {
    let p = Budget::new(100000).unwrap();
    let raw = &avcc(3)[..6];
    let mut strict = AvcConfigParser::new(&p, source(), bytes(&p, raw), alimits()).unwrap();
    assert!(matches!(
        strict.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Invalid("AVC three-byte sample length"))
    ));
    let mut observed =
        ReceiverAvcConfigParser::new_update(&p, source(), bytes(&p, raw), alimits()).unwrap();
    loop {
        match observed.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Err(Error::Truncated) => break,
            Ok(BuildStep::Yield) => (),
            _ => panic!("short record accepted"),
        }
    }
    let rejected = observed.take_update_rejection().unwrap();
    assert_eq!(rejected.source_error(), Error::Truncated);
    assert_eq!(
        rejected.strict_source_refusal(),
        Some(Error::Invalid("AVC three-byte sample length"))
    );
    assert_eq!(rejected.source_bytes().as_ref(), raw);
}
