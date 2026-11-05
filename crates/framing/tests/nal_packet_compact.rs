#![forbid(unsafe_code)]
use std::mem::{needs_drop, size_of};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use triergon_core::{Budget, Bytes, Error, SourceId, SourceLocation, WorkBudget};
use triergon_framing::*;

// Original independently authored AVC/HEVC packet fixtures, not decoder code.
fn avcc(tag: u8) -> Vec<u8> {
    vec![
        1, 66, 0, 10, 255, 225, 0, 5, 0x67, 66, 0, 10, tag, 1, 0, 4, 0x68, 0xce, 6, 0xe2,
    ]
}
fn hvcc() -> Vec<u8> {
    let mut raw = vec![
        1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 120, 240, 0, 252, 253, 248, 248, 0, 0, 15, 3,
    ];
    for kind in [32, 33, 34] {
        raw.extend_from_slice(&[128 | kind, 0, 1, 0, 3, kind << 1, 1, 128]);
    }
    raw
}
fn hidden(pool: &Budget, raw: &[u8]) -> Bytes {
    let mut owner = vec![0xa5; 512];
    owner[32..32 + raw.len()].copy_from_slice(raw);
    Bytes::try_from_vec(pool, owner)
        .map_err(|e| e.error())
        .unwrap()
        .slice(32..32 + raw.len())
        .unwrap()
}
fn config(pool: &Budget, raw: Bytes, codec: Codec) -> NalPacketConfiguration {
    let cancel = AtomicBool::new(false);
    match codec {
        Codec::Avc => {
            let mut parser = AvcConfigParser::new(
                pool,
                AvcConfigSource {
                    source: SourceId(11),
                    generation: 29,
                    offset: 17,
                },
                raw,
                AvcConfigLimits {
                    maximum_record_bytes: 4096,
                    maximum_nals: 64,
                    maximum_nal_bytes: 4096,
                },
            )
            .unwrap();
            for _ in 0..10_000 {
                match parser.step(&mut WorkBudget::new(1), &cancel).unwrap() {
                    BuildStep::Ready(value) => return NalPacketConfiguration::Avc(value),
                    BuildStep::Yield => (),
                    BuildStep::Complete => panic!("missing AVC configuration"),
                }
            }
        }
        Codec::Hevc => {
            let mut parser = HvcConfigParser::new(
                pool,
                HvcConfigSource {
                    source: SourceId(11),
                    generation: 29,
                    offset: 17,
                },
                raw,
                HvcConfigLimits {
                    maximum_record_bytes: 4096,
                    maximum_arrays: 255,
                    maximum_nals: 64,
                    maximum_nal_bytes: 4096,
                },
            )
            .unwrap();
            for _ in 0..10_000 {
                match parser.step(&mut WorkBudget::new(1), &cancel).unwrap() {
                    BuildStep::Ready(value) => return NalPacketConfiguration::Hevc(value),
                    BuildStep::Yield => (),
                    BuildStep::Complete => panic!("missing HEVC configuration"),
                }
            }
        }
    }
    panic!("bounded configuration stalled")
}
fn limits() -> NalPacketLimits {
    NalPacketLimits {
        maximum_packet_bytes: 4096,
        maximum_output_bytes: 8192,
        maximum_nals: 64,
        maximum_segments: 128,
    }
}
fn fields() -> NalPacketFields {
    NalPacketFields {
        pts: i64::MIN,
        dts: i64::MAX,
        duration: -7,
        position: -1,
        flags: u32::MAX,
        stream_index: i32::MIN,
    }
}
fn packet(bytes: Bytes) -> NalPacket {
    NalPacket {
        bytes,
        fields: fields(),
        location: SourceLocation {
            source: SourceId(77),
            offset: 0x1_0000_0001,
        },
        generation: 31,
        new_extradata: None,
    }
}
type Polls = Vec<(u64, Result<NalPacketProgress, Error>)>;
fn drive<O>(
    plan: &[u64],
    mut next: impl FnMut(&mut WorkBudget, &mut Option<O>) -> Result<NalPacketProgress, Error>,
) -> (Polls, Option<O>) {
    let mut ready = None;
    let mut polls = Vec::new();
    for at in 0..10_000 {
        let mut work = WorkBudget::new(plan[at % plan.len()]);
        let status = next(&mut work, &mut ready);
        let ended = !matches!(status, Ok(NalPacketProgress::Yield));
        polls.push((work.remaining(), status));
        if ended {
            return (polls, ready);
        }
        assert!(ready.is_none());
    }
    panic!("bounded packet stalled")
}
type SegmentFacts = (Vec<u8>, NalPacketOrigin, u8, usize);
#[derive(Debug, PartialEq, Eq)]
struct OutputFacts {
    bytes: Vec<u8>,
    source: Vec<u8>,
    configuration: Vec<u8>,
    fields: NalPacketFields,
    location: SourceLocation,
    generation: u64,
    extra: Option<Vec<u8>>,
    config_identity: Option<(SourceLocation, u64)>,
    config_refusal: Option<Error>,
    packet_refusal: Option<Error>,
    epoch: u64,
    forward: bool,
    segments: Vec<SegmentFacts>,
}
macro_rules! output_facts {
    ($value:expr, $parts:expr) => {{
        let value = $value;
        OutputFacts {
            bytes: value.bytes().as_ref().to_vec(),
            source: value.source().bytes.as_ref().to_vec(),
            configuration: value.configuration_bytes().as_ref().to_vec(),
            fields: value.source().fields,
            location: value.source().location,
            generation: value.source().generation,
            extra: value
                .source()
                .new_extradata
                .as_ref()
                .map(|b| b.as_ref().to_vec()),
            config_identity: value.configuration_source_identity(),
            config_refusal: value.configuration_source_refusal(),
            packet_refusal: value.packet_source_refusal(),
            epoch: value.receiver_epoch(),
            forward: value.forward_new_extradata(),
            segments: $parts,
        }
    }};
}
fn owned_facts(value: &NalPacketOutput) -> OutputFacts {
    output_facts!(
        value,
        value
            .segments()
            .iter()
            .map(|p| (
                p.source_bytes().as_ref().to_vec(),
                p.origin.clone(),
                p.prefix_bytes,
                p.output_offset
            ))
            .collect()
    )
}
fn compact_facts(value: &NalPacketCompactOutput) -> OutputFacts {
    output_facts!(
        value,
        value
            .segments()
            .iter()
            .enumerate()
            .map(|(i, p)| (
                value.segment_bytes(i).unwrap().to_vec(),
                p.origin(),
                p.prefix_bytes(),
                p.output_offset()
            ))
            .collect()
    )
}
fn trace(
    codec: Codec,
    raw: &[u8],
    plan: &[u64],
    compact: bool,
) -> (Polls, Option<OutputFacts>, usize) {
    let pool = Budget::new(16_384).unwrap();
    let baseline = pool.used();
    let config_bytes = match codec {
        Codec::Avc => avcc(128),
        Codec::Hevc => hvcc(),
    };
    let mut session = NalPacketSession::new(
        &pool,
        config(&pool, hidden(&pool, &config_bytes), codec),
        limits(),
    )
    .unwrap();
    let source = packet(hidden(&pool, raw));
    let cancel = AtomicBool::new(false);
    let (polls, facts, retained) = if compact {
        let mut job = session.packet_compact(source, None).unwrap();
        let (polls, ready) = drive(plan, |w, out| job.step_into(w, &cancel, out));
        drop(job);
        drop(session);
        let facts = ready.as_ref().map(compact_facts);
        let retained = pool.used();
        drop(ready);
        (polls, facts, retained)
    } else {
        let mut job = session.packet(source, None).unwrap();
        let (polls, ready) = drive(plan, |w, out| job.step_into(w, &cancel, out));
        drop(job);
        drop(session);
        let facts = ready.as_ref().map(owned_facts);
        let retained = pool.used();
        drop(ready);
        (polls, facts, retained)
    };
    assert_eq!(pool.used(), baseline);
    (polls, facts, retained)
}

#[test]
fn descriptor_is_smaller_without_expanding_job_or_output() {
    println!(
        "COMPACT_LAYOUT owned_segment={} compact_segment={} owned_job={} compact_job={} owned_output={} compact_output={} compact_needs_drop={}",
        size_of::<NalPacketSegment>(),
        size_of::<NalPacketRangeSegment>(),
        size_of::<NalPacketJob<'_>>(),
        size_of::<NalPacketCompactJob<'_>>(),
        size_of::<NalPacketOutput>(),
        size_of::<NalPacketCompactOutput>(),
        needs_drop::<NalPacketRangeSegment>()
    );
    assert!(size_of::<NalPacketRangeSegment>() < size_of::<NalPacketSegment>());
    assert!(!needs_drop::<NalPacketRangeSegment>());
    assert_eq!(
        size_of::<NalPacketCompactOutput>(),
        size_of::<NalPacketOutput>()
    );
    assert_eq!(
        size_of::<NalPacketCompactJob<'_>>(),
        size_of::<NalPacketJob<'_>>()
    );
}

#[test]
fn strict_views_match_all_source_and_segment_facts_with_literal_work_and_refusal_traces() {
    for plan in [&[0, 1][..], &[1][..], &[7][..], &[4096][..]] {
        for (codec, raw) in [
            (Codec::Avc, &[0, 0, 0, 3, 0x65, 0xb8, 128][..]),
            (Codec::Avc, &[0, 0, 0, 1, 0x41][..]),
            (Codec::Avc, &[0, 0][..]),
            (Codec::Avc, &[0, 0, 0, 2, 0x41][..]),
            (Codec::Avc, &[0, 0, 0, 1, 0x81][..]),
            (Codec::Hevc, &[0, 0, 0, 3, 38, 1, 128][..]),
            (Codec::Hevc, &[0, 0, 0, 0][..]),
            (Codec::Hevc, &[0, 0, 0, 1, 38][..]),
        ] {
            let (old_polls, old_facts, _) = trace(codec, raw, plan, false);
            let (new_polls, new_facts, _) = trace(codec, raw, plan, true);
            assert_eq!(new_polls, old_polls);
            assert_eq!(new_facts, old_facts);
        }
    }
}

#[test]
fn descriptor_charge_shrinks_while_both_hidden_parent_owners_remain_retained() {
    let raw = &[0, 0, 0, 3, 0x65, 0xb8, 128];
    let (_, old, owned) = trace(Codec::Avc, raw, &[4096], false);
    let (_, new, compact) = trace(Codec::Avc, raw, &[4096], true);
    assert!(old.is_some() && new.is_some());
    assert!(compact < owned);
    // Both raw views came from separate full 512-byte charged parents. The
    // descriptor representation must not release either parent prematurely.
    assert!(compact >= 2 * (512 + Bytes::owner_overhead_bytes()));
}

fn compact_passthrough(pool: &Budget, raw: &[u8]) -> NalPacketCompactOutput {
    let mut session = NalPacketSession::new(
        pool,
        NalPacketConfiguration::Passthrough {
            codec: Codec::Avc,
            raw: Bytes::copy_from_slice(pool, &[]).unwrap(),
        },
        limits(),
    )
    .unwrap();
    let mut job = session
        .packet_compact(packet(hidden(pool, raw)), None)
        .unwrap();
    let (_, ready) = drive(&[4096], |w, out| {
        job.step_into(w, &AtomicBool::new(false), out)
    });
    ready.unwrap()
}

#[test]
fn explicit_materialization_is_bounded_and_survives_output_and_producer_teardown() {
    let pool = Budget::new(16_384).unwrap();
    let baseline = pool.used();
    let output = compact_passthrough(&pool, &[0x61, 7]);
    assert_eq!(output.segment_bytes(0), Some(&[0x61, 7][..]));
    assert!(output.bytes().shares_storage_with(&output.source().bytes));
    let segment = output.materialize_segment(0).unwrap();
    assert!(
        segment
            .source_bytes()
            .shares_storage_with(&output.source().bytes)
    );
    assert!(matches!(
        output.materialize_segment(1),
        Err(Error::Invalid(_))
    ));
    assert_eq!(output.segment_bytes(1), None);
    drop(output);
    assert_eq!(segment.source_bytes().as_ref(), &[0x61, 7]);
    assert!(pool.used() >= baseline + 512 + Bytes::owner_overhead_bytes());
    drop(segment);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn occupied_compact_sink_has_no_work_cancel_owner_or_job_side_effects() {
    let pool = Budget::new(16_384).unwrap();
    let mut ready = Some(compact_passthrough(&pool, &[0x61, 7]));
    let owner = ready.as_ref().unwrap().bytes().clone();
    let mut session = NalPacketSession::new(
        &pool,
        config(&pool, hidden(&pool, &avcc(128)), Codec::Avc),
        limits(),
    )
    .unwrap();
    let mut job = session
        .packet_compact(packet(hidden(&pool, &[0, 0, 0, 1, 0x41])), None)
        .unwrap();
    let before = pool.used();
    let mut work = WorkBudget::new(7);
    assert_eq!(
        job.step_into(&mut work, &AtomicBool::new(true), &mut ready),
        Err(Error::Invalid("packet output sink occupied"))
    );
    assert_eq!(work.remaining(), 7);
    assert_eq!(pool.used(), before);
    assert!(ready.as_ref().unwrap().bytes().shares_storage_with(&owner));
    drop(ready.take());
    assert_eq!(
        job.step_into(
            &mut WorkBudget::new(4096),
            &AtomicBool::new(false),
            &mut ready
        ),
        Ok(NalPacketProgress::Ready)
    );
}

#[test]
fn pressure_retry_and_zero_work_keep_the_compact_sink_empty_and_cancel_is_sticky() {
    let pool = Budget::new(16_384).unwrap();
    let baseline = pool.used();
    let mut session = NalPacketSession::new(
        &pool,
        config(&pool, hidden(&pool, &avcc(128)), Codec::Avc),
        limits(),
    )
    .unwrap();
    let mut job = session
        .packet_compact(packet(hidden(&pool, &[0, 0, 0, 3, 0x65, 0xb8, 128])), None)
        .unwrap();
    let mut ready = None;
    assert_eq!(
        job.step_into(&mut WorkBudget::new(0), &AtomicBool::new(false), &mut ready),
        Ok(NalPacketProgress::Yield)
    );
    let hold = pool.reserve(pool.limit() - pool.used()).unwrap();
    assert!(matches!(
        job.step_into(
            &mut WorkBudget::new(4096),
            &AtomicBool::new(false),
            &mut ready
        ),
        Err(Error::Backpressure { .. })
    ));
    assert!(ready.is_none());
    drop(hold);
    let cancel = AtomicBool::new(true);
    let mut work = WorkBudget::new(1);
    assert_eq!(
        job.step_into(&mut work, &cancel, &mut ready),
        Err(Error::Cancelled)
    );
    assert_eq!(work.remaining(), 1);
    assert!(ready.is_none());
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        job.step_into(&mut WorkBudget::new(4096), &cancel, &mut ready),
        Err(Error::Cancelled)
    );
    drop(job);
    drop(session);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn successful_pressure_retry_retains_packet_configuration_and_segment_facts() {
    let pool = Budget::new(16_384).unwrap();
    let baseline = pool.used();
    let mut session = NalPacketSession::new(
        &pool,
        config(&pool, hidden(&pool, &avcc(128)), Codec::Avc),
        limits(),
    )
    .unwrap();
    let raw = &[0, 0, 0, 3, 0x65, 0xb8, 128];
    let mut job = session
        .packet_compact(packet(hidden(&pool, raw)), None)
        .unwrap();
    let hold = pool.reserve(pool.limit() - pool.used()).unwrap();
    let mut ready = None;
    assert!(matches!(
        job.step_into(
            &mut WorkBudget::new(4096),
            &AtomicBool::new(false),
            &mut ready
        ),
        Err(Error::Backpressure { .. })
    ));
    drop(hold);
    let (_, ready) = drive(&[1], |w, out| {
        job.step_into(w, &AtomicBool::new(false), out)
    });
    let output = ready.unwrap();
    drop(job);
    drop(session);
    assert_eq!(output.source().fields, fields());
    assert_eq!(output.source().bytes.as_ref(), raw);
    assert_eq!(output.configuration_bytes().as_ref(), avcc(128));
    assert_eq!(output.segments().len(), 3);
    assert_eq!(output.segment_bytes(0), Some(&[0x67, 66, 0, 10, 128][..]));
    assert_eq!(output.segment_bytes(1), Some(&[0x68, 0xce, 6, 0xe2][..]));
    assert_eq!(output.segment_bytes(2), Some(&[0x65, 0xb8, 128][..]));
    drop(output);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn noncopy_metadata_and_old_configuration_survive_update_flush_and_producer_close() {
    struct Metadata(Arc<AtomicUsize>);
    impl Drop for Metadata {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let pool = Budget::new(32_768).unwrap();
    let baseline = pool.used();
    let mut session = NalPacketSession::new(
        &pool,
        config(&pool, hidden(&pool, &avcc(128)), Codec::Avc),
        limits(),
    )
    .unwrap();
    let original = packet(hidden(&pool, &[0, 0, 0, 3, 0x65, 0xb8, 128]));
    let source = NalPacket {
        bytes: original.bytes,
        location: original.location,
        generation: original.generation,
        new_extradata: None,
        fields: Metadata(drops.clone()),
    };
    let mut job = session.packet_compact(source, None).unwrap();
    let (_, output) = drive(&[7], |w, out| {
        job.step_into(w, &AtomicBool::new(false), out)
    });
    let output = output.unwrap();
    drop(job);
    let new_raw = hidden(&pool, &avcc(129));
    let update = match config(&pool, new_raw.clone(), Codec::Avc) {
        NalPacketConfiguration::Avc(value) => value,
        _ => unreachable!(),
    };
    let mut second = packet(hidden(&pool, &[0, 0, 0, 3, 0x65, 0xb8, 128]));
    second.new_extradata = Some(new_raw);
    let mut job = session.packet_compact(second, Some(update)).unwrap();
    let (_, second) = drive(&[1], |w, out| {
        job.step_into(w, &AtomicBool::new(false), out)
    });
    drop(job);
    session.flush();
    session.drain();
    drop(session);
    drop(second);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    assert_eq!(output.configuration_bytes().as_ref(), avcc(128));
    assert_eq!(output.segment_bytes(0), Some(&[0x67, 66, 0, 10, 128][..]));
    let original_config_segment = output.materialize_segment(0).unwrap();
    drop(output);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert_eq!(
        original_config_segment.source_bytes().as_ref(),
        &[0x67, 66, 0, 10, 128]
    );
    assert!(pool.used() >= baseline + 512 + Bytes::owner_overhead_bytes());
    drop(original_config_segment);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn observed_packet_keeps_source_refusal_without_changing_compatibility_bytes_or_work() {
    let run = |compact| {
        let pool = Budget::new(16_384).unwrap();
        let baseline = pool.used();
        let mut session = NalPacketSession::new(
            &pool,
            config(&pool, hidden(&pool, &avcc(128)), Codec::Avc),
            limits(),
        )
        .unwrap();
        let source = packet(hidden(&pool, &[0, 0, 0, 1, 0x81]));
        let cancel = AtomicBool::new(false);
        let (polls, facts) = if compact {
            let mut job = session.packet_observed_compact(source, None).unwrap();
            let (polls, ready) = drive(&[0, 1, 7], |w, out| job.step_into(w, &cancel, out));
            (polls, compact_facts(&ready.unwrap()))
        } else {
            let mut job = session.packet_observed(source, None).unwrap();
            let (polls, ready) = drive(&[0, 1, 7], |w, out| job.step_into(w, &cancel, out));
            (polls, owned_facts(&ready.unwrap()))
        };
        assert!(facts.packet_refusal.is_some());
        drop(session);
        assert_eq!(pool.used(), baseline);
        (polls, facts)
    };
    assert_eq!(run(false), run(true));
}

#[test]
fn physical_rejection_retains_verified_update_and_original_source_with_no_partial_output() {
    let pool = Budget::new(32_768).unwrap();
    let baseline = pool.used();
    let mut session = NalPacketSession::new(
        &pool,
        config(&pool, hidden(&pool, &avcc(128)), Codec::Avc),
        limits(),
    )
    .unwrap();
    let new_raw = hidden(&pool, &avcc(129));
    let mut parser = NalObservedAvcConfigParser::new_update(
        &pool,
        AvcConfigSource {
            source: SourceId(12),
            generation: 30,
            offset: 19,
        },
        new_raw.clone(),
        AvcConfigLimits {
            maximum_record_bytes: 4096,
            maximum_nals: 64,
            maximum_nal_bytes: 4096,
        },
    )
    .unwrap();
    let update = loop {
        match parser
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            BuildStep::Ready(value) => break value,
            BuildStep::Yield => (),
            BuildStep::Complete => panic!("missing observed update"),
        }
    };
    drop(parser);
    let mut source = packet(hidden(&pool, &[0, 0]));
    source.new_extradata = Some(new_raw);
    let mut job = session
        .packet_observed_compact(source, Some(update))
        .unwrap();
    let (polls, ready) = drive(&[0, 1, 7], |w, out| {
        job.step_into(w, &AtomicBool::new(false), out)
    });
    assert!(ready.is_none());
    assert_eq!(polls.last().unwrap().1, Err(Error::Truncated));
    let rejection = job.take_receiver_rejection().unwrap();
    assert_eq!(
        rejection.kind(),
        NalPacketReceiverErrorKind::IncompleteLengthPrefix
    );
    assert_eq!(rejection.source_error(), Error::Truncated);
    assert_eq!(rejection.source().bytes.as_ref(), &[0, 0]);
    assert_eq!(rejection.source().fields, fields());
    assert_eq!(rejection.configuration_bytes().as_ref(), avcc(129));
    assert_eq!(rejection.receiver_epoch(), 1);
    assert!(job.take_receiver_rejection().is_none());
    drop(job);
    assert_eq!(session.receiver_epoch(), 1);
    drop(session);
    assert!(pool.used() > baseline);
    drop(rejection);
    assert_eq!(pool.used(), baseline);
}
