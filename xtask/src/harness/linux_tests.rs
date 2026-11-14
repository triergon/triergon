//! Synthetic controls for the seven-hook v3 guard schema; nothing native runs.

use super::linux::{
    self, FILE_GUARD_CASES, FILE_MAPPING_CONTROL, GUARD_CASES, GUARD_ORDER, GUARD_POLICY, OBSERVER,
    SCOPE_ERROR_BIT, TLS_IMPORTS,
};
use crate::json::{Value, parse};

fn v(text: &str) -> Value {
    parse(text).unwrap()
}

fn sha(text: &str) -> String {
    crate::sha256::hex(text.as_bytes())
}

/// Replace `key` in the object reached through `path` (object keys or array indexes).
fn set(value: &mut Value, path: &[&str], key: &str, new: Value) {
    let mut node = value;
    for step in path {
        node = match node {
            Value::Object(m) => &mut m.iter_mut().find(|(k, _)| k == step).unwrap().1,
            Value::Array(items) => &mut items[step.parse::<usize>().unwrap()],
            _ => panic!("cannot descend into {step}"),
        };
    }
    match node {
        Value::Object(m) => match m.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = new,
            None => m.push((key.to_owned(), new)),
        },
        Value::Array(items) => items[key.parse::<usize>().unwrap()] = new,
        _ => panic!("not a container"),
    }
}

type Mutation = Box<dyn Fn(&mut Fixture)>;

struct Fixture {
    records: Vec<Value>,
    linked: Value,
    observer: String,
    probe: String,
    bindings: Value,
}

impl Fixture {
    fn certify(&self) -> Result<Value, String> {
        linux::certify(
            &self.records,
            &self.linked,
            &self.observer,
            &self.probe,
            &self.bindings,
        )
    }
}

/// Two closures sharing a sealed libc and interpreter, matching exports, and
/// the 28 heap/RSS calibration records of the v3 observer with every negative
/// control rejected.
fn fixture() -> Fixture {
    fixture_of("linux-glibc-v3")
}

/// The same for the current observer: its 36 records, a sealed libstdc++
/// defining every operator new and delete form, and a probe that imports and
/// ran them all and reports its admitted file mapping.
fn current() -> Fixture {
    fixture_of(OBSERVER)
}

const LIBSTDCXX: &str = "/sealed/libstdc++.so.6";

fn fixture_of(observer: &str) -> Fixture {
    let v4 = observer == OBSERVER;
    let [libc, loader, observer_path, probe_path] = [
        "libc.so.6",
        "ld-linux-aarch64.so.1",
        "allocations.so",
        "memory-check",
    ]
    .map(|n| format!("/sealed/{n}"));
    let library = |name: &str, path: &str| {
        Value::object([
            ("name", Value::str(name)),
            ("path", Value::str(path)),
            ("realpath", Value::str(path)),
            ("sha256", Value::str(sha(path))),
        ])
    };
    let mut libraries = vec![
        library("libc.so.6", &libc),
        library("ld-linux-aarch64.so.1", &loader),
    ];
    if v4 {
        libraries.push(library("libstdc++.so.6", LIBSTDCXX));
    }
    let libraries = Value::Array(libraries);
    let closure = |path: &str| {
        Value::object([
            ("status", Value::str("recorded")),
            ("interpreter", Value::str(&loader)),
            ("interpreter_sha256", Value::str(sha(&loader))),
            ("libraries", libraries.clone()),
            (
                "inspected_artifact",
                Value::object([
                    ("path", Value::str(path)),
                    ("sha256", Value::str(sha(path))),
                ]),
            ),
        ])
    };
    let linked = Value::object([
        ("observer", closure(&observer_path)),
        ("probe", closure(&probe_path)),
    ]);
    let lines = |names: &[&str], suffix: &str| {
        names
            .iter()
            .map(|n| format!("{n}{suffix}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let public = lines(&GUARD_ORDER, "@@GLIBC_2.17 T 100 20");
    let exports = lines(&GUARD_ORDER, " T 100 20");
    let private = lines(&TLS_IMPORTS, "@@GLIBC_PRIVATE T 100 20");
    let forms: Vec<String> = linux::cxx_forms("m").into_iter().collect();
    let forms: Vec<&str> = forms.iter().map(String::as_str).collect();
    let mut undefined = lines(&GUARD_ORDER, "@GLIBC_2.17 U");
    if v4 {
        undefined = format!("{undefined}\n{}", lines(&forms, "@GLIBCXX_3.4 U"));
    }
    let mut bindings = Value::object([
        ("observer_path", Value::str(&observer_path)),
        ("probe_path", Value::str(&probe_path)),
        ("observer_exports", Value::str(&exports)),
        ("probe_undefined", Value::str(&undefined)),
        (
            "defined_objects",
            Value::object([
                (libc.as_str(), Value::str(&public)),
                (loader.as_str(), Value::str(&private)),
                (observer_path.as_str(), Value::str(&exports)),
                (probe_path.as_str(), Value::str("")),
            ]),
        ),
    ]);
    if v4 {
        let exports = lines(&forms, "@@GLIBCXX_3.4 T 100 20");
        set(
            &mut bindings,
            &["defined_objects"],
            LIBSTDCXX,
            Value::str(exports),
        );
    }
    let mut cases = vec!["default", "scope-startup"];
    cases.extend(GUARD_CASES.iter().map(|c| c.0));
    if v4 {
        cases.push(FILE_MAPPING_CONTROL);
        cases.extend(FILE_GUARD_CASES.iter().map(|c| c.0));
    }
    let zeros = || Value::Array(vec![Value::int(0); 7]);
    let mut records = Vec::new();
    for mode in ["heap", "rss"] {
        for &case in &cases {
            let guard_case = GUARD_CASES
                .iter()
                .chain(&FILE_GUARD_CASES)
                .find(|c| c.0 == case);
            let mut data = v(&format!(
                r#"{{"schema": 2, "observer": "{observer}", "mode": "{mode}", "status": "measured", "error_bits": 0,
                   "begins": 1, "ends": 1, "selected_thread_scope": 1, "unknown_window_releases": 0,
                   "heap_peak_bytes": 8192, "heap_live_start": 128, "heap_live_end": 128, "rss_start": 4096, "rss_peak_observed": 12288}}"#
            ));
            set(&mut data, &[], "guard_order", Value::strings(GUARD_ORDER));
            set(&mut data, &[], "guard_policy", Value::str(GUARD_POLICY));
            set(
                &mut data,
                &[],
                "guard_scope",
                Value::str("observed-interposable-entrypoints"),
            );
            set(&mut data, &[], "guard_resolved_mask", Value::int(127));
            set(
                &mut data,
                &[],
                "private_tls_basis",
                Value::str("no-active-thread-or-loader-entrypoints"),
            );
            set(
                &mut data,
                &[],
                "private_tls_direct_hooks",
                Value::Bool(false),
            );
            for key in [
                "guard_calls",
                "guard_window_calls",
                "guard_window_successes",
                "guard_window_failures",
            ] {
                set(&mut data, &[], key, zeros());
            }
            let mut work = Value::object([
                ("calibration", Value::str(case)),
                ("expect_guard_invalid", Value::Bool(guard_case.is_some())),
                ("expected_peak_bytes", Value::int(8192)),
                ("expected_release_equal", Value::Bool(true)),
                (
                    "guard_oracle_scope",
                    Value::str("authored-top-level-request-minima"),
                ),
                ("required_guard_window_calls", zeros()),
                ("required_guard_window_successes", zeros()),
                ("required_guard_window_failures", zeros()),
            ]);
            if v4 {
                let mapped = if case == FILE_MAPPING_CONTROL {
                    4096
                } else {
                    0
                };
                set(&mut data, &[], "file_mappings", Value::int(mapped / 4096));
                set(&mut data, &[], "file_mapped_bytes", Value::int(mapped));
                set(
                    &mut work,
                    &[],
                    "expected_file_mapped_bytes",
                    Value::int(mapped),
                );
                let ran = if case == "default" {
                    forms.clone()
                } else {
                    vec![]
                };
                set(&mut work, &[], "cxx_forms", Value::strings(ran));
            }
            let mut status = "measured";
            if case == "scope-startup" {
                set(
                    &mut data,
                    &[],
                    "guard_calls",
                    Value::Array(vec![Value::int(2); 7]),
                );
            } else if let Some(&(_, index, successful)) = guard_case {
                status = "observer-invalid";
                set(&mut data, &[], "status", Value::str("invalid"));
                set(&mut data, &[], "error_bits", Value::int(SCOPE_ERROR_BIT));
                let i = index.to_string();
                let outcome = if successful {
                    "guard_window_successes"
                } else {
                    "guard_window_failures"
                };
                for key in ["guard_calls", "guard_window_calls", outcome] {
                    set(&mut data, &[key], &i, Value::int(1));
                }
                let required = if successful {
                    "required_guard_window_successes"
                } else {
                    "required_guard_window_failures"
                };
                for key in ["required_guard_window_calls", required] {
                    set(&mut work, &[key], &i, Value::int(1));
                }
            }
            let observed = Value::object([
                ("status", Value::str(status)),
                ("instrument_sha256", Value::str(sha(&observer_path))),
                ("work", work),
                (
                    if status == "observer-invalid" {
                        "receipt"
                    } else {
                        "observer"
                    },
                    data,
                ),
            ]);
            records.push(Value::object([
                ("mode", Value::str(mode)),
                ("case", Value::str(case)),
                ("observed", observed),
            ]));
        }
    }
    Fixture {
        records,
        linked,
        observer: sha(&observer_path),
        probe: sha(&probe_path),
        bindings,
    }
}

#[test]
fn the_complete_schema_certifies_with_the_reference_digest() {
    let f = fixture();
    let certificate = f.certify().unwrap();
    assert_eq!(
        certificate
            .get("native_calibration_records")
            .and_then(Value::as_array)
            .map(<[_]>::len),
        Some(28)
    );
    // The digest the original collector computes for the same fixture.
    assert_eq!(
        certificate
            .get("certificate_sha256")
            .and_then(Value::as_str),
        Some("752e65f63473e6374f52e232f4ec23e0adff7e6e4ad54a6a414774e41d1d56e9")
    );
    assert!(linux::certificate_valid(
        Some(&certificate),
        &f.linked,
        Some(&f.observer),
        Some(&f.probe)
    ));
    assert!(!linux::certificate_valid(
        Some(&certificate),
        &f.linked,
        Some(&f.observer),
        Some(&"a".repeat(64))
    ));
}

#[test]
fn each_missing_or_duplicated_control_is_a_nonpass() {
    for index in 0..28 {
        let mut f = fixture();
        f.records.remove(index);
        assert!(f.certify().is_err(), "{index}");
    }
    let mut f = fixture();
    f.records.push(f.records[0].clone());
    assert!(f.certify().is_err());
}

#[test]
fn receipt_mutations_are_rejected() {
    let startup = |f: &Fixture, mode: &str| {
        f.records
            .iter()
            .position(|r| {
                r.get("case").and_then(Value::as_str) == Some("scope-startup")
                    && r.get("mode").and_then(Value::as_str) == Some(mode)
            })
            .unwrap()
    };
    let cases: Vec<Mutation> = vec![
        Box::new(|f| {
            set(
                &mut f.records[2],
                &["observed", "receipt"],
                "error_bits",
                Value::int(64),
            );
        }),
        Box::new(move |f| {
            let i = startup(f, "heap");
            set(
                &mut f.records[i],
                &["observed", "observer"],
                "heap_peak_bytes",
                Value::int(8193),
            );
        }),
        Box::new(move |f| {
            let i = startup(f, "heap");
            set(
                &mut f.records[i],
                &["observed", "observer"],
                "heap_live_end",
                Value::int(129),
            );
        }),
        Box::new(move |f| {
            let i = startup(f, "rss");
            set(
                &mut f.records[i],
                &["observed", "observer"],
                "rss_peak_observed",
                Value::int(4096),
            );
        }),
        Box::new(|f| {
            for record in f
                .records
                .iter_mut()
                .filter(|r| r.get("case").and_then(Value::as_str) == Some("guard-mmap-failure"))
            {
                set(
                    record,
                    &["observed", "receipt", "guard_window_successes"],
                    "0",
                    Value::int(1),
                );
                set(
                    record,
                    &["observed", "receipt", "guard_window_failures"],
                    "0",
                    Value::int(0),
                );
            }
        }),
        Box::new(|f| {
            set(
                &mut f.records[0],
                &["observed", "observer"],
                "unknown_window_releases",
                Value::int(1),
            );
        }),
        Box::new(|f| {
            set(
                &mut f.records[0],
                &["observed", "observer", "guard_window_calls"],
                "0",
                Value::Bool(false),
            );
        }),
        Box::new(|f| {
            set(
                &mut f.records[2],
                &[],
                "observed",
                v(r#"{"status": "observer-failed", "exit_code": 126}"#),
            );
        }),
        Box::new(|f| {
            set(
                &mut f.records[2],
                &["observed", "work"],
                "required_guard_window_calls",
                Value::Array(vec![Value::int(0); 7]),
            );
        }),
        Box::new(|f| {
            let extra = "\n_dl_allocate_tls@@GLIBC_PRIVATE T 100 20";
            let old = f
                .bindings
                .get("defined_objects")
                .and_then(|d| d.get("/sealed/libc.so.6"))
                .and_then(Value::as_str)
                .unwrap()
                .to_owned();
            set(
                &mut f.bindings,
                &["defined_objects"],
                "/sealed/libc.so.6",
                Value::str(old + extra),
            );
        }),
    ];
    for (index, mutate) in cases.iter().enumerate() {
        let mut f = fixture();
        mutate(&mut f);
        assert!(f.certify().is_err(), "mutation {index}");
    }
}

#[test]
fn private_tls_is_covered_only_for_the_exact_libc_and_private_version() {
    let f = fixture();
    let certificate = f.certify().unwrap();
    let owner = f
        .linked
        .get("probe")
        .and_then(|p| p.get("libraries"))
        .and_then(Value::as_array)
        .unwrap()[0]
        .clone();
    let raw = "mmap64@GLIBC_2.17 U\n_dl_allocate_tls@GLIBC_PRIVATE U\nother_alloc U\n";
    let parsed = Value::object([
        (
            "uncovered",
            Value::strings(["mmap64", "_dl_allocate_tls", "other_alloc"]),
        ),
        ("raw_listing", Value::str(raw)),
        (
            "raw_undefined_symbols",
            Value::strings(raw.lines().map(|l| l.split_whitespace().next().unwrap())),
        ),
    ]);
    let check = |owner: &Value, parsed: &Value, certificate: Option<&Value>| {
        let admission =
            linux::Admission::new(certificate, &f.linked, Some(&f.observer), Some(&f.probe));
        linux::conditional_coverage(parsed, owner, admission.as_ref())
            .get("remaining_uncovered")
            .cloned()
            .unwrap()
    };
    assert_eq!(
        check(&owner, &parsed, Some(&certificate)),
        Value::strings(["other_alloc"])
    );
    let mut elsewhere = owner.clone();
    set(
        &mut elsewhere,
        &[],
        "realpath",
        Value::str("/worker/fake-libc"),
    );
    assert!(
        check(&elsewhere, &parsed, Some(&certificate))
            .as_array()
            .unwrap()
            .contains(&Value::str("_dl_allocate_tls"))
    );
    let untested = v(
        r#"{"uncovered": ["mmap64"], "raw_listing": "mmap64@GLIBC_UNTESTED U\n", "raw_undefined_symbols": ["mmap64@GLIBC_UNTESTED"]}"#,
    );
    for evidence in [None, Some(&certificate)] {
        assert_eq!(
            check(&v("{}"), &untested, evidence),
            Value::strings(["mmap64"])
        );
    }
    let raw = "_dl_allocate_tls@GLIBC_PRIVATE U\n_dl_allocate_tls@UNTESTED U\n";
    let extra_abi = Value::object([
        ("uncovered", Value::strings(["_dl_allocate_tls"])),
        ("raw_listing", Value::str(raw)),
        (
            "raw_undefined_symbols",
            Value::strings(raw.lines().map(|l| l.split_whitespace().next().unwrap())),
        ),
    ]);
    assert_eq!(
        check(&owner, &extra_abi, Some(&certificate)),
        Value::strings(["_dl_allocate_tls"])
    );
}

#[test]
fn an_active_sample_is_a_nonpass_after_calibration() {
    let f = fixture();
    let certificate = f.certify().unwrap();
    let mut data = f.records[0]
        .get("observed")
        .and_then(|o| o.get("observer"))
        .cloned()
        .unwrap();
    assert!(linux::certificate_valid(
        Some(&certificate),
        &f.linked,
        Some(&f.observer),
        Some(&f.probe)
    ));
    assert!(linux::guard_component_valid(&data));
    for key in [
        "guard_calls",
        "guard_window_calls",
        "guard_window_successes",
    ] {
        set(&mut data, &[key], "6", Value::int(1));
    }
    assert!(!linux::guard_component_valid(&data));
}

#[test]
fn allocator_imports_keep_raw_versions_and_flag_uncovered_entry_points() {
    let listing = "malloc@GLIBC_2.17 U\nmmap64@GLIBC_2.17 U\n_Znwm U\nstrdup@GLIBC_2.17 w\nmalloc_usable_size U\nclone U\nread U\n";
    let parsed = linux::allocator_symbols(listing, true).unwrap();
    assert_eq!(
        parsed.get("allocator_imports"),
        Some(&Value::strings([
            "_Znwm", "clone", "malloc", "mmap64", "strdup"
        ]))
    );
    assert_eq!(
        parsed.get("uncovered"),
        Some(&Value::strings(["_Znwm", "clone", "mmap64"]))
    );
    assert!(linux::allocator_symbols("malloc X\n", true).is_err());
    let darwin =
        linux::allocator_symbols("_malloc\n_free\n  _strdup  \n_valloc\n_read\n", false).unwrap();
    assert_eq!(darwin.get("uncovered"), Some(&Value::strings(["valloc"])));
}

/// What remains uncovered of a worker's `listing` under `certificate`.
fn remaining(f: &Fixture, listing: &str, certificate: Option<&Value>) -> Value {
    let parsed = linux::allocator_symbols(listing, true).unwrap();
    let admission =
        linux::Admission::new(certificate, &f.linked, Some(&f.observer), Some(&f.probe));
    linux::conditional_coverage(&parsed, &v("{}"), admission.as_ref())
        .get("remaining_uncovered")
        .cloned()
        .unwrap()
}

/// Appends `lines` to the export or import transcript at `path` in the bindings.
fn extend(f: &mut Fixture, path: &[&str], key: &str, lines: &str) {
    let mut node = &f.bindings;
    for step in path {
        node = node.get(step).unwrap();
    }
    let text = format!(
        "{}\n{lines}",
        node.get(key).and_then(Value::as_str).unwrap()
    );
    set(&mut f.bindings, path, key, Value::str(text));
}

fn none() -> Value {
    Value::strings::<&str>([])
}

/// The index of the `mode` record of calibration `case`.
fn record(f: &Fixture, mode: &str, case: &str) -> usize {
    f.records
        .iter()
        .position(|r| {
            r.get("case").and_then(Value::as_str) == Some(case)
                && r.get("mode").and_then(Value::as_str) == Some(mode)
        })
        .unwrap()
}

#[test]
fn the_current_observer_certifies_its_calibration_and_mapping_controls() {
    let f = current();
    let certificate = f.certify().unwrap();
    assert_eq!(
        certificate
            .get("native_calibration_records")
            .and_then(Value::as_array)
            .map(<[_]>::len),
        Some(36)
    );
    assert!(linux::certificate_valid(
        Some(&certificate),
        &f.linked,
        Some(&f.observer),
        Some(&f.probe)
    ));
    let mutations: Vec<Mutation> = vec![
        // The probe did not report running a form, or reports none at all.
        Box::new(|f| {
            let i = record(f, "heap", "default");
            set(
                &mut f.records[i],
                &["observed", "work"],
                "cxx_forms",
                Value::strings(["_Znwm"]),
            );
        }),
        Box::new(|f| {
            let i = record(f, "rss", "default");
            set(
                &mut f.records[i],
                &["observed", "work"],
                "cxx_forms",
                Value::Null,
            );
        }),
        // The probe does not import every form.
        Box::new(|f| {
            let undefined = f
                .bindings
                .get("probe_undefined")
                .and_then(Value::as_str)
                .unwrap();
            let fewer = undefined.replace("_ZdaPvm@GLIBCXX_3.4 U", "");
            set(&mut f.bindings, &[], "probe_undefined", Value::str(fewer));
        }),
        // The admitted file mapping is not reported, or with other bytes.
        Box::new(|f| {
            let i = record(f, "heap", FILE_MAPPING_CONTROL);
            set(
                &mut f.records[i],
                &["observed", "observer"],
                "file_mapped_bytes",
                Value::int(0),
            );
        }),
        Box::new(|f| {
            let i = record(f, "rss", FILE_MAPPING_CONTROL);
            set(
                &mut f.records[i],
                &["observed", "observer"],
                "file_mappings",
                Value::int(0),
            );
        }),
        // A record of the earlier observer among the current one's.
        Box::new(|f| {
            let i = record(f, "rss", "guard-file-munmap");
            set(
                &mut f.records[i],
                &["observed", "receipt"],
                "observer",
                Value::str("linux-glibc-v3"),
            );
        }),
    ];
    for (index, mutation) in mutations.iter().enumerate() {
        let mut f = current();
        mutation(&mut f);
        assert!(f.certify().is_err(), "mutation {index}");
    }
    for index in 0..36 {
        let mut f = current();
        f.records.remove(index);
        assert!(f.certify().is_err(), "{index}");
    }
}

#[test]
fn operator_new_and_delete_are_covered_only_from_one_provider_of_every_form() {
    let listing = "_Znam@GLIBCXX_3.4 U\n_ZdlPvm@GLIBCXX_3.4 U\nmalloc@GLIBC_2.17 U\n";
    let f = current();
    let certificate = f.certify().unwrap();
    assert_eq!(remaining(&f, listing, Some(&certificate)), none());
    // Without a certificate, under the earlier observer's, or at a version
    // the probe did not run, nothing is covered.
    assert_eq!(
        remaining(&f, listing, None),
        Value::strings(["_ZdlPvm", "_Znam"])
    );
    let mut earlier = fixture();
    extend(
        &mut earlier,
        &[],
        "probe_undefined",
        "_Znam@GLIBCXX_3.4 U\n_ZdlPvm@GLIBCXX_3.4 U",
    );
    let certificate = earlier.certify().unwrap();
    assert_eq!(
        remaining(&earlier, listing, Some(&certificate)),
        Value::strings(["_ZdlPvm", "_Znam"])
    );
    let certificate = f.certify().unwrap();
    assert_eq!(
        remaining(&f, "_Znam@GLIBCXX_UNTESTED U\n", Some(&certificate)),
        Value::strings(["_Znam"])
    );
    // A library replacing only `_Znwm` would take the allocations libstdc++'s
    // `_Znam` forwards to it: no form is admitted.
    let mut partial = current();
    extend(
        &mut partial,
        &["defined_objects"],
        "/sealed/libc.so.6",
        "_Znwm@@GLIBCXX_3.4 T 100 20",
    );
    let certificate = partial.certify().unwrap();
    assert_eq!(
        remaining(&partial, listing, Some(&certificate)),
        Value::strings(["_ZdlPvm", "_Znam"])
    );
    // Nor when the observer is the only provider.
    let mut observed = current();
    let forms = observed
        .bindings
        .get("defined_objects")
        .and_then(|d| d.get(LIBSTDCXX))
        .cloned();
    set(
        &mut observed.bindings,
        &["defined_objects"],
        LIBSTDCXX,
        Value::str(""),
    );
    let exports = format!(
        "{}\n{}",
        observed
            .bindings
            .get("observer_exports")
            .and_then(Value::as_str)
            .unwrap(),
        forms.as_ref().and_then(Value::as_str).unwrap()
    );
    set(
        &mut observed.bindings,
        &[],
        "observer_exports",
        Value::str(&exports),
    );
    set(
        &mut observed.bindings,
        &["defined_objects"],
        "/sealed/allocations.so",
        Value::str(&exports),
    );
    let certificate = observed.certify().unwrap();
    assert_eq!(
        remaining(&observed, listing, Some(&certificate)),
        Value::strings(["_ZdlPvm", "_Znam"])
    );
}

#[test]
fn only_a_weak_import_nothing_defines_is_covered() {
    let hook = "_ZN9__gnu_cxx21zoneinfo_dir_overrideEv";
    let mut f = current();
    let certificate = f.certify().unwrap();
    for weak in [
        format!("{hook} w\nfree@GLIBC_2.17 U\n"),
        format!("{hook} v\n"),
        format!("{hook}@GLIBCXX_3.4.30 w\n"),
    ] {
        assert_eq!(remaining(&f, &weak, Some(&certificate)), none(), "{weak}");
        assert_eq!(remaining(&f, &weak, None), Value::strings([hook]));
    }
    for strong in [format!("{hook} U\n"), format!("{hook} w\n{hook} U\n")] {
        assert_eq!(
            remaining(&f, &strong, Some(&certificate)),
            Value::strings([hook])
        );
    }
    extend(
        &mut f,
        &["defined_objects"],
        LIBSTDCXX,
        &format!("{hook} T 100 20"),
    );
    let certificate = f.certify().unwrap();
    assert_eq!(
        remaining(&f, &format!("{hook} w\n"), Some(&certificate)),
        Value::strings([hook])
    );
}

/// `data` as observer `observer` listing the first `hooks` hooks.
fn hooked(mut data: Value, observer: &str, hooks: usize) -> Value {
    set(&mut data, &[], "observer", Value::str(observer));
    if observer == OBSERVER {
        for key in ["file_mappings", "file_mapped_bytes"] {
            set(&mut data, &[], key, Value::int(0));
        }
    }
    set(
        &mut data,
        &[],
        "symbol_order",
        Value::strings(linux::ALLOCATORS[..hooks].iter().copied()),
    );
    for key in ["symbol_attempts", "symbol_successes", "symbol_failures"] {
        set(
            &mut data,
            &[],
            key,
            Value::Array(vec![Value::int(1); hooks]),
        );
    }
    data
}

#[test]
fn the_current_observer_lists_eight_hooks_and_earlier_ones_six() {
    let valid = |data: Value, mode: &str| linux::valid_receipt(&data, mode);
    assert!(valid(hooked(receipt("heap"), "linux-glibc-v2", 6), "heap"));
    assert!(!valid(hooked(receipt("heap"), "linux-glibc-v2", 8), "heap"));
    assert!(valid(hooked(kernel_receipt(), "linux-glibc-v3", 6), "rss"));
    assert!(!valid(hooked(kernel_receipt(), "linux-glibc-v3", 8), "rss"));
    assert!(valid(hooked(kernel_receipt(), OBSERVER, 8), "rss"));
    for hooks in [6, 7] {
        assert!(!valid(hooked(kernel_receipt(), OBSERVER, hooks), "rss"));
    }
    let mut short = hooked(kernel_receipt(), OBSERVER, 8);
    set(
        &mut short,
        &[],
        "symbol_failures",
        Value::Array(vec![Value::int(0); 6]),
    );
    assert!(!valid(short, "rss"));
}

#[test]
fn the_kernel_ceiling_adds_each_admitted_file_mapping() {
    let mapped = |bytes: i64, upper: i64| {
        let mut data = hooked(kernel_receipt(), OBSERVER, 8);
        set(&mut data, &[], "file_mappings", Value::int(1));
        set(&mut data, &[], "file_mapped_bytes", Value::int(bytes));
        set(
            &mut data,
            &[],
            "rss_peak_upper_bound_bytes",
            Value::int(upper),
        );
        linux::valid_receipt(&data, "rss")
    };
    // Kernel bound 196,608 bytes and VmPeak 262,144 bytes.
    assert!(mapped(8192, 204_800));
    assert!(!mapped(8192, 196_608));
    assert!(mapped(131_072, 262_144));
    let mut earlier = kernel_receipt();
    set(&mut earlier, &[], "file_mapped_bytes", Value::int(8192));
    set(
        &mut earlier,
        &[],
        "rss_peak_upper_bound_bytes",
        Value::int(204_800),
    );
    assert!(!linux::valid_receipt(&earlier, "rss"));
}

/// A Linux v2 observer receipt with every provenance field.
fn receipt(mode: &str) -> Value {
    v(&format!(
        r#"{{"schema": 2, "status": "measured", "mode": "{mode}", "observer": "linux-glibc-v2",
        "rss_source": "proc-self-smaps_rollup-Rss", "hwm_source": "getrusage-RUSAGE_SELF-ru_maxrss-KiB-times-1024",
        "hwm_scope": "process-lifetime-reported", "rss_exact_peak_status": "unresolved", "hwm_upper_bound_status": "unproven",
        "virtual_peak_source": "proc-self-status-VmPeak", "virtual_peak_start": 300000, "virtual_peak_end": 400000,
        "rss_peak_upper_bound_bytes": 400000, "rss_peak_upper_bound_basis": "process-lifetime-VmPeak",
        "rss_bound_scope": "same-process-ordinary-mappings", "rss_bound_formula": "candidate-ceiling-minus-start-plus-two-pages-v1",
        "rss_samples": 8, "error_bits": 0, "begins": 1, "ends": 1, "unknown_releases": 0,
        "recursive_calls": 0, "selected_thread_scope": 1, "glibc": "authored-test-version", "entry_capacity": 65536,
        "symbol_order": ["malloc", "calloc", "realloc", "free", "posix_memalign", "aligned_alloc"],
        "symbol_attempts": [1, 1, 1, 1, 1, 1], "symbol_successes": [1, 1, 1, 1, 1, 1], "symbol_failures": [0, 0, 0, 0, 0, 0],
        "heap_peak_bytes": 123, "heap_live_start": 16, "heap_live_end": 16, "calls": 10, "window_allocation_calls": 6,
        "rss_start": 100000, "rss_peak_observed": 200000, "rss_end": 110000, "hwm_start": 120000, "hwm_end": 200000, "page_size": 4096}}"#
    ))
}

fn kernel_receipt() -> Value {
    let mut data = receipt("rss");
    let fields = format!(
        r#"{{"observer": "linux-glibc-v3", "guard_policy": "{GUARD_POLICY}", "guard_scope": "observed-interposable-entrypoints",
        "guard_resolved_mask": 127, "private_tls_basis": "no-active-thread-or-loader-entrypoints", "private_tls_direct_hooks": false,
        "unknown_window_releases": 0, "kernel_rss_requested": 1, "kernel_rss_abi": 1, "kernel_rss_initial_bytes": 131072,
        "kernel_rss_positive_pages": 16, "kernel_rss_upper_bytes": 196608, "kernel_rss_events": 7, "kernel_rss_missed": 0,
        "kernel_rss_errors": 0, "kernel_rss_page_size": 4096, "rss_start": 131072, "rss_peak_observed": 163840, "rss_end": 131072,
        "virtual_peak_start": 262144, "virtual_peak_end": 262144, "rss_peak_upper_bound_bytes": 196608,
        "rss_peak_upper_bound_basis": "min-kernel-positive-deltas-lifetime-VmPeak"}}"#
    );
    let Value::Object(extra) = v(&fields) else {
        unreachable!()
    };
    for (key, value) in extra {
        set(&mut data, &[], &key, value);
    }
    set(&mut data, &[], "guard_order", Value::strings(GUARD_ORDER));
    for key in [
        "guard_calls",
        "guard_window_calls",
        "guard_window_successes",
        "guard_window_failures",
    ] {
        set(&mut data, &[], key, Value::Array(vec![Value::int(0); 7]));
    }
    data
}

#[test]
fn receipts_need_complete_provenance_and_consistent_counters() {
    assert!(linux::valid_receipt(&receipt("heap"), "heap"));
    assert!(linux::valid_receipt(&receipt("rss"), "rss"));
    assert!(!linux::valid_receipt(&receipt("heap"), "rss"));
    for (key, value) in [
        ("rss_source", "\"/proc/self/statm\""),
        ("page_size", "0"),
        ("heap_peak_bytes", "true"),
        ("rss_start", "-1"),
        ("hwm_end", "1"),
        ("heap_live_end", "null"),
        ("rss_peak_upper_bound_basis", "\"reported-HWM\""),
        ("rss_peak_upper_bound_bytes", "199999"),
        ("rss_peak_upper_bound_bytes", "399999"),
        ("observer", "\"linux-glibc-v1\""),
        ("error_bits", "1"),
    ] {
        let mut data = receipt("rss");
        set(&mut data, &[], key, v(value));
        assert!(!linux::valid_receipt(&data, "rss"), "{key}={value}");
    }
    let mut data = receipt("heap");
    set(&mut data, &[], "unknown_releases", Value::int(2));
    assert!(
        linux::valid_receipt(&data, "heap"),
        "releases before the window are kept, not failed"
    );
}

#[test]
fn kernel_bounded_receipts_are_exact() {
    assert!(linux::valid_receipt(&kernel_receipt(), "rss"));
    let mut data = kernel_receipt();
    for key in [
        "virtual_peak_end",
        "virtual_peak_start",
        "rss_peak_upper_bound_bytes",
    ] {
        set(&mut data, &[], key, Value::int(4096 * 44));
    }
    assert!(linux::valid_receipt(&data, "rss"));
    set(
        &mut data,
        &[],
        "rss_peak_upper_bound_bytes",
        Value::int(4096 * 43),
    );
    assert!(!linux::valid_receipt(&data, "rss"));
    for (key, value) in [
        ("kernel_rss_upper_bytes", "1"),
        ("kernel_rss_missed", "1"),
        ("kernel_rss_errors", "1"),
        ("kernel_rss_abi", "2"),
        ("kernel_rss_requested", "true"),
        ("kernel_rss_positive_pages", "false"),
        ("kernel_rss_page_size", "8192"),
        ("kernel_rss_events", "0"),
        ("rss_peak_observed", "200704"),
        ("kernel_rss_requested", "0"),
    ] {
        let mut data = kernel_receipt();
        set(&mut data, &[], key, v(value));
        assert!(!linux::valid_receipt(&data, "rss"), "{key}={value}");
    }
    let mut data = kernel_receipt();
    set(&mut data, &["guard_calls"], "3", Value::int(1));
    assert!(
        !linux::valid_receipt(&data, "rss"),
        "threads after the constructor"
    );
    let mut data = kernel_receipt();
    set(&mut data, &[], "mode", Value::str("heap"));
    assert!(!linux::valid_receipt(&data, "heap"));
}

#[test]
fn the_resident_bound_uses_the_declared_ceiling_and_observed_reference_growth() {
    let mut row = Vec::new();
    for name in ["candidate", "reference"] {
        for mode in ["heap", "rss"] {
            let mut data = receipt(mode);
            if mode == "rss" {
                let (observed, peak) = if name == "candidate" {
                    (1_050_000, 2_000_000i64)
                } else {
                    (5_000_000, 1_000_000_000_000)
                };
                for (key, value) in [
                    ("rss_start", 1_000_000),
                    ("rss_end", 1_030_000),
                    ("hwm_start", 1_200_000),
                    ("hwm_end", 9_000_000),
                    ("virtual_peak_start", 1_500_000),
                    ("rss_peak_observed", observed),
                    ("virtual_peak_end", peak),
                    ("rss_peak_upper_bound_bytes", peak),
                ] {
                    set(&mut data, &[], key, Value::int(value));
                }
            }
            row.push((
                format!("{name}_{mode}"),
                Value::object([
                    ("status", Value::str("measured")),
                    ("observer", data),
                    ("work", v(r#"{"packets": 1}"#)),
                    ("instrument_sha256", Value::str("observer")),
                ]),
            ));
        }
    }
    let mut samples = vec![Value::Object(row.clone()), Value::Object(row)];
    let certificate = v(r#"{"platform": "Linux", "observer_sha256": "observer"}"#);
    let result = super::memory::evaluate(
        std::path::Path::new("."),
        &samples,
        crate::harness::timing::Pairs::fixed(2),
        0,
        &certificate,
        None,
    );
    assert_eq!(
        result.get("status").and_then(Value::as_str),
        Some("qualified")
    );
    // The kernel certificate is the collection's and is verified once, but any
    // sample that used the kernel bound, first or later, still needs it.
    for index in [0, 1] {
        let mut kernel = samples.clone();
        set(
            &mut kernel[index],
            &["reference_rss", "observer"],
            "kernel_rss_requested",
            Value::int(1),
        );
        let refused = super::memory::evaluate(
            std::path::Path::new("."),
            &kernel,
            crate::harness::timing::Pairs::fixed(2),
            0,
            &certificate,
            None,
        );
        assert_eq!(
            refused.get("status").and_then(Value::as_str),
            Some("observer-unqualified")
        );
    }
    let outputs = v(
        r#"{"candidate":{"bytes":100,"fnv1a64":"1234"},"reference":{"bytes":120,"fnv1a64":"5678"}}"#,
    );
    let mut sample = samples[0].clone();
    for role in ["candidate", "reference"] {
        for mode in ["heap", "rss"] {
            let key = format!("{role}_{mode}");
            set(
                &mut sample,
                &[key.as_str()],
                "work",
                outputs.get(role).unwrap().clone(),
            );
        }
    }
    let evaluate = |sample: &Value, outputs| {
        super::memory::evaluate(
            std::path::Path::new("."),
            &[sample.clone(), sample.clone()],
            crate::harness::timing::Pairs::fixed(2),
            0,
            &certificate,
            outputs,
        )
    };
    let status = |value: &Value| value.get("status").cloned();
    assert_eq!(
        status(&evaluate(&sample, None)),
        Some(Value::str("unequal-memory-work"))
    );
    assert_eq!(
        status(&evaluate(&sample, Some(&outputs))),
        Some(Value::str("qualified"))
    );
    set(
        &mut sample,
        &["candidate_rss"],
        "work",
        outputs.get("reference").unwrap().clone(),
    );
    assert_eq!(
        status(&evaluate(&sample, Some(&outputs))),
        Some(Value::str("unequal-memory-work"))
    );

    // Graded: the candidate's peak ceiling plus two pages against the
    // reference's observed peak minus two pages.
    assert_eq!(
        result
            .get("total_rss_ratio_upper_95")
            .and_then(Value::as_f64),
        Some(2_008_192.0 / 4_991_808.0)
    );
    // Diagnostic: growth from the starting resident set, with the same margins.
    assert_eq!(
        result
            .get("rss_growth_ratio_upper_95")
            .and_then(Value::as_f64),
        Some(1_008_192.0 / 3_991_808.0)
    );

    // A stable reference resident set gives no positive growth lower bound.
    // Its observed total peak remains available in both samples.
    set(
        &mut samples[0],
        &["reference_rss", "observer"],
        "rss_start",
        Value::int(5_000_000),
    );
    let without_growth = super::memory::evaluate(
        std::path::Path::new("."),
        &samples,
        crate::harness::timing::Pairs::fixed(2),
        0,
        &certificate,
        None,
    );
    assert_eq!(
        without_growth.get("status").and_then(Value::as_str),
        Some("qualified")
    );
    assert_eq!(
        without_growth.get("total_rss_ratio"),
        result.get("total_rss_ratio")
    );
    assert_eq!(without_growth.get("heap_ratio"), result.get("heap_ratio"));
    assert_eq!(without_growth.get("rss_growth_ratio"), Some(&Value::Null));
    assert_eq!(
        without_growth.get("rss_growth_ratio_upper_95"),
        Some(&Value::Null)
    );
}

/// A minimal AArch64 relocatable ELF with names, a build-id note and a NOBITS section.
fn module_elf() -> (Vec<u8>, Vec<u8>) {
    let mut note = Vec::new();
    for word in [4u32, 20, 3] {
        note.extend(word.to_le_bytes());
    }
    note.extend(b"GNU\0authored-build-id123");
    let names = b"\0.shstrtab\0.note.gnu.build-id\0.bss\0";
    let (sections_at, names_at) = (64usize, 64 + 4 * 64);
    let note_at = names_at + names.len();
    let mut binary = vec![0u8; note_at + note.len()];
    binary[..6].copy_from_slice(b"\x7fELF\x02\x01");
    binary[16..18].copy_from_slice(&1u16.to_le_bytes());
    binary[18..20].copy_from_slice(&183u16.to_le_bytes());
    binary[40..48].copy_from_slice(&(sections_at as u64).to_le_bytes());
    for (at, value) in [(58, 64u16), (60, 4), (62, 1)] {
        binary[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }
    for (index, name, kind, start, size) in [
        (1usize, 1u32, 3u32, names_at as u64, names.len() as u64),
        (2, 11, 7, note_at as u64, note.len() as u64),
        (3, 30, 8, 0x100000, 0x400000),
    ] {
        let at = sections_at + index * 64;
        binary[at..at + 4].copy_from_slice(&name.to_le_bytes());
        binary[at + 4..at + 8].copy_from_slice(&kind.to_le_bytes());
        binary[at + 24..at + 32].copy_from_slice(&start.to_le_bytes());
        binary[at + 32..at + 40].copy_from_slice(&size.to_le_bytes());
    }
    binary[names_at..names_at + names.len()].copy_from_slice(names);
    binary[note_at..].copy_from_slice(&note);
    (binary, note)
}

#[test]
fn the_module_build_id_note_is_read_exactly() {
    let (binary, note) = module_elf();
    assert_eq!(linux::elf_build_id_note(&binary).unwrap(), note);
    let mut wrong = binary.clone();
    wrong[18] = 62;
    assert!(linux::elf_build_id_note(&wrong).is_err(), "not AArch64");
    let mut short = binary;
    short.truncate(200);
    assert!(linux::elf_build_id_note(&short).is_err());
}

/// A host whose observer replays the guard fixture and whose closures and
/// symbol tables are authored.
struct FakeHost {
    root: std::path::PathBuf,
    records: Vec<Value>,
    linked: Value,
    calls: std::cell::RefCell<Vec<(String, String)>>,
    corrupt: bool,
}

impl super::memory::Host for FakeHost {
    fn linux(&self) -> bool {
        true
    }

    fn observe(
        &self,
        _: &std::path::Path,
        _: Option<&str>,
        _: Option<&std::path::Path>,
        mode: &str,
        arguments: &[String],
        _: bool,
    ) -> Value {
        let case = arguments.first().map_or("default", String::as_str);
        self.calls
            .borrow_mut()
            .push((mode.to_owned(), case.to_owned()));
        let record = self
            .records
            .iter()
            .find(|r| {
                r.get("mode").and_then(Value::as_str) == Some(mode)
                    && r.get("case").and_then(Value::as_str) == Some(case)
            })
            .unwrap();
        let mut raw = record.get("observed").cloned().unwrap();
        let measured = raw.get("status").and_then(Value::as_str) == Some("measured");
        let slot = if measured { "observer" } else { "receipt" };
        let mut base = receipt(mode);
        if let Some(Value::Object(fields)) = raw.get(slot) {
            for (k, value) in fields {
                set(&mut base, &[], k, value.clone());
            }
        }
        let peak = base.get("heap_peak_bytes").cloned().unwrap();
        set(&mut raw, &[], slot, base);
        set(
            &mut raw,
            &[],
            "instrument_sha256",
            Value::str(linux::file_sha(&self.root.join("allocations.so")).unwrap()),
        );
        set(&mut raw, &["work"], "expected_peak_bytes", peak);
        if self.corrupt && case == "guard-munmap-failure" {
            set(&mut raw, &[], "status", Value::str("observer-failed"));
        }
        raw
    }

    fn linked(&self, worker: &std::path::Path, _: Option<&str>) -> Result<Value, String> {
        let role = ["candidate", "reference", "memory-check", "allocations.so"]
            .into_iter()
            .find(|n| worker.ends_with(n))
            .unwrap();
        let role = match role {
            "memory-check" => "probe",
            "allocations.so" => "observer",
            other => other,
        };
        Ok(self.linked.get(role).cloned().unwrap())
    }

    fn nm(&self, args: &[&str], path: &std::path::Path) -> Result<String, String> {
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        let lines = |names: &[&str], suffix: &str| {
            names
                .iter()
                .map(|n| format!("{n}{suffix}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let forms: Vec<String> = linux::cxx_forms("m").into_iter().collect();
        let forms: Vec<&str> = forms.iter().map(String::as_str).collect();
        Ok(if args.contains(&"--defined-only") {
            match file.as_str() {
                "libstdc++.so.6" => lines(&forms, "@@GLIBCXX_3.4 T 100 20"),
                "libc.so.6" => lines(&GUARD_ORDER, "@@GLIBC_2.17 T 100 20"),
                "ld-linux-aarch64.so.1" => lines(&TLS_IMPORTS, "@@GLIBC_PRIVATE T 100 20"),
                "allocations.so" => {
                    let mut names: Vec<&str> = linux::ALLOCATORS.to_vec();
                    names.extend(["write", "writev"]);
                    names.extend(GUARD_ORDER);
                    lines(&names, " T 100 20")
                }
                _ => String::new(),
            }
        } else {
            match file.as_str() {
                "memory-check" => format!(
                    "{}\n{}",
                    lines(&GUARD_ORDER, "@GLIBC_2.17 U"),
                    lines(&forms, "@GLIBCXX_3.4 U")
                ),
                "libc.so.6" => lines(&TLS_IMPORTS, "@GLIBC_PRIVATE U"),
                "ld-linux-aarch64.so.1" => String::new(),
                _ => "malloc@GLIBC_2.17 U\nfree@GLIBC_2.17 U\nmmap64@GLIBC_2.17 U\nmunmap@GLIBC_2.17 U\n".into(),
            }
        })
    }

    fn environment(&self) -> Result<Value, String> {
        Ok(v("{}"))
    }

    fn kernel_requested(&self) -> Option<String> {
        None
    }
}

#[test]
fn full_qualification_runs_every_control_and_refuses_a_missing_one() {
    let dir = crate::walk::scratch("qualification");
    let harness = dir.join("target/harness");
    std::fs::create_dir_all(&harness).unwrap();
    for name in [
        "candidate",
        "reference",
        "allocations.so",
        "memory-check",
        "libc.so.6",
        "ld-linux-aarch64.so.1",
        "libstdc++.so.6",
    ] {
        std::fs::write(harness.join(name), format!("authored {name}")).unwrap();
    }
    let path = |name: &str| {
        harness
            .join(name)
            .canonicalize()
            .unwrap()
            .display()
            .to_string()
    };
    let library = |name: &str| {
        Value::object([
            ("name", Value::str(name)),
            ("path", Value::str(path(name))),
            ("realpath", Value::str(path(name))),
            (
                "sha256",
                Value::str(linux::file_sha(std::path::Path::new(&path(name))).unwrap()),
            ),
        ])
    };
    let libraries = Value::Array(vec![
        library("libc.so.6"),
        library("ld-linux-aarch64.so.1"),
        library("libstdc++.so.6"),
    ]);
    let linked = Value::object(["candidate", "reference", "probe", "observer"].map(|role| {
        let file = match role {
            "probe" => "memory-check",
            "observer" => "allocations.so",
            other => other,
        };
        (
            role,
            Value::object([
                ("status", Value::str("recorded")),
                ("interpreter", Value::str(path("ld-linux-aarch64.so.1"))),
                (
                    "interpreter_sha256",
                    Value::str(
                        linux::file_sha(std::path::Path::new(&path("ld-linux-aarch64.so.1")))
                            .unwrap(),
                    ),
                ),
                ("libraries", libraries.clone()),
                (
                    "inspected_artifact",
                    Value::object([
                        ("path", Value::str(path(file))),
                        (
                            "sha256",
                            Value::str(linux::file_sha(std::path::Path::new(&path(file))).unwrap()),
                        ),
                    ]),
                ),
            ]),
        )
    }));
    let mut host = FakeHost {
        root: harness.clone(),
        records: current().records,
        linked,
        calls: std::cell::RefCell::default(),
        corrupt: false,
    };
    super::memory::clear_caches();
    let (candidate, reference) = (harness.join("candidate"), harness.join("reference"));
    let result = super::memory::qualification(&host, &dir, &candidate, &reference);
    assert_eq!(
        result.get("status").and_then(Value::as_str),
        Some("qualified-selected-workers"),
        "{:?}",
        result.get("errors")
    );
    assert_eq!(host.calls.borrow().len(), 36);
    let certificate = result.get("runtime_guard_certificate").unwrap();
    assert_eq!(
        certificate
            .get("native_calibration_records")
            .and_then(Value::as_array)
            .map(<[_]>::len),
        Some(36)
    );
    let Some(Value::Object(details)) = result.get("allocator_import_details") else {
        panic!()
    };
    assert!(
        details
            .iter()
            .all(|(_, d)| !d.get("remaining_uncovered").is_some_and(Value::truthy))
    );
    super::memory::clear_caches();
    host.corrupt = true;
    let rejected = super::memory::qualification(&host, &dir, &candidate, &reference);
    assert_eq!(
        rejected.get("status").and_then(Value::as_str),
        Some("unqualified")
    );
    assert_eq!(
        rejected.get("runtime_guard_certificate"),
        Some(&Value::Null)
    );
    assert_eq!(
        rejected
            .get("attempted_guard_calibration")
            .and_then(Value::as_array)
            .map(<[_]>::len),
        Some(36)
    );
    let key = format!("candidate:{}", path("candidate"));
    assert!(
        rejected
            .get("allocator_import_details")
            .and_then(|d| d.get(&key))
            .and_then(|d| d.get("uncovered"))
            .is_some_and(|u| u.as_array().unwrap().contains(&Value::str("mmap64")))
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// The observer is a shared object without PT_INTERP: it must resolve with the
/// interpreter of the executable it is preloaded into, and only with one given.
#[cfg(target_os = "linux")]
#[test]
fn a_shared_object_resolves_with_the_given_interpreter() {
    use std::process::Command;
    let dir = crate::walk::scratch("linked-object");
    let source = dir.join("observer.c");
    // Like the observer, it links libc, so the loader lists a real closure.
    std::fs::write(
        &source,
        "#include <stdlib.h>\nvoid *observed(void) { return malloc(1); }\n",
    )
    .unwrap();
    let object = dir.join("observer.so");
    let built = Command::new("cc")
        .args(["-shared", "-fPIC", "-o"])
        .arg(&object)
        .arg(&source)
        .status()
        .unwrap();
    assert!(built.success());
    let headers = Command::new("readelf")
        .args(["--program-headers", "--wide"])
        .arg(std::env::current_exe().unwrap())
        .output()
        .unwrap();
    let headers = String::from_utf8(headers.stdout).unwrap();
    let interpreter = headers
        .split_once("[Requesting program interpreter: ")
        .and_then(|(_, rest)| rest.split_once(']'))
        .map(|(path, _)| path)
        .unwrap();
    assert!(linux::linked_libraries(&object, None).is_err());
    let linked = linux::linked_libraries(&object, Some(interpreter)).unwrap();
    assert_eq!(linked.get("interpreter"), Some(&Value::str(interpreter)));
    std::fs::remove_dir_all(dir).unwrap();
}

/// The observer and probe as `build-workers` builds them: the probe's exact
/// heap peak, which every hook and all twenty operator new and delete forms
/// contribute to while held live together, is the observer's, and a read-only
/// file mapping in the window is admitted and reported.
#[cfg(target_os = "linux")]
#[test]
fn the_built_observer_counts_every_hook_and_operator_new_form() {
    use std::process::Command;
    let dir = crate::walk::scratch("observer-probe");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    for argv in super::build::instrumentation_commands("cc", root, &dir, true) {
        assert!(
            Command::new(&argv[0])
                .args(&argv[1..])
                .status()
                .unwrap()
                .success()
        );
    }
    let imports = Command::new("nm")
        .args(["-D", "--undefined-only", "--format=posix"])
        .arg(dir.join("memory-check"))
        .output()
        .unwrap();
    let forms = String::from_utf8(imports.stdout)
        .unwrap()
        .lines()
        .filter(|l| {
            ["_Znw", "_Zna", "_Zdl", "_Zda"]
                .iter()
                .any(|p| l.starts_with(p))
        })
        .count();
    assert_eq!(forms, 20);
    let run = |case: &str| {
        let receipt = dir.join(format!("{case}.json"));
        let output = Command::new(dir.join("memory-check"))
            .arg(case)
            .env("LD_PRELOAD", dir.join("allocations.so"))
            .env("TRIERGON_ALLOCATION_REPORT", &receipt)
            .env("TRIERGON_MEMORY_MODE", "heap")
            .output()
            .unwrap();
        assert!(output.status.success(), "{case}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let work = v(stdout.lines().last().unwrap());
        (work, v(&std::fs::read_to_string(&receipt).unwrap()))
    };
    let (work, data) = run("default");
    assert!(linux::valid_receipt(&data, "heap"));
    assert_eq!(data.get("observer"), Some(&Value::str(OBSERVER)));
    assert_eq!(data.get("heap_peak_bytes"), work.get("expected_peak_bytes"));
    assert_eq!(data.get("heap_live_end"), data.get("heap_live_start"));
    assert_eq!(
        data.get("symbol_order"),
        Some(&Value::strings(linux::ALLOCATORS))
    );
    let hits = data
        .get("symbol_successes")
        .and_then(Value::as_array)
        .unwrap();
    assert!(
        hits.iter()
            .all(|n| linux::py_int(Some(n)).is_some_and(|n| n > 0))
    );
    assert_eq!(
        work.get("cxx_forms")
            .and_then(Value::as_array)
            .map(<[_]>::len),
        Some(20)
    );
    // A read-only file mapping in the window is admitted and reported; a
    // writable one still invalidates the receipt.
    let (work, data) = run(FILE_MAPPING_CONTROL);
    assert!(linux::valid_receipt(&data, "heap"));
    assert_eq!(data.get("file_mappings"), Some(&Value::int(1)));
    assert_eq!(
        data.get("file_mapped_bytes"),
        work.get("expected_file_mapped_bytes")
    );
    let (_, data) = run("guard-file-writable");
    assert_eq!(data.get("status"), Some(&Value::str("invalid")));
    std::fs::remove_dir_all(dir).unwrap();
}
