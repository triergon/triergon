use super::*;

#[test]
fn mutations_replay_and_reject_tampered_recipes() {
    let seed: Vec<u8> = (0..200u8).collect();
    let mut rng = PyRandom::new(280);
    for _ in 0..50 {
        let (data, recipe) = mutation(&seed, &mut rng, 4096, 4).unwrap();
        assert_ne!(data, seed);
        assert_eq!(replay(&seed, &recipe, 4096).unwrap(), data);
        let mut tampered = recipe.clone();
        if let Value::Object(m) = &mut tampered {
            m.retain(|(k, _)| k != "result_sha256");
            m.push(("result_sha256".into(), Value::str("0".repeat(64))));
        }
        assert!(replay(&seed, &tampered, 4096).is_err());
    }
    let (empty, _) = mutation(&[], &mut rng, 16, 1).unwrap();
    assert!(!empty.is_empty());
    assert!(
        mutation(&seed, &mut rng, 10, 4).is_err(),
        "the seed exceeds the bound"
    );
}

#[test]
fn mutations_match_the_recorded_python_stream() {
    // The first recipe Python's generator writes for this seed and RNG state.
    let seed = b"0123456789abcdef".to_vec();
    let mut rng = PyRandom::new(7);
    let (_, recipe) = mutation(&seed, &mut rng, 64, 4).unwrap();
    assert_eq!(json::canonical(recipe.get("edits").unwrap()), PYTHON_EDITS);
}

const PYTHON_EDITS: &str = r#"[{"insert_hex":"","offset":12,"remove":1},{"insert_hex":"67","offset":13,"remove":1},{"insert_hex":"","offset":9,"remove":6}]"#;

#[test]
fn strict_json_bounds_documents() {
    assert!(strict_json(b"{\"a\": [1, 2.50]}").is_ok());
    assert!(
        strict_json(
            &[b'['; 300]
                .iter()
                .chain([b']'; 300].iter())
                .copied()
                .collect::<Vec<_>>()
        )
        .is_err()
    );
    assert!(strict_json(b"{\"a\": 1, \"a\": 1}").is_err());
    assert!(strict_json(b"\xff").is_err());
}

#[cfg(unix)]
#[test]
fn observations_classify_bounded_runs() {
    let sh = |script: &str| vec![OsString::from("sh"), "-c".into(), script.into()];
    let (ok, stdout, _) =
        observe(&sh("printf '{\"a\": 1.50}'"), Duration::from_secs(5), 1024).unwrap();
    assert_eq!(s(&ok, "status"), Some("ok"));
    assert_eq!(stdout, b"{\"a\": 1.50}");
    assert_eq!(
        s(
            &observe(&sh("exit 3"), Duration::from_secs(5), 1024)
                .unwrap()
                .0,
            "status"
        ),
        Some("rejected")
    );
    assert_eq!(
        s(
            &observe(&sh("exit 101"), Duration::from_secs(5), 1024)
                .unwrap()
                .0,
            "status"
        ),
        Some("crash")
    );
    assert_eq!(
        s(
            &observe(&sh("kill -SEGV $$"), Duration::from_secs(5), 1024)
                .unwrap()
                .0,
            "status"
        ),
        Some("crash")
    );
    assert_eq!(
        s(
            &observe(&sh("echo nope"), Duration::from_secs(5), 1024)
                .unwrap()
                .0,
            "status"
        ),
        Some("invalid-json")
    );
    let (slow, _, _) = observe(&sh("sleep 10"), Duration::from_millis(200), 1024).unwrap();
    assert_eq!(s(&slow, "status"), Some("timeout"));
    let (big, stdout, _) = observe(
        &sh("head -c 100000 /dev/zero"),
        Duration::from_secs(5),
        1000,
    )
    .unwrap();
    assert_eq!(
        (s(&big, "status"), stdout.len()),
        (Some("output-limit"), 1000)
    );
}

#[test]
fn comparisons_keep_rejections_ungraded_and_decimals_exact() {
    let ok = |data: &str| {
        Value::object([
            ("status", Value::str("ok")),
            ("data", json::parse_strict(data).unwrap()),
        ])
    };
    let result = compare(&[
        ("candidate".into(), ok(r#"{"v": 1.50}"#)),
        ("reference".into(), ok(r#"{"v": 1.5}"#)),
    ])
    .unwrap();
    assert_eq!(result.get("findings"), Some(&Value::Array(vec![])));
    let result = compare(&[
        ("candidate".into(), ok(r#"{"v": 1.50}"#)),
        ("reference".into(), ok(r#"{"v": 1.25}"#)),
    ])
    .unwrap();
    let shown = json::to_python(result.get("findings").unwrap(), None);
    assert!(shown.contains(r#"{"json_decimal": "1.50"}"#), "{shown}");
    // The reference's host-dependence flag is not work the candidate owes.
    let result = compare(&[
        ("candidate".into(), ok(r#"{"v": 1}"#)),
        (
            "reference".into(),
            ok(r#"{"v": 1, "host_dependent": true}"#),
        ),
    ])
    .unwrap();
    assert_eq!(result.get("findings"), Some(&Value::Array(vec![])));
    let rejected = Value::object([
        ("status", Value::str("rejected")),
        ("returncode", Value::int(2)),
        ("stderr_sha256", Value::str("a")),
    ]);
    let both = compare(&[
        ("candidate".into(), rejected.clone()),
        ("reference".into(), rejected.clone()),
    ])
    .unwrap();
    assert_eq!(both.get("findings"), Some(&Value::Array(vec![])));
    let mixed = compare(&[
        ("candidate".into(), ok("{}")),
        ("reference".into(), rejected),
    ])
    .unwrap();
    assert!(json::to_python(&mixed, None).contains("acceptance-disagreement"));
    assert!(compare(&[("candidate".into(), ok("{}"))]).is_err());
}

#[test]
fn replay_rejects_edits_outside_the_current_input() {
    let seed = b"abcdef".to_vec();
    let recipe = |edits: &str, result: &[u8]| {
        json::parse(&format!(
            r#"{{"schema": 1, "seed_sha256": "{}", "edits": {edits}, "result_sha256": "{}"}}"#,
            digest_bytes(&seed),
            digest_bytes(result)
        ))
        .unwrap()
    };
    assert_eq!(
        replay(
            &seed,
            &recipe(
                r#"[{"offset": 1, "remove": 2, "insert_hex": "ff"}]"#,
                b"a\xffdef"
            ),
            16
        )
        .unwrap(),
        b"a\xffdef"
    );
    for edits in [
        r#"[{"offset": 7, "remove": 0, "insert_hex": ""}]"#,
        r#"[{"offset": -1, "remove": 0, "insert_hex": ""}]"#,
        r#"[{"offset": 2, "remove": 5, "insert_hex": ""}]"#,
        r#"[{"offset": 0, "remove": 0, "insert_hex": "zz"}]"#,
        r#"[{"offset": 0, "remove": 0, "insert_hex": "f"}]"#,
        r#"[{"offset": true, "remove": 0, "insert_hex": ""}]"#,
        r#"[{"offset": 0, "remove": 0, "insert_hex": "", "extra": 1}]"#,
        r"[]",
    ] {
        assert!(replay(&seed, &recipe(edits, &seed), 16).is_err(), "{edits}");
    }
    assert!(
        replay(
            &seed,
            &recipe(
                r#"[{"offset": 0, "remove": 0, "insert_hex": "00000000000000000000"}]"#,
                b""
            ),
            8
        )
        .is_err(),
        "over the bound"
    );
}

#[test]
fn booleans_never_equal_numbers_and_long_decimals_stay_exact() {
    let ok = |data: &str| {
        Value::object([
            ("status", Value::str("ok")),
            ("data", json::parse_strict(data).unwrap()),
        ])
    };
    let result = compare(&[
        ("candidate".into(), ok(r#"{"a": true}"#)),
        ("reference".into(), ok(r#"{"a": 1}"#)),
    ])
    .unwrap();
    assert!(result.get("findings").is_some_and(Value::truthy));
    let long = format!("0.{}", "1".repeat(300));
    let result = compare(&[
        ("candidate".into(), ok(&format!(r"[{long}]"))),
        ("reference".into(), ok("[0.5]")),
    ])
    .unwrap();
    assert!(
        json::to_python(&result, None).contains(&long),
        "a long decimal is kept, not elided"
    );
    let shown = compare(&[
        ("candidate".into(), ok(&format!(r#""{}""#, "x".repeat(300)))),
        ("reference".into(), ok(r#""y""#)),
    ])
    .unwrap();
    assert!(json::to_python(&shown, None).contains("<large value>"));
}

#[cfg(unix)]
#[test]
fn a_finished_worker_leaves_no_descendants_behind() {
    let dir = crate::walk::scratch("fuzz-group");
    let marker = dir.join("child.pid");
    let script = format!(
        "sleep 30 >/dev/null 2>&1 & echo $! > {}; printf '{{}}'",
        marker.display()
    );
    let (result, _, _) = observe(
        &[OsString::from("sh"), "-c".into(), script.into()],
        Duration::from_secs(5),
        1024,
    )
    .unwrap();
    assert_eq!(s(&result, "status"), Some("ok"));
    let pid = fs::read_to_string(&marker).unwrap().trim().to_owned();
    std::thread::sleep(Duration::from_millis(100));
    let alive = Command::new("kill")
        .args(["-0", &pid])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(!alive, "the background child was not terminated");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn labels_are_bounded_lowercase_identifiers() {
    assert!(label("candidate") && label("debug-2") && label("a_b"));
    for bad in ["", "Candidate", "../x", "a/b", "1abc", &"a".repeat(65)] {
        assert!(!label(bad), "{bad}");
    }
}

#[cfg(unix)]
#[test]
fn a_campaign_keeps_raw_streams_and_flags_input_tampering() {
    let root = crate::walk::scratch("fuzz-campaign");
    fs::create_dir_all(root.join(".research")).unwrap();
    fs::create_dir_all(root.join("xtask/src/harness")).unwrap();
    for file in [
        "xtask/src/fuzz.rs",
        "xtask/src/json.rs",
        "xtask/src/harness/diff.rs",
        "xtask/src/pyrandom.rs",
    ] {
        fs::write(root.join(file), file).unwrap();
    }
    let worker = |name: &str, body: &str| {
        let path = root.join(name);
        crate::walk::write_script(&path, &format!("#!/bin/sh\n{body}\n"));
        path
    };
    let candidate = worker("candidate", "printf '{\"n\": 1.50}'");
    let reference = worker("reference", "printf '{\"n\": 1.5}'");
    let tamper = worker("tamper", "printf x >> \"$2\"; printf '{\"n\": 1.5}'");
    fs::write(root.join("seed.bin"), b"seed bytes").unwrap();
    let sha = |p: &Path| digest_bytes(&fs::read(p).unwrap());
    let manifest = |comparator: &Path| {
        format!(
            r#"{{"schema": 1, "workers": {{"candidate": {{"path": "{}", "sha256": "{}"}}, "reference": {{"path": "{}", "sha256": "{}"}}}},
                "source_files": {{"seed.bin": "{}"}},
                "operations": [{{"id": "trace", "roles": {{"candidate": ["trace", "{{input}}"], "reference": ["trace", "{{input}}"]}}}}],
                "seeds": [{{"id": "seed", "path": "seed.bin", "sha256": "{}", "rights": "authored-synthetic", "baseline_statuses": {{"trace": {{"candidate": "ok", "reference": "ok"}}}}}}]}}"#,
            candidate.display(),
            sha(&candidate),
            comparator.display(),
            sha(comparator),
            sha(&root.join("seed.bin")),
            sha(&root.join("seed.bin"))
        )
    };
    let run = |name: &str, comparator: &Path| {
        fs::write(root.join(format!("{name}.json")), manifest(comparator)).unwrap();
        let args = Campaign {
            manifest: root.join(format!("{name}.json")),
            output: root.join(".research").join(name),
            rounds: 3,
            seed: 280,
            maximum_input_bytes: 1024,
            maximum_output_bytes: 1024,
            timeout_seconds: Some(5.0),
        };
        (
            campaign(&root, &args).unwrap(),
            json::read(&root.join(".research").join(name).join("report.json")).unwrap(),
        )
    };
    let (code, report) = run("agree", &reference);
    assert_eq!(code, 0);
    assert_eq!(report.get("cases"), Some(&Value::int(4)));
    assert_eq!(
        report.get("collection_status").and_then(Value::as_str),
        Some("complete")
    );
    assert!(
        root.join(".research/agree/case-000003/trace/candidate.stdout")
            .is_file()
    );
    let (code, report) = run("tamper", &tamper);
    assert_eq!(code, 1);
    assert_eq!(
        report.get("collection_status").and_then(Value::as_str),
        Some("input-identity-failed")
    );
    let outside = Campaign {
        manifest: root.join("agree.json"),
        output: root.join("elsewhere"),
        rounds: 1,
        seed: 1,
        maximum_input_bytes: 1024,
        maximum_output_bytes: 1024,
        timeout_seconds: Some(1.0),
    };
    assert!(
        campaign(&root, &outside).is_err(),
        "output must be under .research"
    );
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn seed_arguments_fill_the_arguments_slot_and_hangs_time_out() {
    let root = crate::walk::scratch("fuzz-arguments");
    fs::create_dir_all(root.join(".research")).unwrap();
    fs::create_dir_all(root.join("xtask/src/harness")).unwrap();
    for file in [
        "xtask/src/fuzz.rs",
        "xtask/src/json.rs",
        "xtask/src/harness/diff.rs",
        "xtask/src/pyrandom.rs",
    ] {
        fs::write(root.join(file), file).unwrap();
    }
    let worker = |name: &str, body: &str| {
        let path = root.join(name);
        crate::walk::write_script(&path, &format!("#!/bin/sh\n{body}\n"));
        path
    };
    // `fold INPUT ARGUMENTS...`: echo the arguments; a mutated input hangs.
    fs::write(root.join("seed.bin"), b"seed bytes").unwrap();
    let body = format!(
        "cmp -s \"$2\" '{}' || sleep 30; printf '{{\"a\": \"%s %s\"}}' \"$3\" \"$4\"",
        root.join("seed.bin").display()
    );
    let (candidate, debug) = (worker("candidate", &body), worker("debug", &body));
    let sha = |p: &Path| digest_bytes(&fs::read(p).unwrap());
    let manifest = format!(
        r#"{{"schema": 1, "workers": {{"candidate": {{"path": "{}", "sha256": "{}"}}, "debug": {{"path": "{}", "sha256": "{}"}}}},
            "source_files": {{"seed.bin": "{}"}},
            "operations": [{{"id": "fold", "roles": {{"candidate": ["fold", "{{input}}", "{{arguments}}"], "debug": ["fold", "{{input}}", "{{arguments}}"]}}}}],
            "seeds": [{{"id": "seed", "path": "seed.bin", "sha256": "{}", "rights": "authored-synthetic", "arguments": ["x", "y"], "baseline_statuses": {{"fold": {{"candidate": "ok", "debug": "ok"}}}}}}]}}"#,
        candidate.display(),
        sha(&candidate),
        debug.display(),
        sha(&debug),
        sha(&root.join("seed.bin")),
        sha(&root.join("seed.bin"))
    );
    fs::write(root.join("campaign.json"), manifest).unwrap();
    let args = Campaign {
        manifest: root.join("campaign.json"),
        output: root.join(".research/arguments"),
        rounds: 1,
        seed: 280,
        maximum_input_bytes: 1024,
        maximum_output_bytes: 1024,
        timeout_seconds: Some(1.0),
    };
    assert_eq!(campaign(&root, &args).unwrap(), 1, "the hang is a finding");
    let case = |n: usize| root.join(format!(".research/arguments/case-{n:06}/fold"));
    assert_eq!(
        fs::read(case(0).join("candidate.stdout")).unwrap(),
        b"{\"a\": \"x y\"}"
    );
    let observed =
        json::read(&root.join(".research/arguments/case-000001/observation.json")).unwrap();
    let status = |role: &str| {
        observed
            .get("operations")
            .and_then(Value::as_array)
            .unwrap()[0]
            .get("workers")
            .and_then(|w| w.get(role))
            .and_then(|r| s(r, "status"))
            .map(str::to_owned)
    };
    assert_eq!(status("candidate").as_deref(), Some("timeout"));
    fs::remove_dir_all(root).unwrap();
}
