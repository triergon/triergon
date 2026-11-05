use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, SourceId, SourceLocation, WorkBudget};
use triergon_framing::{
    BuildStep, Codec, NalPacket, NalPacketConfiguration, NalPacketFields, NalPacketLimits,
    NalPacketOrigin, NalPacketOutput, NalPacketSession,
};

fn fields() -> NalPacketFields {
    NalPacketFields {
        pts: -1234,
        dts: i64::MIN,
        duration: 87,
        position: -1,
        flags: 0xa5,
        stream_index: 7,
    }
}

fn limits(maximum_output_bytes: usize) -> NalPacketLimits {
    NalPacketLimits {
        maximum_packet_bytes: 4096,
        maximum_output_bytes,
        maximum_nals: 64,
        maximum_segments: 128,
    }
}

fn input(bytes: Bytes, extra: Option<Bytes>) -> NalPacket {
    NalPacket {
        bytes,
        fields: fields(),
        location: SourceLocation {
            source: SourceId(42),
            offset: 0x1_0000_0020,
        },
        generation: 3,
        new_extradata: extra,
    }
}

fn run(pool: &Budget, codec: Codec, root: &Bytes, length: usize, quantum: u64) -> NalPacketOutput {
    let raw = root.slice(0..0).unwrap();
    let mut session = NalPacketSession::new(
        pool,
        NalPacketConfiguration::Passthrough { codec, raw },
        limits(4096),
    )
    .unwrap();
    let extra = (codec == Codec::Hevc).then(|| root.slice(70..73).unwrap());
    let packet = input(root.slice(32..32 + length).unwrap(), extra);
    let mut job = session.packet_observed(packet, None).unwrap();
    let cancel = AtomicBool::new(false);
    let output = loop {
        match job.step(&mut WorkBudget::new(quantum), &cancel).unwrap() {
            BuildStep::Ready(output) => break output,
            BuildStep::Yield => (),
            BuildStep::Complete => panic!("passthrough packet disappeared"),
        }
    };
    drop(job);
    drop(session);
    output
}

#[test]
fn passthrough_outputs_keep_the_original_storage_after_the_producer_closes() {
    for codec in [Codec::Avc, Codec::Hevc] {
        for quantum in [1, 4096] {
            for length in [0, 7] {
                let pool = Budget::new(16 * 1024).unwrap();
                let baseline = pool.used();
                let root = Bytes::copy_from_slice(&pool, &[0xa5; 512]).unwrap();
                let charge = root.retained_storage_bytes();
                let output = run(&pool, codec, &root, length, quantum);
                assert!(
                    output.bytes().shares_storage_with(&root),
                    "unchanged passthrough bytes acquired a separate payload owner"
                );
                assert!(output.source().bytes.shares_storage_with(&root));
                assert_eq!(output.bytes().retained_storage_bytes(), charge);
                assert_eq!(output.source().fields, fields());
                assert_eq!(output.source().generation, 3);
                assert_eq!(output.source().location.offset, 0x1_0000_0020);
                assert_eq!(output.source().location.source, SourceId(42));
                assert_eq!(output.segments().len(), 1);
                let segment = &output.segments()[0];
                assert!(segment.source_bytes().shares_storage_with(&root));
                assert_eq!(segment.prefix_bytes, 0);
                assert_eq!(segment.output_offset, 0);
                assert!(matches!(
                    &segment.origin,
                    NalPacketOrigin::Packet { source_range } if *source_range == (0..length)
                ));
                if codec == Codec::Hevc {
                    assert!(output.forward_new_extradata());
                    assert!(
                        output
                            .source()
                            .new_extradata
                            .as_ref()
                            .unwrap()
                            .shares_storage_with(&root)
                    );
                }
                drop(root);
                assert_eq!(output.bytes().as_ref(), vec![0xa5; length]);
                assert_eq!(output.source().bytes.as_ref(), output.bytes().as_ref());
                assert!(pool.used() >= baseline + charge);
                drop(output);
                assert_eq!(pool.used(), baseline);
            }
        }
    }
}

#[test]
fn passthrough_aliasing_still_obeys_work_cancellation_and_logical_output_limits() {
    for codec in [Codec::Avc, Codec::Hevc] {
        let pool = Budget::new(16 * 1024).unwrap();
        let baseline = pool.used();
        let root = Bytes::copy_from_slice(&pool, &[0x5a; 512]).unwrap();
        let raw = root.slice(0..0).unwrap();
        let mut session = NalPacketSession::new(
            &pool,
            NalPacketConfiguration::Passthrough { codec, raw },
            limits(7),
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let mut job = session
            .packet_observed(input(root.slice(32..40).unwrap(), None), None)
            .unwrap();
        let before = pool.used();
        assert!(matches!(
            job.step(&mut WorkBudget::new(0), &cancel),
            Ok(BuildStep::Yield)
        ));
        assert_eq!(pool.used(), before);
        cancel.store(true, Ordering::Relaxed);
        assert!(matches!(
            job.step(&mut WorkBudget::new(1), &cancel),
            Err(Error::Cancelled)
        ));
        cancel.store(false, Ordering::Relaxed);
        assert!(matches!(
            job.step(&mut WorkBudget::new(4096), &cancel),
            Err(Error::Cancelled)
        ));
        drop(job);
        for quantum in [1, 4096] {
            let mut job = session
                .packet_observed(input(root.slice(32..40).unwrap(), None), None)
                .unwrap();
            loop {
                match job.step(&mut WorkBudget::new(quantum), &cancel) {
                    Ok(BuildStep::Yield) => (),
                    Err(Error::Limit {
                        requested: 8,
                        maximum: 7,
                    }) => break,
                    _ => panic!("passthrough bypassed its logical output limit"),
                }
            }
        }
        drop(session);
        drop(root);
        assert_eq!(pool.used(), baseline);
    }
}
