use std::fs;
use std::os::unix::fs::PermissionsExt;

use super::*;

const PIN: &str = "1.99.1";

#[test]
fn cpu_lists_expand_ranges() {
    assert_eq!(parse_cpu_list("1"), Some(vec![1]));
    assert_eq!(parse_cpu_list("0-2,7"), Some(vec![0, 1, 2, 7]));
    assert_eq!(parse_cpu_list("1-15").map(|c| c.len()), Some(15));
    assert_eq!(parse_cpu_list("3-1"), None);
    assert_eq!(parse_cpu_list("a"), None);
}

#[test]
fn the_host_is_read_from_rustc_or_refused() {
    let version = "rustc 1.99.1\nbinary: rustc\nhost: aarch64-apple-darwin\nrelease: 1.99.1\n";
    assert_eq!(host_of(version), Ok("aarch64-apple-darwin"));
    assert_eq!(release(version), Some("1.99.1"));
    assert_eq!(
        host_of("rustc 1.99.1\nrelease: 1.99.1\n"),
        Err("rustc reports no host".to_owned())
    );
    assert_eq!(field("hostname: x\n", "host"), None);
}

#[test]
fn workers_compile_for_thumb2_on_32_bit_arm_and_for_the_host_elsewhere() {
    assert_eq!(target_for(ARM32_HOST), ARM32_TARGET);
    assert_eq!(
        target_arguments(ARM32_HOST),
        ["--target", "thumbv7neon-unknown-linux-gnueabihf"]
    );
    assert_eq!(
        release_path(ARM32_HOST),
        "thumbv7neon-unknown-linux-gnueabihf/release"
    );
    for host in [
        "aarch64-unknown-linux-gnu",
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
    ] {
        assert_eq!(target_for(host), host);
        assert!(target_arguments(host).is_empty());
        assert_eq!(release_path(host), "release");
    }
}

/// A record of the gate build with one field replaced.
fn record(key: &str, value: Option<Value>) -> Value {
    let base = [
        ("rustc", Value::str(format!("rustc\nrelease: {PIN}\n"))),
        ("build_environment", Value::Object(Vec::new())),
        ("cargo_configuration", Value::Array(Vec::new())),
        ("link_policy", Value::str(LINK_POLICY)),
    ];
    Value::Object(
        base.into_iter()
            .filter_map(|(name, field)| {
                let field = if name == key { value.clone()? } else { field };
                Some((name.to_owned(), field))
            })
            .collect(),
    )
}

fn scratch(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("gate-build-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn variables(pairs: &[(&str, &str)]) -> Vec<String> {
    let pairs = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()));
    environment(pairs).into_iter().map(|(k, _)| k).collect()
}

#[test]
fn the_gate_build_is_admitted_and_named() {
    let gate = record("", None);
    assert!(refusals(&gate, PIN).is_empty());
    assert_eq!(
        describe(&gate, PIN),
        "build: rustc 1.99.1, default flags (the gate build)"
    );
}

#[test]
fn another_compiler_is_refused() {
    let nightly = record(
        "rustc",
        Some(Value::str("rustc\nrelease: 1.101.0-nightly\n")),
    );
    assert_eq!(
        refusals(&nightly, PIN),
        ["rustc 1.101.0-nightly, not the pinned 1.99.1"]
    );
    let line = describe(&nightly, PIN);
    assert!(line.starts_with("build: rustc 1.101.0-nightly, default flags"));
    assert!(line.ends_with("no gate result"));
}

#[test]
fn a_compiler_ahead_on_the_search_path_is_seen() {
    let directory = scratch("path");
    let fake = directory.join("rustc");
    fs::write(
        &fake,
        "#!/bin/sh\necho rustc 1.101.0-nightly\necho release: 1.101.0-nightly\n",
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let found = compiler(&directory, Some(directory.as_os_str())).unwrap();
    let reasons = refusals(&record("rustc", Some(Value::str(found))), PIN);
    fs::remove_dir_all(&directory).unwrap();
    assert_eq!(reasons, ["rustc 1.101.0-nightly, not the pinned 1.99.1"]);
}

#[test]
fn a_record_that_does_not_say_is_refused() {
    for (key, reason) in [
        ("rustc", "records no compiler"),
        ("build_environment", "records no build environment"),
        ("cargo_configuration", "records no Cargo configuration"),
        ("link_policy", "records no link policy"),
    ] {
        assert_eq!(refusals(&record(key, None), PIN), [reason]);
    }
}

#[test]
fn a_build_linked_under_another_policy_is_refused() {
    let earlier = record("link_policy", Some(Value::str("toolchain-default")));
    assert_eq!(
        refusals(&earlier, PIN),
        ["link policy toolchain-default, not static-toolchain-runtime-trained-order-v1"]
    );
}

#[test]
fn workers_link_in_trained_order_with_the_toolchain_lld() {
    let linker = Path::new("/sysroot/lib/rustlib/host/bin/gcc-ld");
    assert_eq!(
        ordered_link_flags(linker, None),
        [
            "-C",
            "link-arg=-B/sysroot/lib/rustlib/host/bin/gcc-ld",
            "-C",
            "link-arg=-fuse-ld=lld"
        ]
    );
    assert_eq!(
        ordered_link_flags(linker, Some(Path::new("/t/order.txt")))[4..],
        [
            "-C",
            "link-arg=-Wl,--symbol-ordering-file=/t/order.txt",
            "-C",
            "link-arg=-Wl,--no-warn-symbol-ordering"
        ]
    );
}

#[test]
fn gnu_linux_workers_and_references_take_toolchain_runtime_from_static_archives() {
    let flags = ["-L/p", "-lharfbuzz", "-lstdc++", "-lm", "-lz", "-latomic"];
    let flags = || flags.map(String::from).to_vec();
    for target in [
        ARM32_TARGET,
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-gnu",
    ] {
        assert_eq!(worker_link_flags(target), ["-l", "static:-bundle=gcc_eh"]);
        // The C library, libc with libm, and third-party libraries stay shared.
        assert_eq!(
            reference_flags(target, flags()),
            [
                "-L/p",
                "-lharfbuzz",
                "-l:libstdc++.a",
                "-lm",
                "-lz",
                "-l:libatomic.a",
                "-static-libgcc"
            ]
        );
    }
    for target in ["aarch64-apple-darwin", "aarch64-unknown-linux-musl"] {
        assert!(worker_link_flags(target).is_empty());
        assert_eq!(reference_flags(target, flags()), flags());
    }
}

#[test]
fn each_compiler_variable_is_recorded_and_refused() {
    for name in [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTC",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_RUSTC",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_RUSTFLAGS",
        "CARGO_BUILD_TARGET",
        "CARGO_PROFILE_RELEASE_LTO",
        "CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_DEBUG",
        "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
        "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER",
    ] {
        assert_eq!(variables(&[(name, "x")]), [name]);
        let found = environment([(name.to_owned(), "x".to_owned())].into_iter());
        let reasons = refusals(
            &record("build_environment", Some(Value::Object(found))),
            PIN,
        );
        assert_eq!(reasons, [format!("compiler settings: {name}")]);
    }
}

#[test]
fn variables_that_leave_the_compile_alone_are_not_recorded() {
    assert!(
        variables(&[
            ("CARGO_TARGET_DIR", "/tmp/target"),
            ("CARGO_INCREMENTAL", "0"),
            ("RUSTDOCFLAGS", "-D warnings"),
            ("CARGO_PROFILE_TEST_DEBUG", "0"),
            ("CARGO_PROFILE_DEV_OPT_LEVEL", "1"),
            ("RUSTFLAGS", ""),
        ])
        .is_empty()
    );
}

#[test]
fn cargo_configuration_that_changes_the_compile_is_refused() {
    for (text, key) in [
        (
            "[build]\nrustflags = [\"-Ctarget-cpu=native\"]\n",
            "build.rustflags",
        ),
        ("[build]\nrustc = \"/opt/rustc\"\n", "build.rustc"),
        (
            "[build]\nrustc-wrapper = \"sccache\"\n",
            "build.rustc-wrapper",
        ),
        (
            "[build]\nrustc-workspace-wrapper = \"w\"\n",
            "build.rustc-workspace-wrapper",
        ),
        (
            "[build]\ntarget = \"aarch64-unknown-linux-musl\"\n",
            "build.target",
        ),
        (
            "[target.aarch64-unknown-linux-gnu]\nrustflags = [\"-Cx\"]\n",
            "target.aarch64-unknown-linux-gnu.rustflags",
        ),
        (
            "[target.aarch64-unknown-linux-gnu]\nlinker = \"lld\"\n",
            "target.aarch64-unknown-linux-gnu.linker",
        ),
        ("[profile.release]\nlto = false\n", "profile.release"),
    ] {
        let tree = scratch("configuration");
        let inner = tree.join("repository");
        fs::create_dir_all(&inner).unwrap();
        fs::create_dir_all(tree.join(".cargo")).unwrap();
        fs::write(tree.join(".cargo/config.toml"), text).unwrap();
        let found = configuration(&inner, None);
        let suffix = format!(": {key}");
        assert_eq!(found.len(), 1, "{text}");
        assert!(found[0].ends_with(&suffix), "{found:?}");
        let reasons = refusals(
            &record("cargo_configuration", Some(Value::strings(found.clone()))),
            PIN,
        );
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].starts_with("Cargo configuration: "));
        fs::remove_dir_all(&tree).unwrap();
    }
}

#[test]
fn other_profiles_do_not_reach_a_release_worker() {
    let tree = scratch("profiles");
    fs::create_dir_all(tree.join(".cargo")).unwrap();
    fs::write(
        tree.join(".cargo/config.toml"),
        "[profile.test]\ndebug = 0\n[profile.dev]\nopt-level = 1\n",
    )
    .unwrap();
    let found = configuration(&tree, None);
    fs::remove_dir_all(&tree).unwrap();
    assert_eq!(found, Vec::<String>::new());
}

#[test]
fn the_allowed_cpus_come_from_the_process_status() {
    let status = "Name:\tx\nCpus_allowed:\t02\nCpus_allowed_list:\t1\nMems:\t1\n";
    assert_eq!(allowed_cpus(status), Some("1"));
    assert_eq!(allowed_cpus("Name:\tx\n"), None);
}

#[test]
fn the_cargo_home_is_read_and_harmless_configuration_passes() {
    let tree = scratch("home");
    let home = tree.join("home");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join("config.toml"), "[build]\nrustflags = [\"-Cx\"]\n").unwrap();
    let inner = tree.join("repository");
    fs::create_dir_all(inner.join(".cargo")).unwrap();
    fs::write(
        inner.join(".cargo/config.toml"),
        "[alias]\nxtask = \"run --package xtask --quiet --\"\n",
    )
    .unwrap();
    let found = configuration(&inner, Some(&home));
    assert_eq!(found.len(), 1);
    assert!(found[0].ends_with(": build.rustflags"));
    assert!(configuration(&inner, None).is_empty());
    fs::remove_dir_all(&tree).unwrap();
}

#[test]
fn unreadable_configuration_is_refused() {
    let tree = scratch("unreadable");
    fs::create_dir_all(tree.join(".cargo")).unwrap();
    fs::write(tree.join(".cargo/config.toml"), "[build\n").unwrap();
    let found = configuration(&tree, None);
    fs::remove_dir_all(&tree).unwrap();
    assert!(found[0].contains("unreadable"), "{found:?}");
}

#[test]
fn a_pair_outside_the_recorded_build_cannot_gate() {
    let tree = scratch("pair");
    fs::create_dir_all(tree.join("target/harness")).unwrap();
    fs::write(
        tree.join("rust-toolchain.toml"),
        format!("[toolchain]\nchannel = \"{PIN}\"\n"),
    )
    .unwrap();
    let (worker, reference, other) = (tree.join("w"), tree.join("r"), tree.join("o"));
    fs::write(&worker, "worker").unwrap();
    fs::write(&reference, "reference").unwrap();
    fs::write(&other, "other").unwrap();
    assert!(pair_refusals(&tree, &worker, &reference)[0].starts_with("no build identity"));

    fs::create_dir_all(tree.join("harness/link-order")).unwrap();
    fs::write(
        tree.join(super::super::link_training::MANIFEST),
        r#"{"directory": "corpus", "files": [], "operations": []}"#,
    )
    .unwrap();
    let digest = |text: &str| sha256::hex(text.as_bytes());
    let mut identity = record("", None);
    if let Value::Object(fields) = &mut identity {
        let entry = |name: &str, sha: String| {
            Value::object([(
                name,
                Value::object([
                    ("sha256", Value::str(sha)),
                    ("link_order", Value::Null),
                    ("toolchain", Value::str(PIN)),
                    ("rustc", Value::str(format!("rustc\nrelease: {PIN}\n"))),
                ]),
            )])
        };
        fields.push(("candidate_link_order".into(), Value::Null));
        fields.push((
            "additional_candidates".into(),
            entry("font-worker", digest("worker")),
        ));
        fields.push((
            "comparison_references".into(),
            entry("ffmpeg-worker", digest("reference")),
        ));
    }
    crate::corpus::write_json(&tree.join("target/harness/identity.json"), &identity).unwrap();
    assert!(pair_refusals(&tree, &worker, &reference).is_empty());
    assert_eq!(pair_refusals(&tree, &other, &reference).len(), 1);
    assert_eq!(pair_refusals(&tree, &worker, &other).len(), 1);
    fs::remove_dir_all(&tree).unwrap();
}

#[test]
fn each_worker_is_held_to_its_own_toolchain() {
    let worker = |toolchain: &str, release: &str| {
        Value::object([
            ("toolchain", Value::str(toolchain)),
            ("rustc", Value::str(format!("rustc\nrelease: {release}\n"))),
        ])
    };
    let nightly = crate::NIGHTLY;
    assert_eq!(
        toolchain_refusal("font-worker", &worker(PIN, PIN), PIN),
        None
    );
    assert_eq!(
        toolchain_refusal("codec-aac-worker", &worker(nightly, "1.100.0-nightly"), PIN),
        None
    );
    for (name, record) in [
        ("codec-aac-worker", worker(PIN, PIN)),
        ("codec-aac-worker", worker(nightly, PIN)),
        ("codec-worker", worker(nightly, "1.100.0-nightly")),
        ("font-worker", worker(nightly, "1.100.0-nightly")),
        ("font-worker", worker(PIN, "1.98.0")),
        ("font-worker", Value::object([("rustc", Value::str(PIN))])),
        ("no-such-worker", worker(PIN, PIN)),
    ] {
        assert!(
            toolchain_refusal(name, &record, PIN).is_some(),
            "{name} {record:?}"
        );
    }
    let mut identity = record("", None);
    if let Value::Object(fields) = &mut identity {
        fields.push((
            "additional_candidates".into(),
            Value::object([("codec-aac-worker", worker(PIN, PIN))]),
        ));
    }
    assert_eq!(refusals(&identity, PIN).len(), 1);
    assert!(
        name(&identity).ends_with("default flags"),
        "{}",
        name(&identity)
    );
}

#[test]
fn a_refused_build_is_told_how_to_continue() {
    let flagged = record(
        "build_environment",
        Some(Value::object([("RUSTFLAGS", Value::str("-Cx"))])),
    );
    let error = admission(&flagged, PIN, false).unwrap_err();
    assert!(error.starts_with("not the gate build"), "{error}");
    assert!(error.contains("--comparison-build"), "{error}");
    assert!(admission(&flagged, PIN, true).is_ok());
    assert!(admission(&record("", None), PIN, false).is_ok());
}

#[test]
fn the_toolchain_pin_is_read_from_the_repository() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let pinned = pinned_release(root).unwrap();
    assert!(pinned.split('.').all(|part| part.parse::<u32>().is_ok()));
}
