//! Checks of the registered differences that hold for every contract and list: a
//! `compat` policy an entry names exists, and a deviation pins both sides' folds.
//!
//! How a deviating case is judged when a comparison runs is in
//! [`crate::harness::report::ratchet`]; these check the registry itself.

use std::fs;
use std::path::{Path, PathBuf};

use crate::files;
use crate::json::Value;

/// Every `compat::` path the `compat` text of a registry entry names, in any
/// harness data file, that no workspace crate's `compat` module defines: one
/// error per file.
pub fn compat_errors(root: &Path, data: &[(String, Value)]) -> Vec<String> {
    let modules = compat_roots(root);
    let mut errors = Vec::new();
    for (file, document) in data {
        let mut missing = Vec::new();
        for path in compat_mentions(document) {
            if !modules.iter().any(|module| defines(module, &path)) && !missing.contains(&path) {
                missing.push(path);
            }
        }
        if !missing.is_empty() {
            errors.push(format!(
                "{file}: compat names what no crate's compat module defines (name an existing \
                 policy, or describe one not built without a path): {}",
                missing.join(", ")
            ));
        }
    }
    errors
}

/// The `compat::…` paths in every `compat` string of `value`, in order.
fn compat_mentions(value: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    match value {
        Value::Object(members) => {
            for (key, member) in members {
                match member {
                    Value::String(text) if key == "compat" => paths.extend(compat_paths(text)),
                    other => paths.extend(compat_mentions(other)),
                }
            }
        }
        Value::Array(items) => paths.extend(items.iter().flat_map(compat_mentions)),
        _ => {}
    }
    paths
}

/// The `compat::a::b` paths written in `text`.
fn compat_paths(text: &str) -> Vec<String> {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut paths = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("compat::") {
        let starts_word = !rest[..at].ends_with(ident);
        let tail = &rest[at..];
        let mut end = 0;
        for segment in tail.split("::") {
            let length = segment.find(|c: char| !ident(c)).unwrap_or(segment.len());
            if length == 0 {
                break;
            }
            end += length;
            if length < segment.len() || !tail[end..].starts_with("::") {
                break;
            }
            end += 2;
        }
        let path = tail[..end].trim_end_matches("::");
        if starts_word && path.contains("::") {
            paths.push(path.to_owned());
        }
        rest = &tail["compat::".len()..];
    }
    paths
}

/// Each crate's `compat` module: its file and the directory its submodules are in.
fn compat_roots(root: &Path) -> Vec<(PathBuf, PathBuf)> {
    let Ok(entries) = fs::read_dir(root.join("crates")) else {
        return Vec::new();
    };
    let mut roots: Vec<(PathBuf, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let src = entry.path().join("src");
            let directory = src.join("compat");
            [src.join("compat.rs"), directory.join("mod.rs")]
                .into_iter()
                .find(|file| file.is_file())
                .map(|file| (file, directory))
        })
        .collect();
    roots.sort();
    roots
}

/// Whether the module `(file, directory)` holds `path` (`compat::` and then its
/// submodules, ending in a submodule or an item one of their files defines).
fn defines((file, directory): &(PathBuf, PathBuf), path: &str) -> bool {
    let mut files = vec![file.clone()];
    let mut directory = directory.clone();
    let mut segments = path.split("::").skip(1).peekable();
    while let Some(segment) = segments.next() {
        let submodule = [
            directory.join(format!("{segment}.rs")),
            directory.join(segment).join("mod.rs"),
        ]
        .into_iter()
        .find(|file| file.is_file());
        let inline = files.iter().any(|file| {
            read(file).lines().any(|line| {
                let words = words(line);
                words.windows(2).any(|pair| pair == ["mod", segment]) && line.contains('{')
            })
        });
        if submodule.is_some() || inline {
            // An inline module's items are in the file that holds it.
            if let Some(file) = submodule {
                files = vec![file];
            }
            directory = directory.join(segment);
            continue;
        }
        // An item of the module: defined in its files or those of its submodules,
        // and named as the last segment or the type a variant follows.
        let below = files::walk(&directory, &|rel| {
            rel.extension().is_some_and(|e| e == "rs")
        });
        let last = segments.peek().is_none();
        return (last || segments.count() == 1)
            && files
                .iter()
                .chain(&below)
                .any(|file| read(file).lines().any(|line| defines_item(line, segment)));
    }
    true
}

fn read(file: &Path) -> String {
    fs::read_to_string(file).unwrap_or_default()
}

/// The identifiers of a line of Rust, in order.
fn words(line: &str) -> Vec<&str> {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
        .collect()
}

/// Whether `line` defines `name`: an item, an enum variant or a re-export.
fn defines_item(line: &str, name: &str) -> bool {
    const ITEMS: [&str; 9] = [
        "fn", "struct", "enum", "const", "static", "type", "trait", "mod", "union",
    ];
    let words = words(line);
    let item = words
        .windows(2)
        .any(|pair| ITEMS.contains(&pair[0]) && pair[1] == name);
    let variant = line.trim_start().strip_prefix(name).is_some_and(|rest| {
        matches!(
            rest.trim_start().chars().next(),
            None | Some(',' | '(' | '{' | '=')
        )
    });
    let reexport = words.starts_with(&["pub", "use"]) && words.contains(&name);
    item || variant || reexport
}

/// A feature contract whose deviating cases pin only the reference's fold: its id,
/// those cases, its deviating cases, and whether its comparison could pin the
/// candidate's fold (a `cargo xtask report` prefix comparison records it as each
/// case's `candidate_expected`).
pub struct OneSided {
    pub id: String,
    pub unpinned: usize,
    pub cases: usize,
    pub comparison_pins: bool,
}

/// Every feature contract with a deviating case whose candidate fold is not pinned.
pub fn one_sided(data: &[(String, Value)]) -> Vec<OneSided> {
    let document = |file: &str| {
        data.iter()
            .find(|(path, _)| path == file)
            .map(|(_, document)| document)
    };
    let mut found = Vec::new();
    for (file, contract) in data {
        let Some(id) = file
            .strip_prefix("harness/features/")
            .and_then(|rest| rest.strip_suffix("/contract.json"))
        else {
            continue;
        };
        let ids: Vec<&str> = contract
            .get("open_deviations")
            .and_then(Value::as_array)
            .unwrap_or_default()
            .iter()
            .flat_map(|entry| {
                entry
                    .get("cases")
                    .and_then(Value::as_array)
                    .unwrap_or_default()
            })
            .filter_map(Value::as_str)
            .collect();
        if ids.is_empty() {
            continue;
        }
        let rows = document(&format!("harness/features/{id}/corpus.json"))
            .and_then(|corpus| corpus.get("cases"))
            .and_then(Value::as_array)
            .unwrap_or_default();
        let pinned = |case: &str| {
            rows.iter().any(|row| {
                row.get("id").and_then(Value::as_str) == Some(case)
                    && (row.get("candidate_expected").is_some()
                        || row.get("candidate_expected_by_target").is_some())
            })
        };
        let unpinned = ids.iter().filter(|case| !pinned(case)).count();
        if unpinned > 0 {
            found.push(OneSided {
                id: id.to_owned(),
                unpinned,
                cases: ids.len(),
                comparison_pins: contract.get("runner").and_then(Value::as_str)
                    == Some("cargo xtask report")
                    && contract.get("worker_operation").is_some(),
            });
        }
    }
    found
}

/// The line `check-design` prints of [`one_sided`]: reported, not failed.
pub fn one_sided_summary(found: &[OneSided]) -> Option<String> {
    if found.is_empty() {
        return None;
    }
    let list = |pins: bool| {
        found
            .iter()
            .filter(|one| one.comparison_pins == pins)
            .map(|one| format!("{} ({}/{})", one.id, one.unpinned, one.cases))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let (recordable, other) = (list(true), list(false));
    let mut text = String::from(
        "Deviations not pinned on the candidate's side, reported (unpinned/deviating cases):",
    );
    if !recordable.is_empty() {
        text.push_str(&format!("\n  candidate folds to record: {recordable}"));
    }
    if !other.is_empty() {
        text.push_str(&format!(
            "\n  comparisons that pin the reference only: {other}"
        ));
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    #[test]
    fn compat_paths_are_read_from_prose() {
        assert_eq!(
            compat_paths("compat::mov::recovery TimingRunPolicy::BoundedPrefix admits it"),
            ["compat::mov::recovery"]
        );
        assert_eq!(
            compat_paths("a caller would use compat::probe::require_demux_score."),
            ["compat::probe::require_demux_score"]
        );
        assert_eq!(
            compat_paths("a compat::dts policy (not built), or recompat::x::y"),
            ["compat::dts"]
        );
        assert!(compat_paths("compat:: alone, or compat").is_empty());
    }

    #[test]
    fn a_compat_path_must_exist_in_a_crate() {
        let root = files::scratch("registry-compat");
        let compat = root.join("crates/demo/src/compat");
        fs::create_dir_all(compat.join("pes")).unwrap();
        fs::write(
            root.join("crates/demo/src/compat.rs"),
            "pub mod clock;\npub mod pes;\n",
        )
        .unwrap();
        fs::write(
            compat.join("clock.rs"),
            "pub fn signed_epoch() {}\npub mod seek {\n    pub fn landing() {}\n}\n",
        )
        .unwrap();
        fs::write(
            compat.join("pes/mod.rs"),
            "pub enum Policy {\n    DuplicatePayload,\n}\n",
        )
        .unwrap();
        let data = [(
            "harness/features/f/contract.json".to_owned(),
            parse(
                r#"{"open_deviations": [
                    {"compat": "compat::clock::signed_epoch"},
                    {"compat": "compat::clock::seek::landing"},
                    {"compat": "a consumer would use compat::pes::DuplicatePayload"},
                    {"compat": "compat::pes::Policy::DuplicatePayload, as built"},
                    {"compat": "compat::pes"},
                    {"compat": "compat::clock::unsigned_epoch for others"},
                    {"compat": "a compat::dts policy (not built)"}]}"#,
            )
            .unwrap(),
        )];
        assert_eq!(
            compat_errors(&root, &data),
            [
                "harness/features/f/contract.json: compat names what no crate's compat module \
              defines (name an existing policy, or describe one not built without a path): \
              compat::clock::unsigned_epoch, compat::dts"
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_deviation_without_its_candidate_fold_is_reported() {
        let contract = r#"{"runner": "cargo xtask report", "worker_operation": "v-",
            "open_deviations": [{"cases": ["a", "b"]}]}"#;
        let corpus = r#"{"cases": [{"id": "a", "expected": {}, "candidate_expected": {}},
            {"id": "b", "expected": {}}]}"#;
        let data = [
            (
                "harness/features/f/contract.json".to_owned(),
                parse(contract).unwrap(),
            ),
            (
                "harness/features/f/corpus.json".to_owned(),
                parse(corpus).unwrap(),
            ),
        ];
        let found = one_sided(&data);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].unpinned, found[0].cases), (1, 2));
        assert!(found[0].comparison_pins);
        assert_eq!(
            one_sided_summary(&found).unwrap(),
            "Deviations not pinned on the candidate's side, reported (unpinned/deviating \
             cases):\n  candidate folds to record: f (1/2)"
        );
        let pinned = r#"{"cases": [{"id": "a", "candidate_expected": {}},
            {"id": "b", "candidate_expected": {}}]}"#;
        let data = [
            data[0].clone(),
            (
                "harness/features/f/corpus.json".to_owned(),
                parse(pinned).unwrap(),
            ),
        ];
        assert!(one_sided(&data).is_empty());
    }
}
