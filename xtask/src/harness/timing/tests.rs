use super::*;

#[test]
fn a_gate_session_stops_at_the_look_once_settled_below_the_ceiling() {
    let pair = |milli: u32| (f64::from(milli), 1000.0);
    // Alternating, so the pairs show no autocorrelation.
    let fast: Vec<_> = (0..30)
        .map(|i| pair(if i % 2 == 0 { 800 } else { 829 }))
        .collect();
    let gate = Pairs::gate(30, 1.2);
    assert!(
        (1..=30)
            .filter(|&n| n != 20)
            .all(|n| early_stop(&fast[..n], gate).is_none())
    );
    let stop = early_stop(&fast[..20], gate).unwrap();
    assert_eq!(stop.upper_95, 0.829);
    assert!(early_stop(&fast[..20], Pairs::fixed(30)).is_none());
    assert!(early_stop(&fast[..20], Pairs::gate(20, 1.2)).is_none());
    // Two slow pairs among the first ten still settle; one in the second does not.
    let mut mixed = fast.clone();
    mixed[3] = pair(1250);
    mixed[6] = pair(1300);
    assert!(early_stop(&mixed[..20], gate).is_some());
    mixed[14] = pair(1210);
    assert!(early_stop(&mixed[..20], gate).is_none());
    let stop = Value::object([
        ("rule", Value::str(EARLY_STOP_RULE)),
        ("planned_pairs", Value::int(30)),
        ("ceiling", Value::float(1.2)),
        ("stopped_after", Value::int(20)),
    ]);
    assert_eq!(
        replay_early_stop(&stop, &fast[..20], 30, 280)
            .unwrap()
            .upper_95,
        0.829
    );
    // A session must stop where the rule stops, and only there.
    assert!(replay_early_stop(&stop, &fast, 30, 280).is_err());
    assert!(replay_early_stop(&stop, &fast[..10], 30, 280).is_err());
    let mut ran = stop.clone();
    crate::harness::validation::put(&mut ran, "stopped_after", Value::Null).unwrap();
    assert!(replay_early_stop(&ran, &fast, 30, 280).is_err());
    assert_eq!(
        replay_early_stop(&ran, &mixed, 30, 280).unwrap(),
        stats::ratio_interval(&mixed, 280, 4000).unwrap()
    );
    assert!(replay_early_stop(&ran, &mixed[..20], 30, 280).is_err());
}

#[test]
fn worker_failure_and_work_mismatch_precede_contamination() {
    let operation = Operation {
        prefix: String::new(),
        arguments: vec![],
        candidate_options: vec![],
        work: Work::default(),
        timeout: Duration::from_secs(1),
    };
    let run = |packets| {
        Value::object([
            ("status", Value::str("ok")),
            (
                "data",
                Value::object([
                    ("iterations", Value::int(2)),
                    ("packets", Value::int(packets)),
                    ("bytes", Value::int(188)),
                    ("digest", Value::int(17)),
                    ("elapsed_ns", Value::int(20)),
                ]),
            ),
        ])
    };
    let warm =
        super::super::comparison::paired_timing_work(&run(1), &run(1), 2, &operation.work).unwrap();
    let row = |candidate| Value::object([("candidate", candidate), ("reference", run(1))]);
    let check = |candidate| checked_pair(&row(candidate), &warm, 2, &operation, true);
    assert_eq!(
        check(Value::object([("status", Value::str("failed"))])),
        Err("worker-failure")
    );
    assert_eq!(check(run(2)), Err("unequal-work"));
    assert_eq!(check(run(1)), Err("contaminated"));
}

#[test]
fn three_contaminated_attempts_keep_each_seed_and_the_last() {
    let result = retry_contaminated(280, |_| {
        Ok(Value::object([("status", Value::str("contaminated"))]))
    })
    .unwrap();
    assert_eq!(result.get("seed"), Some(&Value::int(282)));
    let seeds: Vec<_> = result
        .get("rejected_contaminated_attempts")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|attempt| attempt.get("seed").cloned())
        .collect();
    assert_eq!(seeds, [280, 281, 282].map(|seed| Some(Value::int(seed))));
}

#[test]
fn failed_retry_preserves_rejected_samples() {
    let first = Value::object([
        ("status", Value::str("contaminated")),
        ("samples", Value::Array(vec![Value::int(17)])),
    ]);
    let result = retry_contaminated(280, |seed| match seed {
        280 => Ok(first.clone()),
        281 => Err("input unreadable".into()),
        _ => panic!("timing must stop on a failed retry"),
    })
    .unwrap();
    assert_eq!(
        result.get("status").and_then(Value::as_str),
        Some("timing-attempt-failed")
    );
    assert_eq!(
        result.get("error").and_then(Value::as_str),
        Some("input unreadable")
    );
    assert_eq!(
        result.get("rejected_contaminated_attempts"),
        Some(&Value::Array(vec![Value::object([
            ("status", Value::str("contaminated")),
            ("samples", Value::Array(vec![Value::int(17)])),
            ("seed", Value::int(280)),
        ])]))
    );
    assert_eq!(result.get("seed"), Some(&Value::int(281)));
}

#[test]
fn failed_timing_preserves_complete_warmups_and_module_observations() {
    let warmups = vec![Value::object([
        ("worker", Value::str("candidate")),
        ("result", Value::object([("digest_b", Value::int(17))])),
    ])];
    let observations = vec![Value::object([(
        "stage",
        Value::str("after-warmup:candidate"),
    )])];
    let result = status_only("unequal-work", &[], &warmups, &observations, vec![]);
    assert_eq!(result.get("warmups"), Some(&Value::Array(warmups)));
    assert_eq!(
        result.get("timing_module_observations"),
        Some(&Value::Array(observations))
    );
    assert_eq!(result.get("samples"), Some(&Value::Array(vec![])));
}

#[test]
fn a_worker_whose_own_output_changes_between_pairs_is_refused() {
    let run = |digest: i64| {
        Value::object([
            ("status", Value::str("ok")),
            (
                "data",
                Value::object([
                    ("iterations", Value::int(1)),
                    ("elapsed_ns", Value::int(100)),
                    ("packets", Value::int(2)),
                    ("bytes", Value::int(40)),
                    ("digest", Value::int(digest)),
                ]),
            ),
        ])
    };
    let hashes = Value::object([
        ("candidate", Value::str("a")),
        ("reference", Value::str("b")),
    ]);
    let interval = stats::ratio_interval(&[(100.0, 100.0); 2], 0, 4000).unwrap();
    let record = |second: i64| {
        let samples = [7, second]
            .into_iter()
            .enumerate()
            .map(|(pair, digest)| {
                Value::object([
                    ("pair", Value::int(pair as i64)),
                    ("order", Value::strings(["candidate", "reference"])),
                    ("candidate", run(digest)),
                    ("reference", run(9)),
                    ("cpu_before", Value::Object(vec![])),
                    ("cpu_after", Value::Object(vec![])),
                ])
            })
            .collect();
        let warmup = |worker: &str, digest: i64| {
            Value::object([("worker", Value::str(worker)), ("result", run(digest))])
        };
        Value::object([
            ("pairs", Value::int(2)),
            ("iterations", Value::int(1)),
            ("seed", Value::int(0)),
            ("worker_hashes", hashes.clone()),
            ("ratio", interval_value(&interval)),
            (
                "warmups",
                Value::Array(vec![warmup("candidate", 7), warmup("reference", 9)]),
            ),
            ("samples", Value::Array(samples)),
        ])
    };
    let own = Work {
        fields: vec![],
        own: vec!["digest".to_owned()],
    };
    let expected = Expected {
        worker_hashes: &hashes,
        input_sha256: None,
        minimum_pairs: 2,
        work: &own,
        require_linux: false,
        module_absence: false,
        ceiling: Some(1.2),
    };
    assert!(validate(&record(7), &expected).is_ok());
    assert_eq!(
        validate(&record(8), &expected).unwrap_err(),
        "timing pair 1 did other work"
    );
    // Without own work, the two workers' different digests are unequal work.
    let shared = Work::default();
    let strict = Expected {
        work: &shared,
        ..expected
    };
    assert!(validate(&record(7), &strict).is_err());
}

#[test]
fn compatibility_options_apply_only_to_the_candidate() {
    let operation = Operation {
        prefix: "open-".into(),
        arguments: ["1", "0", "memory", "4096"].map(OsString::from).to_vec(),
        candidate_options: vec!["--bounded-timing".into(), "--bounded-sync".into()],
        work: Work::default(),
        timeout: worker::DEFAULT_TIMEOUT,
    };
    let expected = ["open-bench", "input.mov", "50", "1", "0", "memory", "4096"]
        .map(OsString::from)
        .to_vec();
    assert_eq!(
        operation.worker_arguments(false, "bench", Path::new("input.mov"), Some(50)),
        expected
    );
    let mut candidate = operation.candidate_options.clone();
    candidate.extend(expected);
    assert_eq!(
        operation.worker_arguments(true, "bench", Path::new("input.mov"), Some(50)),
        candidate
    );
}

#[test]
fn ordinary_memory_arguments_keep_the_existing_order() {
    let operation = Operation {
        prefix: "hls-".into(),
        arguments: vec!["avc".into(), "aac".into()],
        candidate_options: vec![],
        work: Work::default(),
        timeout: worker::DEFAULT_TIMEOUT,
    };
    for candidate in [false, true] {
        assert_eq!(
            operation.worker_arguments(candidate, "memory", Path::new("input.ts"), None),
            ["hls-memory", "input.ts", "avc", "aac"].map(OsString::from)
        );
    }
}

#[test]
fn timed_workers_are_equal_length_copies_removed_afterwards() {
    let dir = crate::walk::scratch("timed-workers");
    fs::create_dir_all(dir.join("other")).unwrap();
    fs::write(dir.join("media-worker"), b"candidate").unwrap();
    fs::write(dir.join("other/ffmpeg-worker"), b"reference").unwrap();
    let timed =
        TimedWorkers::new(&dir.join("media-worker"), &dir.join("other/ffmpeg-worker")).unwrap();
    assert_eq!(fs::read(&timed.candidate).unwrap(), b"candidate");
    assert_eq!(fs::read(&timed.reference).unwrap(), b"reference");
    assert_eq!(
        timed.candidate.as_os_str().len(),
        timed.reference.as_os_str().len()
    );
    let directory = timed.directory.clone();
    drop(timed);
    assert!(!directory.exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn timed_workers_keep_other_output_in_the_staging_directory() {
    let dir = crate::walk::scratch("timed-workers-output");
    let candidate = dir.join("candidate");
    let reference = dir.join("reference");
    fs::write(&candidate, b"candidate").unwrap();
    fs::write(&reference, b"reference").unwrap();
    let timed = TimedWorkers::new(&candidate, &reference).unwrap();
    let grade = timed.directory.join("a/report.json");
    fs::write(&grade, b"saved grade").unwrap();
    assert!(TimedWorkers::new(&candidate, &reference).is_err());
    assert_eq!(fs::read(&grade).unwrap(), b"saved grade");
    drop(timed);
    assert_eq!(fs::read(&grade).unwrap(), b"saved grade");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn timed_workers_keep_another_commands_empty_reservation() {
    let dir = crate::walk::scratch("timed-workers-reserved-slot");
    let mut timed = TimedWorkers::reserve(dir.join("staging")).unwrap();
    let grade = timed.directory.join("b");
    fs::create_dir(&grade).unwrap();
    assert!(timed.reserve_slots().is_err());
    assert!(timed.directory.join("a").is_dir());
    drop(timed);
    assert!(grade.is_dir());
    assert!(!dir.join("staging/a").exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn linux_process_names_follow_the_tracked_set() {
    for name in [
        "rustc",
        "clang",
        "clang-18",
        "clang++-17",
        "gcc-13",
        "g++",
        "ld.lld",
        "cc1plus",
        "ffmpeg",
        "ninja",
    ] {
        assert!(tracked_on_linux(name), "{name}");
    }
    for name in ["clang-", "gcc-x", "python3", "ld.mold", "bash"] {
        assert!(!tracked_on_linux(name), "{name}");
    }
}

#[test]
fn splits_ps_records_like_python() {
    assert_eq!(
        split4("  12 0:01.50 R  /usr/bin/cc -c x.c"),
        Some(["12", "0:01.50", "R", "/usr/bin/cc -c x.c"])
    );
    assert_eq!(split4("12 0:01 R"), None);
}

#[test]
fn darwin_snapshots_parse_ps() {
    if is_linux() {
        return;
    }
    let snapshot = darwin_snapshot().unwrap();
    assert!(matches!(snapshot, Value::Object(_)));
    assert_eq!(
        timing_module_state().get("status").and_then(Value::as_str),
        Some("not-applicable")
    );
}

/// An authored `/proc` with one tracked process (pid 123) and its own
/// aggregate and per-CPU counters; nothing native is read.
struct Proc(PathBuf);

/// A Linux `ps` record whose clock has a days field.
const CLANG: &str = "123 1-02:03:04 S clang\n";
const STAT: &str = "cpu 10 0 20 100 0 0 0 5 2 1\ncpu0 10 0 20 100 0 0 0 5 2 1\n";

/// `/proc/123/stat` with `utime` `cpu`, `stime` 2 and start time `start`.
fn proc_stat(cpu: i64, start: i64) -> String {
    let mut fields = vec!["0".to_owned(); 21];
    fields[0] = "S".into();
    fields[11] = cpu.to_string();
    fields[12] = "2".into();
    fields[19] = start.to_string();
    format!("123 (clang test) {}", fields.join(" "))
}

impl Proc {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("triergon-proc-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("123")).unwrap();
        fs::create_dir_all(root.join("sys/kernel/random")).unwrap();
        let proc = Proc(root);
        proc.write("123/stat", &proc_stat(10, 100));
        proc.write("sys/kernel/random/boot_id", "authored-boot-id\n");
        proc.write("stat", STAT);
        proc
    }

    fn write(&self, path: &str, text: &str) {
        fs::write(self.0.join(path), text).unwrap();
    }

    fn remove(&self, path: &str) {
        fs::remove_file(self.0.join(path)).unwrap();
    }

    fn snapshot(&self, listing: &str) -> Value {
        linux_snapshot_in(&self.0, || Ok(listing.to_owned()), || Ok(100))
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_watch_compares_processes_only_across_a_full_window() {
    let proc = Proc::new();
    let quiet = proc.snapshot(CLANG);
    proc.write("123/stat", &proc_stat(11, 100));
    let busy = proc.snapshot(CLANG);
    let mut watch = ContaminationWatch::new(quiet.clone());
    let start = watch.opened;
    assert!(watch.observe(&busy, start).is_empty());
    assert_eq!(
        watch.observe(&busy, start + CONTAMINATION_WINDOW),
        ["clang".to_owned()]
    );
    // The next window opens at the snapshot that closed this one.
    assert!(
        watch
            .observe(&busy, start + CONTAMINATION_WINDOW * 2)
            .is_empty()
    );
}

fn linux(snapshot: &Value) -> &Value {
    snapshot.get(LINUX_CPU_KEY).unwrap()
}

fn steal(snapshot: &Value) -> &Value {
    &linux(snapshot).get("counters").unwrap().as_array().unwrap()[7]
}

fn findings(before: &Value, after: &Value) -> Vec<String> {
    active_cpu(before, after, false)
}

/// Replace `key` of the Linux accounting record.
fn with_linux(snapshot: &Value, key: &str, value: Value) -> Value {
    let mut changed = snapshot.clone();
    let Value::Object(members) = &mut changed else {
        unreachable!()
    };
    let (_, record) = members
        .iter_mut()
        .find(|(k, _)| k == LINUX_CPU_KEY)
        .unwrap();
    let Value::Object(fields) = record else {
        unreachable!()
    };
    fields.iter_mut().find(|(k, _)| k == key).unwrap().1 = value;
    changed
}

#[test]
fn linux_ps_days_clock_does_not_replace_native_process_ticks() {
    let proc = Proc::new();
    let snapshot = proc.snapshot(CLANG);
    let process = snapshot.get("123").unwrap();
    assert_eq!(process.get("cpu_ticks"), Some(&Value::int(12)));
    assert_eq!(
        process.get("ps_clock_raw").and_then(Value::as_str),
        Some("1-02:03:04")
    );
    assert_eq!(
        linux(&snapshot).get("status").and_then(Value::as_str),
        Some("recorded")
    );
    assert_eq!(steal(&snapshot), &Value::int(5));
}

#[test]
fn process_tick_advance_and_pid_reuse_are_contaminants() {
    let proc = Proc::new();
    let before = proc.snapshot(CLANG);
    for text in [proc_stat(11, 100), proc_stat(10, 101)] {
        proc.write("123/stat", &text);
        assert!(findings(&before, &proc.snapshot(CLANG)).contains(&"clang".to_owned()));
    }
}

#[test]
fn cpu_steal_is_rejected_without_deleting_raw_counters() {
    let proc = Proc::new();
    let before = proc.snapshot("");
    proc.write("stat", &STAT.replace(" 5 2 1", " 6 2 1"));
    let after = proc.snapshot("");
    assert!(findings(&before, &after).contains(&"linux-cpu-steal".to_owned()));
    assert_eq!(steal(&after), &Value::int(6));
}

#[test]
fn missing_proc_or_failed_ps_is_unavailable_never_idle() {
    let proc = Proc::new();
    let before = proc.snapshot("");
    proc.remove("stat");
    let unavailable = "linux-cpu-evidence-unavailable".to_owned();
    assert!(findings(&before, &proc.snapshot("")).contains(&unavailable));
    let failed = linux_snapshot_in(&proc.0, || Err("ps: exit status: 1".to_owned()), || Ok(100));
    assert!(findings(&before, &failed).contains(&unavailable));
}

#[test]
fn missing_process_stat_and_missing_steal_counter_are_not_idle() {
    let proc = Proc::new();
    let before = proc.snapshot(CLANG);
    proc.remove("123/stat");
    let unavailable = "linux-cpu-evidence-unavailable".to_owned();
    assert!(findings(&before, &proc.snapshot(CLANG)).contains(&unavailable));
    proc.write("stat", "cpu 10 0 20 100\n");
    assert!(findings(&before, &proc.snapshot("")).contains(&unavailable));
}

#[test]
fn counter_regression_boot_change_and_missing_linux_marker_fail_closed() {
    let proc = Proc::new();
    let before = proc.snapshot("");
    let regressed = {
        let mut counters = linux(&before).get("counters").unwrap().clone();
        let Value::Array(items) = &mut counters else {
            unreachable!()
        };
        items[7] = Value::int(4);
        with_linux(&before, "counters", counters)
    };
    let rebooted = with_linux(&before, "boot_id", Value::str("another-boot"));
    for after in [regressed, rebooted] {
        assert!(findings(&before, &after).contains(&"linux-cpu-evidence-invalid".to_owned()));
    }
    let empty = Value::Object(Vec::new());
    assert!(active_cpu(&empty, &empty, true).contains(&"linux-cpu-evidence-missing".to_owned()));
}

#[test]
fn darwin_process_snapshot_shape_and_fractional_clock_are_unchanged() {
    let snapshot = darwin_processes("123 1:02.34 S /usr/bin/clang\n").unwrap();
    let expected =
        crate::json::parse(r#"{"123": {"name": "clang", "cpu_ticks": 6234, "state": "S"}}"#)
            .unwrap();
    assert!(equal(&snapshot, &expected), "{snapshot:?}");
}
