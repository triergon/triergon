//! Gate behaviour: missing, tampered or contaminated evidence never passes.

use super::acceptance::{acceptance, active_cpu};
use super::comparison::{reference_record, reference_summary, rule_for, validate_case_evidence};
use super::diff::differences;
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
                         "runtime_ratio_max": 0.8, "pending_module_requirements": []}"#,
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
            r#"{{"status": "measured", "instrument_sha256": "observer-hash", "work": {{"packets": 10}},
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
    let memory = super::memory::evaluate(root(), &samples, 30, 0, &certificate);
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
    let identity = Value::object([
        ("source_revision", Value::str("pin")),
        ("binaries", hashes),
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
            "memory": {{"status": "qualified", "rss_ratio_upper_95": {ratio}, "heap_ratio_upper_95": {ratio}}}}}"#
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
fn every_contract_keeps_the_delivery_ceiling_and_optimisation_target() {
    for prefix in [
        "matroska-demux",
        "matroska-header-removal",
        "matroska-zlib",
        "matroska-cues-preroll",
        "matroska-colour",
        "matroska-segment-metadata",
    ] {
        let contract = load(&format!("harness/features/{prefix}/contract.json"));
        assert_eq!(
            contract.get("runtime_ratio_max").and_then(Value::as_f64),
            Some(1.2),
            "{prefix}"
        );
        assert_eq!(
            contract
                .get("runtime_optimization_ratio_target")
                .and_then(Value::as_f64),
            Some(0.8),
            "{prefix}"
        );
        assert_eq!(
            contract.get("runtime_policy").and_then(Value::as_str),
            Some("runtime-ceiling-1.20"),
            "{prefix}"
        );
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
    let (mut contract, mut rows, identity) = matched_gate_fixture();
    put(&mut contract, &[], "runtime_ratio_max", Value::float(1.2));
    for (elapsed, passes) in [(119, true), (121, false)] {
        set_elapsed(&mut rows, elapsed);
        assert_eq!(verdict(&contract, &rows, &identity).1, passes, "{elapsed}");
    }
    set_elapsed(&mut rows, 119);
    let mut strict = contract.clone();
    put(&mut strict, &[], "runtime_ratio_max", Value::float(0.8));
    assert!(
        !verdict(&strict, &rows, &identity).1,
        "the optimisation target is stricter"
    );
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
