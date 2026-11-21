use super::{
    PrefixFeature, entity_input_failure, measure_pinned, open_cases, ratchet, recorded_corpus,
    reviewed_inputs, rights_reviewed, s,
};
use crate::json::{self, Value, parse};
use std::collections::BTreeMap;
use std::path::Path;

#[test]
fn ordered_input_hashes_are_checked_before_running_the_workers() {
    let root = crate::walk::scratch("ordered-input-hashes");
    std::fs::create_dir_all(&root).unwrap();
    let input = b"known entity bytes";
    let hash = crate::sha256::hex(input);
    let row = format!(
        r#"{{"file":"first.ts","sha256":"{hash}","arguments":["188","1","next.ts"],
                 "entities":[{{"file":"first.ts","sha256":"{hash}"}},
                             {{"file":"next.ts","sha256":"{hash}"}}]}}"#
    );
    let case = parse(&row).unwrap();
    let unpinned = parse(r#"{"arguments":["188","1","next.ts"]}"#).unwrap();
    assert_eq!(
        entity_input_failure(&root, &unpinned, Some("ts-")).unwrap_err(),
        "worker entity argument has no input pin"
    );
    let mismatched =
        parse(&row.replace(r#"["188","1","next.ts"]"#, r#"["188","1","different.ts"]"#)).unwrap();
    assert_eq!(
        entity_input_failure(&root, &mismatched, Some("ts-")).unwrap_err(),
        "ordered entity does not match its worker argument"
    );
    assert_eq!(
        entity_input_failure(&root, &case, Some("ts-")).unwrap(),
        Some("missing-entity")
    );
    std::fs::write(root.join("next.ts"), b"other entity bytes").unwrap();
    assert_eq!(
        entity_input_failure(&root, &case, Some("ts-")).unwrap(),
        Some("entity-hash-mismatch")
    );
    std::fs::write(root.join("next.ts"), input).unwrap();
    assert_eq!(
        entity_input_failure(&root, &case, Some("ts-")).unwrap(),
        None
    );
    for namespace in ["--dvb", "--atsc"] {
        let with_namespace = parse(&row.replace(
            r#"["188","1","next.ts"]"#,
            &format!(r#"["188","1","{namespace}","next.ts"]"#),
        ))
        .unwrap();
        assert_eq!(
            entity_input_failure(&root, &with_namespace, Some("ts-")).unwrap(),
            None
        );
    }
    let measured = measure_pinned(
        || entity_input_failure(&root, &case, Some("ts-")),
        || {
            std::fs::write(root.join("next.ts"), b"changed while measuring").unwrap();
            Ok(Value::object([("status", Value::str("measured"))]))
        },
    )
    .unwrap();
    assert_eq!(s(&measured, "status"), Some("entity-hash-mismatch"));
    assert_eq!(
        measured
            .get("invalidated_measurement")
            .and_then(|v| s(v, "status")),
        Some("measured")
    );
    let measured = std::cell::Cell::new(false);
    let result = measure_pinned(
        || {
            if measured.get() {
                Err("input unreadable".into())
            } else {
                Ok(None)
            }
        },
        || {
            measured.set(true);
            Ok(Value::object([("status", Value::str("measured"))]))
        },
    )
    .unwrap();
    assert_eq!(s(&result, "status"), Some("input-check-failed"));
    assert!(result.get("invalidated_measurement").is_some());
    std::fs::remove_file(root.join("next.ts")).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn dts_identity_is_a_single_entity_access_unit_option() {
    let root = Path::new(".");
    let selected = parse(r#"{"arguments":["188","1","--dvb","--dts-core=256"]}"#).unwrap();
    assert_eq!(
        entity_input_failure(root, &selected, Some("ts-au-")).unwrap(),
        None
    );
    assert!(entity_input_failure(root, &selected, Some("ts-")).is_err());
    for options in [r#""next.ts","--dts-core=256""#, r#""--dts-core=8191""#] {
        let case = parse(&format!(r#"{{"arguments":["188","1",{options}]}}"#)).unwrap();
        assert!(entity_input_failure(root, &case, Some("ts-au-")).is_err());
    }
}

#[test]
fn pcr_compatibility_leaves_companion_input_pins_required() {
    let root = Path::new(".");
    let selected =
        parse(r#"{"arguments":["188","1","--dts-core=256","--ignore-pcr-reserved-bits"]}"#)
            .unwrap();
    assert_eq!(
        entity_input_failure(root, &selected, Some("ts-au-")).unwrap(),
        None
    );
    assert!(entity_input_failure(root, &selected, Some("ts-")).is_err());
    let unpinned =
        parse(r#"{"arguments":["188","1","next.ts","--ignore-pcr-reserved-bits"]}"#).unwrap();
    assert!(entity_input_failure(root, &unpinned, Some("ts-au-")).is_err());
}

#[test]
fn unreadable_input_before_memory_keeps_the_completed_timing_row() {
    let timing = measure_pinned(
        || Ok(None),
        || Ok(Value::object([("status", Value::str("measured"))])),
    )
    .unwrap();
    let memory = measure_pinned(
        || Err("input unreadable".into()),
        || panic!("memory must not run without a verified input"),
    )
    .unwrap();
    let row = Value::object([("timing", timing), ("memory", memory)]);
    assert_eq!(
        row.get("timing").and_then(|v| s(v, "status")),
        Some("measured")
    );
    let memory = row.get("memory").unwrap();
    assert_eq!(s(memory, "status"), Some("input-check-failed"));
    assert_eq!(s(memory, "input_error"), Some("input unreadable"));
}

#[test]
fn rights_are_synthetic_or_a_recorded_reviewed_licence() {
    let reviewed = [
            parse(
                r#"{"sources": [{"licence": "OFL-1.1", "files": [{"path": "a.ttf", "sha256": "aa"}]}]}"#,
            )
            .unwrap(),
            parse(r#"{"samples": [{"path": "fate/a.ts", "sha256": "cc"}]}"#).unwrap(),
        ];
    let case = |text: &str| parse(text).unwrap();
    assert!(rights_reviewed(
        &reviewed,
        &case(r#"{"rights": "authored-synthetic"}"#)
    ));
    let font = r#"{"rights": "reviewed-licence", "licence": "OFL-1.1", "sha256": "aa"}"#;
    assert!(rights_reviewed(&reviewed, &case(font)));
    let fetched = r#"{"rights": "fetched-not-redistributed", "sha256": "cc"}"#;
    assert!(rights_reviewed(&reviewed, &case(fetched)));
    for unreviewed in [
        r#"{"rights": "fetched-not-redistributed", "sha256": "aa"}"#,
        r#"{"rights": "fetched-not-redistributed", "source": "fate-suite/a.ts"}"#,
        r#"{"rights": "reviewed-licence", "licence": "OFL-1.1", "sha256": "cc"}"#,
        r#"{"rights": "reviewed-licence", "licence": "Apache-2.0", "sha256": "aa"}"#,
        r#"{"rights": "reviewed-licence", "licence": "OFL-1.1", "sha256": "bb"}"#,
        r#"{"rights": "reviewed-licence", "sha256": "aa"}"#,
        r#"{"rights": "OFL-1.1", "licence": "OFL-1.1", "sha256": "aa"}"#,
        r#"{"sha256": "aa"}"#,
    ] {
        assert!(
            !rights_reviewed(&reviewed, &case(unreviewed)),
            "{unreviewed}"
        );
    }
}

#[test]
fn every_subtitle_case_may_be_measured() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let reviewed = reviewed_inputs(root).unwrap();
    for feature in ["pgs", "dvb", "dvd", "cc", "teletext"] {
        let path = root.join(format!("harness/features/subtitle-{feature}/corpus.json"));
        let manifest = json::read(&path).unwrap();
        for case in manifest.get("cases").and_then(Value::as_array).unwrap() {
            assert!(
                rights_reviewed(&reviewed, case),
                "{}",
                s(case, "id").unwrap()
            );
        }
    }
}

#[test]
fn registered_prefix_features_use_supplied_corpus_directories() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for (candidate, corpora) in [
        (
            "playback-worker",
            vec![
                ("hls-segments", root.join("harness/fixtures/hls")),
                ("mp4-range", root.join(".research/mp4-range-fixtures")),
                (
                    "progressive-ts188",
                    root.join("harness/features/progressive-ts188/inputs"),
                ),
            ],
        ),
        (
            "playback-matroska-worker",
            vec![(
                "matroska-startup-seek",
                root.join(".research/startup-seek-fixtures"),
            )],
        ),
    ] {
        let mut corpora: BTreeMap<String, std::path::PathBuf> = corpora
            .into_iter()
            .map(|(id, directory)| (id.to_owned(), directory))
            .collect();
        let features = PrefixFeature::registered(root, candidate, &corpora).unwrap();
        for (id, directory) in &corpora {
            let feature = features.iter().find(|feature| feature.id == *id).unwrap();
            assert_eq!(&feature.corpus, directory);
            assert!(!feature.cases.is_empty());
        }
        corpora.insert("unused".to_owned(), root.to_path_buf());
        assert!(
            PrefixFeature::registered(root, candidate, &corpora)
                .err()
                .unwrap()
                .contains("no registered prefix feature unused")
        );
    }
}

#[test]
fn dvd_palette_options_use_the_selected_corpus_and_keep_absence() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let manifest = json::read(&root.join("harness/features/subtitle-dvd/corpus.json")).unwrap();
    let cases = manifest.get("cases").and_then(Value::as_array).unwrap();
    let case = |id| cases.iter().find(|case| s(case, "id") == Some(id)).unwrap();
    let scratch = crate::walk::scratch("dvd-ifo");
    let corpus = scratch.join("relocated");
    std::fs::create_dir_all(corpus.join("dvd")).unwrap();
    let resolve = |id| {
        let case = case(id);
        super::resolved_case_arguments(&corpus, case, super::case_arguments(case))
    };
    let palette = corpus.join("dvd/VTS_01_0.IFO");
    std::fs::write(&palette, b"palette").unwrap();
    let arguments = resolve("vobsub-ifo-palette-vts").unwrap();
    assert_eq!(
        arguments[1],
        format!("opt:ifo_palette={}", palette.display()).as_str()
    );

    let missing = corpus.join("dvd/VTS_99_0.IFO");
    let arguments = resolve("vobsub-ifo-palette-missing").unwrap();
    assert_eq!(
        arguments[1],
        format!("opt:ifo_palette={}", missing.display()).as_str()
    );
    std::fs::write(&missing, b"unexpected palette").unwrap();
    assert_eq!(
        resolve("vobsub-ifo-palette-missing").unwrap_err(),
        "IFO palette input has no pin: dvd/VTS_99_0.IFO"
    );
    std::fs::remove_dir_all(scratch).unwrap();
}

#[test]
fn the_font_features_are_registered_prefix_features() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let features = PrefixFeature::registered(root, "font-worker", &BTreeMap::new()).unwrap();
    let found: Vec<(&str, &str, usize)> = features
        .iter()
        .map(|f| (f.id.as_str(), f.prefix.as_str(), f.cases.len()))
        .collect();
    assert_eq!(
        found,
        [
            ("font-hinting-default", "font-sized-", 159),
            ("font-hinting-modes", "font-sized-", 954),
            ("font-hinting-none", "font-sized-", 159),
            ("font-outlines", "font-", 53),
        ]
    );
    let reviewed = reviewed_inputs(root).unwrap();
    for feature in &features {
        assert_eq!(feature.corpus, root.join(".research/fonts/files"));
        assert!(
            feature
                .cases
                .iter()
                .all(|case| rights_reviewed(&reviewed, case))
        );
    }
    assert!(
        features[0]
            .input(&parse(r#"{"file": "../x.ttf"}"#).unwrap())
            .is_err()
    );
    assert_eq!(recorded_corpus(root, &Value::Object(vec![])), None);
}

#[test]
fn open_cases_only_shrink() {
    let rows = |pass: &[(&str, bool)]| -> Vec<Value> {
        pass.iter()
            .map(|(id, ok)| {
                let status = if *ok { "pass" } else { "mismatch" };
                parse(&format!(
                    r#"{{"id": "{id}", "parity": {{"status": "{status}"}}}}"#
                ))
                .unwrap()
            })
            .collect()
    };
    let open = vec!["b".to_owned()];
    assert!(ratchet("f", &rows(&[("a", true), ("b", false)]), &open));
    assert!(
        !ratchet("f", &rows(&[("a", false), ("b", false)]), &open),
        "unlisted failure"
    );
    assert!(
        !ratchet("f", &rows(&[("a", true), ("b", true)]), &open),
        "listed case passes"
    );
    assert!(ratchet("f", &rows(&[("a", true)]), &[]));
    let contract = |requirement: &str| {
        parse(&format!(
                r#"{{"pending_module_requirements": ["x"], "open_cases": [{{"id": "b", "requirement": "{requirement}"}}]}}"#
            ))
            .unwrap()
    };
    let manifest = parse(r#"{"cases": [{"id": "a"}, {"id": "b"}]}"#).unwrap();
    assert_eq!(open_cases(&contract("x"), &manifest, "t").unwrap(), open);
    assert!(
        open_cases(&contract("y"), &manifest, "t").is_err(),
        "requirement not pending"
    );
    let other = parse(r#"{"cases": [{"id": "a"}]}"#).unwrap();
    assert!(
        open_cases(&contract("x"), &other, "t").is_err(),
        "unknown case"
    );
    // An open case scoped to targets is open there only.
    let scoped = |targets: &str| {
        parse(&format!(
            r#"{{"required_targets": ["t", "u"], "pending_module_requirements": ["x"],
                    "open_cases": [{{"id": "b", "requirement": "x", "targets": {targets}}}]}}"#
        ))
        .unwrap()
    };
    assert_eq!(
        open_cases(&scoped(r#"["t"]"#), &manifest, "t").unwrap(),
        open
    );
    assert!(
        open_cases(&scoped(r#"["t"]"#), &manifest, "u")
            .unwrap()
            .is_empty()
    );
    for wrong in ["[]", r#"["v"]"#] {
        assert!(
            open_cases(&scoped(wrong), &manifest, "t").is_err(),
            "{wrong}"
        );
    }
}
