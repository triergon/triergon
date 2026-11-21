use super::*;
use std::cell::RefCell;

#[test]
fn prefix_traces_require_the_reference_and_secondary_input_pins() {
    let root = crate::walk::scratch("prefix-traces");
    let corpus = root.join("relocated");
    fs::create_dir(&corpus).unwrap();
    fs::write(root.join("reference"), b"reference").unwrap();
    fs::write(corpus.join("first"), b"first").unwrap();
    let input = crate::sha256::hex(b"first");
    let secondary = crate::sha256::hex(b"original");
    let identity = |pin: &str| {
        crate::json::parse(&format!(
                r#"{{"comparison_references":{{"reference":{{"path":"reference","sha256":"{pin}"}}}}}}"#,
            ))
            .unwrap()
    };
    for (prefix, inputs, mismatch) in [
        (
            "ts-",
            format!(
                r#""arguments":["188","1","next"],"entities":[{{"file":"first","sha256":"{input}"}},{{"file":"next","sha256":"{secondary}"}}]"#,
            ),
            "entity-hash-mismatch",
        ),
        (
            "sub-",
            format!(r#""companions":[{{"file":"next","sha256":"{secondary}"}}]"#),
            "companion-hash-mismatch",
        ),
    ] {
        let case = crate::json::parse(&format!(
            r#"{{"id":"case","file":"first","sha256":"{input}",{inputs}}}"#,
        ))
        .unwrap();
        let features = [PrefixFeature {
            id: "feature".into(),
            prefix: prefix.into(),
            candidate: "candidate".into(),
            reference: "reference".into(),
            contract: root.join("contract.json"),
            manifest: root.join("corpus.json"),
            cases: vec![case],
            corpus: corpus.clone(),
            work_fields: vec![],
            timeout: super::super::worker::DEFAULT_TIMEOUT,
            per_target: false,
        }];
        let run = |identity: &Value| {
            prefix_traces(
                &root,
                &root.join("candidate"),
                &features,
                identity,
                &root.join("raw"),
                &mut Vec::new(),
            )
            .unwrap_err()
        };
        fs::write(corpus.join("next"), b"changed").unwrap();
        assert_eq!(
            run(&identity(&"0".repeat(64))),
            "reference changed since build identity; rebuild"
        );
        assert_eq!(
            run(&identity(&crate::sha256::hex(b"reference"))),
            format!("feature/case: {mismatch}")
        );
    }
    assert!(!root.join("raw").exists());
    fs::remove_dir_all(root).unwrap();
}

struct Fake {
    parity: Option<&'static str>,
    make: Option<(i64, &'static str)>,
    stale: bool,
    calls: RefCell<Vec<String>>,
}

impl FateHost for Fake {
    fn download(&self, _: &str, _: usize) -> R<Vec<u8>> {
        Err("no network in tests".into())
    }

    fn make(&self, _: &Path, case: &str, _: &Path) -> R<(i64, String, String)> {
        self.calls.borrow_mut().push(format!("make {case}"));
        let (code, out) = self.make.ok_or("make not expected")?;
        Ok((code, out.into(), String::new()))
    }

    fn parity(&self, run: &Run<'_>, sequential: bool) -> R<Value> {
        self.calls.borrow_mut().push(format!(
            "parity {} {sequential}",
            run.input.file_name().unwrap().to_string_lossy()
        ));
        Ok(Value::object([(
            "status",
            Value::str(self.parity.ok_or("parity not expected")?),
        )]))
    }

    fn source_error(&self, _: &Path, _: &Value) -> Option<String> {
        self.stale.then(|| "changed-source".to_owned())
    }
}

fn fake(parity: Option<&'static str>, make: Option<(i64, &'static str)>) -> Fake {
    Fake {
        parity,
        make,
        stale: false,
        calls: RefCell::new(vec![]),
    }
}

fn setup(with_workers: bool) -> PathBuf {
    let root = crate::walk::scratch("fate-run");
    let cache = root.join(".research/reference");
    fs::create_dir_all(cache.join("FFmpeg-frozen-fixture")).unwrap();
    fs::write(
        cache.join("build.json"),
        r#"{"revision": "frozen-fixture"}"#,
    )
    .unwrap();
    fs::write(
        cache.join("fate-list.txt"),
        "fate-matroska-xiph-lacing\nfate-matroska-opus-remux\n",
    )
    .unwrap();
    fs::write(
        cache.join("FFmpeg-frozen-fixture/ffmpeg"),
        "unit-fixture-only-oracle",
    )
    .unwrap();
    fs::create_dir_all(root.join(".research/fate-samples/mkv")).unwrap();
    for name in ["xiph_lacing.mka", "codec_delay_opus.mkv"] {
        fs::write(
            root.join(".research/fate-samples/mkv").join(name),
            "unit-fixture-only-input",
        )
        .unwrap();
    }
    if with_workers {
        let release = root.join(gate_build::release_dir().unwrap());
        fs::create_dir_all(&release).unwrap();
        fs::create_dir_all(root.join("target/harness")).unwrap();
        fs::write(release.join("media-worker"), "candidate").unwrap();
        fs::write(root.join("target/harness/ffmpeg-worker"), "reference").unwrap();
        let identity = format!(
            r#"{{"binaries": {{"candidate": "{}", "reference": "{}"}}}}"#,
            crate::sha256::hex(b"candidate"),
            crate::sha256::hex(b"reference")
        );
        fs::write(root.join("target/harness/identity.json"), identity).unwrap();
    }
    root
}

fn receipt(root: &Path, label: &str) -> Value {
    read_json(&root.join(format!(".research/reference/fate-{label}.json"))).unwrap()
}

#[test]
fn demux_only_needs_both_workers_and_runs_nothing_upstream() {
    let root = setup(false);
    let host = fake(None, None);
    assert!(
        fate_run(&root, "fate-matroska-xiph-lacing", true, &host)
            .unwrap_err()
            .contains("candidate and reference workers")
    );
    assert!(host.calls.borrow().is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn upstream_only_runs_make_and_records_no_candidate_test() {
    let root = setup(false);
    let host = fake(None, Some((0, "native FATE output")));
    assert_eq!(
        fate_run(&root, "fate-matroska-xiph-lacing", false, &host).unwrap(),
        0
    );
    let r = receipt(&root, "xiph");
    assert_eq!(r.get("upstream_case_executed"), Some(&Value::Bool(true)));
    assert_eq!(
        s(&r, "candidate_status"),
        Some("not tested by upstream FATE")
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_demux_comparison_uses_the_original_vector_and_keeps_failures() {
    let root = setup(true);
    let host = fake(Some("pass"), None);
    assert_eq!(
        fate_run(&root, "fate-matroska-opus-remux", true, &host).unwrap(),
        0
    );
    assert_eq!(*host.calls.borrow(), ["parity codec_delay_opus.mkv true"]);
    let r = receipt(&root, "opus");
    assert_eq!(s(&r, "reference_status"), Some("not-run-demux-only"));
    assert!(
        s(&r, "candidate_status")
            .unwrap()
            .contains("not complete upstream FATE execution")
    );
    assert!(
        s(&r, "rights")
            .unwrap()
            .contains("redistribution unreviewed")
    );
    fs::remove_file(root.join(".research/reference/fate-opus.json")).unwrap();
    assert_eq!(
        fate_run(
            &root,
            "fate-matroska-opus-remux",
            true,
            &fake(Some("mismatch"), None)
        )
        .unwrap(),
        1
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn changed_inputs_stale_workers_and_upstream_failures_fail_closed() {
    let root = setup(true);
    fs::write(
        root.join(".research/reference/fate-xiph.json"),
        r#"{"sample_sha256": "changed-input"}"#,
    )
    .unwrap();
    assert!(
        fate_run(
            &root,
            "fate-matroska-xiph-lacing",
            true,
            &fake(Some("pass"), None)
        )
        .unwrap_err()
        .contains("FATE input changed")
    );
    fs::remove_file(root.join(".research/reference/fate-xiph.json")).unwrap();
    let mut stale = fake(Some("pass"), None);
    stale.stale = true;
    assert!(
        fate_run(&root, "fate-matroska-xiph-lacing", true, &stale)
            .unwrap_err()
            .contains("Stale adapter identity")
    );
    assert!(stale.calls.borrow().is_empty());
    assert_eq!(
        fate_run(
            &root,
            "fate-matroska-xiph-lacing",
            false,
            &fake(Some("pass"), Some((7, "")))
        )
        .unwrap(),
        7
    );
    fs::remove_dir_all(root).unwrap();
}
