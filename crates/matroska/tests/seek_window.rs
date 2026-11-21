//! Seek windows and scan projection through the blocking adapter: provider calls,
//! transport failures, cancellation, scan limits and retained owners.
#[path = "support/ebml.rs"]
mod ebml_kit;
#[path = "support/seek_window.rs"]
mod seek_window_kit;
use seek_window_kit::{Call, facts, fixture, policy_cases, source};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::Budget;
use triergon_matroska::{BlockingReader, Error, Limits, PayloadOwnership, TypedPacket};

#[test]
fn seek_window_fragmented_packets_and_retained_parent_lifetimes() {
    for policy in policy_cases() {
        for chunk in [1, 7, 8192] {
            let pool = Budget::new(512 * 1024).unwrap();
            let base = pool.used();
            let cancel = Arc::new(AtomicBool::new(false));
            let (source, log, _) = source(fixture(), chunk, &pool, &cancel);
            let mut reader = BlockingReader::open_seekable_in_with_payload_ownership(
                source,
                Limits::default(),
                &pool,
                policy,
            )
            .unwrap();
            let mut held = Vec::new();
            while let Some(packet) = reader.next_typed_packet(&cancel).unwrap() {
                held.push(packet);
            }
            let actual: Vec<_> = held.iter().map(facts).collect();
            assert_eq!(
                actual,
                [(0, b'a'), (40_000_000, b'b'), (80_000_000, b'c')].map(|(at, byte)| (
                    Some(at),
                    vec![byte; 32],
                    b"exact-private".to_vec()
                ))
            );
            let calls = log.lock().unwrap().clone();
            drop(reader);
            assert!(pool.used() > base);
            assert_eq!(held.iter().map(facts).collect::<Vec<_>>(), actual);
            drop(held);
            assert_eq!(pool.used(), base);
            println!("SEEK_WINDOW_CONTROL plain-{policy:?}-{chunk} {calls:?} {actual:?}");
        }
    }
}

#[test]
fn seek_window_reentrant_seeks_keep_complete_receipts_and_provider_order() {
    for policy in policy_cases() {
        for chunk in [1, 7, 8192] {
            let pool = Budget::new(512 * 1024).unwrap();
            let base = pool.used();
            let cancel = Arc::new(AtomicBool::new(false));
            let (source, log, _) = source(fixture(), chunk, &pool, &cancel);
            let mut reader = BlockingReader::open_seekable_in_with_payload_ownership(
                source,
                Limits::default(),
                &pool,
                policy,
            )
            .unwrap();
            let held = reader.next_typed_packet(&cancel).unwrap().unwrap();
            let mut observations = Vec::new();
            for target in [40_000_000, 0, 80_000_000] {
                let receipt = reader.seek_for_playback(0, target, &cancel).unwrap();
                assert_eq!(receipt.actual_ns, target);
                let mut packets = Vec::new();
                while let Some(packet) = reader.next_typed_packet(&cancel).unwrap() {
                    packets.push(facts(&packet));
                }
                assert_eq!(packets.first().unwrap().0, Some(target));
                assert_eq!(packets.last().unwrap().0, Some(80_000_000));
                observations.push(format!("{receipt:?}:{packets:?}:used={}", pool.used()));
            }
            let calls = log.lock().unwrap().clone();
            drop(reader);
            assert_eq!(held.packet.data.as_ref(), &[b'a'; 32]);
            assert!(pool.used() > base);
            drop(held);
            assert_eq!(pool.used(), base);
            println!("SEEK_WINDOW_CONTROL seek-{policy:?}-{chunk} {calls:?} {observations:?}");
        }
    }
}

#[test]
fn seek_window_late_transport_and_cancellation_priorities() {
    for policy in policy_cases() {
        for kind in [1, 2, 3] {
            let pool = Budget::new(512 * 1024).unwrap();
            let base = pool.used();
            let cancel = Arc::new(AtomicBool::new(false));
            let (source, log, mode) = source(fixture(), 7, &pool, &cancel);
            let mut reader = BlockingReader::open_seekable_in_with_payload_ownership(
                source,
                Limits::default(),
                &pool,
                policy,
            )
            .unwrap();
            mode.store(kind, Ordering::Relaxed);
            let error = reader
                .seek_for_playback(0, 40_000_000, &cancel)
                .unwrap_err();
            if kind == 1 {
                assert!(matches!(error, Error::Cancelled));
            } else {
                assert!(matches!(error, Error::Io(_)));
            }
            let observed = format!("{error:?}");
            let calls = log.lock().unwrap().clone();
            drop(reader);
            assert_eq!(pool.used(), base);
            println!("SEEK_WINDOW_CONTROL failure-{policy:?}-{kind} {calls:?} {observed}");
        }
    }
}

#[test]
fn seek_window_whole_core_response_reduces_physical_callback_bounces() {
    for policy in policy_cases() {
        let pool = Budget::new(512 * 1024).unwrap();
        let base = pool.used();
        let cancel = Arc::new(AtomicBool::new(false));
        let (input, log, _) = source(fixture(), 8192, &pool, &cancel);
        let mut reader = BlockingReader::open_seekable_in_with_payload_ownership(
            input,
            Limits::default(),
            &pool,
            policy,
        )
        .unwrap();
        let initial = log.lock().unwrap().len();
        let receipt = reader.seek_for_playback(0, 40_000_000, &cancel).unwrap();
        assert_eq!(receipt.actual_ns, 40_000_000);
        let after_seek = log.lock().unwrap().len();
        let calls = log.lock().unwrap()[initial..after_seek].to_vec();
        let reads = calls
            .iter()
            .filter(|call| matches!(call, Call::Read { .. }))
            .count();
        let mut packets = Vec::new();
        while let Some(packet) = reader.next_typed_packet(&cancel).unwrap() {
            packets.push(facts(&packet));
        }
        assert_eq!(
            packets,
            [(40_000_000, b'b'), (80_000_000, b'c')].map(|(at, byte)| (
                Some(at),
                vec![byte; 32],
                b"exact-private".to_vec()
            ))
        );
        drop(reader);
        assert_eq!(pool.used(), base);
        println!(
            "SEEK_WINDOW_COUNTER {policy:?} reads={reads} calls={calls:?} {receipt:?} {packets:?}"
        );
        assert!(
            reads <= 12,
            "checked whole input window must remove callback bounces; observed {reads}"
        );
    }
}

#[test]
fn seek_window_truncated_seek_keeps_error_and_old_output_owner() {
    for policy in policy_cases() {
        for chunk in [1, 7, 8192] {
            let mut raw = fixture();
            assert!(raw.pop().is_some()); // Only the final last-Cluster byte.
            let pool = Budget::new(512 * 1024).unwrap();
            let base = pool.used();
            let cancel = Arc::new(AtomicBool::new(false));
            let (input, log, _mode) = source(raw, chunk, &pool, &cancel);
            let mut reader = BlockingReader::open_seekable_in_with_payload_ownership(
                input,
                Limits::default(),
                &pool,
                policy,
            )
            .unwrap();
            let held = reader.next_typed_packet(&cancel).unwrap().unwrap();
            let before = facts(&held);
            let error = reader
                .seek_for_playback(0, 40_000_000, &cancel)
                .unwrap_err();
            assert!(
                matches!(error, Error::Truncated),
                "{policy:?}/{chunk}: {error:?}"
            );
            let calls = log.lock().unwrap().clone();
            drop(reader);
            assert_eq!(facts(&held), before);
            assert!(pool.used() > base);
            drop(held);
            assert_eq!(pool.used(), base);
            println!("SEEK_WINDOW_EXTRA truncated-seek-{policy:?}-{chunk} {error:?} {calls:?}");
        }
    }
}

#[test]
fn seek_window_scan_limit_keeps_refusal_and_retained_output() {
    let raw = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/seek/cues-front.mkv"
    ));
    for policy in policy_cases() {
        for chunk in [7, 8192] {
            let pool = Budget::new(512 * 1024).unwrap();
            let base = pool.used();
            let cancel = Arc::new(AtomicBool::new(false));
            let (input, log, _mode) = source(raw.to_vec(), chunk, &pool, &cancel);
            // This fixture opens within 4KiB; reaching the selected later
            // replay boundary requires more than 4KiB of seek validation/state.
            let limits = Limits {
                scan_bytes: 4096,
                ..Limits::default()
            };
            let mut reader = BlockingReader::open_seekable_in_with_payload_ownership(
                input, limits, &pool, policy,
            )
            .unwrap();
            let held = reader.next_typed_packet(&cancel).unwrap().unwrap();
            let before = facts(&held);
            let error = reader
                .seek_for_playback(0, 7_500_000_000, &cancel)
                .unwrap_err();
            assert!(
                matches!(error, Error::Policy(_)),
                "{policy:?}/{chunk}: {error:?}"
            );
            let calls = log.lock().unwrap().clone();
            drop(reader);
            assert_eq!(facts(&held), before);
            assert!(pool.used() > base);
            drop(held);
            assert_eq!(pool.used(), base);
            println!("SEEK_WINDOW_EXTRA limited-seek-{policy:?}-{chunk} {error:?} {calls:?}");
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
