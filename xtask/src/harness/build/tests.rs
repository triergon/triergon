use super::*;

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn only_an_uninstrumented_build_of_the_same_libraries_measures_for_a_reference() {
    let contract = |reference: &str, measurement: &str| {
        json::parse(&format!(
            r#"{{"reference":"{reference}","measurement_reference":"{measurement}"}}"#
        ))
        .unwrap()
    };
    let error =
        |reference, measurement| measurement_reference_error(&contract(reference, measurement));
    assert_eq!(error("libass-ass-native", "libass-player-native"), None);
    assert_eq!(
        error("ffmpeg-subtitles-native", "ffmpeg-subtitles-plain-native"),
        None
    );
    assert!(error("ffmpeg-subtitles-plain-native", "ffmpeg-subtitles-native").is_some());
    assert!(error("libass-ass-native", "libass-ass-native").is_some());
    assert!(error("libass-native", "libass-player-native").is_some());
    assert!(error("libass-ass-native", "freetype-native").is_some());
    assert!(error("libass-ass-native", "unbuilt").is_some());
    let plain = json::parse(r#"{"reference":"libass-ass-native"}"#).unwrap();
    assert_eq!(measurement_reference_error(&plain), None);
}

#[test]
fn worker_options_preserve_defaults_and_accept_either_flag_order() {
    assert_eq!(options(&[]).unwrap(), (false, None, false));
    assert_eq!(
        options(&args(&["--comparison-build"])).unwrap(),
        (true, None, false)
    );
    assert_eq!(
        options(&args(&["--all-references"])).unwrap(),
        (false, None, true)
    );
    assert!(options(&args(&["--all-references", "--all-references"])).is_err());
    for name in
        std::iter::once("media-worker").chain(ADDITIONAL_CANDIDATES.iter().map(|spec| spec.name))
    {
        assert_eq!(
            options(&args(&["--only", name])).unwrap(),
            (false, Some(name), false)
        );
        for arguments in [
            args(&["--only", name, "--comparison-build"]),
            args(&["--comparison-build", "--only", name]),
        ] {
            assert_eq!(options(&arguments).unwrap(), (true, Some(name), false));
        }
        assert_eq!(
            options(&args(&["--all-references", "--only", name])).unwrap(),
            (false, Some(name), true)
        );
    }
}

#[test]
fn worker_selection_uses_the_registry_and_keeps_default_order() {
    let all = selected_candidates(None).unwrap();
    assert_eq!(all.len(), ADDITIONAL_CANDIDATES.len());
    for (actual, registered) in all.iter().zip(&ADDITIONAL_CANDIDATES) {
        assert_eq!(*actual, registered);
        let selected = selected_candidates(Some(registered.name)).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0], registered);
    }
    assert!(
        selected_candidates(Some("media-worker"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn invalid_worker_options_are_refused_before_build_admission() {
    let root = Path::new("/nonexistent-worker-build-root");
    for arguments in [
        args(&["--only"]),
        args(&["--only", "framing-worker", "--only", "framing-worker"]),
        args(&["--only", "framing-worker", "--only", "codec-worker"]),
        args(&["--comparison-build", "--comparison-build"]),
        args(&["--unknown"]),
        args(&["framing-worker"]),
    ] {
        assert_eq!(main(root, &arguments).unwrap_err(), USAGE);
    }
    for name in ["unknown-worker", "--comparison-build", ""] {
        let error = main(root, &args(&["--only", name])).unwrap_err();
        assert_eq!(error, format!("unknown measurement worker {name}"));
    }
}

#[test]
fn selected_workers_still_require_the_gate_build() {
    let root = std::env::temp_dir().join(format!("worker-build-gate-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join(".cargo")).unwrap();
    fs::write(
        root.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.99.1\"\n",
    )
    .unwrap();
    fs::write(
        root.join(".cargo/config.toml"),
        "[build]\nrustflags = [\"--cfg=worker_selection_guard\"]\n",
    )
    .unwrap();
    for arguments in [
        Vec::new(),
        args(&["--only", "media-worker"]),
        args(&["--only", "framing-worker"]),
    ] {
        let error = main(&root, &arguments).unwrap_err();
        assert!(error.contains("not the gate build"), "{error}");
        assert!(error.contains("build.rustflags"), "{error}");
        assert!(!root.join("target").exists());
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn primary_only_identity_does_not_adopt_stale_additional_binaries() {
    let root = std::env::temp_dir().join(format!("worker-build-stale-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let release = root.join(gate_build::release_dir().unwrap());
    fs::create_dir_all(&release).unwrap();
    for spec in &ADDITIONAL_CANDIDATES {
        fs::write(release.join(spec.name), b"stale").unwrap();
    }
    let candidates = candidate_records(&root, &[]).unwrap();
    assert!(matches!(&candidates, Value::Object(records) if records.is_empty()));
    let identity = Value::object([("additional_candidates", candidates)]);
    for spec in &ADDITIONAL_CANDIDATES {
        let error = candidate_worker(&root, &identity, spec.name).unwrap_err();
        assert_eq!(
            error,
            format!("{} is not in the build identity; rebuild", spec.name)
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn isobmff_comparison_graph_enables_trace_without_changing_defaults() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let comparison = candidate_graph(root, "isobmff-worker").unwrap();
    let production = selected_graph(
        root,
        &cargo_metadata(root).unwrap(),
        "triergon-isobmff",
        &[],
    )
    .unwrap();
    for (graph, enabled) in [(&comparison, true), (&production, false)] {
        let package = graph
            .get("packages")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .find(|package| package.get("name").and_then(Value::as_str) == Some("triergon-isobmff"))
            .unwrap();
        let features = package.get("features").and_then(Value::as_array).unwrap();
        assert_eq!(
            features
                .iter()
                .any(|f| f.as_str() == Some("reference-trace")),
            enabled
        );
    }
}

#[test]
fn splits_pkg_config_output_like_a_shell() {
    assert_eq!(
        shell_words("-I/x/include  -L'/a b/lib' -lz \"-DX=\\\"1\\\"\" a\\ b\n").unwrap(),
        ["-I/x/include", "-L/a b/lib", "-lz", "-DX=\"1\"", "a b"]
    );
    assert!(shell_words("'open").is_err());
}

#[test]
fn instrumentation_commands_follow_the_platform() {
    let (root, out) = (Path::new("/r"), Path::new("/r/target/harness"));
    let darwin = instrumentation_commands("cc", root, out, false);
    assert!(
        darwin[0].contains(&"-dynamiclib".to_owned())
            && darwin[0].contains(&"/r/harness/allocations.c".to_owned())
    );
    assert!(darwin[1].contains(&"/r/harness/memory-check.c".to_owned()));
    let linux = instrumentation_commands("cc", root, out, true);
    assert!(
        linux[0].contains(&"-Wl,-z,now".to_owned())
            && linux[0].ends_with(&["-ldl".to_owned(), "-pthread".to_owned()])
    );
    assert!(linux[1].contains(&"/r/harness/portable-memory-check.c".to_owned()));
}
