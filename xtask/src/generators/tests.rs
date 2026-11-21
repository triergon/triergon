use super::*;

/// The features `COMMAND compare ARGS` reports, in order.
fn compared(generator: &Generator, args: &[&str]) -> Vec<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let args: Vec<String> = args.iter().map(ToString::to_string).collect();
    generator
        .reports(root, &args)
        .unwrap()
        .iter()
        .map(|list| list[1].split('/').nth(2).unwrap().to_owned())
        .collect()
}

#[test]
fn compare_selects_features_by_their_contracts_gate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let [fonts, shape, raster, ass] = GENERATORS;
    assert_eq!(
        compared(fonts, &[]),
        ["font-outlines", "font-hinting-none", "font-hinting-default"]
    );
    assert_eq!(compared(fonts, &["--all"]), fonts.features(root).unwrap());
    assert_eq!(
        compared(shape, &[]),
        [
            "shape-text",
            "shape-libass",
            "shape-bidi",
            "shape-simple",
            "shape-unicode",
            "shape-extents"
        ]
    );
    assert_eq!(compared(shape, &["--all"]), shape.features(root).unwrap());
    assert!(compared(shape, &["--diagnostic"]).is_empty());
    let required = ["raster-glyphs", "raster-outlines", "raster-bitmaps"];
    assert_eq!(compared(raster, &[]), required);
    assert_eq!(
        compared(raster, &["--all"]),
        [
            "raster-glyphs",
            "raster-outlines",
            "raster-bitmaps",
            "raster-sweep"
        ]
    );
    assert_eq!(compared(raster, &["--diagnostic"]), ["raster-simd"]);
    for flags in [["--all", "--diagnostic"], ["--diagnostic", "--all"]] {
        assert_eq!(
            compared(raster, &flags),
            raster.features(root).unwrap(),
            "{flags:?}"
        );
    }
    assert_eq!(compared(ass, &[]), ass.features(root).unwrap());
    assert_eq!(compared(ass, &["--all"]), ass.features(root).unwrap());
}

#[test]
fn a_feature_takes_its_cases_and_pins_from_cases_from() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let root = crate::walk::scratch("generator-cases-from");
    let features = root.join("harness/features");
    std::fs::create_dir_all(features.join("font-outlines")).unwrap();
    std::fs::create_dir_all(features.join("font-hinting-none")).unwrap();
    std::fs::create_dir_all(features.join("shape-text")).unwrap();
    std::fs::create_dir_all(root.join("harness/fonts")).unwrap();
    for file in [
        "harness/fonts/corpus.json",
        "harness/features/font-outlines/contract.json",
        "harness/features/font-outlines/pins.json",
        "harness/features/shape-text/contract.json",
    ] {
        std::fs::copy(source.join(file), root.join(file)).unwrap();
    }
    let generator = &crate::fonts::GENERATOR;
    let derived = features.join("font-hinting-none");
    let check = |contract: &str| {
        let contract = contract.replacen(
            '{',
            r#"{"id":"font-hinting-none","candidate":"font-worker","#,
            1,
        );
        std::fs::write(derived.join("contract.json"), contract).unwrap();
        generator.check(&root)
    };
    let diagnostic = r#"{"gate": "diagnostic", "cases_from": ["font-outlines"]}"#;
    assert_eq!(check(diagnostic), Vec::<String>::new());
    let cases = |id| generator.manifest(&root, id).unwrap().get("cases").cloned();
    assert_eq!(cases("font-hinting-none"), cases("font-outlines"));
    assert_eq!(
        generator.inputs(&root, "font-hinting-none")[0],
        "harness/features/font-outlines/pins.json"
    );
    for (contract, error) in [
        (
            r#"{"gate": "required", "cases_from": ["font-outlines"]}"#,
            "diagnostic",
        ),
        (r#"{"gate": "diagnostic", "cases_from": []}"#, "not a list"),
        (
            r#"{"gate": "diagnostic", "cases_from": ["font-hinting-none"]}"#,
            "not another",
        ),
        (
            r#"{"gate": "diagnostic", "cases_from": ["shape-text"]}"#,
            "not another",
        ),
        (
            r#"{"gate": "diagnostic", "cases_from": ["font-outlines"], "pin_operation": "font-"}"#,
            "worker_operation",
        ),
    ] {
        let found = check(contract);
        assert!(
            found.iter().any(|e| e.contains(error)),
            "{contract}: {found:?}"
        );
    }
    std::fs::copy(
        features.join("font-outlines/pins.json"),
        derived.join("pins.json"),
    )
    .unwrap();
    assert!(check(diagnostic)[0].contains("takes its pins from cases_from"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_contract_without_a_known_gate_fails_the_check() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let root = crate::walk::scratch("generator-gate");
    let feature = root.join("harness/features/font-outlines");
    std::fs::create_dir_all(&feature).unwrap();
    std::fs::create_dir_all(root.join("harness/fonts")).unwrap();
    for file in [
        "harness/fonts/corpus.json",
        "harness/features/font-outlines/pins.json",
    ] {
        std::fs::copy(source.join(file), root.join(file)).unwrap();
    }
    let contract =
        std::fs::read_to_string(source.join("harness/features/font-outlines/contract.json"))
            .unwrap();
    let generator = &crate::fonts::GENERATOR;
    for (gate, errors) in [("\"required\"", 0), ("\"sometimes\"", 1), ("true", 1)] {
        let edited = contract.replace("\"required\"", gate);
        std::fs::write(feature.join("contract.json"), edited).unwrap();
        let found = generator.check(&root);
        assert_eq!(found.len(), errors, "{gate}: {found:?}");
        assert!(found.iter().all(|e| e.contains("gate")), "{found:?}");
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn alternate_references_get_their_own_pin_note() {
    let generator = &crate::fonts::GENERATOR;
    let contract = Value::Object(Vec::new());
    let pins = generator.pin_file("alternate-font-reference", &contract, vec![]);
    assert_eq!(
        pins.get("reference").and_then(Value::as_str),
        Some("alternate-font-reference")
    );
    assert_eq!(
        pins.get("note").and_then(Value::as_str),
        Some(
            "Folds recorded with alternate-font-reference by `cargo xtask fonts manifest --record`."
        )
    );
}

#[test]
fn recording_resolves_each_reference_before_writing_any_pins() {
    let root = crate::walk::scratch("generator-reference-selection");
    let features = root.join("harness/features");
    for (feature, reference) in [("font-one", "reference-one"), ("font-two", "reference-two")] {
        let directory = features.join(feature);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("contract.json"),
            format!(r#"{{"id":"{feature}","candidate":"font-worker","reference":"{reference}"}}"#),
        )
        .unwrap();
    }
    let mut requested = Vec::new();
    let result = crate::fonts::GENERATOR.record_with(&root, "test-target", false, |name| {
        requested.push(name.to_owned());
        if name == "reference-two" {
            return Err("missing worker".into());
        }
        Ok(root.join("first-worker"))
    });
    assert_eq!(requested, ["reference-one", "reference-two"]);
    assert_eq!(result.unwrap_err(), "missing worker");
    assert!(!features.join("font-one/pins.json").exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn compare_passes_report_options_through() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let args = |list: &[&str]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
    let reports = crate::shape::GENERATOR
        .reports(root, &args(&["--output", "out", "--memory"]))
        .unwrap();
    assert_eq!(
        reports[0],
        args(&[
            "--contract",
            "harness/features/shape-text/contract.json",
            "--manifest",
            "harness/features/shape-text/corpus.json",
            "--output",
            "out/shape-text",
            "--memory",
        ])
    );
}
