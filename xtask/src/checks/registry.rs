//! Checks of the registry of differences ([`crate::harness::registry`]) that hold for
//! every registry file: a feature's contract, a container pin file and the codec
//! manifests. Every entry has the fields of its form; what it cites exists (a
//! `compat` policy, the place a rule is `documented`); every listed case is a case
//! and pins the candidate's output, and a case that compares nothing of the
//! reference pins none of the reference's.
//!
//! How a listed case is judged when a comparison runs is in
//! [`crate::harness::report::ratchet`]; these check the registry itself.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::files;
use crate::harness::registry::{self, Form};
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
                // A crate may also write its `compat` module inline in its root.
                .or_else(|| Some(src.join("lib.rs")).filter(|lib| inline_module(lib, "compat")))
                .map(|file| (file, directory))
        })
        .collect();
    roots.sort();
    roots
}

/// Whether `file` opens the module `name` inline (`mod name {`).
fn inline_module(file: &Path, name: &str) -> bool {
    read(file)
        .lines()
        .any(|line| words(line).windows(2).any(|pair| pair == ["mod", name]) && line.contains('{'))
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
        let inline = files.iter().any(|file| inline_module(file, segment));
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

/// The codec manifests' pins of both sides, by manifest path and lane.
const CODEC_PINS: &str = "harness/codecs/open-pins.json";

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value.get(key).and_then(Value::as_array).unwrap_or_default()
}

/// The feature a registry file under `harness/features/` belongs to, when `file` is
/// that feature's `name`.
fn feature<'a>(file: &'a str, name: &str) -> Option<&'a str> {
    file.strip_prefix("harness/features/")?
        .strip_suffix(name)?
        .strip_suffix('/')
        .filter(|id| !id.contains('/'))
}

/// Why the registry files of `data` (the parsed JSON files under `harness/` by
/// their path from the repository root) are not well formed, one error per file
/// and finding. A feature contract and a container pin file are read as the report
/// reads them, on every gate target ([`registry::read`]); a codec manifest keeps
/// fields of its own, so its entries are held to the fields of their form alone.
/// Then, for every listed case: its candidate pin is recorded, unless
/// `open_unpinned` registers why not; and a refusal, or a case not comparable with
/// the reference, carries no reference pin.
pub fn listing_errors(root: &Path, data: &[(String, Value)]) -> Vec<String> {
    let mut errors = Vec::new();
    for (file, document) in data {
        let found = if let Some(id) = feature(file, "contract.json") {
            contract_errors(root, id, document)
        } else if feature(file, "container-pins.json").is_some() {
            container_errors(document)
        } else if file.starts_with("harness/codecs/") {
            let pins = data
                .iter()
                .find(|(path, _)| path == CODEC_PINS)
                .and_then(|(_, pins)| pins.get("pins"))
                .and_then(|pins| pins.get(file));
            codec_errors(document, pins)
        } else {
            continue;
        };
        errors.extend(found.into_iter().map(|error| format!("{file}: {error}")));
        for (form, entry) in registry::groups(document) {
            if matches!(form, Form::Policy | Form::Unimplemented)
                && let Some(documented) = text(entry, "documented")
                && let Some(error) = documented_error(root, documented)
            {
                errors.push(format!("{file}: {}: {error}", form.list()));
            }
        }
    }
    errors
}

/// Whether `document` holds an entry in any list of the registry.
fn lists_cases(document: &Value) -> bool {
    Form::ALL
        .into_iter()
        .any(|form| !array(document, form.list()).is_empty())
        || !array(document, registry::UNPINNED).is_empty()
}

/// The errors of feature `id`'s contract against its manifest.
fn contract_errors(root: &Path, id: &str, contract: &Value) -> Vec<String> {
    if !lists_cases(contract) {
        return Vec::new();
    }
    let corpus = root.join("harness/features").join(id).join("corpus.json");
    let manifest = match crate::harness::manifest::read(root, &corpus) {
        Ok((manifest, _)) => manifest,
        Err(error) => return vec![format!("its listed cases need its manifest: {error}")],
    };
    let rows = array(&manifest, "cases");
    let row = |case: &str| rows.iter().find(|row| text(row, "id") == Some(case));
    // The targets the feature runs on: those it requires, else the gate's.
    let required: Vec<&str> = array(contract, "required_targets")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let targets = if required.is_empty() {
        crate::gate::TARGETS.to_vec()
    } else {
        required
    };
    let mut errors = BTreeSet::new();
    for target in targets {
        let listed = match registry::read(contract, &manifest, target) {
            Ok(listed) => listed,
            Err(error) => {
                errors.insert(error);
                continue;
            }
        };
        let (mut unpinned, mut registered, mut pinned_reference) = (0, 0, 0);
        for case in listed.ids() {
            let (Some(listing), Some(row)) = (listed.get(case), row(case)) else {
                continue;
            };
            let candidate = crate::harness::report::candidate_expected_for(row, target).is_some();
            unpinned += usize::from(!candidate && !listing.unpinned);
            registered += usize::from(candidate && listing.unpinned);
            pinned_reference += usize::from(
                listing.candidate_only
                    && crate::harness::report::expected_for(row, target).is_some(),
            );
        }
        for (count, finding) in [
            (
                unpinned,
                "listed cases pin no candidate fold; record it, or register in open_unpinned \
                 why it cannot be recorded yet",
            ),
            (
                registered,
                "cases of open_unpinned have a candidate pin; take them out of the list",
            ),
            (
                pinned_reference,
                "refusals, or cases not comparable with the reference, pin the reference's fold",
            ),
        ] {
            if count > 0 {
                errors.insert(format!("{count} {finding} ({target})"));
            }
        }
    }
    errors.into_iter().collect()
}

/// The errors of a container pin file: its lists scoped by the gate's targets,
/// its cases the keys of `pins`, its candidate pins those of `candidate_pins`.
fn container_errors(document: &Value) -> Vec<String> {
    let keys = |name: &str| -> Vec<&str> {
        match document.get(name) {
            Some(Value::Object(pins)) => pins.iter().map(|(id, _)| id.as_str()).collect(),
            _ => Vec::new(),
        }
    };
    let manifest = Value::object([(
        "cases",
        Value::Array(
            keys("pins")
                .into_iter()
                .map(|id| Value::object([("id", Value::str(id))]))
                .collect(),
        ),
    )]);
    // The kinds of difference an entry declares are the runner's, not the registry's.
    let mut declarations = vec![(
        "required_targets".to_owned(),
        Value::strings(crate::gate::TARGETS),
    )];
    for form in Form::GROUPS {
        let entries = array(document, form.list())
            .iter()
            .map(|entry| match entry {
                Value::Object(fields) => Value::Object(
                    fields
                        .iter()
                        .filter(|(key, _)| key != "differences")
                        .cloned()
                        .collect(),
                ),
                other => other.clone(),
            });
        declarations.push((form.list().to_owned(), Value::Array(entries.collect())));
    }
    let declarations = Value::Object(declarations);
    let candidates = keys("candidate_pins");
    let mut errors = BTreeSet::new();
    for target in crate::gate::TARGETS {
        match registry::read(&declarations, &manifest, target) {
            Ok(listed) => {
                let unpinned = listed
                    .ids()
                    .iter()
                    .filter(|id| !candidates.contains(&id.as_str()))
                    .count();
                if unpinned > 0 {
                    errors.insert(format!(
                        "{unpinned} listed cases have no entry in candidate_pins"
                    ));
                }
            }
            Err(error) => {
                errors.insert(error);
            }
        }
    }
    errors.into_iter().collect()
}

/// The errors of a codec manifest: each entry has the fields of its form, and each
/// vector an open case, a deviation or a policy difference lists has its pin, in
/// `pins` (this manifest's part of [`CODEC_PINS`], by lane) or, for the seek
/// comparisons, in its entry's `expected_by_case`.
fn codec_errors(document: &Value, pins: Option<&Value>) -> Vec<String> {
    let mut errors = Vec::new();
    let mut listed: Vec<&str> = array(document, Form::Defect.list())
        .iter()
        .filter_map(|case| case.as_str().or_else(|| text(case, "name")))
        .collect();
    let mut own_pins = false;
    for (form, entry) in registry::groups(document) {
        if form == Form::Unimplemented {
            // A codec refusal keeps the fields its runner reads; the registry adds
            // where the crate lists the feature as not implemented.
            let missing = ["reason", "documented", "clause", "detail"]
                .into_iter()
                .find(|key| text(entry, key).is_none_or(|line| line.trim().is_empty()));
            errors.extend(missing.map(|key| format!("{}: an entry states its {key}", form.list())));
        } else {
            errors.extend(registry::entry_error(form, entry, false));
        }
        if matches!(form, Form::Departure | Form::Policy) {
            let cases = array(entry, "cases").iter().filter_map(Value::as_str);
            match entry.get("expected_by_case") {
                Some(expected) => {
                    own_pins = true;
                    let missing = cases.filter(|case| expected.get(case).is_none()).count();
                    if missing > 0 {
                        errors.push(format!(
                            "{}: {missing} cases have no observation in expected_by_case",
                            form.list()
                        ));
                    }
                }
                None => listed.extend(cases),
            }
        }
    }
    let pinned = |case: &str| match pins {
        Some(Value::Object(lanes)) => lanes.iter().any(|(_, lane)| lane.get(case).is_some()),
        _ => false,
    };
    // An open seek case states its own expectation.
    let unpinned = if own_pins && document.get("vectors").is_none() {
        0
    } else {
        listed.iter().filter(|case| !pinned(case)).count()
    };
    if unpinned > 0 {
        errors.push(format!(
            "{unpinned} listed vectors have no pin in {CODEC_PINS}; record them with \
             `cargo xtask codecs summarize RUN OUT --record-pins`"
        ));
    }
    errors
}

/// Why `documented` does not name a place a consumer reads a rule: a section of a
/// crate README (`crates/NAME/README.md#anchor`, the anchor of one of its headings)
/// or a public item of a workspace crate (`triergon_core::signature::select`).
fn documented_error(root: &Path, documented: &str) -> Option<String> {
    if let Some((file, anchor)) = documented.split_once('#') {
        let readme = file.starts_with("crates/") && file.ends_with("/README.md");
        let Some(text) = readme
            .then(|| fs::read_to_string(root.join(file)).ok())
            .flatten()
        else {
            return Some(format!("documented: {file} is not a crate README"));
        };
        let mut fenced = false;
        let found = text.lines().any(|line| {
            fenced ^= line.starts_with("```");
            !fenced && line.starts_with('#') && slug(line.trim_start_matches('#')) == anchor
        });
        return (!found).then(|| format!("documented: {file} has no section #{anchor}"));
    }
    let (Some((package, _)), Some(item)) =
        (documented.split_once("::"), documented.rsplit("::").next())
    else {
        return Some(format!(
            "documented: {documented} is neither crates/NAME/README.md#section nor a public item"
        ));
    };
    let Some(source) = crate_sources(root, package) else {
        return Some(format!("documented: no workspace crate is named {package}"));
    };
    let defined = files::walk(&source, &|rel| rel.extension().is_some_and(|e| e == "rs"))
        .iter()
        .any(|file| {
            read(file).lines().any(|line| {
                // An enum variant has no visibility of its own.
                defines_item(line, item)
                    && (line.contains("pub") || line.trim_start().starts_with(item))
            })
        });
    (!defined).then(|| format!("documented: {package} defines no public item {item}"))
}

/// The anchor of a Markdown heading: lower case, spaces as hyphens, punctuation
/// dropped.
fn slug(heading: &str) -> String {
    heading
        .trim()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            '-' | '_' => Some(c),
            c if c.is_alphanumeric() => Some(c.to_ascii_lowercase()),
            _ => None,
        })
        .collect()
}

/// The `src` directory of the workspace crate whose library is named `package`
/// (its package name with `_` for `-`).
fn crate_sources(root: &Path, package: &str) -> Option<PathBuf> {
    fs::read_dir(root.join("crates"))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|directory| {
            fs::read_to_string(directory.join("Cargo.toml"))
                .ok()
                .and_then(|manifest| crate::toml::parse(&manifest).ok())
                .and_then(|manifest| {
                    Some(
                        manifest
                            .get("package")?
                            .get("name")?
                            .as_str()?
                            .replace('-', "_"),
                    )
                })
                .is_some_and(|name| name == package)
        })
        .map(|directory| directory.join("src"))
}

/// The line `check-design` prints of the cases `open_unpinned` registers: the
/// features whose listed cases still wait for a candidate pin, with their causes.
pub fn unpinned_summary(data: &[(String, Value)]) -> Option<String> {
    let lines: Vec<String> = data
        .iter()
        .filter_map(|(file, document)| {
            let id = feature(file, "contract.json")?;
            let entries = array(document, registry::UNPINNED);
            let cases: usize = entries.iter().map(|e| array(e, "cases").len()).sum();
            let causes: BTreeSet<&str> = entries.iter().filter_map(|e| text(e, "cause")).collect();
            (cases > 0).then(|| {
                format!(
                    "  {id}: {cases} ({})",
                    causes.into_iter().collect::<Vec<_>>().join("; ")
                )
            })
        })
        .collect();
    (!lines.is_empty()).then(|| {
        format!(
            "Listed cases whose candidate fold is not yet recorded, as open_unpinned registers:\n{}",
            lines.join("\n")
        )
    })
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
    fn a_compat_module_written_inline_in_the_crate_root_is_found() {
        let root = files::scratch("registry-compat-inline");
        fs::create_dir_all(root.join("crates/demo/src")).unwrap();
        fs::write(
            root.join("crates/demo/src/lib.rs"),
            "pub mod compat {\n    pub mod layout {\n        pub enum Policy {\n            \
             Specification,\n            ConventionalSurround,\n        }\n    }\n}\n",
        )
        .unwrap();
        let data = [(
            "harness/features/f/contract.json".to_owned(),
            parse(
                r#"{"open_deviations": [
                    {"compat": "demo::compat::layout::Policy::ConventionalSurround"},
                    {"compat": "compat::layout::Downmix"}]}"#,
            )
            .unwrap(),
        )];
        assert_eq!(
            compat_errors(&root, &data),
            [
                "harness/features/f/contract.json: compat names what no crate's compat module \
              defines (name an existing policy, or describe one not built without a path): \
              compat::layout::Downmix"
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_documented_rule_names_a_readme_section_or_a_public_item() {
        let root = files::scratch("registry-documented");
        let demo = root.join("crates/demo");
        fs::create_dir_all(demo.join("src")).unwrap();
        fs::write(
            demo.join("Cargo.toml"),
            "[package]\nname = \"triergon-demo\"\n",
        )
        .unwrap();
        fs::write(
            demo.join("README.md"),
            "# Demo\n\n## Recovery from `bad` input\n\n```sh\n# Not a heading\n```\n",
        )
        .unwrap();
        fs::write(
            demo.join("src/lib.rs"),
            "pub fn probe() {}\nfn hidden() {}\npub enum Probed {\n    Need(usize),\n}\n",
        )
        .unwrap();
        for (documented, holds) in [
            ("crates/demo/README.md#recovery-from-bad-input", true),
            ("crates/demo/README.md#not-a-heading", false),
            ("crates/demo/README.md#limits", false),
            ("docs/quality.md#gates", false),
            ("triergon_demo::probe", true),
            ("triergon_demo::Probed::Need", true),
            ("triergon_demo::hidden", false),
            ("triergon_other::probe", false),
            ("the README says so", false),
        ] {
            assert_eq!(
                documented_error(&root, documented).is_none(),
                holds,
                "{documented}"
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    fn feature_files(contract: &str, corpus: &str) -> (PathBuf, Vec<(String, Value)>) {
        let root = files::scratch("registry-listing");
        let directory = root.join("harness/features/f");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("contract.json"), contract).unwrap();
        fs::write(directory.join("corpus.json"), corpus).unwrap();
        let data = vec![(
            "harness/features/f/contract.json".to_owned(),
            parse(contract).unwrap(),
        )];
        (root, data)
    }

    const DEVIATION: &str = r#""open_deviations": [{"reading": "r", "clause": "c",
        "candidate": "x", "reference": "y", "cases": ["a", "b"]}]"#;

    #[test]
    fn a_listed_case_without_its_candidate_fold_fails_unless_registered() {
        let contract = |more: &str| {
            format!(r#"{{"required_targets": ["aarch64-unknown-linux-gnu"], {DEVIATION}{more}}}"#)
        };
        let corpus = r#"{"cases": [{"id": "a", "expected": {}, "candidate_expected": {}},
            {"id": "b", "expected": {}}]}"#;
        let (root, data) = feature_files(&contract(""), corpus);
        let errors = listing_errors(&root, &data);
        assert!(
            errors.len() == 1 && errors[0].contains("1 listed cases pin no candidate fold"),
            "{errors:?}"
        );
        fs::remove_dir_all(root).unwrap();
        // The missing pin is registered with its cause, on the target it is due on.
        let registered = contract(
            r#", "open_unpinned": [{"cases": ["b"], "cause": "recorded on the gate host"}]"#,
        );
        let (root, data) = feature_files(&registered, corpus);
        assert_eq!(listing_errors(&root, &data), Vec::<String>::new());
        assert_eq!(
            unpinned_summary(&data).unwrap(),
            "Listed cases whose candidate fold is not yet recorded, as open_unpinned \
             registers:\n  f: 1 (recorded on the gate host)"
        );
        fs::remove_dir_all(root).unwrap();
        // Once recorded, the case leaves the list.
        let pinned = r#"{"cases": [{"id": "a", "expected": {}, "candidate_expected": {}},
            {"id": "b", "expected": {}, "candidate_expected": {}}]}"#;
        let (root, data) = feature_files(&registered, pinned);
        let errors = listing_errors(&root, &data);
        assert!(
            errors
                .iter()
                .all(|e| e.contains("take them out of the list"))
                && !errors.is_empty(),
            "{errors:?}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_refusal_pins_nothing_of_the_reference() {
        let contract = r#"{"required_targets": ["aarch64-unknown-linux-gnu"],
            "open_refusals": [{"reason": "r", "clause": "c", "class": "unsupported",
            "detail": "d", "cases": ["a"]}]}"#;
        let (root, data) = feature_files(
            contract,
            r#"{"cases": [{"id": "a", "expected": {}, "candidate_expected": {}}]}"#,
        );
        let errors = listing_errors(&root, &data);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("pin the reference's fold")),
            "{errors:?}"
        );
        fs::remove_dir_all(root).unwrap();
        let (root, data) = feature_files(
            contract,
            r#"{"cases": [{"id": "a", "candidate_expected": {}}]}"#,
        );
        assert_eq!(listing_errors(&root, &data), Vec::<String>::new());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_container_pin_file_and_a_codec_manifest_follow_the_same_rules() {
        let root = files::scratch("registry-other-files");
        let entry = r#"{"reading": "r", "clause": "c", "candidate": "x", "reference": "y",
            "differences": ["pts"], "cases": ["a", "b"]}"#;
        let container = |candidates: &str| {
            (
                "harness/features/f/container-pins.json".to_owned(),
                parse(&format!(
                    r#"{{"open_deviations": [{entry}], "pins": {{"a": "1", "b": "2"}},
                        "candidate_pins": {candidates}}}"#
                ))
                .unwrap(),
            )
        };
        assert_eq!(
            listing_errors(&root, &[container(r#"{"a": "3", "b": "4"}"#)]),
            Vec::<String>::new()
        );
        assert_eq!(
            listing_errors(&root, &[container(r#"{"a": "3"}"#)]),
            [
                "harness/features/f/container-pins.json: 1 listed cases have no entry in candidate_pins"
            ]
        );
        let manifest = |list: &str| {
            (
                "harness/codecs/m.json".to_owned(),
                parse(&format!(
                    r#"{{"vectors": [], "open_cases": ["v"], {list}: [{{"reading": "r",
                        "clause": "c", "candidate": "x", "reference": "y", "cases": ["w"]}}]}}"#
                ))
                .unwrap(),
            )
        };
        let pins = |vectors: &str| {
            (
                CODEC_PINS.to_owned(),
                parse(&format!(
                    r#"{{"pins": {{"harness/codecs/m.json": {{"lane": {vectors}}}}}}}"#
                ))
                .unwrap(),
            )
        };
        let deviations = manifest(r#""open_deviations""#);
        assert_eq!(
            listing_errors(&root, &[deviations.clone(), pins(r#"{"v": {}, "w": {}}"#)]),
            Vec::<String>::new()
        );
        let errors = listing_errors(&root, &[deviations, pins(r#"{"v": {}}"#)]);
        assert!(
            errors.len() == 1 && errors[0].contains("1 listed vectors have no pin"),
            "{errors:?}"
        );
        // A deviation's fields are not a policy difference's.
        let errors = listing_errors(
            &root,
            &[
                manifest(r#""open_policies""#),
                pins(r#"{"v": {}, "w": {}}"#),
            ],
        );
        assert!(
            errors.len() == 1 && errors[0].contains("open_policies"),
            "{errors:?}"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
