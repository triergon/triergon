#![forbid(unsafe_code)]
//! Cooperative packet jobs: `step` and `step_into` publish the same outputs,
//! rejections, work and pool traces; an occupied sink, zero work, pressure and
//! cancellation leave the sink empty; outputs keep their source, caller metadata
//! and configuration owners after the job and session close.
use std::mem::needs_drop;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use triergon_core::{Budget, Bytes, Error, SourceId, SourceLocation, WorkBudget};
use triergon_framing::compat::nal::*;
use triergon_framing::*;

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

fn input(bytes: Bytes) -> NalPacket {
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

fn avcc(tag: u8) -> Vec<u8> {
    vec![
        1, 66, 0, 10, 255, 225, 0, 5, 0x67, 66, 0, 10, tag, 1, 0, 4, 0x68, 0xce, 6, 0xe2,
    ]
}

/// A slice of a separately charged 512-byte parent.
fn hidden(pool: &Budget, raw: &[u8]) -> Bytes {
    let mut owner = vec![0xa5; 512];
    owner[32..32 + raw.len()].copy_from_slice(raw);
    Bytes::try_from_vec(pool, owner)
        .map_err(|e| e.error())
        .unwrap()
        .slice(32..32 + raw.len())
        .unwrap()
}

fn parse_avc(pool: &Budget, raw: Bytes) -> AvcConfiguration {
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
        match parser
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            BuildStep::Ready(value) => return value,
            BuildStep::Yield => (),
            BuildStep::Complete => panic!("missing AVC configuration"),
        }
    }
    panic!("bounded configuration stalled")
}

/// Strict avcC session whose configuration is a hidden-parent slice.
fn avc_session(pool: &Budget) -> NalPacketSession {
    let config = parse_avc(pool, hidden(pool, &avcc(128)));
    NalPacketSession::new(pool, NalPacketConfiguration::Avc(config), limits()).unwrap()
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

fn configuration(pool: &Budget, framed: bool) -> NalPacketConfiguration {
    if !framed {
        return NalPacketConfiguration::Passthrough {
            codec: Codec::Avc,
            raw: Bytes::copy_from_slice(pool, &[]).unwrap(),
        };
    }
    let mut parser = ReceiverAvcConfigParser::new(
        pool,
        AvcConfigSource {
            source: SourceId(11),
            generation: 29,
            offset: 17,
        },
        Bytes::copy_from_slice(pool, &[1, 66, 0, 10, 255, 224, 0]).unwrap(),
        AvcConfigLimits {
            maximum_record_bytes: 4096,
            maximum_nals: 64,
            maximum_nal_bytes: 4096,
        },
    )
    .unwrap();
    loop {
        match parser
            .step(&mut WorkBudget::new(4096), &AtomicBool::new(false))
            .unwrap()
        {
            BuildStep::Ready(value) => return NalPacketConfiguration::ReceiverAvc(value),
            BuildStep::Yield => (),
            BuildStep::Complete => panic!("missing configuration"),
        }
    }
}

fn poll(
    job: &mut NalPacketJob<'_>,
    work: &mut WorkBudget,
    cancel: &AtomicBool,
    ready: &mut Option<NalPacketOutput>,
    direct: bool,
) -> Result<NalPacketProgress, Error> {
    if direct {
        job.step_into(work, cancel, ready)
    } else {
        match job.step(work, cancel)? {
            BuildStep::Yield => Ok(NalPacketProgress::Yield),
            BuildStep::Complete => Ok(NalPacketProgress::Complete),
            BuildStep::Ready(value) => {
                *ready = Some(value);
                Ok(NalPacketProgress::Ready)
            }
        }
    }
}

fn trace(framed: bool, raw: &[u8], plan: &[u64], direct: bool) -> Vec<String> {
    let pool = Budget::new(16384).unwrap();
    let baseline = pool.used();
    let mut session = NalPacketSession::new(&pool, configuration(&pool, framed), limits()).unwrap();
    let mut job = session
        .receiver_packet(input(hidden(&pool, raw)), None)
        .unwrap();
    let cancel = AtomicBool::new(false);
    let mut ready = None;
    let mut events = Vec::new();
    let mut finished = false;
    for at in 0..2000 {
        let quantum = plan[at % plan.len()];
        let mut work = WorkBudget::new(quantum);
        let result = poll(&mut job, &mut work, &cancel, &mut ready, direct);
        events.push(format!("{result:?}:{}:{}", work.remaining(), pool.used()));
        match result {
            Ok(NalPacketProgress::Yield) => assert!(ready.is_none()),
            Ok(NalPacketProgress::Ready) => {
                assert!(ready.is_some());
                finished = true;
                break;
            }
            Ok(NalPacketProgress::Complete) => panic!("output disappeared"),
            Err(error) => {
                assert!(ready.is_none());
                let mut retry_work = WorkBudget::new(17);
                assert_eq!(
                    poll(&mut job, &mut retry_work, &cancel, &mut ready, direct),
                    Err(error)
                );
                assert_eq!(retry_work.remaining(), 17);
                if let Some(rejection) = job.take_receiver_rejection() {
                    events.push(format!(
                        "{:?}:{:?}:{}",
                        rejection.kind(),
                        rejection.source().bytes.as_ref(),
                        rejection.source().generation
                    ));
                }
                finished = true;
                break;
            }
        }
    }
    assert!(finished, "bounded operation stalled");
    if let Some(output) = ready.take() {
        events.push(format!(
            "{:?}:{:?}:{}:{}",
            output.bytes().as_ref(),
            output.source().fields,
            output.source().generation,
            output.source().location.offset
        ));
        for (i, segment) in output.segments().iter().enumerate() {
            events.push(format!(
                "{:?}:{:?}:{}:{}",
                output.segment_bytes(i).unwrap(),
                segment.origin(),
                segment.prefix_bytes(),
                segment.output_offset()
            ));
        }
        assert_eq!(
            poll(
                &mut job,
                &mut WorkBudget::new(0),
                &cancel,
                &mut ready,
                direct
            ),
            Ok(NalPacketProgress::Complete)
        );
        assert!(ready.is_none());
        drop(job);
        drop(session);
        assert_eq!(output.source().fields, fields());
        assert_eq!(output.source().generation, 31);
        assert!(pool.used() > baseline);
        drop(output);
    } else {
        drop(job);
        drop(session);
    }
    assert_eq!(pool.used(), baseline);
    events
}

#[test]
fn direct_and_compatibility_paths_keep_literal_work_owner_and_failure_traces() {
    for plan in [&[0, 1][..], &[1][..], &[7][..], &[4096][..]] {
        for (framed, raw) in [
            (false, &[0x61, 7][..]),
            (true, &[0, 0, 0, 1, 0x61][..]),
            (true, &[0, 0][..]),
            (true, &[0, 0, 0, 2, 0x61][..]),
        ] {
            assert_eq!(
                trace(framed, raw, plan, false),
                trace(framed, raw, plan, true)
            );
        }
    }
}

fn passthrough_output(pool: &Budget, raw: &[u8]) -> NalPacketOutput {
    let mut session = NalPacketSession::new(pool, configuration(pool, false), limits()).unwrap();
    let mut job = session
        .receiver_packet(input(hidden(pool, raw)), None)
        .unwrap();
    let mut ready = None;
    assert_eq!(
        job.step_into(
            &mut WorkBudget::new(4096),
            &AtomicBool::new(false),
            &mut ready
        ),
        Ok(NalPacketProgress::Ready)
    );
    ready.unwrap()
}

#[test]
fn occupied_sink_is_preserved_without_work_job_or_cancel_side_effects() {
    let pool = Budget::new(16384).unwrap();
    let mut ready = Some(passthrough_output(&pool, &[0x65]));
    let owner = ready.as_ref().unwrap().bytes().clone();
    let mut session = NalPacketSession::new(&pool, configuration(&pool, false), limits()).unwrap();
    let mut job = session
        .receiver_packet(
            input(Bytes::copy_from_slice(&pool, &[0x61, 9]).unwrap()),
            None,
        )
        .unwrap();
    let before = pool.used();
    let mut work = WorkBudget::new(9);
    assert_eq!(
        job.step_into(&mut work, &AtomicBool::new(true), &mut ready),
        Err(Error::Invalid("packet output sink occupied"))
    );
    assert_eq!(work.remaining(), 9);
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
    assert_eq!(ready.unwrap().bytes().as_ref(), &[0x61, 9]);
}

#[test]
fn zero_work_never_publishes_and_cancellation_is_sticky() {
    let pool = Budget::new(16384).unwrap();
    let mut session = NalPacketSession::new(&pool, configuration(&pool, true), limits()).unwrap();
    let mut job = session
        .receiver_packet(
            input(Bytes::copy_from_slice(&pool, &[0, 0, 0, 1, 0x61]).unwrap()),
            None,
        )
        .unwrap();
    let mut ready = None;
    let before = pool.used();
    assert_eq!(
        job.step_into(&mut WorkBudget::new(0), &AtomicBool::new(false), &mut ready),
        Ok(NalPacketProgress::Yield)
    );
    assert!(ready.is_none());
    assert_eq!(pool.used(), before);
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
    assert!(ready.is_none());
}

#[test]
fn recoverable_pressure_keeps_sink_empty_and_retry_keeps_all_source_facts() {
    let pool = Budget::new(16384).unwrap();
    let baseline = pool.used();
    let mut session = NalPacketSession::new(&pool, configuration(&pool, true), limits()).unwrap();
    let mut job = session
        .receiver_packet(
            input(Bytes::copy_from_slice(&pool, &[0, 0, 0, 1, 0x61]).unwrap()),
            None,
        )
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
    assert!(ready.is_none());
    drop(hold);
    assert_eq!(
        job.step_into(
            &mut WorkBudget::new(4096),
            &AtomicBool::new(false),
            &mut ready
        ),
        Ok(NalPacketProgress::Ready)
    );
    let output = ready.take().unwrap();
    assert_eq!(output.bytes().as_ref(), &[0, 0, 0, 1, 0x61]);
    assert_eq!(output.source().fields, fields());
    assert_eq!(output.source().generation, 31);
    drop(job);
    drop(session);
    assert_eq!(output.source().bytes.as_ref(), &[0, 0, 0, 1, 0x61]);
    drop(output);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn non_copy_caller_metadata_moves_once_and_outlives_the_job_and_session() {
    use std::sync::{Arc, atomic::AtomicUsize};
    struct Metadata(Arc<AtomicUsize>);
    impl Drop for Metadata {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let pool = Budget::new(16384).unwrap();
    let baseline = pool.used();
    let source = input(Bytes::copy_from_slice(&pool, &[0x61]).unwrap());
    let packet = NalPacket {
        bytes: source.bytes,
        fields: Metadata(drops.clone()),
        location: source.location,
        generation: source.generation,
        new_extradata: source.new_extradata,
    };
    let mut session = NalPacketSession::new(&pool, configuration(&pool, false), limits()).unwrap();
    let mut job = session.receiver_packet(packet, None).unwrap();
    let mut ready = None;
    assert_eq!(
        job.step_into(
            &mut WorkBudget::new(4096),
            &AtomicBool::new(false),
            &mut ready
        ),
        Ok(NalPacketProgress::Ready)
    );
    drop(job);
    drop(session);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    let output = ready.take().unwrap();
    assert_eq!(output.bytes().as_ref(), &[0x61]);
    assert_eq!(output.source().generation, 31);
    drop(output);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn retained_segment_bytes_are_bounded_and_survive_output_and_producer_teardown() {
    assert!(!needs_drop::<NalPacketSegment>());
    let pool = Budget::new(16_384).unwrap();
    let baseline = pool.used();
    let output = passthrough_output(&pool, &[0x61, 7]);
    assert_eq!(output.segment_bytes(0), Some(&[0x61, 7][..]));
    assert!(output.bytes().shares_storage_with(&output.source().bytes));
    let segment = output.retain_segment_bytes(0).unwrap();
    assert!(segment.shares_storage_with(&output.source().bytes));
    assert!(matches!(
        output.retain_segment_bytes(1),
        Err(Error::Invalid(_))
    ));
    assert_eq!(output.segment_bytes(1), None);
    drop(output);
    assert_eq!(segment.as_ref(), &[0x61, 7]);
    assert!(pool.used() >= baseline + 512 + Bytes::owner_overhead_bytes());
    drop(segment);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn pressure_then_cancellation_on_an_injecting_packet_keeps_the_sink_empty() {
    let pool = Budget::new(16_384).unwrap();
    let baseline = pool.used();
    let mut session = avc_session(&pool);
    let mut job = session
        .packet(input(hidden(&pool, &[0, 0, 0, 3, 0x65, 0xb8, 128])), None)
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
    let mut session = avc_session(&pool);
    let raw = &[0, 0, 0, 3, 0x65, 0xb8, 128];
    let mut job = session.packet(input(hidden(&pool, raw)), None).unwrap();
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
    // The packet and configuration came from separate 512-byte charged
    // parents; the descriptors must not release either parent prematurely.
    assert!(pool.used() >= 2 * (512 + Bytes::owner_overhead_bytes()));
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
    let mut session = avc_session(&pool);
    let original = input(hidden(&pool, &[0, 0, 0, 3, 0x65, 0xb8, 128]));
    let source = NalPacket {
        bytes: original.bytes,
        location: original.location,
        generation: original.generation,
        new_extradata: None,
        fields: Metadata(drops.clone()),
    };
    let mut job = session.packet(source, None).unwrap();
    let (_, output) = drive(&[7], |w, out| {
        job.step_into(w, &AtomicBool::new(false), out)
    });
    let output = output.unwrap();
    drop(job);
    let new_raw = hidden(&pool, &avcc(129));
    let update = parse_avc(&pool, new_raw.clone());
    let mut second = input(hidden(&pool, &[0, 0, 0, 3, 0x65, 0xb8, 128]));
    second.new_extradata = Some(new_raw);
    let mut job = session.packet(second, Some(update)).unwrap();
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
    let original_config_segment = output.retain_segment_bytes(0).unwrap();
    drop(output);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert_eq!(original_config_segment.as_ref(), &[0x67, 66, 0, 10, 128]);
    assert!(pool.used() >= baseline + 512 + Bytes::owner_overhead_bytes());
    drop(original_config_segment);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn observed_packet_publishes_with_its_strict_header_refusal_attached() {
    let pool = Budget::new(16_384).unwrap();
    let baseline = pool.used();
    let mut session = avc_session(&pool);
    let mut job = session
        .receiver_packet(input(hidden(&pool, &[0, 0, 0, 1, 0x81])), None)
        .unwrap();
    let (_, ready) = drive(&[0, 1, 7], |w, out| {
        job.step_into(w, &AtomicBool::new(false), out)
    });
    let output = ready.unwrap();
    assert!(output.packet_source_refusal().is_some());
    drop(job);
    drop(session);
    drop(output);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn physical_rejection_retains_verified_update_and_original_source_with_no_partial_output() {
    let pool = Budget::new(32_768).unwrap();
    let baseline = pool.used();
    let mut session = avc_session(&pool);
    let new_raw = hidden(&pool, &avcc(129));
    let mut parser = ReceiverAvcConfigParser::new_update(
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
    let mut source = input(hidden(&pool, &[0, 0]));
    source.new_extradata = Some(new_raw);
    let mut job = session.receiver_packet(source, Some(update)).unwrap();
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

fn hvcc() -> Vec<u8> {
    let mut raw = vec![
        1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 120, 240, 0, 252, 253, 248, 248, 0, 0, 15, 3,
    ];
    for kind in [32, 33, 34] {
        raw.extend_from_slice(&[128 | kind, 0, 1, 0, 3, kind << 1, 1, 128]);
    }
    raw
}

/// Strict hvcC session whose configuration is a hidden-parent slice.
fn hevc_session(pool: &Budget) -> NalPacketSession {
    let mut parser = HvcConfigParser::new(
        pool,
        HvcConfigSource {
            source: SourceId(11),
            generation: 29,
            offset: 17,
        },
        hidden(pool, &hvcc()),
        HvcConfigLimits {
            maximum_record_bytes: 4096,
            maximum_arrays: 255,
            maximum_nals: 64,
            maximum_nal_bytes: 4096,
        },
    )
    .unwrap();
    let config = loop {
        match parser
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            BuildStep::Ready(value) => break value,
            BuildStep::Yield => (),
            BuildStep::Complete => panic!("missing HEVC configuration"),
        }
    };
    NalPacketSession::new(pool, NalPacketConfiguration::Hevc(config), limits()).unwrap()
}

/// Every published fact of an output, segment payloads included.
fn facts(out: &NalPacketOutput) -> String {
    let segments: Vec<_> = out
        .segments()
        .iter()
        .enumerate()
        .map(|(i, s)| {
            (
                out.segment_bytes(i),
                s.origin(),
                s.prefix_bytes(),
                s.output_offset(),
            )
        })
        .collect();
    format!(
        "{:?} {:?} {:?} {:?} {:?} {} {:?} {:?} {:?} {:?} {} {} {:?}",
        out.bytes().as_ref(),
        out.source().bytes.as_ref(),
        out.configuration_bytes().as_ref(),
        out.source().fields,
        out.source().location,
        out.source().generation,
        out.source()
            .new_extradata
            .as_ref()
            .map(|b| b.as_ref().to_vec()),
        out.configuration_source_identity(),
        out.configuration_source_refusal(),
        out.packet_source_refusal(),
        out.receiver_epoch(),
        out.forward_new_extradata(),
        segments,
    )
}

#[test]
fn strict_packets_keep_every_poll_and_fact_across_work_plans() {
    // Recorded on `main` at `0f154393` from the strict compact path, per plan.
    const DIGESTS: [u64; 4] = [
        13_338_352_700_024_966_442,
        8_938_238_107_557_535_556,
        928_101_546_738_566_266,
        6_227_540_478_076_583_636,
    ];
    let packets: [(Codec, &[u8]); 8] = [
        (Codec::Avc, &[0, 0, 0, 3, 0x65, 0xb8, 128]),
        (Codec::Avc, &[0, 0, 0, 1, 0x41]),
        (Codec::Avc, &[0, 0]),
        (Codec::Avc, &[0, 0, 0, 2, 0x41]),
        (Codec::Avc, &[0, 0, 0, 1, 0x81]),
        (Codec::Hevc, &[0, 0, 0, 3, 38, 1, 128]),
        (Codec::Hevc, &[0, 0, 0, 0]),
        (Codec::Hevc, &[0, 0, 0, 1, 38]),
    ];
    let plans: [&[u64]; 4] = [&[0, 1], &[1], &[7], &[4096]];
    for (plan, expected) in plans.into_iter().zip(DIGESTS) {
        let mut log = String::new();
        for (codec, raw) in packets {
            let pool = Budget::new(16_384).unwrap();
            let baseline = pool.used();
            let mut session = match codec {
                Codec::Avc => avc_session(&pool),
                Codec::Hevc => hevc_session(&pool),
            };
            let cancel = AtomicBool::new(false);
            let mut job = session.packet(input(hidden(&pool, raw)), None).unwrap();
            let (polls, ready) = drive(plan, |w, out| job.step_into(w, &cancel, out));
            drop(job);
            drop(session);
            log += &format!("{polls:?} {:?}\n", ready.as_ref().map(facts));
            drop(ready);
            assert_eq!(pool.used(), baseline);
        }
        let digest = log.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
        });
        assert_eq!(digest, expected, "plan {plan:?}\n{log}");
    }
}
