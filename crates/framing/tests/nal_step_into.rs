#![forbid(unsafe_code)]
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, SourceId, SourceLocation, WorkBudget};
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

fn configuration(pool: &Budget, framed: bool) -> NalPacketConfiguration {
    if !framed {
        return NalPacketConfiguration::Passthrough {
            codec: Codec::Avc,
            raw: Bytes::copy_from_slice(pool, &[]).unwrap(),
        };
    }
    let mut parser = NalObservedAvcConfigParser::new(
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
            BuildStep::Ready(value) => return NalPacketConfiguration::ObservedAvc(value),
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
    let mut backing = vec![0xa5; 512];
    backing[32..32 + raw.len()].copy_from_slice(raw);
    let owner = Bytes::try_from_vec(&pool, backing)
        .map_err(|e| e.error())
        .unwrap();
    let mut job = session
        .packet_observed(input(owner.slice(32..32 + raw.len()).unwrap()), None)
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
        for segment in output.segments() {
            events.push(format!(
                "{:?}:{:?}:{}:{}",
                segment.source_bytes().as_ref(),
                segment.origin,
                segment.prefix_bytes,
                segment.output_offset
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
        drop(owner);
        assert_eq!(output.source().fields, fields());
        assert_eq!(output.source().generation, 31);
        assert!(pool.used() > baseline);
        drop(output);
    } else {
        drop(job);
        drop(session);
        drop(owner);
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

fn passthrough_output(pool: &Budget) -> NalPacketOutput {
    let mut session = NalPacketSession::new(pool, configuration(pool, false), limits()).unwrap();
    let mut job = session
        .packet_observed(input(Bytes::copy_from_slice(pool, &[0x65]).unwrap()), None)
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
    let mut ready = Some(passthrough_output(&pool));
    let owner = ready.as_ref().unwrap().bytes().clone();
    let mut session = NalPacketSession::new(&pool, configuration(&pool, false), limits()).unwrap();
    let mut job = session
        .packet_observed(
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
        .packet_observed(
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
        .packet_observed(
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
    let mut job = session.packet_observed(packet, None).unwrap();
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
