use super::*;

#[test]
fn host_memory_records_available_memory_and_reclaim_changes() {
    let counters = |scan: i128, steal: i128| {
        vec![
            ("pgscan_kswapd".to_owned(), scan),
            ("pgsteal_kswapd".to_owned(), steal),
        ]
    };
    let change = host_memory_change(
        Some((8 << 30, counters(100, 40))),
        Some((6 << 30, counters(160, 70))),
    )
    .unwrap();
    assert_eq!(
        py_int(change.get("mem_available_start_bytes")),
        Some(8 << 30)
    );
    assert_eq!(py_int(change.get("mem_available_end_bytes")), Some(6 << 30));
    let reclaim = change.get("reclaim").unwrap();
    assert_eq!(py_int(reclaim.get("pgscan_kswapd")), Some(60));
    assert_eq!(py_int(reclaim.get("pgsteal_kswapd")), Some(30));
    assert!(host_memory_change(None, Some((1, vec![]))).is_none());
    if cfg!(target_os = "linux") {
        let (available, counters) = host_memory().unwrap();
        assert!(available > 0 && counters.iter().any(|(n, _)| n == "pgscan_kswapd"));
    }
}

#[cfg(unix)]
#[test]
fn eviction_leaves_the_workers_unchanged() {
    let root = crate::walk::scratch("memory-worker-eviction");
    fs::create_dir_all(&root).unwrap();
    let workers = [root.join("candidate"), root.join("reference")];
    for (worker, bytes) in workers.iter().zip([&b"candidate"[..], b"reference"]) {
        fs::write(worker, bytes).unwrap();
    }
    let host = Native {
        root: &root,
        timeout: Duration::from_secs(5),
    };
    host.evict(&[&workers[0], &workers[1]]).unwrap();
    assert_eq!(fs::read(&workers[0]).unwrap(), b"candidate");
    assert_eq!(fs::read(&workers[1]).unwrap(), b"reference");
    assert!(host.evict(&[&root.join("missing")]).is_err() || !host.linux());
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn memory_preparation_replaces_both_files_and_keeps_certificate_paths() {
    use std::os::unix::fs::MetadataExt;

    let root = crate::walk::scratch("memory-worker-copies");
    fs::create_dir_all(&root).unwrap();
    let candidate = root.join("candidate");
    let reference = root.join("longer-reference-name");
    fs::write(&candidate, b"candidate").unwrap();
    fs::write(&reference, b"reference").unwrap();
    let slot = Slot {
        index: 0,
        cpu: None,
    };
    let paths = prepare_workers(&slot, &root, &candidate, &reference).unwrap();
    let first = paths.each_ref().map(|path| fs::File::open(path).unwrap());
    assert_eq!(paths[0].as_os_str().len(), paths[1].as_os_str().len());
    assert_eq!(
        paths,
        prepare_workers(&slot, &root, &candidate, &reference).unwrap()
    );
    let other = prepare_workers(
        &Slot {
            index: 11,
            cpu: None,
        },
        &root,
        &candidate,
        &reference,
    )
    .unwrap();
    for (path, own) in other.iter().zip(&paths) {
        assert_ne!(path, own);
        assert_eq!(path.as_os_str().len(), own.as_os_str().len());
    }
    for (path, before) in paths.iter().zip(first) {
        assert_ne!(
            fs::metadata(path).unwrap().ino(),
            before.metadata().unwrap().ino()
        );
    }
    assert_eq!(fs::read(&paths[0]).unwrap(), fs::read(&candidate).unwrap());
    assert_eq!(fs::read(&paths[1]).unwrap(), fs::read(&reference).unwrap());

    let roles = ["candidate", "reference"];
    let hashes = paths.each_ref().map(|path| file_sha(path).unwrap());
    let certificate =
        Value::object([
            (
                "worker_hashes",
                Value::object(roles.into_iter().zip(hashes.iter().map(Value::str))),
            ),
            (
                "linked_libraries",
                Value::object(roles.into_iter().zip(paths.iter().zip(&hashes).map(
                    |(path, hash)| {
                        Value::object([(
                            "inspected_artifact",
                            Value::object([
                                ("path", Value::str(path.display().to_string())),
                                ("sha256", Value::str(hash)),
                            ]),
                        )])
                    },
                ))),
            ),
        ]);

    fs::write(&reference, b"rebuilt reference").unwrap();
    let rebuilt = prepare_workers(&slot, &root, &candidate, &reference).unwrap();
    assert_ne!(rebuilt, paths);
    assert!(linux::measured_workers_current(&certificate));
    fs::write(&paths[1], b"different bytes").unwrap();
    assert!(!linux::measured_workers_current(&certificate));
    fs::remove_file(&paths[1]).unwrap();
    assert!(!linux::measured_workers_current(&certificate));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_memory_session_settles_only_when_heap_and_total_rss_both_do() {
    let heaps = vec![(0.0, 4096.0); 20];
    let mut totals: Vec<(f64, f64)> = (0..20)
        .map(|i| (if i % 2 == 0 { 900.0 } else { 910.0 }, 1000.0))
        .collect();
    let ceiling = crate::harness::policy::Memory::load(&crate::root())
        .unwrap()
        .ceiling();
    let gate = Pairs::gate(30, ceiling);
    assert!(settled(&heaps[..19], &totals[..19], gate).is_none());
    assert!(settled(&heaps, &totals, gate).is_some());
    assert!(settled(&heaps, &totals, Pairs::fixed(30)).is_none());
    assert!(settled(&heaps, &totals, Pairs::gate(20, ceiling)).is_none());
    // Equal to the reference is not lower; two of the first ten may be.
    totals[2] = (1000.0, 1000.0);
    totals[5] = (1100.0, 1000.0);
    assert!(settled(&heaps, &totals, gate).is_some());
    totals[12] = (1000.0, 1000.0);
    assert!(settled(&heaps, &totals, gate).is_none());
    totals[12] = (900.0, 1000.0);
    let mut heavy = heaps.clone();
    heavy[15] = (4096.0, 4096.0);
    assert!(settled(&heavy, &totals, gate).is_none());
}

#[test]
fn slots_never_outnumber_the_kernel_sessions() {
    let module = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../harness/rss-bound/triergon_rss_bound.c"),
    )
    .unwrap();
    assert!(module.contains(&format!("#define MAX_SESSIONS {MAX_SLOTS}\n")));
}

#[test]
fn jobs_sharing_a_file_share_a_group_in_job_order() {
    let workers = WorkerPaths {
        candidate: Path::new("candidate"),
        reference: Path::new("reference"),
        registered: true,
    };
    let job = |files: &[&str]| {
        Job::new(&workers, files.iter().map(PathBuf::from), |_: &Slot| {
            Ok(Value::Null)
        })
    };
    let jobs = vec![
        job(&["a"]),
        job(&["b"]),
        job(&["c", "d"]),
        job(&["e"]),
        job(&["d", "b"]),
        job(&["a"]),
    ];
    // Missing workers are not relocatable: their own files join every job.
    assert_eq!(shared_file_groups(&jobs), vec![vec![0, 1, 2, 3, 4, 5]]);
    let relocated_jobs: Vec<Job<'_>> =
        [&["a"][..], &["b"], &["c", "d"], &["e"], &["d", "b"], &["a"]]
            .into_iter()
            .map(|files| Job {
                files: files.iter().map(PathBuf::from).collect(),
                collect: Box::new(|_: &Slot| Ok(Value::Null)),
            })
            .collect();
    assert_eq!(
        shared_file_groups(&relocated_jobs),
        vec![vec![0, 5], vec![1, 2, 4], vec![3]]
    );
}

#[test]
fn collections_return_in_job_order() {
    let jobs = (0..40)
        .map(|n| Job {
            files: vec![PathBuf::from(format!("input-{}", n % 7))],
            collect: Box::new(move |_: &Slot| Ok(Value::int(n))),
        })
        .collect();
    let results: Vec<Value> = run_in_slots(jobs)
        .unwrap()
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert_eq!(results, (0..40).map(Value::int).collect::<Vec<_>>());
}

#[test]
fn unreadable_memory_input_returns_a_failed_row() {
    let root = crate::walk::scratch("unreadable-memory-input");
    let input = root.join("missing.ts");
    let certificate = Value::object([("status", Value::str("qualified-selected-workers"))]);
    let result = measure(
        &Native {
            root: &root,
            timeout: worker::DEFAULT_TIMEOUT,
        },
        &root,
        &root,
        &root,
        Pairs::fixed(1),
        0,
        &certificate,
        Invocation {
            selection: Selection::Both,
            operation: Some("memory"),
            input: &input,
            arguments: &[],
            candidate_arguments: None,
            expected_outputs: None,
            work_fields: None,
            timeout: worker::DEFAULT_TIMEOUT,
        },
    );
    assert_eq!(s(&result, "status"), Some("input-check-failed"));
    assert!(s(&result, "input_error").unwrap().contains("missing.ts"));
    assert_eq!(result.get("certificate"), Some(&certificate));
}

#[test]
fn memory_work_requires_both_digests_even_when_all_roles_omit_the_second() {
    let expected = json::parse(r#"{"packets":1,"bytes":2,"digest":3,"digest_b":4}"#).unwrap();
    let fields = vec!["digest_b".to_owned()];
    let observed = Value::object([("work", expected.clone())]);
    let sample = Value::object(
        [
            "candidate_heap",
            "candidate_rss",
            "reference_heap",
            "reference_rss",
        ]
        .map(|role| (role, observed.clone())),
    );
    assert!(work_valid(
        std::slice::from_ref(&sample),
        &fields,
        Some(&expected),
        &[]
    ));
    for bad in [Some(Value::int(5)), None] {
        for roles in [
            vec!["candidate_rss"],
            vec!["reference_heap"],
            vec![
                "candidate_heap",
                "candidate_rss",
                "reference_heap",
                "reference_rss",
            ],
        ] {
            let Value::Object(mut entries) = sample.clone() else {
                unreachable!()
            };
            for (role, observed) in &mut entries {
                if roles.contains(&role.as_str()) {
                    let Value::Object(wrapper) = observed else {
                        unreachable!()
                    };
                    let Value::Object(work) =
                        &mut wrapper.iter_mut().find(|(k, _)| k == "work").unwrap().1
                    else {
                        unreachable!()
                    };
                    work.retain(|(k, _)| k != "digest_b");
                    if let Some(bad) = &bad {
                        work.push(("digest_b".into(), bad.clone()));
                    }
                }
            }
            assert!(!work_valid(
                &[Value::Object(entries)],
                &fields,
                Some(&expected),
                &[]
            ));
        }
    }
}

#[test]
fn distinct_commands_keep_compatibility_options_on_the_candidate() {
    let reference = ["open-memory", "input.mov", "1"].map(String::from);
    let candidate = ["--bounded-timing", "open-memory", "input.mov", "1"].map(String::from);
    let invocation = Invocation {
        selection: Selection::Both,
        operation: None,
        input: Path::new("input.mov"),
        arguments: &reference,
        candidate_arguments: Some(&candidate),
        expected_outputs: None,
        work_fields: None,
        timeout: worker::DEFAULT_TIMEOUT,
    };
    assert_eq!(invocation.arguments_for(true), candidate);
    assert_eq!(invocation.arguments_for(false), reference);
}

#[test]
fn shared_commands_remain_identical_for_both_roles() {
    let arguments = ["avc", "aac"].map(String::from);
    let invocation = Invocation {
        selection: Selection::Both,
        operation: Some("hls-memory"),
        input: Path::new("input.ts"),
        arguments: &arguments,
        candidate_arguments: None,
        expected_outputs: None,
        work_fields: None,
        timeout: worker::DEFAULT_TIMEOUT,
    };
    assert_eq!(invocation.arguments_for(true), arguments);
    assert_eq!(invocation.arguments_for(false), arguments);
}

#[test]
fn a_deferred_memory_bound_collects_and_grades_only_the_selected_observer() {
    let observed = json::parse(
        r#"{"status":"measured","work":{"packets":1,"bytes":1,"digest":1},
            "observer":{"heap_peak_bytes":50,"page_size":10,"rss_start":100,
            "hwm_end":300,"rss_peak_observed":300}}"#,
    )
    .unwrap();
    for (selection, measured, omitted) in [
        (
            Selection::Heap,
            "heap_ratio_upper_95",
            "total_rss_ratio_upper_95",
        ),
        (
            Selection::Rss,
            "total_rss_ratio_upper_95",
            "heap_ratio_upper_95",
        ),
    ] {
        let sample =
            |roles: &[&str]| Value::object(roles.iter().map(|&role| (role, observed.clone())));
        let grade = |row: Value| {
            evaluate_selected(
                Path::new("."),
                &[row.clone(), row],
                Pairs::fixed(2),
                0,
                &Value::Null,
                None,
                selection,
            )
        };
        let accepted = grade(sample(selection.roles()));
        assert_eq!(s(&accepted, "status"), Some("qualified"));
        assert!(accepted.get(measured).and_then(Value::as_f64).is_some());
        assert_eq!(accepted.get(omitted), Some(&Value::Null));
        let surplus = grade(sample(Selection::Both.roles()));
        assert_eq!(s(&surplus, "status"), Some("invalid-memory-observation"));
    }
}
