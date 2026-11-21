//! Seek windows and scan projection that need the adapter's private state:
//! pressure retries, the epoch certificate and the public work trace.
use crate::reader::blocking::*;
use crate::seek_window_kit::{facts, fixture, policy_cases, source};
use std::sync::Arc;
use triergon_core::{Bytes, InputWindow};

#[test]
fn seek_window_pressure_retry_and_truncation_keep_source_and_owner_facts() {
    for policy in policy_cases() {
        let pool = Budget::new(512 * 1024).unwrap();
        let base = pool.used();
        let cancel = Arc::new(AtomicBool::new(false));
        let (input, log, _) = source(fixture(), 7, &pool, &cancel);
        let mut reader = BlockingReader::open_seekable_in_with_payload_ownership(
            input,
            Limits::default(),
            &pool,
            policy,
        )
        .unwrap();
        let held = reader.next_typed_packet(&cancel).unwrap().unwrap();
        let pressure = pool.reserve(pool.available()).unwrap();
        let attempt = reader.seek_for_playback(0, 40_000_000, &cancel);
        assert!(attempt.is_ok() || matches!(&attempt, Err(Error::Backpressure { .. })));
        assert!(!reader.reader.failed);
        let observed = format!("{attempt:?}:used={}", pool.used());
        drop(pressure);
        let receipt = reader.seek_for_playback(0, 40_000_000, &cancel).unwrap();
        assert_eq!(receipt.actual_ns, 40_000_000);
        let mut packets = Vec::new();
        while let Some(packet) = reader.next_typed_packet(&cancel).unwrap() {
            packets.push(facts(&packet));
        }
        assert_eq!(packets.len(), 2);
        let calls = log.lock().unwrap().clone();
        drop(reader);
        assert_eq!(held.packet.data.as_ref(), &[b'a'; 32]);
        drop(held);
        assert_eq!(pool.used(), base);
        println!("SEEK_WINDOW_CONTROL pressure-{policy:?} {calls:?} {observed} {packets:?}");
        let pool = Budget::new(512 * 1024).unwrap();
        let base = pool.used();
        let mut raw = fixture();
        raw.pop();
        let cancel = Arc::new(AtomicBool::new(false));
        let (input, log, _) = source(raw, 1, &pool, &cancel);
        let mut reader = BlockingReader::open_seekable_in_with_payload_ownership(
            input,
            Limits::default(),
            &pool,
            policy,
        )
        .unwrap();
        let mut packets = Vec::new();
        let error = loop {
            match reader.next_typed_packet(&cancel) {
                Ok(Some(packet)) => packets.push(facts(&packet)),
                Ok(None) => panic!("truncated source became clean EOF"),
                Err(error) => break error,
            }
        };
        assert!(matches!(error, Error::Truncated));
        let observed = format!("{error:?}");
        let calls = log.lock().unwrap().clone();
        drop(reader);
        assert_eq!(pool.used(), base);
        println!("SEEK_WINDOW_CONTROL truncated-{policy:?} {calls:?} {observed} {packets:?}");
    }
}

#[test]
fn seek_window_epoch_certificate_keeps_public_hint_and_stops_before_replay() {
    let raw = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/seek/cues-end.mkv"
    ));
    let pool = Budget::new(512 * 1024).unwrap();
    let base = pool.used();
    let bytes = Bytes::copy_from_slice(&pool, raw).unwrap();
    let mut reader = Reader::new(
        &pool,
        SourceId(2497),
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        Limits::default(),
        8192,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut sought = false;
    let mut checked = false;
    for _ in 0..20_000 {
        if !sought && reader.is_ready() {
            reader.seek_for_playback(0, 7_500_000_000).unwrap();
            sought = true;
        }
        match reader
            .step_parse(&mut WorkBudget::new(8192), &cancel)
            .unwrap()
        {
            ParseStep::NeedInput(request) => {
                if sought
                    && matches!(reader.seek_stage, SeekStage::Epoch(_))
                    && request.offset == reader.first_cluster.as_ref().unwrap().offset()
                    && request.maximum == 1
                {
                    let plan = reader
                        .blocking_seek_header_refill(&request)
                        .unwrap()
                        .expect("new seek engine must offer a real bounded input response");
                    let stop = reader.last_location.as_ref().unwrap().cluster_offset();
                    assert!(stop > request.offset);
                    assert!(plan.expanded().maximum > request.maximum);
                    assert!(plan.expanded().maximum <= 4096);
                    assert!(plan.expanded().offset + plan.expanded().maximum as u64 <= stop);
                    let ParseStep::NeedInput(public) =
                        reader.canonical_input_hint(request.clone()).unwrap()
                    else {
                        panic!("canonical request disappeared")
                    };
                    assert_eq!(public.source, request.source);
                    assert_eq!(public.token, request.token);
                    assert_eq!(public.offset, request.offset);
                    assert_eq!(public.maximum, 1);
                    assert!(reader.ordinary_header_refill().unwrap().is_none());
                    println!(
                        "SEEK_WINDOW_BOUND exact={request:?} expanded={:?} stop={stop}",
                        plan.expanded()
                    );
                    checked = true;
                    break;
                }
                let at = request.offset as usize;
                if at == raw.len() {
                    reader.finish(request.source, request.token).unwrap();
                } else {
                    let end = at + request.maximum.min(raw.len() - at);
                    reader
                        .provide(
                            &InputWindow::new(
                                request.source,
                                request.token,
                                request.offset,
                                bytes.slice(at..end).unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                }
            }
            ParseStep::Finished => break,
            _ => (),
        }
    }
    assert!(
        checked,
        "authored operation must reach real Epoch range header"
    );
    drop((reader, bytes));
    assert_eq!(pool.used(), base);
}

#[test]
fn scan_projection_public_work_trace_and_held_alias_survive_seek() {
    for chunk in [1, 7, 8192] {
        for quantum in [1, 7, 8192] {
            let raw = fixture();
            let pool = Budget::new(512 * 1024).unwrap();
            let base = pool.used();
            let mut reader = Reader::new(
                &pool,
                SourceId(9517),
                SourceCapabilities {
                    length: Some(raw.len() as u64),
                    random_access: true,
                },
                Limits::default(),
                8192,
            )
            .unwrap();
            let cancel = AtomicBool::new(false);
            let mut sought = false;
            let mut receipt = false;
            let mut held = Vec::new();
            let mut tape = Vec::new();
            for turn in 0..50_000 {
                let allowance = if turn % 3 == 0 { 0 } else { quantum };
                let mut work = WorkBudget::new(allowance);
                let before = reader.envelope.position();
                let step = reader.step(&mut work, &cancel).unwrap();
                assert!(work.spent() <= allowance);
                let item = match step {
                    Step::NeedInput(request) => {
                        let core = reader.request();
                        assert_eq!(request.source, core.source);
                        assert_eq!(request.token, core.token);
                        assert_eq!(request.offset, core.offset);
                        let at = core.offset as usize;
                        if at == raw.len() {
                            reader.finish(core.source, core.token).unwrap();
                        } else {
                            let end = raw.len().min(at + chunk.min(core.maximum));
                            let bytes = Bytes::copy_from_slice(&pool, &raw[at..end]).unwrap();
                            reader
                                .provide(
                                    &InputWindow::new(core.source, core.token, core.offset, bytes)
                                        .unwrap(),
                                )
                                .unwrap();
                        }
                        format!("N:{request:?}")
                    }
                    Step::Ready(Event::Packet(packet)) => {
                        if !sought {
                            assert_eq!(packet.packet.pts_ns, Some(0));
                        }
                        let result = format!(
                            "P:{:?}:{}:{:?}",
                            facts(&packet),
                            packet.configuration.revision(),
                            packet.group
                        );
                        held.push(packet);
                        if !sought {
                            reader.seek_for_playback(0, 40_000_000).unwrap();
                            sought = true;
                        }
                        result
                    }
                    Step::Ready(Event::Seek(value)) => {
                        assert!(sought && !receipt);
                        assert_eq!(value.requested_ns, 40_000_000);
                        assert_eq!(value.actual_ns, 40_000_000);
                        receipt = true;
                        format!("R:{value:?}")
                    }
                    Step::Ready(event) => format!("E:{event:?}"),
                    Step::Yield => "Y".to_owned(),
                    Step::Backpressure(_) => panic!("ample authored pool"),
                    Step::End(_) => {
                        tape.push(format!(
                            "F:{before}:{}:{}:{}",
                            reader.envelope.position(),
                            work.spent(),
                            pool.used()
                        ));
                        break;
                    }
                };
                tape.push(format!(
                    "{item}:{before}:{}:{}:{}",
                    reader.envelope.position(),
                    work.spent(),
                    pool.used()
                ));
                assert!(turn < 49_999, "bounded source must finish");
            }
            assert!(receipt);
            assert_eq!(
                held.iter().map(|p| p.packet.pts_ns).collect::<Vec<_>>(),
                [Some(0), Some(40_000_000), Some(80_000_000)]
            );
            drop(reader);
            assert!(pool.used() > base);
            assert_eq!(held[0].packet.data.as_ref(), &[b'a'; 32]);
            drop(held);
            assert_eq!(pool.used(), base);
            println!("SCAN_PROJECTION_PUBLIC {chunk}/{quantum} {tape:?}");
        }
    }
}

#[test]
fn scan_projection_pressure_retry_keeps_both_caller_aliases_and_configuration() {
    for policy in policy_cases() {
        let pool = Budget::new(512 * 1024).unwrap();
        let base = pool.used();
        let cancel = Arc::new(AtomicBool::new(false));
        let (input, log, _) = source(fixture(), 7, &pool, &cancel);
        let mut reader = BlockingReader::open_seekable_in_with_payload_ownership(
            input,
            Limits::default(),
            &pool,
            policy,
        )
        .unwrap();
        let held = reader.next_typed_packet(&cancel).unwrap().unwrap();
        let alias = held.clone();
        let pressure = pool.reserve(pool.available()).unwrap();
        let attempt = reader.seek_for_playback(0, 40_000_000, &cancel);
        assert!(attempt.is_ok() || matches!(&attempt, Err(Error::Backpressure { .. })));
        assert!(!reader.reader.failed);
        let observed = format!("{attempt:?}:{}", pool.used());
        drop(pressure);
        let receipt = reader.seek_for_playback(0, 40_000_000, &cancel).unwrap();
        assert_eq!(receipt.actual_ns, 40_000_000);
        let next = reader.next_typed_packet(&cancel).unwrap().unwrap();
        assert_eq!(next.packet.pts_ns, Some(40_000_000));
        let calls = log.lock().unwrap().clone();
        drop(next);
        drop(reader);
        drop(held);
        assert!(pool.used() > base);
        assert_eq!(
            facts(&alias),
            (Some(0), vec![b'a'; 32], b"exact-private".to_vec())
        );
        drop(alias);
        assert_eq!(pool.used(), base);
        println!("SCAN_PROJECTION_PRESSURE {policy:?} {observed} {receipt:?} {calls:?}");
    }
}
