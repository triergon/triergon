//! Gate behaviour: missing, tampered or contaminated evidence never passes.

use super::acceptance::{acceptance, active_cpu};
use super::comparison::{reference_record, reference_summary, rule_for, validate_case_evidence};
use super::diff::differences;
use super::gate_build;
use super::parity::review_native_difference;
use crate::json::{Value, parse};

fn root() -> &'static std::path::Path {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
}

fn load(path: &str) -> Value {
    crate::checks::read_json(&root().join(path)).unwrap()
}

fn v(text: &str) -> Value {
    parse(text).unwrap()
}

/// Insert or replace `key` at the object path `at`.
fn put(value: &mut Value, at: &[&str], key: &str, new: Value) {
    let mut node = value;
    for step in at {
        let Value::Object(m) = node else {
            panic!("not an object at {step}")
        };
        node = &mut m.iter_mut().find(|(k, _)| k == step).unwrap().1;
    }
    let Value::Object(m) = node else {
        panic!("not an object")
    };
    match m.iter_mut().find(|(k, _)| k == key) {
        Some(slot) => slot.1 = new,
        None => m.push((key.to_owned(), new)),
    }
}

fn remove(value: &mut Value, key: &str) {
    if let Value::Object(m) = value {
        m.retain(|(k, _)| k != key);
    }
}

fn interval(pairs: &[(f64, f64)]) -> Value {
    let i = crate::stats::ratio_interval(pairs, 0, 4000).unwrap();
    Value::object([
        ("median", Value::float(i.median)),
        ("lower_95", Value::float(i.lower_95)),
        ("upper_95", Value::float(i.upper_95)),
    ])
}

/// Thirty matched timing pairs at 0.79 and thirty Darwin memory samples where
/// the candidate uses less heap and RSS: the smallest complete passing case.
fn matched_gate_fixture() -> (Value, Vec<Value>, Value) {
    let contract = v(
        r#"{"required_profiles": ["fixture"], "required_targets": ["host"], "minimum_pairs": 30,
                         "pending_module_requirements": []}"#,
    );
    let result = |ns: i64| {
        v(&format!(
            r#"{{"status": "ok", "data": {{"elapsed_ns": {ns}, "iterations": 100, "packets": 10, "bytes": 100, "digest": 9}}}}"#
        ))
    };
    let pairs: Vec<Value> = (0..30)
        .map(|i| {
            Value::object([
                ("pair", Value::int(i)),
                ("candidate", result(79)),
                ("reference", result(100)),
                ("cpu_before", Value::object(Vec::<(String, Value)>::new())),
                ("cpu_after", Value::object(Vec::<(String, Value)>::new())),
            ])
        })
        .collect();
    let hashes = v(r#"{"candidate": "candidate-hash", "reference": "reference-hash"}"#);
    let certificate = v(
        r#"{"status": "qualified-selected-workers", "errors": [], "worker_hashes": {"candidate": "candidate-hash", "reference": "reference-hash"},
        "observer_sha256": "observer-hash", "probe_sha256": "probe-hash",
        "probe_heap": {"status": "measured", "observer": {"heap_peak_bytes": 100, "heap_live_start": 0, "heap_live_end": 0}, "work": {"expected_peak_bytes": 100}}}"#,
    );
    let sample = |candidate: bool| {
        let (heap, peak) = if candidate { (79, 160) } else { (100, 300) };
        v(&format!(
            r#"{{"status": "measured", "instrument_sha256": "observer-hash", "work": {{"packets": 10, "bytes": 100, "digest": 9}},
            "observer": {{"status": "measured", "heap_peak_bytes": {heap}, "rss_start": 100, "rss_peak_observed": {peak}, "hwm_end": {peak}, "page_size": 1}}}}"#
        ))
    };
    let samples: Vec<Value> = (0..30)
        .map(|_| {
            Value::object([
                ("candidate_heap", sample(true)),
                ("candidate_rss", sample(true)),
                ("reference_heap", sample(false)),
                ("reference_rss", sample(false)),
            ])
        })
        .collect();
    let memory = super::memory::evaluate(root(), &samples, 30, 0, &certificate, None);
    let timing = Value::object([
        ("status", Value::str("measured")),
        ("worker_hashes", hashes.clone()),
        ("samples", Value::Array(pairs)),
        ("pairs", Value::int(30)),
        ("iterations", Value::int(100)),
        ("seed", Value::int(0)),
        ("ratio", interval(&[(79.0, 100.0); 30])),
    ]);
    let row = Value::object([
        ("id", Value::str("fixture")),
        ("profile", Value::str("fixture")),
        (
            "parity",
            v(r#"{"status": "pass", "source_timing_status": "pass"}"#),
        ),
        ("timing", timing),
        ("memory", memory),
    ]);
    let compiler = format!(
        "rustc\nrelease: {}\n",
        gate_build::pinned_release(root()).unwrap()
    );
    let identity = Value::object([
        ("source_revision", Value::str("pin")),
        ("binaries", hashes),
        ("rustc", Value::str(compiler)),
        ("build_environment", Value::Object(Vec::new())),
        ("cargo_configuration", Value::Array(Vec::new())),
        (
            "instrumentation",
            v(r#"{"probe_sha256": "probe-hash", "observer_sha256": "observer-hash"}"#),
        ),
    ]);
    (contract, vec![row], identity)
}

fn verdict(contract: &Value, rows: &[Value], identity: &Value) -> (bool, bool) {
    let result = acceptance(
        root(),
        contract,
        rows,
        identity,
        "host",
        &[v(r#"{"status": "pass"}"#)],
        &[],
    );
    (
        result.get("accepted") == Some(&Value::Bool(true)),
        result.get("slice_accepted") == Some(&Value::Bool(true)),
    )
}

fn four_field_gate_fixture() -> (Value, Vec<Value>, Value) {
    let (mut contract, mut rows, identity) = matched_gate_fixture();
    put(
        &mut contract,
        &[],
        "work_fields",
        Value::strings(["packets", "bytes", "digest", "digest_b"]),
    );
    let mut timed = rows[0].get("timing").unwrap().clone();
    let mut samples = timed
        .get("samples")
        .and_then(Value::as_array)
        .unwrap()
        .to_vec();
    for sample in &mut samples {
        for role in ["candidate", "reference"] {
            put(sample, &[role, "data"], "digest_b", Value::int(4));
        }
    }
    let warmups = ["candidate", "reference"].map(|role| {
        Value::object([
            ("worker", Value::str(role)),
            ("result", samples[0].get(role).unwrap().clone()),
        ])
    });
    put(&mut timed, &[], "samples", Value::Array(samples));
    put(&mut timed, &[], "warmups", Value::Array(warmups.into()));
    put(&mut rows[0], &[], "timing", timed);
    let mut samples = rows[0]
        .get("memory")
        .unwrap()
        .get("samples")
        .and_then(Value::as_array)
        .unwrap()
        .to_vec();
    for sample in &mut samples {
        for role in [
            "candidate_heap",
            "candidate_rss",
            "reference_heap",
            "reference_rss",
        ] {
            put(sample, &[role, "work"], "digest_b", Value::int(4));
        }
    }
    put(&mut rows[0], &["memory"], "samples", Value::Array(samples));
    put(
        &mut rows[0],
        &["parity"],
        "expected",
        v(r#"{"packets":10,"bytes":100,"digest":9,"digest_b":4}"#),
    );
    for role in ["candidate", "reference"] {
        let expected = rows[0]
            .get("parity")
            .unwrap()
            .get("expected")
            .unwrap()
            .clone();
        put(&mut rows[0], &["parity"], role, expected);
    }
    (contract, rows, identity)
}

#[test]
fn declared_work_fields_are_required_in_timing_warmups_samples_and_memory() {
    let (contract, rows, identity) = four_field_gate_fixture();
    assert_eq!(verdict(&contract, &rows, &identity), (true, true));
    for (section, collection) in [
        ("timing", "samples"),
        ("timing", "warmups"),
        ("memory", "samples"),
    ] {
        for role_set in [
            vec!["candidate"],
            vec!["reference"],
            vec!["candidate", "reference"],
        ] {
            for bad in [Some(Value::int(5)), None] {
                let mut changed = rows.clone();
                let mut values = changed[0]
                    .get(section)
                    .unwrap()
                    .get(collection)
                    .and_then(Value::as_array)
                    .unwrap()
                    .to_vec();
                for value in &mut values {
                    if collection == "warmups" {
                        if role_set.contains(&value.get("worker").and_then(Value::as_str).unwrap())
                        {
                            let mut data =
                                value.get("result").unwrap().get("data").unwrap().clone();
                            remove(&mut data, "digest_b");
                            if let Some(bad) = &bad {
                                put(&mut data, &[], "digest_b", bad.clone());
                            }
                            put(value, &["result"], "data", data);
                        }
                    } else {
                        let roles = if section == "memory" {
                            vec![
                                "candidate_heap",
                                "candidate_rss",
                                "reference_heap",
                                "reference_rss",
                            ]
                        } else {
                            vec!["candidate", "reference"]
                        };
                        for role in roles {
                            let worker_role = role.split('_').next().unwrap();
                            if role_set.contains(&worker_role) {
                                let key = if section == "memory" { "work" } else { "data" };
                                let mut data = value.get(role).unwrap().get(key).unwrap().clone();
                                remove(&mut data, "digest_b");
                                if let Some(bad) = &bad {
                                    put(&mut data, &[], "digest_b", bad.clone());
                                }
                                put(value, &[role], key, data);
                            }
                        }
                    }
                }
                put(
                    &mut changed[0],
                    &[section],
                    collection,
                    Value::Array(values),
                );
                assert_eq!(
                    verdict(&contract, &changed, &identity),
                    (false, false),
                    "{section}/{collection} {role_set:?} {bad:?}"
                );
            }
        }
    }
    let mut changed = rows;
    let mut timing = changed[0].get("timing").unwrap().clone();
    remove(&mut timing, "warmups");
    put(&mut changed[0], &[], "timing", timing);
    assert_eq!(verdict(&contract, &changed, &identity), (false, false));
}

#[test]
fn acceptance_rechecks_declared_parity_fields_even_when_status_says_pass() {
    let (contract, rows, identity) = four_field_gate_fixture();
    assert_eq!(verdict(&contract, &rows, &identity), (true, true));
    for bad in [Some(Value::int(5)), None] {
        for roles in [
            vec!["candidate"],
            vec!["reference"],
            vec!["candidate", "reference"],
            vec!["candidate", "reference", "expected"],
        ] {
            let mut changed = rows.clone();
            for role in roles {
                let mut work = changed[0].get("parity").unwrap().get(role).unwrap().clone();
                remove(&mut work, "digest_b");
                if let Some(bad) = &bad {
                    put(&mut work, &[], "digest_b", bad.clone());
                }
                put(&mut changed[0], &["parity"], role, work);
            }
            // Changing the expectation as well also invalidates the unchanged
            // memory work, which remains bound to the declared parity work.
            assert_eq!(verdict(&contract, &changed, &identity), (false, false));
        }
    }
}

#[test]
fn shared_acceptance_requires_the_declared_iteration_count() {
    let (mut contract, rows, identity) = matched_gate_fixture();
    put(&mut contract, &[], "iterations", Value::int(50));
    for (iterations, accepted) in [(50, true), (1, false), (100, false)] {
        let mut row = rows[0].clone();
        let mut timing = row.get("timing").unwrap().clone();
        let mut samples = timing.get("samples").unwrap().as_array().unwrap().to_vec();
        for sample in &mut samples {
            for role in ["candidate", "reference"] {
                put(
                    sample,
                    &[role, "data"],
                    "iterations",
                    Value::int(iterations),
                );
            }
        }
        put(&mut timing, &[], "iterations", Value::int(iterations));
        put(&mut timing, &[], "samples", Value::Array(samples));
        put(&mut row, &[], "timing", timing);
        assert_eq!(verdict(&contract, &[row], &identity), (accepted, accepted));
    }
}

#[test]
fn empty_or_missing_evidence_cannot_pass() {
    let result = acceptance(
        root(),
        &load("harness/features/matroska-demux/contract.json"),
        &[],
        &v("{}"),
        "aarch64-apple-darwin",
        &[],
        &[],
    );
    assert_eq!(result.get("accepted"), Some(&Value::Bool(false)));
    assert!(result.get("missing_profiles").is_some_and(Value::truthy));
}

#[test]
fn payload_time_key_and_unknowns_are_not_normalised_away() {
    let left = v(r#"{"payload": "01", "pts_ns": -1, "dts_ns": null, "key": false}"#);
    let right = v(r#"{"payload": "02", "pts_ns": 0, "dts_ns": 0, "key": true}"#);
    assert_eq!(differences(&left, &right).len(), 4);
    assert!(!differences(&v(r#"{"a": false}"#), &v(r#"{"a": 0}"#)).is_empty());
    assert!(!differences(&v("[1]"), &v("[]")).is_empty());
}

#[test]
fn a_fast_run_does_not_override_correctness() {
    let rows = [v(
        r#"{"id": "fast", "profile": "avc-aac-sub", "parity": {"status": "mismatch"},
                     "timing": {"status": "measured", "pairs": 30, "ratio": {"upper_95": 0.1}, "contaminants": []}}"#,
    )];
    let result = acceptance(
        root(),
        &load("harness/features/matroska-demux/contract.json"),
        &rows,
        &v("{}"),
        "aarch64-apple-darwin",
        &[],
        &[],
    );
    assert_eq!(result.get("accepted"), Some(&Value::Bool(false)));
}

#[test]
fn idle_process_names_are_not_active_work_but_any_cpu_advance_is() {
    let before = v(r#"{"1": {"name": "ffmpeg", "cpu_ticks": 100, "state": "S"}}"#);
    assert!(active_cpu(&before, &before, false).is_empty());
    assert_eq!(
        active_cpu(
            &before,
            &v(r#"{"1": {"name": "ffmpeg", "cpu_ticks": 101, "state": "S"}}"#),
            false
        ),
        ["ffmpeg"]
    );
    assert_eq!(
        active_cpu(
            &before,
            &v(r#"{"1": {"name": "ffmpeg", "cpu_ticks": 100, "state": "R"}}"#),
            false
        ),
        ["ffmpeg"]
    );
    assert_eq!(
        active_cpu(&v("{}"), &v("{}"), true),
        ["linux-cpu-evidence-missing"]
    );
}

#[test]
fn non_finite_or_incomplete_measurements_never_satisfy_a_gate() {
    for ratio in ["NaN", "Infinity", "0", "-1"] {
        let row = v(&format!(
            r#"{{"id": "bad", "profile": "avc-aac-sub", "parity": {{"status": "pass"}},
            "timing": {{"status": "measured", "pairs": 30, "ratio": {{"upper_95": {ratio}}}}},
            "memory": {{"status": "qualified", "total_rss_ratio_upper_95": {ratio}, "heap_ratio_upper_95": {ratio}}}}}"#
        ));
        let result = acceptance(
            root(),
            &load("harness/features/matroska-demux/contract.json"),
            &[row],
            &v(r#"{"source_revision": "pin"}"#),
            "aarch64-apple-darwin",
            &[v(r#"{"status": "pass"}"#)],
            &[],
        );
        let reasons: Vec<&str> = result
            .get("reasons")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(reasons.iter().any(|r| r.contains("runtime gate")));
        assert!(reasons.iter().any(|r| r.contains("allocation gate")));
    }
}

#[test]
fn complete_matched_evidence_can_pass_and_summary_tampering_fails() {
    let (mut contract, mut rows, mut identity) = matched_gate_fixture();
    assert_eq!(verdict(&contract, &rows, &identity), (true, true));
    put(&mut identity, &[], "schema", Value::int(2));
    assert!(
        !verdict(&contract, &rows, &identity).1,
        "schema 2 needs memory CPU evidence"
    );
    for key in ["cpu_before", "cpu_after"] {
        put(&mut rows[0], &["memory"], key, v("{}"));
    }
    put(&mut rows[0], &["memory"], "contaminants", v("[]"));
    assert!(verdict(&contract, &rows, &identity).1);
    put(
        &mut rows[0],
        &["memory"],
        "cpu_after",
        v(r#"{"1": {"name": "rustc", "cpu_ticks": 1, "state": "R"}}"#),
    );
    assert!(!verdict(&contract, &rows, &identity).1);
    put(&mut rows[0], &["memory"], "cpu_after", v("{}"));
    put(
        &mut identity,
        &["instrumentation"],
        "probe_sha256",
        Value::str("rebuilt-probe"),
    );
    assert!(!verdict(&contract, &rows, &identity).1);
    put(
        &mut identity,
        &["instrumentation"],
        "probe_sha256",
        Value::str("probe-hash"),
    );
    put(
        &mut rows[0],
        &["timing", "worker_hashes"],
        "reference",
        Value::str("a-different-reference"),
    );
    assert!(!verdict(&contract, &rows, &identity).1);
    put(
        &mut rows[0],
        &["timing", "worker_hashes"],
        "reference",
        Value::str("reference-hash"),
    );
    put(
        &mut contract,
        &[],
        "pending_module_requirements",
        Value::strings(["future module work"]),
    );
    assert_eq!(verdict(&contract, &rows, &identity), (false, true));
    put(&mut contract, &[], "pending_module_requirements", v("[]"));
    put(
        &mut rows[0],
        &["timing", "ratio"],
        "upper_95",
        Value::float(0.1),
    );
    assert!(!verdict(&contract, &rows, &identity).0);
    put(
        &mut rows[0],
        &["timing"],
        "ratio",
        interval(&[(79.0, 100.0); 30]),
    );
    put(
        &mut rows[0],
        &["memory"],
        "heap_ratio_upper_95",
        Value::float(0.1),
    );
    assert!(!verdict(&contract, &rows, &identity).1);
}

#[test]
fn per_worker_memory_outputs_reuse_the_same_observation_validation() {
    let (contract, rows, identity) = matched_gate_fixture();
    let selected = identity.get("binaries").unwrap();
    let recorded = rows[0].get("memory").unwrap();
    let certificate = recorded.get("certificate").unwrap();
    let expected = v(r#"{
        "candidate":{"packets":10,"bytes":100,"digest":9},
        "reference":{"packets":10,"bytes":100,"digest":9,"metadata":"reference-only"}
    }"#);
    let mut samples = recorded
        .get("samples")
        .unwrap()
        .as_array()
        .unwrap()
        .to_vec();
    for sample in &mut samples {
        for role in ["candidate", "reference"] {
            for mode in ["heap", "rss"] {
                put(
                    sample,
                    &[&format!("{role}_{mode}")],
                    "work",
                    expected.get(role).unwrap().clone(),
                );
            }
        }
    }
    let measured = super::memory::evaluate(root(), &samples, 30, 0, certificate, Some(&expected));
    let valid = |memory: &Value, outputs: &Value| {
        super::memory::evidence_valid_with_outputs(
            root(),
            &rows[0],
            memory,
            &contract,
            &identity,
            selected,
            false,
            Some(outputs),
        )
    };
    assert!(valid(&measured, &expected));
    assert!(!super::memory::evidence_valid(
        root(),
        &rows[0],
        &measured,
        &contract,
        &identity,
        selected,
        false,
    ));
    let mut wrong = expected.clone();
    put(
        &mut wrong,
        &["reference"],
        "metadata",
        Value::str("changed"),
    );
    assert!(!valid(&measured, &wrong));
    let mut changed = measured.clone();
    put(
        &mut changed,
        &["certificate"],
        "worker_hashes",
        Value::Object(vec![]),
    );
    assert!(!valid(&changed, &expected));
    let mut changed = measured;
    put(&mut changed, &["heap_ratio"], "upper_95", Value::float(0.0));
    assert!(!valid(&changed, &expected));
}

#[test]
fn pair_identity_and_iteration_counts_bind_the_timing_samples() {
    let fixture = matched_gate_fixture();
    let edit = |change: &dyn Fn(&mut Value)| {
        let (contract, mut rows, identity) = matched_gate_fixture();
        change(&mut rows[0]);
        verdict(&contract, &rows, &identity).1
    };
    assert!(verdict(&fixture.0, &fixture.1, &fixture.2).1);
    for value in [None, Some("0"), Some("false"), Some("0.0")] {
        assert!(!edit(&|row| {
            let Some(Value::Array(samples)) =
                row.get("timing").and_then(|t| t.get("samples")).cloned()
            else {
                panic!()
            };
            let changed: Vec<Value> = samples
                .into_iter()
                .map(|mut s| {
                    match value {
                        None => remove(&mut s, "pair"),
                        Some(text) => put(&mut s, &[], "pair", v(text)),
                    }
                    s
                })
                .collect();
            put(row, &["timing"], "samples", Value::Array(changed));
        }));
    }
    for value in [None, Some("0"), Some("true"), Some("100.0")] {
        assert!(!edit(&|row| match value {
            None => {
                let mut timing = row.get("timing").cloned().unwrap();
                remove(&mut timing, "iterations");
                put(row, &[], "timing", timing);
            }
            Some(text) => put(row, &["timing"], "iterations", v(text)),
        }));
    }
}

fn comparison_identity() -> Value {
    let registry = load("harness/comparison-rules.json");
    Value::object([
        (
            "source_revision",
            registry.get("reference_source_revision").cloned().unwrap(),
        ),
        ("comparison_rules", registry),
        (
            "binaries",
            v(r#"{"candidate": "rust", "reference": "native"}"#),
        ),
        (
            "comparison_references",
            v(
                r#"{"ffmpeg-native": {"path": "target/harness/ffmpeg-worker", "sha256": "native", "kind": "pinned native FFmpeg"},
                  "ffmpeg-combined-native": {"path": "target/harness/ffmpeg-combined-worker", "sha256": "combined", "kind": "explicit combined reference"}}"#,
            ),
        ),
    ])
}

fn mixed_case() -> Value {
    load("harness/features/matroska-zlib/corpus.json")
        .get("cases")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .find(|c| c.get("id").and_then(Value::as_str) == Some("zlib-mixed"))
        .cloned()
        .unwrap()
}

#[test]
fn shared_operation_selection_covers_every_registered_corpus_case() {
    let mut identity = comparison_identity();
    for name in ["ffmpeg-video-native", "ffmpeg-formats-native"] {
        put(
            &mut identity,
            &["comparison_references"],
            name,
            v(
                r#"{"path":"target/harness/selected-worker","sha256":"selected","kind":"selected reference"}"#,
            ),
        );
    }
    let mut checked = 0;
    for (feature, view, candidate, reference) in [
        (
            "matroska-demux",
            "source-frames-ffmpeg9-clock-v1",
            None,
            None,
        ),
        (
            "matroska-header-removal",
            "source-frames-ffmpeg9-clock-v1",
            None,
            None,
        ),
        (
            "matroska-zlib",
            "source-frames-ffmpeg9-clock-v1",
            None,
            None,
        ),
        (
            "matroska-cues-preroll",
            "source-frames-ffmpeg9-clock-v1",
            None,
            None,
        ),
        (
            "matroska-colour",
            "colour-source-and-ffmpeg9-v1",
            None,
            None,
        ),
        (
            "matroska-segment-metadata",
            "catalogue-source-and-native-v1",
            None,
            None,
        ),
        (
            "matroska-startup-seek",
            "playback-feed-fold-v1",
            Some("playback-worker"),
            Some("ffmpeg-video-native"),
        ),
        (
            "hls-segments",
            "playback-hls-fold-v1",
            Some("playback-worker"),
            Some("ffmpeg-formats-native"),
        ),
        (
            "matroska-audio-clocks",
            "playback-mkv-audio-fold-v1",
            Some("playback-worker"),
            Some("ffmpeg-video-native"),
        ),
        (
            "mp4-forward",
            "playback-mp4-forward-fold-v1",
            Some("playback-worker"),
            Some("ffmpeg-formats-native"),
        ),
        (
            "ts-program",
            "playback-ts-program-fold-v1",
            Some("playback-worker"),
            Some("ffmpeg-formats-native"),
        ),
        (
            "raw-annex-b",
            "playback-raw-video-fold-v1",
            Some("playback-worker"),
            Some("ffmpeg-formats-native"),
        ),
        (
            "subtitles-pgs",
            "playback-pgs-fold-v1",
            Some("playback-worker"),
            Some("ffmpeg-formats-native"),
        ),
        (
            "subtitles-text",
            "playback-subtitle-text-fold-v1",
            Some("playback-worker"),
            Some("ffmpeg-formats-native"),
        ),
    ] {
        let contract = load(&format!("harness/features/{feature}/contract.json"));
        let manifest = load(&format!("harness/features/{feature}/corpus.json"));
        for case in manifest.get("cases").unwrap().as_array().unwrap() {
            let selected =
                super::validation::selected_operation(case, &contract, &identity).unwrap();
            let id = case.get("id").unwrap().as_str().unwrap();
            let reviewed = matches!(id, "zlib-empty" | "zlib-mixed");
            assert_eq!(selected.rule.is_some(), reviewed, "{id}");
            assert_eq!(selected.candidate_worker, candidate, "{id}");
            assert_eq!(selected.reference_worker, reference, "{id}");
            assert_eq!(
                selected.reference.get("id").and_then(Value::as_str),
                Some(if id == "zlib-mixed" {
                    "ffmpeg-combined-native"
                } else {
                    reference.unwrap_or("ffmpeg-native")
                }),
                "{id}"
            );
            let operation = if candidate.is_some() {
                Value::object([
                    ("name", contract.get("operation").unwrap().clone()),
                    ("view", Value::str(view)),
                ])
            } else {
                let seek = case.get("seek_target_ns").unwrap_or(&Value::Null);
                Value::object([
                    (
                        "name",
                        Value::str(if *seek == Value::Null {
                            "container-demux-v1"
                        } else {
                            "container-seek-preroll-v1"
                        }),
                    ),
                    ("seek_target_ns", seek.clone()),
                    (
                        "view",
                        Value::str(if reviewed {
                            "optional-pcm-decoder-packets-v1"
                        } else {
                            view
                        }),
                    ),
                ])
            };
            assert!(
                differences(&selected.operation, &operation).is_empty(),
                "{id}"
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 823);
    let mut contract = load("harness/features/matroska-zlib/contract.json");
    put(&mut contract, &[], "reference", Value::str("not-built"));
    assert!(super::validation::selected_operation(&mixed_case(), &contract, &identity).is_ok());
}

#[test]
fn shared_operation_selection_rejects_missing_operation_and_reviewed_view() {
    let identity = comparison_identity();
    let mut contract = v(r#"{"id":"generic-feed","worker_operation":"feed-","operation":"feed"}"#);
    let case = v(r#"{"id":"authored","profile":"authored","sha256":"input"}"#);
    assert!(super::validation::selected_operation(&case, &contract, &identity).is_ok());
    for value in [None, Some(Value::Null)] {
        remove(&mut contract, "operation");
        if let Some(value) = value {
            put(&mut contract, &[], "operation", value);
        }
        assert_eq!(
            super::validation::selected_operation(&case, &contract, &identity)
                .err()
                .as_deref(),
            Some("missing operation")
        );
    }

    let contract = load("harness/features/matroska-zlib/contract.json");
    let case = mixed_case();
    for value in [None, Some(Value::Null)] {
        let mut changed = identity.clone();
        let mut rules = changed
            .get("comparison_rules")
            .unwrap()
            .get("rules")
            .unwrap()
            .as_array()
            .unwrap()
            .to_vec();
        let rule = rules
            .iter_mut()
            .find(|r| r.get("case") == case.get("id"))
            .unwrap();
        remove(rule, "view");
        if let Some(value) = value {
            put(rule, &[], "view", value);
        }
        put(
            &mut changed,
            &["comparison_rules"],
            "rules",
            Value::Array(rules),
        );
        assert_eq!(
            super::validation::selected_operation(&case, &contract, &changed)
                .err()
                .as_deref(),
            Some("missing view")
        );
    }
}

#[test]
fn external_contract_derivation_preserves_registered_gates_and_requirements() {
    let contract = load("harness/features/matroska-segment-metadata/contract.json");
    let spec = &contract
        .get("required_additional_vectors")
        .unwrap()
        .as_array()
        .unwrap()[0];
    let file = spec.get("file").unwrap().as_str().unwrap();
    let derived = super::validation::external_contract(&contract, file).unwrap();
    assert_eq!(
        derived.get("required_profiles"),
        Some(&Value::strings([file]))
    );
    assert_eq!(
        derived.get("required_additional_vectors"),
        Some(&Value::Array(vec![]))
    );
    let Value::Object(fields) = &contract else {
        unreachable!()
    };
    for (key, value) in fields {
        if !matches!(
            key.as_str(),
            "required_profiles" | "required_additional_vectors"
        ) {
            assert_eq!(derived.get(key), Some(value), "{key}");
        }
    }
    assert!(super::validation::external_contract(&Value::Null, file).is_err());
}

#[test]
fn kernel_measurement_selection_checks_certificate_and_each_rss_role() {
    let standard = v(r#"{"kernel_rss_required":false}"#);
    let mut samples = v(r#"[
        {"candidate_rss":{"observer":{"kernel_rss_requested":0}}},
        {"reference_rss":{"observer":{"kernel_rss_requested":0}}}
    ]"#);
    let used = |certificate: &Value, samples: &Value| {
        super::memory::kernel_used(certificate, samples.as_array().unwrap())
    };
    assert!(!used(&standard, &samples));
    assert!(used(&v(r#"{"kernel_rss_required":true}"#), &samples));
    for (index, role) in [(0, "candidate_rss"), (1, "reference_rss")] {
        let Value::Array(rows) = &mut samples else {
            unreachable!()
        };
        put(
            &mut rows[index],
            &[role, "observer"],
            "kernel_rss_requested",
            Value::int(1),
        );
        assert!(used(&standard, &samples));
        let Value::Array(rows) = &mut samples else {
            unreachable!()
        };
        put(
            &mut rows[index],
            &[role, "observer"],
            "kernel_rss_requested",
            Value::int(0),
        );
    }
    assert!(!used(
        &standard,
        &v(r#"[null,{"candidate_heap":{"observer":{"kernel_rss_requested":1}}}]"#)
    ));
}

const ZLIB: Option<&str> = Some("matroska-zlib");

#[test]
fn a_comparison_rule_is_bound_to_approval_source_input_profile_and_contract() {
    let mut identity = comparison_identity();
    let case = mixed_case();
    let rule = rule_for(&case, &identity, ZLIB).unwrap();
    assert_eq!(
        reference_record(&identity, rule)
            .unwrap()
            .get("sha256")
            .and_then(Value::as_str),
        Some("combined")
    );
    for (field, value) in [
        ("sha256", Value::str("changed")),
        ("profile", Value::str("another")),
        ("seek_target_ns", Value::int(1)),
    ] {
        let mut changed = case.clone();
        put(&mut changed, &[], field, value);
        assert!(rule_for(&changed, &identity, ZLIB).is_err(), "{field}");
    }
    for contract in [None, Some("matroska-cues-preroll")] {
        assert!(rule_for(&case, &identity, contract).is_err());
    }
    put(
        &mut identity,
        &["comparison_rules", "approval"],
        "status",
        Value::str("pending"),
    );
    assert!(rule_for(&case, &identity, ZLIB).is_err());
    put(
        &mut identity,
        &["comparison_rules", "approval"],
        "status",
        Value::str("approved"),
    );
    put(&mut identity, &[], "source_revision", Value::str("changed"));
    assert!(rule_for(&case, &identity, ZLIB).is_err());
}

#[test]
fn the_selected_worker_and_complete_source_proof_cannot_be_substituted() {
    let identity = comparison_identity();
    let case = mixed_case();
    let rule = rule_for(&case, &identity, ZLIB).unwrap().unwrap().clone();
    let field = |k: &str| rule.get(k).cloned().unwrap();
    let mut row = Value::object([
        ("id", case.get("id").cloned().unwrap()),
        ("profile", case.get("profile").cloned().unwrap()),
        ("input_sha256", case.get("sha256").cloned().unwrap()),
        (
            "reference",
            reference_summary(&reference_record(&identity, Some(&rule)).unwrap()),
        ),
        ("operation", Value::object([("view", field("view"))])),
        (
            "parity",
            Value::object([
                ("comparison_rule", field("id")),
                ("source_content_status", Value::str("pass")),
                (
                    "raw_native_disposition",
                    Value::object([("status", Value::str("reviewed")), ("rule", field("id"))]),
                ),
                (
                    "raw_native_output_sha256",
                    field("raw_native_output_sha256"),
                ),
            ]),
        ),
    ]);
    let hashes = validate_case_evidence(&row, &identity, ZLIB).unwrap();
    assert_eq!(
        crate::json::to_python(&hashes, None),
        r#"{"candidate": "rust", "reference": "combined"}"#
    );
    for key in [
        "source_content_status",
        "raw_native_disposition",
        "comparison_rule",
        "raw_native_output_sha256",
    ] {
        let mut changed = row.clone();
        let mut parity = changed.get("parity").cloned().unwrap();
        remove(&mut parity, key);
        put(&mut changed, &[], "parity", parity);
        assert!(
            validate_case_evidence(&changed, &identity, ZLIB).is_err(),
            "{key}"
        );
    }
    put(&mut row, &["reference"], "sha256", Value::str("native"));
    assert!(validate_case_evidence(&row, &identity, ZLIB).is_err());
}

fn reasons(result: &Value, key: &str) -> Vec<String> {
    result
        .get(key)
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .filter_map(|r| r.as_str().map(str::to_owned))
        .collect()
}

#[test]
fn missing_colour_or_catalogue_source_proof_cannot_satisfy_the_gate() {
    let colour = acceptance(
        root(),
        &load("harness/features/matroska-colour/contract.json"),
        &[v(
            r#"{"id": "colour-absent", "profile": "colour-absent", "parity": {"status": "pass", "source_timing_status": "pass"}}"#,
        )],
        &v("{}"),
        "aarch64-apple-darwin",
        &[],
        &[],
    );
    assert!(
        reasons(&colour, "slice_reasons")
            .iter()
            .any(|r| r.contains("complete source/native colour gate"))
    );
    let catalogue = acceptance(
        root(),
        &load("harness/features/matroska-segment-metadata/contract.json"),
        &[v(
            r#"{"id": "catalogue-empty", "profile": "catalogue-empty", "parity": {"status": "pass", "source_timing_status": "pass"}}"#,
        )],
        &v("{}"),
        "aarch64-apple-darwin",
        &[],
        &[],
    );
    assert!(
        reasons(&catalogue, "slice_reasons")
            .iter()
            .any(|r| r.contains("complete source/native catalogue gate"))
    );
}

#[test]
fn the_required_external_vector_cannot_be_missing_or_substituted() {
    let contract = load("harness/features/matroska-segment-metadata/contract.json");
    let file = contract
        .get("required_additional_vectors")
        .and_then(Value::as_array)
        .unwrap()[0]
        .get("file")
        .cloned()
        .unwrap();
    let substituted = Value::object([
        ("id", file.clone()),
        ("profile", file),
        ("input_sha256", Value::str("substituted")),
    ]);
    for additional in [vec![], vec![substituted]] {
        let verdict = acceptance(
            root(),
            &contract,
            &[],
            &v("{}"),
            "aarch64-apple-darwin",
            &[],
            &additional,
        );
        assert!(
            reasons(&verdict, "slice_reasons")
                .iter()
                .any(|r| r.contains("required additional vector missing or changed"))
        );
    }
}

#[test]
fn no_contract_restates_the_policy_gates() {
    let features = root().join("harness/features");
    for entry in std::fs::read_dir(&features).unwrap() {
        let feature = entry.unwrap().file_name().into_string().unwrap();
        let contract = load(&format!("harness/features/{feature}/contract.json"));
        for key in [
            "runtime_ratio_max",
            "runtime_optimization_ratio_target",
            "runtime_policy",
            "memory_ratio_max_exclusive",
        ] {
            assert!(contract.get(key).is_none(), "{feature} restates {key}");
        }
    }
}

#[test]
fn the_delivery_ceiling_is_inclusive_and_does_not_replace_correctness_or_memory() {
    let set_elapsed = |rows: &mut Vec<Value>, elapsed: i64| {
        let Some(Value::Array(samples)) = rows[0]
            .get("timing")
            .and_then(|t| t.get("samples"))
            .cloned()
        else {
            panic!()
        };
        let samples: Vec<Value> = samples
            .into_iter()
            .map(|mut s| {
                put(
                    &mut s,
                    &["candidate", "data"],
                    "elapsed_ns",
                    Value::int(elapsed),
                );
                s
            })
            .collect();
        put(&mut rows[0], &["timing"], "samples", Value::Array(samples));
        put(
            &mut rows[0],
            &["timing"],
            "ratio",
            interval(&[(elapsed as f64, 100.0); 30]),
        );
    };
    let (contract, mut rows, identity) = matched_gate_fixture();
    let ceiling = (crate::harness::policy::Runtime::load(root())
        .unwrap()
        .ceiling
        * 100.0)
        .round() as i64;
    for (elapsed, passes) in [(ceiling, true), (ceiling + 1, false)] {
        set_elapsed(&mut rows, elapsed);
        assert_eq!(verdict(&contract, &rows, &identity).1, passes, "{elapsed}");
    }
    set_elapsed(&mut rows, ceiling - 1);
    put(&mut rows[0], &["parity"], "status", Value::str("mismatch"));
    assert!(!verdict(&contract, &rows, &identity).1);
    put(&mut rows[0], &["parity"], "status", Value::str("pass"));
    put(
        &mut rows[0],
        &["memory"],
        "heap_ratio_upper_95",
        Value::float(1.0),
    );
    assert!(!verdict(&contract, &rows, &identity).1);
}

#[test]
fn same_difference_paths_do_not_allow_changed_native_values() {
    let source = v(r#"{"packets": [{"payload": "02"}]}"#);
    // SHA-256 of the compact key-sorted JSON of the reviewed native output,
    // {"packets":[{"payload":"01"}]}, as Python's hashlib and json write it.
    let rule = v(
        r#"{"id": "test", "raw_native_difference_paths": ["/packets/0/payload"],
        "raw_native_output_sha256": "84100540687d0b5ca39405399357d4ea135b7e847c99739cdaff664a93038807"}"#,
    );
    let status = |native: &Value| {
        let (_, disposition, _) = review_native_difference(&source, native, &rule);
        disposition
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let native = v(r#"{"packets": [{"payload": "01"}]}"#);
    assert_eq!(status(&native).as_deref(), Some("reviewed"));
    let altered = v(r#"{"packets": [{"payload": "03"}]}"#);
    assert_eq!(
        status(&altered).as_deref(),
        Some("unexpected-native-difference")
    );
}

#[test]
fn a_report_off_the_gate_build_fails_both_verdicts_and_says_why() {
    let (contract, rows, identity) = matched_gate_fixture();
    assert_eq!(verdict(&contract, &rows, &identity), (true, true));
    let reasons = |identity: &Value| {
        let result = acceptance(
            root(),
            &contract,
            &rows,
            identity,
            "host",
            &[v(r#"{"status": "pass"}"#)],
            &[],
        );
        python_list(result.get("slice_reasons"))
    };
    let mut nightly = identity.clone();
    put(
        &mut nightly,
        &[],
        "rustc",
        Value::str("rustc\nrelease: 1.101.0-nightly\n"),
    );
    assert_eq!(verdict(&contract, &rows, &nightly), (false, false));
    assert!(reasons(&nightly)[0].contains("1.101.0-nightly"));

    let mut flagged = identity.clone();
    put(
        &mut flagged,
        &[],
        "build_environment",
        v(r#"{"RUSTFLAGS": "-C target-cpu=neoverse-v2"}"#),
    );
    assert_eq!(verdict(&contract, &rows, &flagged), (false, false));
    assert!(reasons(&flagged)[0].contains("RUSTFLAGS"));

    let mut silent = identity.clone();
    if let Value::Object(fields) = &mut silent {
        fields.retain(|(key, _)| key != "cargo_configuration");
    }
    assert_eq!(verdict(&contract, &rows, &silent), (false, false));
    assert!(reasons(&silent)[0].contains("no Cargo configuration"));
}

fn python_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}
