use super::*;

fn measured(heap: f64, rss: f64) -> Value {
    Value::object([
        ("status", Value::str("qualified")),
        ("heap_ratio_upper_95", Value::float(heap)),
        ("total_rss_ratio_upper_95", Value::float(rss)),
    ])
}

fn gates(value: &Value) -> (bool, bool, bool) {
    let flag = |k: &str| value.get(k).is_some_and(Value::truthy);
    (flag("heap_gate"), flag("rss_gate"), flag("memory_gate"))
}

/// The verdict under the repository's memory ceilings.
fn verdict(measured: &Value, pairs: usize, platform: bool, gate_build: bool) -> Value {
    let ceilings = policy::Memory::load(&crate::root()).unwrap();
    super::verdict(measured, pairs, platform, gate_build, ceilings)
}

#[test]
fn parts_take_every_nth_workload_and_together_take_each_once() {
    let taken: Vec<Vec<usize>> = (0..3)
        .map(|i| (0..8).filter(|&w| in_part(w, Some((i, 3)))).collect())
        .collect();
    assert_eq!(taken, vec![vec![0, 3, 6], vec![1, 4, 7], vec![2, 5]]);
    assert!((0..8).all(|w| in_part(w, None)));
}

#[test]
fn a_failed_bound_fails_where_gated_and_open_failures_read_as_listed() {
    let results = |held: &[(&str, bool)]| -> Vec<(String, bool)> {
        held.iter().map(|(id, h)| ((*id).to_owned(), *h)).collect()
    };
    let list = json::parse(
        r#"{"open_workloads": [{"id": "hevc-main10-2160p", "cause": "measured"}],
                "workloads": [{"id": "aac-lc-stereo"}, {"id": "hevc-main10-2160p"}]}"#,
    )
    .unwrap();
    let gate = Some("aarch64-unknown-linux-gnu");
    let bounds = |what, results: &[(String, bool)], gate| {
        super::super::bounds(&list, "workloads", what, results, gate).unwrap()
    };
    let all = results(&[("aac-lc-stereo", true), ("hevc-main10-2160p", true)]);
    assert_eq!(
        bounds("timing", &all, gate),
        (
            vec!["workloads timing: 2 of 2 cases pass".to_owned()],
            false
        )
    );
    let open = results(&[("aac-lc-stereo", true), ("hevc-main10-2160p", false)]);
    assert_eq!(
        bounds("timing", &open, gate),
        (
            vec![
                "workloads timing: 1 of 2 cases pass".to_owned(),
                "workloads timing: 1 open failures, as listed: hevc-main10-2160p".to_owned(),
            ],
            true
        )
    );
    let other = results(&[("aac-lc-stereo", false), ("hevc-main10-2160p", true)]);
    let (lines, failed) = bounds("memory", &other, gate);
    assert!(failed);
    assert_eq!(lines[1], "workloads memory: failing: aac-lc-stereo");
    // A platform the gate does not apply to reports its bounds and fails nothing.
    let (lines, failed) = bounds("memory", &other, None);
    assert!(!failed);
    assert_eq!(
        lines,
        vec!["workloads memory: diagnostic on this target, not gated: 1 of 2 within the bound"]
    );
}

#[test]
fn an_open_workload_with_targets_is_open_on_those_targets_alone() {
    let list = |open: &str| {
        json::parse(&format!(
            r#"{{"open_workloads": [{open}],
                    "workloads": [{{"id": "aac-he-stereo"}}, {{"id": "aac-lc-stereo"}}]}}"#
        ))
        .unwrap()
    };
    let armv7 = list(
        r#"{"id": "aac-he-stereo", "cause": "SBR",
                "targets": ["thumbv7neon-unknown-linux-gnueabihf"]}"#,
    );
    assert_eq!(
        super::super::open_workloads(&armv7, "thumbv7neon-unknown-linux-gnueabihf"),
        Ok(vec!["aac-he-stereo"])
    );
    assert_eq!(
        super::super::open_workloads(&armv7, "aarch64-unknown-linux-gnu"),
        Ok(vec![])
    );
    // On AArch64 a failure of a workload open only on ARMv7 fails as any other.
    let failing = vec![("aac-he-stereo".to_owned(), false)];
    let (lines, failed) = super::super::bounds(
        &armv7,
        "workloads",
        "timing",
        &failing,
        Some("aarch64-unknown-linux-gnu"),
    )
    .unwrap();
    assert!(failed);
    assert_eq!(lines[1], "workloads timing: failing: aac-he-stereo");
    for (open, error) in [
        (
            r#"{"id": "aac-he-stereo"}"#,
            "an open workload needs an id and a cause",
        ),
        (
            r#"{"id": "aac-he-stereo", "cause": " "}"#,
            "open workload aac-he-stereo: its cause is empty",
        ),
        (
            r#"{"id": "dts-core-es-6.1", "cause": "DTS"}"#,
            "open workload dts-core-es-6.1: not a workload of the list",
        ),
        (
            r#"{"id": "aac-he-stereo", "cause": "SBR"}, {"id": "aac-he-stereo", "cause": "SBR"}"#,
            "open workload aac-he-stereo: listed twice",
        ),
        (
            r#"{"id": "aac-he-stereo", "cause": "SBR", "targets": []}"#,
            "open workload aac-he-stereo: its targets must be some of the gate's targets",
        ),
        (
            r#"{"id": "aac-he-stereo", "cause": "SBR", "targets": ["x86_64-unknown-linux-gnu"]}"#,
            "open workload aac-he-stereo: its targets must be some of the gate's targets",
        ),
    ] {
        assert_eq!(
            super::super::open_workloads(&list(open), "aarch64-unknown-linux-gnu"),
            Err(error.to_owned()),
            "{open}"
        );
    }
}

#[test]
fn a_collection_fails_when_any_workload_is_not_measured() {
    assert_eq!(measured_all("memory", &[]), Ok(()));
    assert_eq!(
        measured_all("memory", &["hevc-main10-2160p".into()]),
        Err("memory not measured for 1 workloads: hevc-main10-2160p".into())
    );
}

#[test]
fn the_run_limit_is_the_workload_list_s_for_the_target() {
    let manifest = json::parse(
        r#"{"memory_timeout_seconds": {"aarch64-unknown-linux-gnu": 300}, "workloads": []}"#,
    )
    .unwrap();
    assert_eq!(
        run_limit(&manifest, "aarch64-unknown-linux-gnu"),
        Ok(Duration::from_secs(300))
    );
    assert_eq!(
        run_limit(&manifest, "thumbv7neon-unknown-linux-gnueabihf"),
        Ok(worker::DEFAULT_TIMEOUT)
    );
    let invalid =
        json::parse(r#"{"memory_timeout_seconds": {"aarch64-unknown-linux-gnu": 0}}"#).unwrap();
    assert!(run_limit(&invalid, "aarch64-unknown-linux-gnu").is_err());
}

#[test]
fn both_paired_upper_bounds_must_be_below_one() {
    assert_eq!(
        gates(&verdict(&measured(0.5, 0.9), 30, true, true)),
        (true, true, true)
    );
    assert_eq!(
        gates(&verdict(&measured(0.5, 1.0), 30, true, true)),
        (true, false, false)
    );
    assert_eq!(
        gates(&verdict(&measured(1.2, 0.9), 30, true, true)),
        (false, true, false)
    );
}

#[test]
fn no_gate_without_admission_pairs_platform_or_gate_build() {
    let failed = Value::object([("status", Value::str("observer-unqualified"))]);
    assert_eq!(
        gates(&verdict(&failed, 30, true, true)),
        (false, false, false)
    );
    let good = measured(0.5, 0.5);
    assert_eq!(
        gates(&verdict(&good, 29, true, true)),
        (false, false, false)
    );
    assert_eq!(
        gates(&verdict(&good, 30, false, true)),
        (false, false, false)
    );
    assert_eq!(
        gates(&verdict(&good, 30, true, false)),
        (false, false, false)
    );
    assert_eq!(
        verdict(&Value::Null, 30, true, true)
            .get("memory_status")
            .and_then(Value::as_str),
        Some("invalid-memory-observation")
    );
}
