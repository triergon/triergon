use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, SourceId, SourceLocation, WorkBudget};
use triergon_framing::{
    BuildStep, Codec, NalPacket, NalPacketConfiguration, NalPacketLimits, NalPacketSegment,
    NalPacketSession,
};

#[test]
fn unchanged_packet_fits_without_a_second_payload_owner() {
    for codec in [Codec::Avc, Codec::Hevc] {
        for quantum in [1, 4096] {
            let length = 1024;
            let baseline = Budget::control_storage_bytes();
            let pool = Budget::new(
                baseline + Bytes::owner_overhead_bytes() + length + size_of::<NalPacketSegment>(),
            )
            .unwrap();
            let bytes = Bytes::copy_from_slice(&pool, &[0xa5; 1024]).unwrap();
            let mut session = NalPacketSession::new(
                &pool,
                NalPacketConfiguration::Passthrough {
                    codec,
                    raw: bytes.slice(0..0).unwrap(),
                },
                NalPacketLimits {
                    maximum_packet_bytes: length,
                    maximum_output_bytes: length,
                    maximum_nals: 1,
                    maximum_segments: 1,
                },
            )
            .unwrap();
            let mut job = session
                .packet(
                    NalPacket {
                        bytes: bytes.clone(),
                        location: SourceLocation {
                            source: SourceId(7),
                            offset: 0,
                        },
                        generation: 0,
                        fields: (),
                        new_extradata: None,
                    },
                    None,
                )
                .unwrap();
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
            assert!(output.bytes().shares_storage_with(&bytes));
            assert_eq!(pool.used(), pool.limit());
            drop(bytes);
            assert_eq!(output.bytes().as_ref(), &[0xa5; 1024]);
            drop(output);
            assert_eq!(pool.used(), baseline);
        }
    }
}
