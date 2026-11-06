// Independent additive public-API controls. Append inside the seek-window test
// module, which owns fixture/source/facts/policy_cases. No new production API.
// Expected classes follow the existing finite-file and scan-limit contracts;
// actual baseline execution is still required. Do not treat an authoring or
// setup failure as production RED, and preserve every original failure.

#[test]
fn seek_window_extra_truncated_seek_keeps_error_and_old_output_owner() {
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
fn seek_window_extra_scan_limit_keeps_refusal_and_retained_output() {
    let raw = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../.research/seek-fixtures/cues-front.mkv"
    ));
    for policy in policy_cases() {
        for chunk in [7, 8192] {
            let pool = Budget::new(512 * 1024).unwrap();
            let base = pool.used();
            let cancel = Arc::new(AtomicBool::new(false));
            let (input, log, _mode) = source(raw.to_vec(), chunk, &pool, &cancel);
            // This frozen fixture opens within 4KiB; reaching the selected later
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
