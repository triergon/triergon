use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

#[test]
fn only_the_listed_targets_qualify() {
    assert_eq!(
        qualified("thumbv7neon-unknown-linux-gnueabihf"),
        "thumbv7neon-unknown-linux-gnueabihf"
    );
    for other in [
        "arm-unknown-linux-gnueabihf",
        "armv6-unknown-linux-gnueabihf",
        "armv7-unknown-linux-gnueabihf",
        "aarch64-unknown-linux-musl",
    ] {
        assert_eq!(qualified(other), format!("unqualified-{other}"));
    }
    let host = gate_target().unwrap();
    assert!(
        QUALIFIED_TARGETS.contains(&host.as_str()) || host.starts_with("unqualified-"),
        "{host}"
    );
}

struct Output(PathBuf);

impl Output {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "triergon-case-report-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn measured_checkpoint_keeps_samples_separate_and_checks_their_digest() {
    let output = Output::new();
    fs::create_dir(output.0.join("measurements")).unwrap();
    let timing = json::parse_strict(
        r#"{"ratio":{"upper_95":1.01234567890123456789},"samples":[18446744073709551615]}"#,
    )
    .unwrap();
    let memory = json::parse_observations(
            r#"{"status":"invalid-memory-work","samples":[{"reference_rss":9007199254740993,"candidate_heap":NaN}]}"#,
        )
        .unwrap();
    let row = Value::object([
        ("id", Value::str("sample")),
        (
            "timing",
            store_measurement(&output.0, 0, "timing", &timing).unwrap(),
        ),
        (
            "memory",
            store_measurement(&output.0, 0, "memory", &memory).unwrap(),
        ),
    ]);
    let checkpoint = Value::object([
        ("collection_status", Value::str("partial")),
        ("case_storage", Value::str("sha256-files")),
        ("cases", Value::Array(vec![row.clone()])),
    ]);
    let path = output.0.join("partial.json");
    save_checkpoint(&path, &checkpoint).unwrap();
    let text = fs::read_to_string(path).unwrap();
    assert!(!text.contains("samples"));
    assert!(!output.0.join("report.json").exists());
    let expanded = validation::expand_measurements(&output.0, &row).unwrap();
    assert_eq!(
        json::canonical(expanded.get("timing").unwrap()),
        json::canonical(&timing)
    );
    let memory = json::observations(&memory);
    assert_eq!(json::observations(expanded.get("memory").unwrap()), memory);
    let inline = output.0.join("inline");
    create_output(&inline).unwrap();
    stream_report(
        &inline,
        &Value::Object(Vec::new()),
        [Ok(expanded)].into_iter(),
        false,
    )
    .unwrap();
    assert!(
        fs::read_to_string(inline.join("report.json"))
            .unwrap()
            .contains(&memory)
    );
    assert!(store_measurement(&output.0, 0, "memory", &timing).is_err());
    fs::write(output.0.join("measurements/0000-memory.json"), b"{}\n").unwrap();
    assert!(
        validation::expand_measurements(&output.0, &row)
            .unwrap_err()
            .contains("raw measurement changed")
    );
}

#[test]
fn case_report_preserves_raw_rows_and_number_precision() {
    let output = Output::new();
    let header = Value::object([
        (
            "z",
            Value::object([("b", Value::int(2)), ("a", Value::int(1))]),
        ),
        ("cases", Value::strings(["replace this value"])),
        ("a", Value::str("complete")),
    ]);
    let raw = [
        r#"{"id":"one","timing":{"samples":[{"digest":18446744073709551615,"elapsed_ns":9007199254740993,"ratio":0.123456789012345678901234567890}]}}"#,
        r#"{"id":"two","memory":{"samples":[{"low":-170141183460469231731687303715884105728,"bytes":170141183460469231731687303715884105727}]},"proof":{"valid":true,"source":"input"}}"#,
    ];
    write_case_report(
        &output.0,
        &header,
        raw.iter()
            .map(|text| json::parse_strict(text).map_err(|error| error.to_string())),
    )
    .unwrap();
    let expected = Value::object([
        ("z", header.get("z").unwrap().clone()),
        ("a", Value::str("complete")),
        (
            "cases",
            Value::Array(
                raw.iter()
                    .map(|text| json::parse_strict(text).unwrap())
                    .collect(),
            ),
        ),
    ]);
    let text = fs::read_to_string(output.0.join("report.json")).unwrap();
    assert_eq!(text, format!("{}\n", json::canonical(&expected)));
    for number in [
        "18446744073709551615",
        "9007199254740993",
        "0.123456789012345678901234567890",
        "-170141183460469231731687303715884105728",
    ] {
        assert!(text.contains(number));
    }
    assert!(!output.0.join("report.json.tmp").exists());
}

#[test]
fn case_report_iteration_failure_is_not_published() {
    let output = Output::new();
    let rows = [Ok(Value::int(1)), Err("raw case unavailable".to_owned())]
        .into_iter()
        .chain(std::iter::once_with(|| {
            panic!("iteration continued after error")
        }));
    let error = write_case_report(&output.0, &Value::Object(Vec::new()), rows).unwrap_err();
    assert!(error.contains("raw case unavailable"));
    assert!(!output.0.join("report.json").exists());
    assert!(!output.0.join("report.json.tmp").exists());
}

#[test]
fn parity_only_cannot_overwrite_a_measured_report() {
    let parent = Output::new();
    let output = parent.0.join("run");
    create_output(&output).unwrap();
    let path = output.join("partial.json");
    fs::write(&path, "measurement checkpoint").unwrap();
    let options = Options {
        output,
        ..Options::defaults(&parent.0)
    };
    assert!(
        run(&parent.0, &options)
            .unwrap_err()
            .contains("use a new output directory")
    );
    assert_eq!(fs::read_to_string(path).unwrap(), "measurement checkpoint");
}

#[test]
fn case_report_keeps_existing_destination() {
    let output = Output::new();
    let destination = output.0.join("report.json");
    fs::write(&destination, b"existing report\n").unwrap();
    assert!(
        write_case_report(
            &output.0,
            &Value::object([("schema", Value::int(1))]),
            std::iter::once(Ok(Value::int(2))),
        )
        .is_err()
    );
    assert_eq!(fs::read(&destination).unwrap(), b"existing report\n");
    assert!(!output.0.join("report.json.tmp").exists());
}

#[test]
fn complete_json_keeps_existing_destination() {
    let output = Output::new();
    let destination = output.0.join("report.json");
    fs::write(&destination, b"existing grade\n").unwrap();
    assert!(publish_json(&destination, "{}\n").is_err());
    assert_eq!(fs::read(&destination).unwrap(), b"existing grade\n");
    assert!(!output.0.join("report.json.tmp").exists());
}

#[test]
fn case_output_keeps_existing_nested_traces() {
    let output = Output::new();
    let nested = output.0.join("nested/case");
    create_output(&nested).unwrap();
    let trace = nested.join("progressive-fold.json");
    fs::write(&trace, b"existing trace\n").unwrap();
    for id in ["nested/case", "../outside"] {
        let case = Value::object([("id", Value::str(id))]);
        assert!(reserve_case_outputs(&output.0, &[case]).is_err());
    }
    assert_eq!(fs::read(&trace).unwrap(), b"existing trace\n");
}

#[test]
fn case_outputs_reserve_nested_cases_in_either_order() {
    for child in ["nested/child", "Nested/child"] {
        for ids in [[child, "nested"], ["nested", child]] {
            let output = Output::new();
            let cases = ids.map(|id| Value::object([("id", Value::str(id))]));
            reserve_case_outputs(&output.0, &cases).unwrap();
            assert!(output.0.join(child).is_dir());
        }
    }
}

#[test]
fn case_report_keeps_existing_temporary_file() {
    let output = Output::new();
    let temporary = output.0.join("report.json.tmp");
    fs::write(&temporary, b"in progress\n").unwrap();
    assert!(
        write_case_report(
            &output.0,
            &Value::object([("schema", Value::int(1))]),
            std::iter::empty(),
        )
        .is_err()
    );
    assert_eq!(fs::read(&temporary).unwrap(), b"in progress\n");
    assert!(!output.0.join("report.json").exists());
}

#[test]
fn final_validation_failures_are_written_in_both_report_forms() {
    let output = Output::new();
    let mut proof = Value::object([
        ("passed", Value::Bool(true)),
        ("path", Value::str("controls/report.json")),
    ]);
    reject_controls(&mut proof, "functional control proof hash mismatch").unwrap();
    let header = Value::object([
        ("functional_controls", proof),
        (
            "summary",
            Value::object([("source_and_workers_unchanged", Value::Bool(false))]),
        ),
    ]);
    publish_measurements(&output.0, header, &[Value::int(1)], |case| Ok(case.clone())).unwrap();
    for name in ["collection.json", "report.json"] {
        let report = json::read(&output.0.join(name)).unwrap();
        assert_eq!(
            report
                .get("summary")
                .and_then(|summary| summary.get("source_and_workers_unchanged")),
            Some(&Value::Bool(false))
        );
        let proof = report.get("functional_controls").unwrap();
        assert_eq!(proof.get("passed"), Some(&Value::Bool(false)));
        assert_eq!(
            s(proof, "error"),
            Some("functional control proof hash mismatch")
        );
    }
}
