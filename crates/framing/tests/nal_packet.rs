#![forbid(unsafe_code)]
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, SourceId, SourceLocation, WorkBudget};
use triergon_framing::*;

fn pool() -> Budget {
    Budget::new(2_000_000).unwrap()
}
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
fn avcc(tag: u8) -> Vec<u8> {
    vec![
        1, 66, 0, 10, 255, 225, 0, 5, 0x67, 66, 0, 10, tag, 1, 0, 4, 0x68, 0xce, 6, 0xe2,
    ]
}
fn hvcc() -> Vec<u8> {
    let mut b = vec![
        1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 120, 240, 0, 252, 253, 248, 248, 0, 0, 15, 3,
    ];
    for k in [32, 33, 34] {
        b.extend_from_slice(&[128 | k, 0, 1, 0, 3, k << 1, 1, 128]);
    }
    b
}
fn avc(p: &Budget, tag: u8) -> AvcConfiguration {
    let mut a = AvcConfigParser::new(
        p,
        AvcConfigSource {
            source: SourceId(9),
            generation: 12,
            offset: 1_u64 << 34,
        },
        bytes(p, &avcc(tag)),
        AvcConfigLimits {
            maximum_record_bytes: 4096,
            maximum_nals: 64,
            maximum_nal_bytes: 4096,
        },
    )
    .unwrap();
    loop {
        if let BuildStep::Ready(v) = a
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            return v;
        }
    }
}
fn hevc(p: &Budget) -> HvcConfiguration {
    let mut a = HvcConfigParser::new(
        p,
        HvcConfigSource {
            source: SourceId(9),
            generation: 12,
            offset: 1_u64 << 34,
        },
        bytes(p, &hvcc()),
        HvcConfigLimits {
            maximum_record_bytes: 4096,
            maximum_arrays: 255,
            maximum_nals: 64,
            maximum_nal_bytes: 4096,
        },
    )
    .unwrap();
    loop {
        if let BuildStep::Ready(v) = a
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            return v;
        }
    }
}
fn fields() -> NalPacketFields {
    NalPacketFields {
        pts: i64::MIN,
        dts: -1007,
        duration: 31,
        position: 4294967297,
        flags: 3,
        stream_index: 7,
    }
}
fn packet(p: &Budget, nals: &[&[u8]]) -> NalPacket {
    let mut b = Vec::new();
    for n in nals {
        b.extend_from_slice(&(n.len() as u32).to_be_bytes());
        b.extend_from_slice(n);
    }
    NalPacket {
        bytes: bytes(p, &b),
        location: SourceLocation {
            source: SourceId(77),
            offset: 1_u64 << 40,
        },
        generation: 12,
        fields: fields(),
        new_extradata: None,
    }
}
fn run<M>(
    session: &mut NalPacketSession,
    source: NalPacket<M>,
    update: Option<AvcConfiguration>,
    quantum: u64,
) -> Result<NalPacketOutput<M>, Error> {
    let mut j = session.packet(source, update)?;
    for _ in 0..100_000 {
        match j.step(&mut WorkBudget::new(quantum), &AtomicBool::new(false))? {
            BuildStep::Ready(v) => return Ok(v),
            BuildStep::Yield => {}
            BuildStep::Complete => panic!("no packet publication"),
        }
    }
    panic!("bounded packet job stalled")
}
fn session(p: &Budget) -> NalPacketSession {
    NalPacketSession::new(p, NalPacketConfiguration::Avc(avc(p, 128)), limits()).unwrap()
}
fn hn(k: u8) -> [u8; 3] {
    [k << 1, 1, 128]
}
fn conf() -> Vec<u8> {
    vec![
        0, 0, 0, 1, 0x67, 66, 0, 10, 128, 0, 0, 0, 1, 0x68, 0xce, 6, 0xe2,
    ]
}
fn framed(prefix: u8, n: &[u8]) -> Vec<u8> {
    let mut b = vec![0; usize::from(prefix) - 1];
    b.push(1);
    b.extend_from_slice(n);
    b
}
const IDR: &[u8] = &[0x65, 0xb8, 128];
const CONT: &[u8] = &[0x65, 0x4e, 128];
const NON: &[u8] = &[0x41, 0xb8, 128];
const SPS: &[u8] = &[0x67, 66, 0, 10, 128];
const PPS: &[u8] = &[0x68, 0xce, 6, 0xe2];

#[test]
fn repeated_first_slices_inject_but_continuation_does_not() {
    let p = pool();
    let mut s = session(&p);
    let expected = [conf(), framed(3, IDR)].concat();
    assert_eq!(
        run(&mut s, packet(&p, &[IDR]), None, 1)
            .unwrap()
            .bytes()
            .as_ref(),
        expected
    );
    assert_eq!(
        run(&mut s, packet(&p, &[CONT]), None, 1)
            .unwrap()
            .bytes()
            .as_ref(),
        framed(4, CONT)
    );
    assert_eq!(
        run(&mut s, packet(&p, &[IDR]), None, 1)
            .unwrap()
            .bytes()
            .as_ref(),
        expected
    );
}
#[test]
fn in_band_sets_persist_and_non_idr_rearms_injection() {
    let p = pool();
    let mut s = session(&p);
    let first = run(&mut s, packet(&p, &[SPS, PPS, IDR]), None, 1).unwrap();
    assert_eq!(
        first.bytes().as_ref(),
        [framed(4, SPS), framed(4, PPS), framed(3, IDR)].concat()
    );
    assert_eq!(
        run(&mut s, packet(&p, &[IDR]), None, 1)
            .unwrap()
            .bytes()
            .as_ref(),
        framed(4, IDR)
    );
    run(&mut s, packet(&p, &[NON]), None, 1).unwrap();
    assert_eq!(
        run(&mut s, packet(&p, &[IDR]), None, 1)
            .unwrap()
            .bytes()
            .as_ref(),
        [conf(), framed(3, IDR)].concat()
    );
}
#[test]
fn pps_without_sps_gets_config_sps_before_its_original_bytes() {
    let p = pool();
    let mut s = session(&p);
    let out = run(&mut s, packet(&p, &[PPS, IDR]), None, 1).unwrap();
    assert_eq!(
        out.bytes().as_ref(),
        [framed(4, SPS), framed(4, PPS), framed(3, IDR)].concat()
    );
    assert!(matches!(
        out.segments()[0].origin,
        NalPacketOrigin::Configuration { nal_index: 0, .. }
    ));
    assert_eq!(
        out.source().bytes.as_ref(),
        packet(&p, &[PPS, IDR]).bytes.as_ref()
    );
}
#[test]
fn only_in_band_sps_requires_pps_on_each_new_idr() {
    let p = pool();
    let mut s = session(&p);
    run(&mut s, packet(&p, &[SPS, IDR]), None, 1).unwrap();
    assert_eq!(
        run(&mut s, packet(&p, &[IDR]), None, 1)
            .unwrap()
            .bytes()
            .as_ref(),
        [framed(4, PPS), framed(3, IDR)].concat()
    );
}
#[test]
fn aud_and_sei_keep_order_and_native_three_byte_prefix() {
    let p = pool();
    let mut s = session(&p);
    let aud = &[9, 240];
    let sei = &[6, 5, 1, 127, 128];
    let out = run(&mut s, packet(&p, &[aud, sei, IDR]), None, 1).unwrap();
    assert_eq!(
        out.bytes().as_ref(),
        [framed(4, aud), framed(3, sei), conf(), framed(3, IDR)].concat()
    );
}
#[test]
fn multiple_idr_slices_are_not_dropped_or_sorted() {
    let p = pool();
    let mut s = session(&p);
    let out = run(&mut s, packet(&p, &[IDR, CONT, IDR]), None, 1).unwrap();
    assert_eq!(
        out.bytes().as_ref(),
        [
            conf(),
            framed(3, IDR),
            framed(3, CONT),
            conf(),
            framed(3, IDR)
        ]
        .concat()
    );
}
#[test]
fn hevc_injects_before_first_irap_per_packet_including_in_band_sets() {
    let p = pool();
    let mut s =
        NalPacketSession::new(&p, NalPacketConfiguration::Hevc(hevc(&p)), limits()).unwrap();
    let v = hn(32);
    let seq = hn(33);
    let pic = hn(34);
    let irap = hn(19);
    let extra = [framed(4, &v), framed(4, &seq), framed(4, &pic)].concat();
    let out = run(&mut s, packet(&p, &[&v, &seq, &pic, &irap, &irap]), None, 1).unwrap();
    assert_eq!(
        out.bytes().as_ref(),
        [
            extra.clone(),
            extra.clone(),
            framed(4, &irap),
            framed(4, &irap)
        ]
        .concat()
    );
    assert_eq!(
        run(&mut s, packet(&p, &[&irap]), None, 1)
            .unwrap()
            .bytes()
            .as_ref(),
        [extra, framed(4, &irap)].concat()
    );
}
#[test]
fn hevc_update_is_forwarded_but_does_not_change_source_configuration() {
    let p = pool();
    let mut s =
        NalPacketSession::new(&p, NalPacketConfiguration::Hevc(hevc(&p)), limits()).unwrap();
    let mut src = packet(&p, &[&hn(19)]);
    src.new_extradata = Some(bytes(&p, b"uninterpreted update"));
    let o = run(&mut s, src, None, 1).unwrap();
    assert!(o.forward_new_extradata());
    assert_eq!(
        o.source().new_extradata.as_ref().unwrap().as_ref(),
        b"uninterpreted update"
    );
    assert_eq!(o.receiver_epoch(), 0);
    assert_eq!(o.configuration_bytes().as_ref(), hvcc());
}
#[test]
fn avc_update_is_atomic_and_source_side_data_remains_retained() {
    let p = pool();
    let mut s = session(&p);
    let mut src = packet(&p, &[IDR]);
    src.new_extradata = Some(bytes(&p, &avcc(130)));
    let out = run(&mut s, src, Some(avc(&p, 130)), 1).unwrap();
    assert_eq!(out.receiver_epoch(), 1);
    assert!(!out.forward_new_extradata());
    assert_eq!(
        out.source().new_extradata.as_ref().unwrap().as_ref(),
        avcc(130)
    );
    assert_eq!(out.configuration_bytes().as_ref(), avcc(130));
    assert_eq!(s.receiver_epoch(), 1);
}
#[test]
fn empty_avc_update_is_present_source_but_not_new_epoch() {
    let p = pool();
    let mut s = session(&p);
    let mut src = packet(&p, &[IDR]);
    src.new_extradata = Some(bytes(&p, &[]));
    let out = run(&mut s, src, None, 1).unwrap();
    assert!(out.source().new_extradata.as_ref().unwrap().is_empty());
    assert_eq!(out.receiver_epoch(), 0);
    assert!(out.forward_new_extradata());
}
#[test]
fn update_mismatch_fails_before_commit() {
    let p = pool();
    let mut s = session(&p);
    let mut src = packet(&p, &[IDR]);
    src.new_extradata = Some(bytes(&p, &avcc(131)));
    assert!(matches!(
        run(&mut s, src, Some(avc(&p, 130)), 1),
        Err(Error::Invalid(_))
    ));
    assert_eq!(s.receiver_epoch(), 0);
    assert_eq!(s.configuration().bytes().as_ref(), avcc(128));
}
#[test]
fn work_zero_yields_and_one_unit_matches_large_partition() {
    let p = pool();
    let mut s = session(&p);
    let source = packet(&p, &[IDR, CONT]);
    let mut job = s.packet(source.clone(), None).unwrap();
    for _ in 0..20 {
        let mut w = WorkBudget::new(0);
        assert!(matches!(
            job.step(&mut w, &AtomicBool::new(false)).unwrap(),
            BuildStep::Yield
        ));
        assert_eq!(w.spent(), 0);
    }
    drop(job);
    let a = run(&mut s, source, None, 1).unwrap();
    s.flush();
    let b = run(&mut s, packet(&p, &[IDR, CONT]), None, 65536).unwrap();
    assert_eq!(a.bytes().as_ref(), b.bytes().as_ref());
}
#[test]
fn cancellation_is_sticky_and_does_not_commit_update() {
    let p = pool();
    let mut s = session(&p);
    let mut src = packet(&p, &[IDR]);
    src.new_extradata = Some(bytes(&p, &avcc(130)));
    let mut j = s.packet(src, Some(avc(&p, 130))).unwrap();
    let c = AtomicBool::new(true);
    assert!(matches!(
        j.step(&mut WorkBudget::new(0), &c),
        Err(Error::Cancelled)
    ));
    c.store(false, Ordering::Relaxed);
    assert!(matches!(
        j.step(&mut WorkBudget::new(1000), &c),
        Err(Error::Cancelled)
    ));
    drop(j);
    assert_eq!(s.receiver_epoch(), 0);
}
#[test]
fn malformed_packet_is_not_joined_to_next_packet_or_committed() {
    let p = pool();
    let mut s = session(&p);
    let mut src = packet(&p, &[SPS]);
    let mut b = src.bytes.as_ref().to_vec();
    b.extend_from_slice(&[0, 0, 0, 5, 0x65]);
    src.bytes = bytes(&p, &b);
    assert!(matches!(run(&mut s, src, None, 1), Err(Error::Truncated)));
    assert_eq!(
        run(&mut s, packet(&p, &[IDR]), None, 1)
            .unwrap()
            .bytes()
            .as_ref(),
        [conf(), framed(3, IDR)].concat()
    );
}
#[test]
fn zero_nal_length_keeps_original_wire_and_emits_no_fake_nal() {
    let p = pool();
    let mut s = session(&p);
    let o = run(&mut s, packet(&p, &[&[]]), None, 1).unwrap();
    assert!(o.bytes().is_empty());
    assert_eq!(o.source().bytes.as_ref(), [0, 0, 0, 0]);
    assert!(o.segments().is_empty());
}
#[test]
fn source_metadata_signed_extremes_and_sparse_offset_are_unchanged() {
    let p = pool();
    let mut s = session(&p);
    let out = run(&mut s, packet(&p, &[IDR]), None, 1).unwrap();
    assert_eq!(out.source().fields, fields());
    assert_eq!(out.source().location.offset, 1_u64 << 40);
    assert_eq!(out.source().generation, 12);
}
#[test]
fn offset_overflow_is_checked_without_consuming_session() {
    let p = pool();
    let mut s = session(&p);
    let mut src = packet(&p, &[IDR]);
    src.location.offset = u64::MAX;
    assert!(matches!(s.packet(src, None), Err(Error::Overflow)));
    assert_eq!(s.receiver_epoch(), 0);
}
#[test]
fn foreign_pool_is_rejected_in_source_and_side_data() {
    let p = pool();
    let q = pool();
    let mut s = session(&p);
    assert!(matches!(
        s.packet(packet(&q, &[IDR]), None),
        Err(Error::ForeignPool)
    ));
    let mut src = packet(&p, &[IDR]);
    src.new_extradata = Some(bytes(&q, &[]));
    assert!(matches!(s.packet(src, None), Err(Error::ForeignPool)));
}
#[test]
fn drain_rejects_new_packets_until_flush_without_dropping_retained_output() {
    let p = pool();
    let mut s = session(&p);
    let a = run(&mut s, packet(&p, &[IDR]), None, 1).unwrap();
    let expected = a.bytes().as_ref().to_vec();
    s.drain();
    assert!(matches!(
        s.packet(packet(&p, &[IDR]), None),
        Err(Error::Invalid(_))
    ));
    s.flush();
    assert_eq!(
        run(&mut s, packet(&p, &[IDR]), None, 1)
            .unwrap()
            .bytes()
            .as_ref(),
        expected
    );
    drop(s);
    assert_eq!(a.bytes().as_ref(), expected);
}
#[test]
fn output_limit_is_checked_before_publication() {
    let p = pool();
    let mut l = limits();
    l.maximum_output_bytes = 4;
    let mut s = NalPacketSession::new(&p, NalPacketConfiguration::Avc(avc(&p, 128)), l).unwrap();
    assert!(matches!(
        run(&mut s, packet(&p, &[IDR]), None, 1),
        Err(Error::Limit { .. })
    ));
}
#[test]
fn nal_and_segment_limits_are_distinct_and_do_not_drop_source() {
    let p = pool();
    let mut l = limits();
    l.maximum_nals = 1;
    let mut s = NalPacketSession::new(&p, NalPacketConfiguration::Avc(avc(&p, 128)), l).unwrap();
    assert!(matches!(
        run(&mut s, packet(&p, &[NON, NON]), None, 1),
        Err(Error::Limit { .. })
    ));
    let mut l = limits();
    l.maximum_segments = 1;
    let mut s = NalPacketSession::new(&p, NalPacketConfiguration::Avc(avc(&p, 128)), l).unwrap();
    assert!(matches!(
        run(&mut s, packet(&p, &[IDR]), None, 1),
        Err(Error::Limit { .. })
    ));
}
#[test]
fn reserved_pool_pressure_recovers_without_publishing_partial_bytes() {
    let p = pool();
    let mut s = session(&p);
    let source = packet(&p, &[IDR]);
    let blocker = p.reserve(p.available() - 1).unwrap();
    let mut j = s.packet(source, None).unwrap();
    let error = loop {
        match j.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Err(e) => break e,
            Ok(BuildStep::Yield) => {}
            _ => panic!("unexpected publication under pressure"),
        }
    };
    assert!(matches!(error, Error::Backpressure { .. }));
    drop(blocker);
    let out = loop {
        if let BuildStep::Ready(v) = j
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            break v;
        }
    };
    assert_eq!(out.bytes().as_ref(), [conf(), framed(3, IDR)].concat());
}
#[test]
fn strict_forbidden_header_refusal_does_not_erase_the_raw_packet() {
    let p = pool();
    let mut s = session(&p);
    let source = packet(&p, &[&[0xe5, 128]]);
    let raw = source.bytes.clone();
    let mut j = s.packet(source, None).unwrap();
    loop {
        match j.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Err(Error::Invalid(_)) => break,
            Ok(BuildStep::Yield) => {}
            _ => panic!("invalid fixed header accepted"),
        }
    }
    assert_eq!(j.source().unwrap().bytes.as_ref(), raw.as_ref());
}
#[test]
fn explicit_passthrough_preserves_every_byte_and_does_not_certify_headers() {
    let p = pool();
    let mut s = NalPacketSession::new(
        &p,
        NalPacketConfiguration::Passthrough {
            codec: Codec::Avc,
            raw: bytes(&p, &[]),
        },
        limits(),
    )
    .unwrap();
    let mut src = packet(&p, &[]);
    src.bytes = bytes(&p, &[0, 0, 1, 255, 0, 0, 9]);
    let out = run(&mut s, src, None, 1).unwrap();
    assert_eq!(out.bytes().as_ref(), [0, 0, 1, 255, 0, 0, 9]);
    assert_eq!(out.segments()[0].prefix_bytes, 0);
}

#[test]
fn native_injected_sps_before_pps_is_not_injected_again_at_later_pps() {
    let p = pool();
    let mut s = session(&p);
    assert_eq!(
        run(&mut s, packet(&p, &[PPS]), None, 1)
            .unwrap()
            .bytes()
            .as_ref(),
        [framed(4, SPS), framed(4, PPS)].concat()
    );
    assert_eq!(
        run(&mut s, packet(&p, &[PPS]), None, 1)
            .unwrap()
            .bytes()
            .as_ref(),
        framed(4, PPS)
    );
    assert_eq!(
        run(&mut s, packet(&p, &[IDR]), None, 1)
            .unwrap()
            .bytes()
            .as_ref(),
        framed(4, IDR)
    );
}

#[test]
fn non_idr_rearms_even_when_no_preceding_idr_was_emitted() {
    let p = pool();
    let mut s = session(&p);
    run(&mut s, packet(&p, &[SPS]), None, 1).unwrap();
    run(&mut s, packet(&p, &[NON]), None, 1).unwrap();
    assert_eq!(
        run(&mut s, packet(&p, &[IDR]), None, 1)
            .unwrap()
            .bytes()
            .as_ref(),
        [conf(), framed(3, IDR)].concat()
    );
}

#[test]
fn valid_native_config_update_survives_bad_packet_as_separate_fact() {
    let p = pool();
    let mut s = session(&p);
    let mut src = packet(&p, &[]);
    src.bytes = bytes(&p, &[0, 0]);
    src.new_extradata = Some(bytes(&p, &avcc(131)));
    assert!(matches!(
        run(&mut s, src, Some(avc(&p, 131)), 1),
        Err(Error::Truncated)
    ));
    assert_eq!(s.receiver_epoch(), 1);
    assert_eq!(s.configuration().bytes().as_ref(), avcc(131));
    let out = run(&mut s, packet(&p, &[IDR]), None, 1).unwrap();
    assert_eq!(out.configuration_bytes().as_ref(), avcc(131));
    assert_eq!(out.source().fields, fields());
}

fn init(s: &NalPacketSession, p: &Budget, q: u64) -> Result<NalPacketConfigOutput, Error> {
    let mut j = NalPacketConfigWriter::new(p, s.configuration(), 131072)?;
    for _ in 0..100_000 {
        if let BuildStep::Ready(v) = j.step(&mut WorkBudget::new(q), &AtomicBool::new(false))? {
            return Ok(v);
        }
    }
    panic!("configuration writer stalled")
}

#[test]
fn initialization_is_literal_cooperative_and_retained_after_session_drop() {
    let p = pool();
    let s = session(&p);
    let out = init(&s, &p, 1).unwrap();
    assert_eq!(out.bytes().as_ref(), conf());
    assert_eq!(out.source_bytes().as_ref(), avcc(128));
    drop(s);
    assert_eq!(out.source_bytes().as_ref(), avcc(128));
    assert_eq!(out.bytes().as_ref(), conf());
}

#[test]
fn configuration_writer_cancellation_is_sticky_even_at_work_zero() {
    let p = pool();
    let s = session(&p);
    let mut j = NalPacketConfigWriter::new(&p, s.configuration(), 131072).unwrap();
    let c = AtomicBool::new(false);
    assert!(matches!(
        j.step(&mut WorkBudget::new(0), &c).unwrap(),
        BuildStep::Yield
    ));
    c.store(true, Ordering::Relaxed);
    assert!(matches!(
        j.step(&mut WorkBudget::new(0), &c),
        Err(Error::Cancelled)
    ));
    c.store(false, Ordering::Relaxed);
    assert!(matches!(
        j.step(&mut WorkBudget::new(4096), &c),
        Err(Error::Cancelled)
    ));
}

#[test]
fn hevc_unknown_empty_array_is_preserved_source_but_refused_at_initialization() {
    let p = pool();
    let mut b = hvcc();
    b[22] = 4;
    b.extend_from_slice(&[55, 0, 0]);
    let mut parser = HvcConfigParser::new(
        &p,
        HvcConfigSource {
            source: SourceId(8),
            generation: 7,
            offset: 50,
        },
        bytes(&p, &b),
        HvcConfigLimits {
            maximum_record_bytes: 4096,
            maximum_arrays: 255,
            maximum_nals: 64,
            maximum_nal_bytes: 4096,
        },
    )
    .unwrap();
    let cfg = loop {
        if let BuildStep::Ready(v) = parser
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            break v;
        }
    };
    assert_eq!(cfg.arrays()[3].declared_nal_type(), 55);
    assert_eq!(cfg.source_bytes().as_ref(), b);
    let s = NalPacketSession::new(&p, NalPacketConfiguration::Hevc(cfg), limits()).unwrap();
    assert!(matches!(init(&s, &p, 1), Err(Error::Unsupported(_))));
    assert_eq!(s.configuration().bytes().as_ref(), b);
}

#[test]
fn hevc_lookahead_does_not_inject_without_irap_but_precedes_earliest_parameter_set() {
    let p = pool();
    let mut s =
        NalPacketSession::new(&p, NalPacketConfiguration::Hevc(hevc(&p)), limits()).unwrap();
    let v = hn(32);
    let non = hn(1);
    let irap = hn(19);
    assert_eq!(
        run(&mut s, packet(&p, &[&v, &non]), None, 1)
            .unwrap()
            .bytes()
            .as_ref(),
        [framed(4, &v), framed(4, &non)].concat()
    );
    let extra = [framed(4, &hn(32)), framed(4, &hn(33)), framed(4, &hn(34))].concat();
    let o = run(&mut s, packet(&p, &[&non, &v, &irap]), None, 1).unwrap();
    assert_eq!(
        o.bytes().as_ref(),
        [framed(4, &non), extra, framed(4, &v), framed(4, &irap)].concat()
    );
    assert!(matches!(
        o.segments()[0].origin,
        NalPacketOrigin::Packet { .. }
    ));
    assert!(matches!(
        o.segments()[1].origin,
        NalPacketOrigin::Configuration { .. }
    ));
    assert_eq!(
        o.configuration_source_identity().unwrap().0.offset,
        1_u64 << 34
    );
}

#[test]
fn hevc_lookahead_honors_nal_limit_before_reserving_output() {
    let p = pool();
    let mut l = limits();
    l.maximum_nals = 1;
    let mut s = NalPacketSession::new(&p, NalPacketConfiguration::Hevc(hevc(&p)), l).unwrap();
    let before = p.used();
    let result = run(&mut s, packet(&p, &[&hn(32), &hn(19)]), None, 1);
    assert!(matches!(result, Err(Error::Limit { maximum: 1, .. })));
    assert_eq!(p.used(), before);
}

#[test]
fn empty_avc_packet_and_zero_length_hevc_nal_remain_explicit_receiver_errors() {
    let p = pool();
    let mut a = session(&p);
    assert!(matches!(
        run(&mut a, packet(&p, &[]), None, 1),
        Err(Error::Invalid(_))
    ));
    let mut h =
        NalPacketSession::new(&p, NalPacketConfiguration::Hevc(hevc(&p)), limits()).unwrap();
    assert!(matches!(
        run(&mut h, packet(&p, &[&[]]), None, 1),
        Err(Error::Invalid(_))
    ));
    assert!(
        run(&mut h, packet(&p, &[]), None, 1)
            .unwrap()
            .bytes()
            .is_empty()
    );
}

#[test]
fn all_source_and_receiver_allocations_release_to_pool_control_only() {
    let p = pool();
    {
        let mut s = session(&p);
        let config = init(&s, &p, 1).unwrap();
        let o = run(&mut s, packet(&p, &[SPS, PPS, IDR, CONT]), None, 1).unwrap();
        drop(s);
        assert_eq!(config.source_bytes().as_ref(), avcc(128));
        assert_eq!(o.source().fields, fields());
    }
    assert_eq!(p.used(), Budget::control_storage_bytes());
}

#[test]
fn shared_caller_backing_is_charged_once_across_configuration_and_packet() {
    let p = Budget::new(3500).unwrap();
    let mut data = vec![0; 1800];
    data[..20].copy_from_slice(&avcc(128));
    data[200..207].copy_from_slice(&[0, 0, 0, 3, 0x65, 0xb8, 128]);
    let backing = bytes(&p, &data);
    let mut a = AvcConfigParser::new(
        &p,
        AvcConfigSource {
            source: SourceId(1),
            generation: 3,
            offset: 0,
        },
        backing.slice(0..20).unwrap(),
        AvcConfigLimits {
            maximum_record_bytes: 4096,
            maximum_nals: 64,
            maximum_nal_bytes: 4096,
        },
    )
    .unwrap();
    let cfg = loop {
        if let BuildStep::Ready(v) = a
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            break v;
        }
    };
    drop(a);
    let mut s = NalPacketSession::new(&p, NalPacketConfiguration::Avc(cfg), limits()).unwrap();
    let packet = NalPacket {
        bytes: backing.slice(200..207).unwrap(),
        location: SourceLocation {
            source: SourceId(1),
            offset: 200,
        },
        generation: 3,
        fields: fields(),
        new_extradata: None,
    };
    drop(backing);
    let out = run(&mut s, packet, None, 1).unwrap();
    assert!(
        out.source()
            .bytes
            .shares_storage_with(out.configuration_bytes())
    );
    assert_eq!(out.bytes().as_ref(), [conf(), framed(3, IDR)].concat());
    drop(out);
    drop(s);
    assert_eq!(p.used(), Budget::control_storage_bytes());
}

#[test]
fn nonclone_canonical_metadata_keeps_exact_and_unknown_clocks_without_projection() {
    struct Caller {
        pts: triergon_core::Time,
        dts: Option<triergon_core::Time>,
        marker: Box<u8>,
    }
    let p = pool();
    let mut s = session(&p);
    let raw = packet(&p, &[IDR]);
    let exact = triergon_core::Time::from_ticks(
        (1_i128 << 100) + 3,
        triergon_core::TimeBase::new(7, 99991).unwrap(),
    )
    .unwrap();
    let source = NalPacket {
        bytes: raw.bytes,
        location: raw.location,
        generation: raw.generation,
        new_extradata: None,
        fields: Caller {
            pts: exact,
            dts: None,
            marker: Box::new(42),
        },
    };
    let before = std::ptr::from_ref(source.fields.marker.as_ref());
    let out = run(&mut s, source, None, 1).unwrap();
    drop(s);
    assert_eq!(out.source().fields.pts, exact);
    assert!(out.source().fields.dts.is_none());
    assert_eq!(
        std::ptr::from_ref(out.source().fields.marker.as_ref()),
        before
    );
    assert_eq!(*out.source().fields.marker, 42);
}
