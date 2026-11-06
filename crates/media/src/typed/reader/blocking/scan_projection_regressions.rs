//! Neutral public traces.
include!("scan_projection_helpers.rs");
use triergon_core::{Bytes, InputWindow};

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
fn scan_projection_group_fallback_keeps_typed_facts_and_owner_lifetimes() {
    let raw = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/scan_projection_video_group_v3.mkv"
    ));
    for policy in policy_cases() {
        for chunk in [1, 7, 8192] {
            let pool = Budget::new(4 * 1024 * 1024).unwrap();
            let base = pool.used();
            let cancel = Arc::new(AtomicBool::new(false));
            let (input, log, _) = source(raw.to_vec(), chunk, &pool, &cancel);
            let mut reader = BlockingReader::open_seekable_in_with_payload_ownership(
                input,
                Limits::default(),
                &pool,
                policy,
            )
            .unwrap();
            let mut held = Vec::new();
            while let Some(packet) = reader.next_typed_packet(&cancel).unwrap() {
                held.push(packet);
            }
            assert!(held.iter().any(|p| p.group.is_some()));
            assert_eq!(
                held.iter().map(|p| p.packet.pts_ns).collect::<Vec<_>>(),
                [Some(0), Some(40_000_000), Some(80_000_000)]
            );
            assert_eq!(
                held.iter().map(|p| p.packet.keyframe).collect::<Vec<_>>(),
                [true, false, true]
            );
            assert_eq!(
                held.iter()
                    .map(|p| p.packet.duration_ns)
                    .collect::<Vec<_>>(),
                [Some(40_000_000); 3]
            );
            assert_eq!(
                held.iter()
                    .map(|p| p.packet.data.as_ref().to_vec())
                    .collect::<Vec<_>>(),
                [vec![b'a'; 32], vec![b'b'; 32], vec![b'c'; 32]]
            );
            let target = 40_000_000;
            let expected = held
                .iter()
                .filter(|p| {
                    p.packet.track == 0
                        && p.packet.keyframe
                        && p.packet.pts_ns.is_some_and(|at| at <= target)
                })
                .filter_map(|p| p.packet.pts_ns)
                .max();
            let result = reader.seek_for_playback(0, target, &cancel);
            match (expected, &result) {
                (Some(at), Ok(receipt)) => assert_eq!(receipt.actual_ns, at),
                (None, Err(Error::Unsupported("no previous random-access block"))) => (),
                (_, other) => panic!("typed scan fallback changed: {other:?}"),
            }
            let observed = format!(
                "{result:?}:{:?}:{:?}:{}",
                held.iter().map(facts).collect::<Vec<_>>(),
                held.iter()
                    .map(|p| format!("{:?}", p.group))
                    .collect::<Vec<_>>(),
                pool.used()
            );
            let calls = log.lock().unwrap().clone();
            drop(reader);
            assert!(pool.used() > base);
            assert!(held.iter().all(|p| !p.packet.data.is_empty()));
            drop(held);
            assert_eq!(pool.used(), base);
            println!("SCAN_PROJECTION_GROUP {policy:?}/{chunk} {observed} {calls:?}");
        }
    }
}

#[test]
fn scan_projection_late_transport_cancel_and_reentrant_reset_keep_priority() {
    for policy in policy_cases() {
        for mode in [1, 2, 3] {
            let pool = Budget::new(512 * 1024).unwrap();
            let base = pool.used();
            let cancel = Arc::new(AtomicBool::new(false));
            let (input, log, flag) = source(fixture(), 7, &pool, &cancel);
            let mut reader = BlockingReader::open_seekable_in_with_payload_ownership(
                input,
                Limits::default(),
                &pool,
                policy,
            )
            .unwrap();
            let held = reader.next_typed_packet(&cancel).unwrap().unwrap();
            flag.store(mode, Ordering::Relaxed);
            let error = reader
                .seek_for_playback(0, 40_000_000, &cancel)
                .unwrap_err();
            match mode {
                1 => assert!(matches!(error, Error::Cancelled)),
                _ => assert!(matches!(error, Error::Io(_))),
            }
            let initial = format!("{error:?}:{}", pool.used());
            cancel.store(false, Ordering::Relaxed);
            let receipt = reader.seek_for_playback(0, 0, &cancel).unwrap();
            assert_eq!(receipt.actual_ns, 0);
            let packet = reader.next_typed_packet(&cancel).unwrap().unwrap();
            assert_eq!(facts(&packet), facts(&held));
            let calls = log.lock().unwrap().clone();
            drop(reader);
            assert_eq!(held.packet.data.as_ref(), &[b'a'; 32]);
            drop(packet);
            drop(held);
            assert_eq!(pool.used(), base);
            println!("SCAN_PROJECTION_FAILURE {policy:?}/{mode} {initial} {receipt:?} {calls:?}");
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
