#![forbid(unsafe_code)]
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, SourceId, SourceLocation, WorkBudget};
use triergon_framing::*;

fn configuration(pool: &Budget, codec: Codec, width: u8) -> NalPacketConfiguration {
    let cancel = AtomicBool::new(false);
    match codec {
        Codec::Avc => {
            let raw = [1, 66, 0, 10, 252 | (width - 1), 224, 0];
            let mut parser = NalObservedAvcConfigParser::new(
                pool,
                AvcConfigSource {
                    source: SourceId(11),
                    generation: 29,
                    offset: 17,
                },
                Bytes::copy_from_slice(pool, &raw).unwrap(),
                AvcConfigLimits {
                    maximum_record_bytes: 4096,
                    maximum_nals: 64,
                    maximum_nal_bytes: 4096,
                },
            )
            .unwrap();
            loop {
                if let BuildStep::Ready(value) =
                    parser.step(&mut WorkBudget::new(4096), &cancel).unwrap()
                {
                    return NalPacketConfiguration::ObservedAvc(value);
                }
            }
        }
        Codec::Hevc => {
            let raw = [
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
                12 | (width - 1),
                0,
            ];
            let mut parser = NalObservedHevcConfigParser::new(
                pool,
                HvcConfigSource {
                    source: SourceId(11),
                    generation: 29,
                    offset: 17,
                },
                Bytes::copy_from_slice(pool, &raw).unwrap(),
                HvcConfigLimits {
                    maximum_record_bytes: 4096,
                    maximum_arrays: 255,
                    maximum_nals: 64,
                    maximum_nal_bytes: 4096,
                },
            )
            .unwrap();
            loop {
                if let BuildStep::Ready(value) =
                    parser.step(&mut WorkBudget::new(4096), &cancel).unwrap()
                {
                    return NalPacketConfiguration::ObservedHevc(value);
                }
            }
        }
    }
}

fn limits() -> NalPacketLimits {
    NalPacketLimits {
        maximum_packet_bytes: 4096,
        maximum_output_bytes: 8192,
        maximum_nals: 64,
        maximum_segments: 128,
    }
}

fn source(pool: &Budget, raw: &[u8]) -> NalPacket {
    let mut backing = vec![0x7f; 1024];
    backing[100..100 + raw.len()].copy_from_slice(raw);
    let owner = Bytes::try_from_vec(pool, backing)
        .map_err(|e| e.error())
        .unwrap();
    NalPacket {
        bytes: owner.slice(100..100 + raw.len()).unwrap(),
        location: SourceLocation {
            source: SourceId(77),
            offset: 900,
        },
        generation: 31,
        fields: NalPacketFields {
            pts: i64::MIN,
            dts: i64::MAX,
            duration: -7,
            position: 0x1_0000_0001,
            flags: u32::MAX,
            stream_index: i32::MIN,
        },
        new_extradata: Some(Bytes::copy_from_slice(pool, &[]).unwrap()),
    }
}

fn encoded(width: u8, nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes()[4 - usize::from(width)..]);
        out.extend_from_slice(nal);
    }
    out
}

// These complete public traces are captured on the original and changed
// library, then compared literally, excluding only test-runner timing text.
fn trace(codec: Codec, width: u8, raw: &[u8], plan: &[u64]) {
    let pool = Budget::new(2_000_000).unwrap();
    let baseline = pool.used();
    let mut session =
        NalPacketSession::new(&pool, configuration(&pool, codec, width), limits()).unwrap();
    let input = source(&pool, raw);
    let fields = input.fields;
    let mut job = session.packet_observed(input, None).unwrap();
    let mut events = Vec::new();
    let cancel = AtomicBool::new(false);
    let mut done = false;
    for poll in 0..20000 {
        let mut work = WorkBudget::new(plan[poll % plan.len()]);
        let status = match job.step(&mut work, &cancel) {
            Ok(BuildStep::Yield) => "yield".to_owned(),
            Ok(BuildStep::Ready(output)) => {
                assert_eq!(output.source().fields, fields);
                assert_eq!(output.source().bytes.as_ref(), raw);
                assert!(output.source().bytes.retained_capacity() >= 1024);
                done = true;
                format!(
                    "ready:{:?}:{:?}:{:?}:{:?}:{}:{}",
                    output.bytes().as_ref(),
                    output.configuration_bytes().as_ref(),
                    output.configuration_source_refusal(),
                    output.packet_source_refusal(),
                    output.receiver_epoch(),
                    output.forward_new_extradata()
                )
            }
            Ok(BuildStep::Complete) => panic!("missing output"),
            Err(error) => {
                let rejection = job.take_receiver_rejection().map(|r| {
                    assert_eq!(r.source().fields, fields);
                    assert_eq!(r.source().bytes.as_ref(), raw);
                    assert!(r.source().bytes.retained_capacity() >= 1024);
                    format!(
                        "{:?}:{:?}:{}:{}:{:?}",
                        r.kind(),
                        r.source_error(),
                        r.native_receive_result(),
                        r.receiver_epoch(),
                        r.packet_source_refusal()
                    )
                });
                done = true;
                format!("error:{error:?}:{rejection:?}")
            }
        };
        events.push((
            status,
            work.spent(),
            work.remaining(),
            pool.used(),
            pool.peak(),
        ));
        if done {
            break;
        }
    }
    assert!(done, "authored bounded packet stalled");
    drop(job);
    drop(session);
    assert_eq!(pool.used(), baseline);
    let name = if codec == Codec::Avc { "avc" } else { "hevc" };
    println!("PREFIX_TRACE {name}:{width}:{raw:?}:{plan:?}:{events:?}");
}

fn plans() -> [&'static [u64]; 6] {
    [&[1], &[2], &[3], &[4], &[0, 1, 0, 2, 4], &[4096]]
}

#[test]
fn avc_all_widths_partial_prefix_and_output_traces() {
    for width in 1..=4 {
        for plan in plans() {
            trace(
                Codec::Avc,
                width,
                &encoded(width, &[&[0x65, 0xb8, 0x80], &[0x41, 0xe0]]),
                plan,
            );
        }
    }
}

#[test]
fn hevc_all_widths_both_lookahead_and_scan_traces() {
    for width in 1..=4 {
        for plan in plans() {
            trace(
                Codec::Hevc,
                width,
                &encoded(width, &[&[2, 1, 0x80], &[38, 1, 0x80]]),
                plan,
            );
        }
    }
}

#[test]
fn truncated_prefix_and_payload_keep_source_receiver_and_work_traces() {
    for codec in [Codec::Avc, Codec::Hevc] {
        for width in 1..=4 {
            for plan in plans() {
                for available in 0..usize::from(width) {
                    trace(codec, width, &vec![0; available], plan);
                }
                let mut raw = vec![0; usize::from(width)];
                raw[usize::from(width) - 1] = 8;
                raw.extend_from_slice(&[0x41]);
                trace(codec, width, &raw, plan);
            }
        }
    }
}

#[test]
fn missing_prefix_byte_spends_one_failed_read_token_after_zero_work_yield() {
    for codec in [Codec::Avc, Codec::Hevc] {
        let pool = Budget::new(100000).unwrap();
        let mut session =
            NalPacketSession::new(&pool, configuration(&pool, codec, 4), limits()).unwrap();
        let mut job = session
            .packet_observed(source(&pool, &[0, 0]), None)
            .unwrap();
        for units in [2, 0] {
            let mut work = WorkBudget::new(units);
            assert!(matches!(
                job.step(&mut work, &AtomicBool::new(false)),
                Ok(BuildStep::Yield)
            ));
            assert_eq!(work.spent(), units);
        }
        let mut work = WorkBudget::new(1);
        assert!(matches!(
            job.step(&mut work, &AtomicBool::new(false)),
            Err(Error::Truncated)
        ));
        assert_eq!(work.spent(), 1);
        let rejection = job.take_receiver_rejection().unwrap();
        assert_eq!(
            rejection.kind(),
            NalPacketReceiverErrorKind::IncompleteLengthPrefix
        );
        assert_eq!(rejection.source_error(), Error::Truncated);
        assert_eq!(rejection.source().bytes.as_ref(), &[0, 0]);
    }
}

#[test]
fn cancellation_between_partial_prefix_polls_precedes_more_input_or_work() {
    for codec in [Codec::Avc, Codec::Hevc] {
        let pool = Budget::new(100000).unwrap();
        let mut session =
            NalPacketSession::new(&pool, configuration(&pool, codec, 4), limits()).unwrap();
        let mut job = session
            .packet_observed(source(&pool, &[0, 0, 0, 3, 2, 1, 128]), None)
            .unwrap();
        let cancel = AtomicBool::new(false);
        let mut work = WorkBudget::new(2);
        assert!(matches!(job.step(&mut work, &cancel), Ok(BuildStep::Yield)));
        assert_eq!(work.spent(), 2);
        cancel.store(true, Ordering::Relaxed);
        let mut work = WorkBudget::new(4);
        assert!(matches!(
            job.step(&mut work, &cancel),
            Err(Error::Cancelled)
        ));
        assert_eq!(work.spent(), 0);
        assert!(job.take_receiver_rejection().is_none());
        cancel.store(false, Ordering::Relaxed);
        assert!(matches!(
            job.step(&mut work, &cancel),
            Err(Error::Cancelled)
        ));
        assert_eq!(work.spent(), 0);
    }
}

#[test]
fn maximum_u32_prefix_preserves_end_overflow_and_failed_payload_priority() {
    for codec in [Codec::Avc, Codec::Hevc] {
        let pool = Budget::new(100000).unwrap();
        let mut session =
            NalPacketSession::new(&pool, configuration(&pool, codec, 4), limits()).unwrap();
        let mut job = session
            .packet_observed(source(&pool, &[255; 4]), None)
            .unwrap();
        let mut prefix = WorkBudget::new(4);
        assert!(matches!(
            job.step(&mut prefix, &AtomicBool::new(false)),
            Ok(BuildStep::Yield)
        ));
        assert_eq!(prefix.spent(), 4);
        let mut end = WorkBudget::new(1);
        let expected = if usize::BITS == 32 {
            Error::Overflow
        } else {
            Error::Truncated
        };
        assert!(matches!(job.step(&mut end, &AtomicBool::new(false)), Err(e) if e == expected));
        assert_eq!(end.spent(), 1);
    }
}

#[test]
fn zero_lengths_and_header_refusals_keep_literal_phase_order() {
    for codec in [Codec::Avc, Codec::Hevc] {
        for plan in plans() {
            trace(codec, 4, &[0; 4], plan);
            trace(codec, 4, &[0, 0, 0, 1, 0x80], plan);
            trace(codec, 4, &[0, 0, 0, 0, 0, 0, 0, 2, 2, 1], plan);
        }
    }
}

fn expected_hevc_refusal(header: &[u8]) -> Option<Error> {
    if header.len() < 2 {
        Some(Error::Truncated)
    } else if header[0] & 128 != 0 {
        Some(Error::Invalid("NAL forbidden_zero_bit"))
    } else if header[1] & 7 == 0 {
        Some(Error::Invalid("HEVC temporal_id_plus1 is zero"))
    } else {
        None
    }
}

#[test]
fn every_one_and_two_byte_hevc_header_keeps_strict_and_observed_facts() {
    for observed in [false, true] {
        let pool = Budget::new(2_000_000).unwrap();
        let mut session =
            NalPacketSession::new(&pool, configuration(&pool, Codec::Hevc, 4), limits()).unwrap();
        let baseline = pool.used();
        let mut accepted = 0_usize;
        let mut refused = 0_usize;
        let mut spent = 0_u64;
        let mut digest = 0_u64;
        for length in 1..=2 {
            let count = if length == 1 { 256_u32 } else { 65536_u32 };
            for bits in 0..count {
                let storage = (bits as u16).to_be_bytes();
                let header = &storage[2 - length..];
                let refusal = expected_hevc_refusal(header);
                let bytes = encoded(4, &[header]);
                let input = source(&pool, &bytes);
                let fields = input.fields;
                let mut job = if observed {
                    session.packet_observed(input, None).unwrap()
                } else {
                    session.packet(input, None).unwrap()
                };
                let mut work = WorkBudget::new(4096);
                match job.step(&mut work, &AtomicBool::new(false)) {
                    Ok(BuildStep::Ready(output)) => {
                        assert!(observed || refusal.is_none());
                        assert_eq!(
                            output.packet_source_refusal(),
                            if observed { refusal } else { None }
                        );
                        assert_eq!(output.source().fields, fields);
                        assert_eq!(output.source().bytes.as_ref(), bytes.as_slice());
                        let mut expected = vec![0, 0, 0, 1];
                        expected.extend_from_slice(header);
                        assert_eq!(output.bytes().as_ref(), expected.as_slice());
                        assert_eq!(output.receiver_epoch(), 0);
                        assert!(output.forward_new_extradata());
                        digest = digest
                            .wrapping_mul(131)
                            .wrapping_add(u64::from((header[0] >> 1) & 63));
                        accepted += 1;
                    }
                    Err(error) => {
                        assert!(!observed);
                        assert_eq!(Some(error), refusal);
                        assert!(job.take_receiver_rejection().is_none());
                        let mut empty = WorkBudget::new(0);
                        assert!(
                            matches!(job.step(&mut empty, &AtomicBool::new(false)), Err(e) if e == error)
                        );
                        assert_eq!(empty.spent(), 0);
                        refused += 1;
                    }
                    _ => panic!("bounded authored header did not complete"),
                }
                spent += work.spent();
                digest = digest.wrapping_mul(31).wrapping_add(work.spent());
                drop(job);
                assert_eq!(pool.used(), baseline);
            }
        }
        assert_eq!(accepted, if observed { 65792 } else { 28672 });
        assert_eq!(refused, if observed { 0 } else { 37120 });
        println!(
            "HEVC_HEADER_DOMAIN {observed}:{accepted}:{refused}:{spent}:{digest}:{}",
            pool.peak()
        );
    }
}

#[test]
fn hevc_first_refusal_and_strict_failure_precede_scan_for_both_orders() {
    let forbidden = [128, 1];
    let temporal_zero = [2, 0];
    for headers in [[forbidden, temporal_zero], [temporal_zero, forbidden]] {
        for quantum in [1, 2, 4096] {
            let expected = expected_hevc_refusal(&headers[0]).unwrap();
            let pool = Budget::new(2_000_000).unwrap();
            let bytes = encoded(4, &[&headers[0], &headers[1]]);
            for observed in [false, true] {
                let mut session =
                    NalPacketSession::new(&pool, configuration(&pool, Codec::Hevc, 4), limits())
                        .unwrap();
                let input = source(&pool, &bytes);
                let mut job = if observed {
                    session.packet_observed(input, None).unwrap()
                } else {
                    session.packet(input, None).unwrap()
                };
                let mut spent = 0_u64;
                let mut complete = false;
                for _ in 0..256 {
                    let mut work = WorkBudget::new(quantum);
                    match job.step(&mut work, &AtomicBool::new(false)) {
                        Ok(BuildStep::Yield) => (),
                        Ok(BuildStep::Ready(output)) => {
                            assert!(observed);
                            assert_eq!(output.packet_source_refusal(), Some(expected));
                            assert_eq!(output.source().bytes.as_ref(), bytes.as_slice());
                            complete = true;
                        }
                        Err(error) => {
                            assert!(!observed);
                            assert_eq!(error, expected);
                            assert!(job.take_receiver_rejection().is_none());
                            complete = true;
                        }
                        _ => panic!("missing authored packet result"),
                    }
                    spent += work.spent();
                    if complete {
                        break;
                    }
                }
                assert!(complete);
                if !observed {
                    assert_eq!(spent, 5);
                }
                println!(
                    "HEVC_REFUSAL_ORDER {headers:?}:{quantum}:{observed}:{expected:?}:{spent}"
                );
            }
        }
    }
}
