use std::path::PathBuf;

use super::*;

const A: &str = "aarch64-unknown-linux-gnu";
const T: &str = "thumbv7neon-unknown-linux-gnueabihf";

fn result(path: &str, command: &str, target: Option<&str>, measured: &str) -> String {
    let target = target.map_or_else(String::new, |t| format!(r#""target": "{t}", "#));
    let measured = if measured.is_empty() {
        String::new()
    } else {
        format!(r#", "measured": {measured}"#)
    };
    format!(
        r#"{{"path": "{path}/gate-result.json", "command": "{command}", {target}"key": "{path}", "status": 0{measured}}}"#
    )
}

fn timed(upper: f64, worst: &str, cases: u32) -> String {
    format!(
        r#"{{"runtime": {{"upper": {upper}, "worst": "{worst}", "passing": {cases}, "cases": {cases}}}}}"#
    )
}

fn both(upper: f64, worst: &str, heap: f64, rss: f64, cases: u32) -> String {
    format!(
        r#"{{"runtime": {{"upper": {upper}, "worst": "{worst}", "passing": {cases}, "cases": {cases}}},
            "memory": {{"heap": {heap}, "total_rss": {rss}, "lower": {cases}, "cases": {cases}}}}}"#
    )
}

const ROWS_JSON: &str = r#"{
  "tables": [
    {"heading": "Linux AArch64", "target": "aarch64-unknown-linux-gnu", "run": "run-a"},
    {"heading": "32-bit ARMv7", "target": "thumbv7neon-unknown-linux-gnueabihf", "run": "run-t"}
  ],
  "runs": {
    "run-a": {"target": "aarch64-unknown-linux-gnu", "revision": "5b8dd88f"},
    "run-t": {"target": "thumbv7neon-unknown-linux-gnueabihf", "revision": "5b8dd88f"},
    "other-t": {"target": "thumbv7neon-unknown-linux-gnueabihf", "revision": "964b36cb"}
  },
  "rows": [
    {"area": "Core", "targets": ["aarch64-unknown-linux-gnu", "thumbv7neon-unknown-linux-gnueabihf"], "text": "via dependents"},
    {"area": "F", "targets": ["aarch64-unknown-linux-gnu", "thumbv7neon-unknown-linux-gnueabihf"], "results": ["f", "g"]},
    {"area": "Parts", "targets": ["aarch64-unknown-linux-gnu", "thumbv7neon-unknown-linux-gnueabihf"], "runs": ["other-t"],
     "parts": [{"label": "`f`", "results": ["f"]},
               {"label": "`h`", "results": ["h"], "targets": ["aarch64-unknown-linux-gnu"]}]}
  ],
  "open_cells": [{"area": "F", "targets": ["thumbv7neon-unknown-linux-gnueabihf"]}]
}"#;

const PAGE_TEXT: &str = "# Status

## Linux AArch64

| Area | Verified | Runtime | Memory |
|---|---|---|---|
| Core | tests | via dependents | via dependents |
| F | 4 cases | 0.9124 (4/4 pass, worst `y`) at `5b8dd88f` | heap ≤0.4000, total RSS ≤0.7000 (4/4 lower) at `5b8dd88f` |
| Parts | f and h | `f` 0.9124 (3/3 pass, worst `y`) at `5b8dd88f`<br>`h` 0.6000 (2/2 pass, worst `w`) at `5b8dd88f` | `f` heap ≤0.4000, total RSS ≤0.7000 (3/3 lower) at `5b8dd88f`<br>`h` heap ≤0.1000, total RSS ≤0.2000 (2/2 lower) at `5b8dd88f` |

## 32-bit ARMv7

| Area | Verified | Runtime | Memory |
|---|---|---|---|
| Core | tests | via dependents | via dependents |
| F | 4 cases | 0.95 as written | heap as written |
| Parts | f | 0.7000 (3/3 pass, worst `v`) at `964b36cb` | heap ≤0.3000, total RSS ≤0.5000 (3/3 lower) at `964b36cb` |
";

/// A repository with two runs on Linux AArch64's (sharded) and ARMv7's, an older ARMv7
/// result without its target, and the page as the rows render it.
fn repository(name: &str) -> PathBuf {
    let root = crate::walk::scratch(name);
    let sets = root.join(crate::evidence::SETS);
    fs::create_dir_all(&sets).unwrap();
    let set = |name: &str, results: &[String]| {
        fs::write(
            sets.join(format!("{name}.json")),
            format!(
                r#"{{"name": "{name}", "retention": "retained", "results": [{}]}}"#,
                results.join(", ")
            ),
        )
        .unwrap();
    };
    set(
        "run-a-shard-1",
        &[
            result(
                "benchmark/f",
                "report --benchmark",
                Some(A),
                &timed(0.5, "x", 3),
            ),
            result(
                "graded/f",
                "grade",
                Some(A),
                &both(0.91234, "y", 0.4, 0.7, 3),
            ),
            result("graded/g", "grade", Some(A), &both(0.8, "z", 0.2, 0.3, 1)),
            result("tests/checked", "gate test", Some(A), ""),
        ],
    );
    set(
        "run-a-shard-2",
        &[
            result(
                "benchmark/h",
                "isobmff-seek --benchmark",
                Some(A),
                &timed(0.6, "w", 2),
            ),
            result(
                "memory/h",
                "isobmff-seek --memory",
                Some(A),
                r#"{"memory": {"heap": 0.1, "total_rss": 0.2, "lower": 2, "cases": 2}}"#,
            ),
        ],
    );
    set(
        "run-t",
        &[
            result("graded/f", "grade", None, ""),
            result("graded/g", "grade", None, ""),
        ],
    );
    set(
        "other-t",
        &[result(
            "graded/f",
            "grade",
            Some(T),
            &both(0.7, "v", 0.3, 0.5, 3),
        )],
    );
    fs::write(root.join(ROWS), ROWS_JSON).unwrap();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(root.join(PAGE), PAGE_TEXT).unwrap();
    root
}

fn data(root: &Path) -> Vec<(String, Value)> {
    crate::walk::files(&root.join("harness"), &|rel| {
        rel.extension().is_some_and(|e| e == "json")
    })
    .into_iter()
    .map(|path| {
        let relative = path.strip_prefix(root).unwrap().display().to_string();
        (relative, json::read(&path).unwrap())
    })
    .collect()
}

fn edit(path: &Path, from: &str, to: &str) {
    let text = fs::read_to_string(path).unwrap();
    assert!(text.contains(from), "{from}");
    fs::write(path, text.replacen(from, to, 1)).unwrap();
}

#[test]
fn the_tables_are_rendered_from_the_results_their_rows_cite() {
    let root = repository("status-rendered");
    assert_eq!(check(&root, &data(&root)), Vec::<String>::new());
    // A cell that does not say what its results give fails, naming its line.
    edit(&root.join(PAGE), "0.9124 (4/4 pass", "0.9100 (4/4 pass");
    assert_eq!(
        check(&root, &data(&root)),
        ["docs/status.md:8: F Runtime is not what its results give: \
          0.9124 (4/4 pass, worst `y`) at `5b8dd88f`"]
    );
    // `status --write` writes what the rows render; open cells stay as written.
    assert_eq!(
        tables(&root, true).unwrap(),
        "docs/status.md: tables written"
    );
    assert_eq!(fs::read_to_string(root.join(PAGE)).unwrap(), PAGE_TEXT);
    let printed = tables(&root, false).unwrap();
    assert!(
        printed.starts_with("## Linux AArch64\n\n| Area | Verified"),
        "{printed}"
    );
    assert!(printed.contains("| F | 4 cases | 0.95 as written | heap as written |"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn open_cells_are_only_those_of_results_that_predate_the_measured_record() {
    let root = repository("status-open");
    let rows = root.join(ROWS);
    // A result that records its target is rendered from what it measured.
    let set = root.join(crate::evidence::SETS).join("run-t.json");
    edit(
        &set,
        r#""command": "grade", "key": "graded/f""#,
        &format!(r#""command": "grade", "target": "{T}", "key": "graded/f""#),
    );
    assert_eq!(
        check(&root, &data(&root)),
        [format!(
            "F on {T}: f/ records what it measured; render the cells and take the row out of \
             open_cells"
        )]
    );
    edit(&set, &format!(r#""target": "{T}", "#), "");
    // Without the open entry, a result that recorded nothing cannot be rendered.
    edit(
        &rows,
        r#"[{"area": "F", "targets": ["thumbv7neon-unknown-linux-gnueabihf"]}]"#,
        "[]",
    );
    let errors = check(&root, &data(&root));
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(errors[0].starts_with(&format!("F on {T}: f/ records nothing it measured")));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_row_cites_results_its_run_holds_once() {
    let root = repository("status-cited");
    let rows = root.join(ROWS);
    edit(
        &rows,
        r#""results": ["f", "g"]"#,
        r#""results": ["f", "gone"]"#,
    );
    assert_eq!(
        check(&root, &data(&root)),
        [
            format!("F on {A}: run run-a holds no result in gone/"),
            format!("F on {T}: run run-t holds no result in gone/"),
        ]
    );
    edit(
        &rows,
        r#""results": ["f", "gone"]"#,
        r#""results": ["f", "g"]"#,
    );
    // Two results measuring one directory's runtime need a grade to choose between them.
    let shard = root.join(crate::evidence::SETS).join("run-a-shard-2.json");
    edit(&shard, "isobmff-seek --memory", "isobmff-seek --benchmark");
    edit(
        &shard,
        r#"{"memory": {"heap": 0.1"#,
        r#"{"runtime": {"upper": 0.1, "worst": "w", "passing": 1, "cases": 1}, "memory": {"heap": 0.1"#,
    );
    assert_eq!(
        check(&root, &data(&root)),
        [format!(
            "Parts on {A}: h/: 2 results measure its runtime and no grade joins them"
        )]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_rows_name_one_table_per_gate_target_and_known_runs() {
    let parse = |text: &str| {
        Rows::parse(&json::parse(text).unwrap())
            .err()
            .unwrap_or_default()
    };
    let one_table = ROWS_JSON.replace(
        r#",
    {"heading": "32-bit ARMv7", "target": "thumbv7neon-unknown-linux-gnueabihf", "run": "run-t"}"#,
        "",
    );
    assert_eq!(
        parse(&one_table),
        [format!("{ROWS}: each gate target has one table")]
    );
    let unknown = ROWS_JSON.replace(r#""runs": ["other-t"]"#, r#""runs": ["run-a", "nowhere"]"#);
    assert_eq!(
        parse(&unknown),
        [format!(
            "{ROWS}: Parts: nowhere is not a run on one of its targets, one per target"
        )]
    );
    let unlabelled = ROWS_JSON.replace(r#""label": "`h`", "#, "");
    assert_eq!(
        parse(&unlabelled),
        [format!("{ROWS}: Parts: each of several parts has a label")]
    );
    let open_text = ROWS_JSON.replace(
        r#""open_cells": [{"area": "F""#,
        r#""open_cells": [{"area": "Core""#,
    );
    assert_eq!(
        parse(&open_text),
        [format!(
            "{ROWS}: open cells of Core: not a row with results"
        )]
    );
    assert!(parse(ROWS_JSON).is_empty());
}

#[test]
fn a_cited_run_is_kept_and_at_its_revision() {
    let root = repository("status-runs");
    let set = root.join(crate::evidence::SETS).join("other-t.json");
    edit(
        &set,
        r#""retention": "retained""#,
        r#""retention": "90 days", "source_revisions": ["0123456789abcdef"]"#,
    );
    assert_eq!(
        check(&root, &data(&root)),
        [
            format!("{ROWS}: run other-t: other-t is not kept"),
            format!("{ROWS}: run other-t: other-t records another source revision than 964b36cb"),
        ]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_cell_rounds_its_bounds_up() {
    assert_eq!(bound(0.9807), "0.9807");
    assert_eq!(bound(0.980_701), "0.9808");
    assert_eq!(bound(0.4), "0.4000");
}
