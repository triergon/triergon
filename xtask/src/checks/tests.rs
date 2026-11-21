use super::*;

#[test]
fn cites_hexadecimal_code_spans_of_commit_length() {
    let text = "at `ceb5076` and `03f3efd3a0fde0d558c68f5408c521d632802fb7`, again \
                    `ceb5076`; FFmpeg `d32b387f`; not `abc123`, `ABCDEF0`, `deadbeefx`, \
                    `1000 ms`, a digest `6c0cc3a669152b3eeba9a1189939c53b1b68e2f5cb9ab6cb81f903ec4358634d` \
                    or text between `spans` cafe1234 `here`";
    assert_eq!(
        cited_revisions(text, &["d32b387f2b0a484599d4587d651891f0c63c4238"]),
        ["ceb5076", "03f3efd3a0fde0d558c68f5408c521d632802fb7"]
    );
}

#[test]
fn pairs_backtick_runs_of_the_same_length() {
    let text = "at `` abc1234 `` and `def1234`, with ``embedded `bad1234` ticks`` and `cafe1234";
    assert_eq!(cited_revisions(text, &[]), ["abc1234", "def1234"]);
    assert_eq!(
        cited_revisions("`` unmatched before `abc1234`", &[]),
        ["abc1234"]
    );
}

#[test]
fn escaped_openers_leave_later_citations_visible() {
    assert_eq!(
        cited_revisions(r"Literal \` marker. Measured at `deadbee`.", &[]),
        ["deadbee"]
    );
    assert_eq!(cited_revisions(r"Slash \\`deadbee`.", &[]), ["deadbee"]);
    assert_eq!(cited_revisions(r"Literal \```deadbee``.", &[]), ["deadbee"]);
}

#[test]
fn unclosed_delimiters_do_not_cross_status_blocks() {
    let text = "Unclosed ` marker.\n \nMeasured at `deadbee`.\n\n\
                    | Result | Revision |\n| --- | --- |\n\
                    | Unclosed ` marker | `def1234` |\n\
                    | Unclosed ` marker |\n| `abc1234` |";
    assert_eq!(
        cited_revisions(text, &[]),
        ["deadbee", "def1234", "abc1234"]
    );
    assert_eq!(
        cited_revisions("Measured at `\ndeadbee`.", &[]),
        ["deadbee"]
    );
}

#[test]
fn table_cells_can_omit_outer_pipes() {
    let text = "| Result | Revision |\n| :--- | ---: |\n\
                    Literal ` marker | `deadbee` |\n\
                    Escaped \\| literal ` marker | `abc1234`";
    assert_eq!(cited_revisions(text, &[]), ["deadbee", "abc1234"]);
}

#[test]
fn escaped_table_pipes_are_consumed_before_inline_escapes() {
    let text = concat!(
        "| Result | Notes |\n",
        "| --- | --- |\n",
        r"| `x\\|` measured at `deadbee` | ok |"
    );
    assert_eq!(cited_revisions(text, &[]), ["deadbee"]);
}

#[test]
fn atx_heading_can_interrupt_a_paragraph() {
    assert_eq!(
        cited_revisions("Unclosed ` marker.\n## Measured `deadbee`", &[]),
        ["deadbee"]
    );
    assert_eq!(
        cited_revisions("Start `\n#tag ` measured at `deadbee`.", &[]),
        ["deadbee"]
    );
}

#[test]
fn finds_link_targets_like_the_markdown_pattern() {
    assert_eq!(
        link_targets("[a](b.md) [c](d#e) ]() [f](g"),
        vec!["b.md", "d#e"]
    );
    assert!(link_targets("[x]()(y)").is_empty());
    assert_eq!(link_targets("[x](a\nb)"), vec!["a\nb"]);
}

#[test]
fn admits_one_worker_entry_point_in_a_denying_crate_root() {
    let entry = "#![deny(unsafe_code)]\n#![cfg_attr(not(test), no_main)]\n\
                     #[allow(unsafe_code)]\n#[unsafe(no_mangle)]\nextern \"C\" fn main() -> i32 { 0 }\n";
    assert!(!has_unsafe_syntax(&without_worker_entry(entry).unwrap()));
    assert!(without_worker_entry(&entry.replace("deny", "warn")).is_err());
    let twice = format!("{entry}{}", &entry[entry.find("#[allow").unwrap()..]);
    assert!(without_worker_entry(&twice).is_err());
    let exported = entry.replace("#[allow(unsafe_code)]\n", "");
    assert!(has_unsafe_syntax(&without_worker_entry(&exported).unwrap()));
}

#[test]
fn reads_lint_tables_without_comments() {
    let manifest = "[package]\nname = \"x\"\n\n[lints.rust]\n# why\nunsafe_code = \"deny\"\n\n\
                        [lints.clippy]\nall = \"warn\"\n\n[features]\nf = []\n";
    assert_eq!(
        lint_table(manifest, "[lints."),
        "[lints.rust]\nunsafe_code = \"deny\"\n[lints.clippy]\nall = \"warn\"\n"
    );
}

#[test]
fn detects_unsafe_syntax_only_at_word_starts() {
    for text in [
        "unsafe { x }",
        "unsafe impl Send",
        "pub unsafe fn f()",
        "unsafe\n  extern \"C\"",
        "unsafe trait T",
        "#[unsafe(no_mangle)]",
        "#[unsafe (export_name = \"f\")]",
    ] {
        assert!(has_unsafe_syntax(text), "{text}");
    }
    for text in [
        "unsafe_code = forbid",
        "#![forbid(unsafe_code)]",
        "not_unsafe {",
        "unsafety fn",
        "unsafe code",
    ] {
        assert!(!has_unsafe_syntax(text), "{text}");
    }
}

#[test]
fn design_names_each_contract_with_an_invalid_timeout() {
    let root = walk::scratch("contract-fields");
    for (id, contract) in [
        ("absent", r#"{"id": "absent"}"#),
        ("long", r#"{"id": "long", "timeout_seconds": 120}"#),
        ("zero", r#"{"id": "zero", "timeout_seconds": 0}"#),
        ("text", r#"{"id": "text", "timeout_seconds": "120"}"#),
    ] {
        let directory = root.join("harness/features").join(id);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("contract.json"), contract).unwrap();
    }
    let errors = contract_fields(&root);
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(errors[0].starts_with("harness/features/text/contract.json: timeout_seconds"));
    assert!(errors[1].starts_with("harness/features/zero/contract.json: timeout_seconds"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn reports_missing_links_outside_build_output() {
    let root = walk::scratch("links");
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::create_dir_all(root.join("target")).unwrap();
    fs::write(
        root.join("docs/a.md"),
        "[ok](b.md#x) [web](https://x) [gone](missing.md) [self](#top)",
    )
    .unwrap();
    fs::write(root.join("docs/b.md"), "").unwrap();
    fs::write(root.join("target/t.md"), "[gone](missing.md)").unwrap();
    assert_eq!(links(&root), vec!["docs/a.md: missing link missing.md"]);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn link_components_must_match_case_exactly() {
    let root = walk::scratch("links-case");
    fs::create_dir_all(root.join("docs/Sub")).unwrap();
    fs::write(root.join("docs/Sub/Note.md"), "").unwrap();
    fs::write(root.join("README.md"), "").unwrap();
    fs::write(
            root.join("docs/a.md"),
            "[ok](Sub/Note.md) [up](../README.md) [dir](./Sub) [file](sub/Note.md) [name](Sub/note.md) [parent](../readme.md)",
        )
        .unwrap();
    assert_eq!(
        links(&root),
        vec![
            "docs/a.md: missing link sub/Note.md",
            "docs/a.md: missing link Sub/note.md",
            "docs/a.md: missing link ../readme.md",
        ]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn design_reports_invalid_json_and_gate_changes() {
    let root = walk::scratch("design");
    fs::create_dir_all(root.join("harness/sub")).unwrap();
    fs::create_dir_all(root.join("harness/fate")).unwrap();
    fs::create_dir_all(root.join("harness/features")).unwrap();
    let policy = r#"{"runtime": {"ratio_max": 1.2, "optimisation_target": 0.8, "minimum_pairs": 30},
            "memory": {"peak_live_heap_ratio_max_exclusive": 1.0,
                       "total_resident_peak_rss_ratio_max_exclusive": 1.0},
            "qualification_targets": ["thumbv7neon-unknown-linux-gnueabihf", "aarch64-unknown-linux-gnu"]}"#;
    fs::write(root.join("harness/policy.json"), policy).unwrap();
    fs::write(
        root.join("harness/fate/index.json"),
        r#"{"cases": [{"status": "unmapped", "rights": "unreviewed"}]}"#,
    )
    .unwrap();
    stated_policy(&root);
    fs::write(
        root.join(crate::exceptions::NOT_COMPARED),
        r#"{"crates": {}}"#,
    )
    .unwrap();
    let missing = design(&root).unwrap_err();
    assert_eq!(
        missing,
        ["harness/gate-exceptions.json: no recorded counts"]
    );
    let counts = crate::exceptions::counted(&[]);
    fs::write(
        root.join(crate::exceptions::FILE),
        json::to_python(&Value::object([("counts", counts)]), None),
    )
    .unwrap();
    let ok = design(&root).unwrap();
    assert!(
        ok.starts_with("Design integrity OK: 4 harness data files, 1 unmapped FATE identities.")
    );
    fs::write(root.join("harness/sub/bad.json"), "{").unwrap();
    fs::write(
        root.join("harness/policy.json"),
        policy.replace("1.2,", "1.25,"),
    )
    .unwrap();
    fs::write(
        root.join("harness/fate/index.json"),
        r#"{"cases": [{"status": "mapped", "rights": "unreviewed"}]}"#,
    )
    .unwrap();
    let errors = design(&root).unwrap_err();
    assert_eq!(errors.len(), 3, "{errors:?}");
    assert!(errors[0].starts_with("harness/sub/bad.json: "));
    assert!(errors.contains(&"docs/quality.md: does not state the policy value 1.25".into()));
    assert!(errors.contains(&"FATE index misrepresents coverage/rights".into()));
    fs::remove_dir_all(root).unwrap();
}

/// The quality guide stating the policy of 1.20 and 0.80, and one passing status row.
fn stated_policy(root: &Path) {
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(
        root.join("docs/quality.md"),
        "Runtime at most **1.20** per case. The target is **0.80**.\n",
    )
    .unwrap();
    fs::write(
        root.join("docs/status.md"),
        "| Area | Runtime | Open |\n|---|---|---|\n| HLS | 1.1808 (71/71 pass) | — |\n",
    )
    .unwrap();
}

#[test]
fn the_runtime_ceiling_is_stated_once_and_bounds_every_status_row() {
    let root = walk::scratch("ceiling");
    fs::create_dir_all(root.join("harness")).unwrap();
    fs::write(
        root.join("harness/policy.json"),
        r#"{"runtime": {"ratio_max": 1.2, "optimisation_target": 0.8}}"#,
    )
    .unwrap();
    stated_policy(&root);
    fs::write(root.join("README.md"), "Measured at 1.2041 and 11.20.\n").unwrap();
    assert_eq!(runtime_mentions(&root), Vec::<String>::new());
    fs::write(root.join("README.md"), "At most 1.20× per case.\n").unwrap();
    fs::write(
            root.join("docs/status.md"),
            "| Area | Runtime |\n|---|---|\n| HLS | 1.2125 (71/71 pass) |\n\
             | Framing | 1.3846 (45/125 pass for AVC/HEVC) |\n| PGS | PGS 1.11 |\n| TS | not measured |\n\
             | CC | 0.6557 (461/461 pass); runs take 1.300 of main's instructions (main at `1.5`) |\n\
             \nOn ARMv7:\n\n| Area | Verified | Runtime |\n|---|---|---|\n\
             | ISO | 956/956 | 1.2500 (955/956 timed operations pass, median 1.3) |\n\
             | Playback | 58/58 | MP4 forward 1.2500 (51/51 pass, worst `a`), raw Annex B 0.6286 (7/7 pass) |\n\
             | Text | 390/390 | `text` 0.8008 (367/367 pass)<br>`text-host` 1.3000 (23/23 timed operations pass) |\n",
        )
        .unwrap();
    assert_eq!(
        runtime_mentions(&root),
        [
            "README.md: restates the runtime ceiling 1.20; refer to docs/quality.md",
            "docs/status.md: HLS runtime 1.2125 is above the ceiling 1.20, \
                 but the row does not say which cases fail",
            "docs/status.md: Playback runtime 1.2500 is above the ceiling 1.20, \
                 but the row does not say which cases fail",
            "docs/status.md: Text runtime 1.3000 is above the ceiling 1.20, \
                 but the row does not say which cases fail",
        ]
    );
    fs::remove_dir_all(root).unwrap();
}
