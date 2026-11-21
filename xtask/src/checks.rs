//! Repository integrity checks: local Markdown links, harness data files and the
//! safe-Rust, dependency-free workspace rule.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::json::{self, Value};
use crate::walk;

/// Every relative link target in a tracked-style Markdown file must exist.
pub fn links(root: &Path) -> Vec<String> {
    let mut errors = Vec::new();
    for path in walk::files(root, &|rel| rel.extension().is_some_and(|e| e == "md")) {
        let rel = path.strip_prefix(root).expect("walked under root");
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                errors.push(format!("{}: {error}", rel.display()));
                continue;
            }
        };
        for target in link_targets(&text) {
            if target.contains("://") || target.starts_with('#') {
                continue;
            }
            let file = target.split('#').next().unwrap_or_default();
            if !exists_exactly(path.parent().expect("file has a parent"), Path::new(file)) {
                errors.push(format!("{}: missing link {target}", rel.display()));
            }
        }
    }
    errors
}

/// Whether `relative` names an existing entry under `base` with exactly the spelling
/// of every component, so a link that only resolves on a case-insensitive file system
/// is reported on every platform.
fn exists_exactly(base: &Path, relative: &Path) -> bool {
    use std::path::Component;
    let mut current = base.to_path_buf();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !current.is_dir() || !current.pop() {
                    return false;
                }
            }
            Component::Normal(name) => {
                let Ok(entries) = fs::read_dir(&current) else {
                    return false;
                };
                if !entries.flatten().any(|entry| entry.file_name() == name) {
                    return false;
                }
                current.push(name);
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    current.exists()
}

/// Targets of `](target)`: the shortest non-empty run up to the next `)`.
pub fn link_targets(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(found) = text[from..].find("](") {
        let start = from + found + 2;
        match text[start..].find(')') {
            Some(0) => from = start - 1,
            Some(len) => {
                out.push(&text[start..start + len]);
                from = start + len + 1;
            }
            None => break,
        }
    }
    out
}

/// Harness data parses, and recorded gates keep their declared values.
pub fn design(root: &Path) -> Result<String, Vec<String>> {
    let mut errors = Vec::new();
    let mut data = Vec::new();
    for path in walk::files(&root.join("harness"), &|rel| {
        rel.extension().is_some_and(|e| e == "json")
    }) {
        let rel = path.strip_prefix(root).expect("walked under root");
        match json::read(&path) {
            Ok(document) => data.push((rel.display().to_string(), document)),
            Err(error) => errors.push(format!("{}: {error}", rel.display())),
        }
    }
    errors.extend(runtime_mentions(root));
    errors.extend(policy_values(root));
    errors.extend(contract_fields(root));
    let document = |file: &str| {
        data.iter()
            .find(|(path, _)| path == file)
            .map(|(_, document)| document)
    };
    errors.extend(crate::exceptions::check(
        document(crate::exceptions::FILE),
        &data,
    ));
    errors.extend(crate::exceptions::not_compared_errors(
        root,
        document(crate::exceptions::NOT_COMPARED),
    ));
    errors.extend(crate::status::check(root, &data));
    errors.extend(crate::test_skips::design_errors(&data));
    // Generated manifests: the case ids and the pins match, in the recorded form.
    for generator in crate::generators::GENERATORS {
        errors.extend(generator.check(root));
    }
    let fate = data
        .iter()
        .find(|(path, _)| path == "harness/fate/index.json")
        .and_then(|(_, index)| index.get("cases"))
        .and_then(Value::as_array);
    let Some(cases) = fate else {
        errors.push("harness/fate/index.json: no FATE cases".into());
        return Err(errors);
    };
    let mapped = |c: &Value| {
        c.get("status").and_then(Value::as_str) != Some("unmapped")
            || c.get("rights").and_then(Value::as_str) != Some("unreviewed")
    };
    if cases.iter().any(mapped) {
        errors.push("FATE index misrepresents coverage/rights".into());
    }
    if errors.is_empty() {
        Ok(format!(
            "Design integrity OK: {} harness data files, {} unmapped FATE identities.\n\
             This is not a feature-parity, performance, memory or FATE pass.",
            data.len(),
            cases.len()
        ))
    } else {
        Err(errors)
    }
}

/// Why a contract's bound exceptions are invalid against its registered corpus.
fn open_bounds_error(root: &Path, contract_path: &Path, contract: &Value) -> Option<String> {
    if contract.get("open_bounds").is_none() && contract.get("open_unmeasured").is_none() {
        return None;
    }
    let corpus = contract_path.with_file_name("corpus.json");
    let manifest = match crate::harness::manifest::read(root, &corpus) {
        Ok((manifest, _)) => manifest,
        Err(error) => {
            return Some(format!(
                "bound exceptions need the registered corpus: {error}"
            ));
        }
    };
    crate::gate::TARGETS.iter().find_map(|target| {
        crate::harness::bounds::open_bounds(contract, &manifest, target)
            .err()
            .or_else(|| crate::harness::bounds::Unmeasured::read(contract, &manifest, target).err())
    })
}

/// The fields every feature contract (`harness/features/<id>/contract.json`) may
/// declare that the harness reads the same way for all of them: `timeout_seconds`,
/// `measurement_reference`, oracle pins and case-specific bound exceptions.
fn contract_fields(root: &Path) -> Vec<String> {
    let features = root.join("harness/features");
    walk::files(&features, &|rel| rel.ends_with("contract.json"))
        .into_iter()
        .flat_map(|path| {
            let rel = path
                .strip_prefix(root)
                .expect("walked under root")
                .display()
                .to_string();
            let errors = match json::read(&path) {
                Ok(contract) => [
                    crate::harness::worker::contract_timeout(&contract)
                        .and(crate::harness::worker::contract_parity_timeout(&contract))
                        .err(),
                    crate::harness::build::measurement_reference_error(&contract),
                    open_bounds_error(root, &path, &contract),
                    contract.get("oracle").and_then(|_| {
                        crate::harness::oracle_compare::kind(&contract)
                            .err()
                            .or_else(|| {
                                let manifest = json::read(&path.with_file_name("corpus.json"));
                                manifest
                                    .and_then(|manifest| {
                                        crate::harness::manifest::validate_required(
                                            &contract, &manifest,
                                        )
                                    })
                                    .err()
                            })
                    }),
                ]
                .into_iter()
                .flatten()
                .collect(),
                Err(error) => vec![error],
            };
            errors
                .into_iter()
                .map(move |error| format!("{rel}: {error}"))
        })
        .collect()
}

/// What `harness/policy.json` states beside the runtime gate agrees with the code that
/// applies it: the memory ceilings admit nothing at or above the reference, and the
/// pairs and targets it names are the ones the gate runs.
fn policy_values(root: &Path) -> Vec<String> {
    use crate::harness::policy::Memory;
    let mut errors = Vec::new();
    match Memory::load(root) {
        Ok(memory) => {
            if !(0.0 < memory.ceiling() && memory.heap <= 1.0 && memory.total_rss <= 1.0) {
                errors.push(
                    "harness/policy.json: a memory ceiling must lie above 0 and not above 1, \
                     the reference"
                        .into(),
                );
            }
        }
        Err(error) => errors.push(error),
    }
    let policy = json::read(&root.join("harness/policy.json")).unwrap_or(Value::Null);
    let pairs = policy
        .get("runtime")
        .and_then(|runtime| runtime.get("minimum_pairs"))
        .and_then(Value::as_f64);
    let gate_pairs = crate::harness::timing::GATE_PAIRS;
    #[allow(clippy::float_cmp)] // A small whole number.
    if pairs != Some(gate_pairs as f64) {
        errors.push(format!(
            "harness/policy.json: runtime.minimum_pairs is not the gate's {gate_pairs} pairs"
        ));
    }
    let targets: Vec<&str> = policy
        .get("qualification_targets")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    if targets != crate::gate::TARGETS {
        errors.push(format!(
            "harness/policy.json: qualification_targets are not the gate's targets {:?}",
            crate::gate::TARGETS
        ));
    }
    errors
}

/// `harness/policy.json` is the only definition of the runtime ceiling and target.
/// `docs/quality.md` states both once, and no other document restates the ceiling. A
/// row of `docs/status.md` above the ceiling must say how many of its cases pass, and
/// not all of them, so lowering the ceiling flags every result that no longer holds.
fn runtime_mentions(root: &Path) -> Vec<String> {
    let gate = match crate::harness::policy::Runtime::load(root) {
        Ok(gate) => gate,
        Err(error) => return vec![error],
    };
    let mut errors = Vec::new();
    if !(0.0 < gate.target && gate.target < gate.ceiling) {
        errors.push("harness/policy.json: the runtime target must lie below the ceiling".into());
    }
    let (ceiling, target) = (
        format!("{:.2}", gate.ceiling),
        format!("{:.2}", gate.target),
    );
    let read = |rel: &str| fs::read_to_string(root.join(rel)).unwrap_or_default();
    let quality = read("docs/quality.md");
    for value in [&ceiling, &target] {
        if !quality.contains(&format!("**{value}**")) {
            errors.push(format!(
                "docs/quality.md: does not state the policy value {value}"
            ));
        }
    }
    for rel in documents(root) {
        if rel != "docs/quality.md" && mentions(&read(&rel), &ceiling) {
            errors.push(format!(
                "{rel}: restates the runtime ceiling {ceiling}; refer to docs/quality.md"
            ));
        }
    }
    for (area, cell) in status_runtimes(&read("docs/status.md")) {
        for (runtime, count) in cell_runtimes(&cell) {
            let above = runtime.parse().is_ok_and(|r: f64| r > gate.ceiling);
            let claims_every_case = count.is_none_or(|(passing, cases)| passing >= cases);
            if above && claims_every_case {
                errors.push(format!(
                    "docs/status.md: {area} runtime {runtime} is above the ceiling {ceiling}, \
                     but the row does not say which cases fail"
                ));
            }
        }
    }
    errors
}

/// The prose documents a reader meets: top-level guides, `docs/` and crate READMEs.
fn documents(root: &Path) -> Vec<String> {
    let names = |dir: &str| -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(root.join(dir))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    let guides = ["README.md", "CONTRIBUTING.md", "AGENTS.md"].map(String::from);
    let docs = names("docs")
        .into_iter()
        .filter(|name| Path::new(name).extension().is_some_and(|e| e == "md"))
        .map(|name| format!("docs/{name}"));
    let crates = names("crates")
        .into_iter()
        .map(|name| format!("crates/{name}/README.md"));
    guides
        .into_iter()
        .chain(docs)
        .chain(crates)
        .filter(|rel| root.join(rel).is_file())
        .collect()
}

/// Whether `text` contains `value` as a whole number, not as the prefix of a longer one.
fn mentions(text: &str, value: &str) -> bool {
    text.match_indices(value).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + value.len()..].chars().next();
        !before.is_some_and(|c| c.is_ascii_digit() || c == '.')
            && !after.is_some_and(|c| c.is_ascii_digit())
    })
}

/// The area and runtime cell of every row of each status table with an "Area" and a
/// "Runtime" column (one table per gate target).
fn status_runtimes(status: &str) -> Vec<(String, String)> {
    let cells = |line: &str| -> Vec<String> {
        line.trim()
            .trim_matches('|')
            .split('|')
            .map(|cell| cell.trim().to_owned())
            .collect()
    };
    let mut rows = Vec::new();
    let mut lines = status.lines();
    while let Some(line) = lines.next() {
        let header = cells(line);
        if header.first().is_none_or(|c| c != "Area") {
            continue;
        }
        let Some(column) = header.iter().position(|c| c == "Runtime") else {
            continue;
        };
        for line in lines
            .by_ref()
            .skip(1)
            .take_while(|line| line.starts_with('|'))
        {
            let row = cells(line);
            if let (Some(area), Some(cell)) = (row.first(), row.get(column)) {
                rows.push((area.clone(), cell.clone()));
            }
        }
    }
    rows
}

/// Each runtime a status cell states, with the `n/m` of its cases that pass when it says.
/// A cell holds one result or several, separated by `<br>`, `;` or a comma outside
/// parentheses and code spans, as in "MP4 forward 0.7670 (51/51 pass), raw Annex B 0.6286
/// (7/7 pass)". A result's runtime is its first decimal number outside parentheses and
/// code spans, unless "of" follows it, as in "0.921 of main's instructions".
fn cell_runtimes(cell: &str) -> Vec<(&str, Option<(u64, u64)>)> {
    // The cell with code spans and parenthesised text blanked byte for byte, so that
    // offsets into it are offsets into the cell.
    let mut top = String::with_capacity(cell.len());
    let (mut depth, mut code) = (0u32, false);
    for c in cell.chars() {
        let inside = code || depth > 0;
        match c {
            '`' => code = !code,
            '(' if !code => depth += 1,
            ')' if !code => depth = depth.saturating_sub(1),
            _ => {}
        }
        if inside || code || depth > 0 {
            top.extend(std::iter::repeat_n(' ', c.len_utf8()));
        } else {
            top.push(c);
        }
    }
    let top = top.replace("<br>", ";;;;");
    let mut results = Vec::new();
    let mut start = 0;
    for end in top
        .match_indices([';', ','])
        .map(|(at, _)| at)
        .chain([top.len()])
    {
        let (shown, result) = (&top[start..end], &cell[start..end]);
        start = end + 1;
        let runtime = decimals(shown).find(|&(_, after)| {
            let rest = shown[after..].trim_start();
            !(rest == "of" || rest.starts_with("of "))
        });
        if let Some((at, after)) = runtime {
            results.push((&result[at..after], pass_count(result)));
        }
    }
    results
}

/// The start and end of every number with a decimal point in `text` that is not part of
/// a word or a longer number, such as 0.7670 in "MP4 forward 0.7670" but not 264 in H.264.
fn decimals(text: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let joined = |c: char| c.is_alphanumeric() || c == '.' || c == '_';
    text.char_indices().filter_map(move |(at, c)| {
        if !c.is_ascii_digit() || text[..at].chars().next_back().is_some_and(joined) {
            return None;
        }
        let run = text[at..]
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .map_or(text.len(), |end| at + end);
        let number = text[at..run].trim_end_matches('.');
        number.contains('.').then_some((at, at + number.len()))
    })
}

/// The last `n/m` before "pass", such as (45, 125) in "1.3846 (45/125 pass for AVC/HEVC)"
/// or (956, 956) in "0.7395 (956/956 timed operations pass)".
fn pass_count(result: &str) -> Option<(u64, u64)> {
    let words: Vec<&str> = result
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .filter(|word| !word.is_empty())
        .collect();
    let pass = words.iter().position(|word| word.starts_with("pass"))?;
    words[..pass].iter().rev().find_map(|word| {
        let (passing, cases) = word.split_once('/')?;
        Some((passing.parse().ok()?, cases.parse().ok()?))
    })
}

/// Every commit `docs/status.md` cites is an ancestor of `HEAD`, so the
/// source revision of a result stays fetchable once its pull request is merged.
///
/// A citation is a code span of 7 to 40 lowercase hexadecimal digits that is not a
/// prefix of the pinned FFmpeg revision.
pub fn revisions(root: &Path) -> Result<String, Vec<String>> {
    let one = |error: String| vec![error];
    let status = fs::read_to_string(root.join("docs/status.md"))
        .map_err(|e| one(format!("docs/status.md: {e}")))?;
    if git(root, &["rev-parse", "--is-shallow-repository"]).map_err(one)? == "true" {
        return Err(one(
            "the checkout is shallow: cited revisions are checked against the full history".into(),
        ));
    }
    let reachable = git(root, &["rev-list", "HEAD"]).map_err(one)?;
    let reachable: HashSet<&str> = reachable.lines().collect();
    let cited = cited_revisions(&status, &[crate::prepare::REVISION]);
    let mut errors = Vec::new();
    for &revision in &cited {
        let objects =
            git(root, &["rev-parse", &format!("--disambiguate={revision}")]).map_err(one)?;
        let mut matches = objects.lines();
        let error = match (matches.next(), matches.next()) {
            (_, Some(_)) => "matches more than one Git object: cite more digits",
            (Some(commit), None) if reachable.contains(commit) => continue,
            _ => "does not name a commit in HEAD's history",
        };
        errors.push(format!("docs/status.md cites `{revision}`, which {error}"));
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(format!(
        "Cited revisions OK: {} citations reachable from HEAD",
        cited.len()
    ))
}

/// Distinct commit citations in the status document's paragraphs, headings and tables.
fn cited_revisions<'a>(text: &'a str, external: &[&str]) -> Vec<&'a str> {
    let mut cited = Vec::new();
    let mut paragraph = 0;
    let mut offset = 0;
    let mut table = false;
    let mut lines = text.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        let end = offset + line.len();
        let boundary = line.trim().is_empty() || is_atx_heading(line);
        table = !boundary && (table || lines.peek().is_some_and(|next| starts_table(line, next)));
        if boundary || table {
            cite_inline(&text[paragraph..offset], external, &mut cited);
            if table {
                for cell in pipe_cells(line) {
                    cite_inline(cell, external, &mut cited);
                }
            } else {
                cite_inline(line, external, &mut cited);
            }
            paragraph = end;
        }
        offset = end;
    }
    cite_inline(&text[paragraph..], external, &mut cited);
    cited
}

fn is_atx_heading(line: &str) -> bool {
    let line = line.trim_end_matches(['\r', '\n']);
    let start = line.trim_start_matches(' ');
    let content = start.trim_start_matches('#');
    line.len() - start.len() <= 3
        && (1..=6).contains(&(start.len() - content.len()))
        && (content.is_empty() || content.starts_with([' ', '\t']))
}

fn starts_table(header: &str, delimiter: &str) -> bool {
    if table_pipes(header).next().is_none() && table_pipes(delimiter).next().is_none() {
        return false;
    }
    let mut width = 0;
    for cell in pipe_cells(delimiter) {
        let cell = cell.trim();
        let dashes = cell.strip_prefix(':').unwrap_or(cell);
        let dashes = dashes.strip_suffix(':').unwrap_or(dashes);
        if dashes.is_empty() || !dashes.bytes().all(|byte| byte == b'-') {
            return false;
        }
        width += 1;
    }
    width == pipe_cells(header).count()
}

fn table_pipes(line: &str) -> impl Iterator<Item = usize> + '_ {
    // Table splitting precedes inline escape processing.
    line.match_indices('|')
        .filter_map(move |(at, _)| (!line[..at].ends_with('\\')).then_some(at))
}

pub(crate) fn pipe_cells(line: &str) -> impl Iterator<Item = &str> {
    let line = line.trim();
    let row = line
        .strip_suffix('|')
        .filter(|before| !before.ends_with('\\'))
        .unwrap_or(line);
    let row = row.strip_prefix('|').unwrap_or(row);
    let mut start = 0;
    table_pipes(row)
        .chain(std::iter::once(row.len()))
        .map(move |end| {
            let cell = &row[start..end];
            start = end + 1;
            cell
        })
}

fn escaped(text: &str, offset: usize) -> bool {
    text.as_bytes()[..offset]
        .iter()
        .rev()
        .take_while(|&&byte| byte == b'\\')
        .count()
        % 2
        != 0
}

fn cite_inline<'a>(mut text: &'a str, external: &[&str], cited: &mut Vec<&'a str>) {
    while let Some(open) = text.find('`') {
        let start = &text[open + usize::from(escaped(text, open))..];
        let content = start.trim_start_matches('`');
        let width = start.len() - content.len();
        text = content;
        if width == 0 {
            continue;
        }

        let mut search = content;
        while let Some(close) = search.find('`') {
            let delimiter = &search[close..];
            search = delimiter.trim_start_matches('`');
            if delimiter.len() - search.len() != width {
                continue;
            }

            let span = content[..content.len() - delimiter.len()].trim();
            let hex = span
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if hex
                && (7..=40).contains(&span.len())
                && !external.iter().any(|e| e.starts_with(span))
                && !cited.contains(&span)
            {
                cited.push(span);
            }
            text = search;
            break;
        }
    }
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let text = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
    Ok(text.trim_end().to_owned())
}

/// The workspace has only owned packages, no build scripts, and no unsafe code.
pub fn safety(root: &Path) -> Result<String, String> {
    let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["metadata", "--format-version", "1", "--locked", "--offline"])
        .env("CARGO_INCREMENTAL", "0")
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run cargo metadata: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let text = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
    let metadata = json::parse(&text).map_err(|e| format!("cargo metadata: {e}"))?;
    let owned = [root.join("crates"), root.join("xtask")];
    let workspace_toml = fs::read_to_string(root.join("Cargo.toml")).map_err(|e| e.to_string())?;
    let worker_lints = lint_table(&workspace_toml, "[workspace.lints.")
        .replace("[workspace.lints.", "[lints.")
        .replace("unsafe_code = \"forbid\"", "unsafe_code = \"deny\"");
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .unwrap_or_default();
    for package in packages {
        let name = package.get("name").and_then(Value::as_str).unwrap_or("?");
        let manifest = PathBuf::from(
            package
                .get("manifest_path")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        let local = package.get("source").is_some_and(|s| *s == Value::Null);
        // Repository tooling is never part of a library's dependency graph.
        let dependencies = package
            .get("dependencies")
            .and_then(Value::as_array)
            .unwrap_or_default();
        if dependencies
            .iter()
            .any(|d| d.get("name").and_then(Value::as_str) == Some("xtask"))
        {
            return Err(format!("{name} depends on the xtask tooling package"));
        }
        if !local || !owned.iter().any(|dir| manifest.starts_with(dir)) {
            return Err(format!("Unadmitted dependency: {name}"));
        }
        let cargo_toml =
            fs::read_to_string(&manifest).map_err(|e| format!("{}: {e}", manifest.display()))?;
        if manifest.starts_with(root.join("crates"))
            && !cargo_toml.contains("[lints]\nworkspace = true\n")
            && lint_table(&cargo_toml, "[lints.") != worker_lints
        {
            return Err(format!(
                "{name}: [lints] is neither the workspace's nor the workers' (the workspace's with unsafe_code denied)"
            ));
        }
        for target in package
            .get("targets")
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            let kinds = target
                .get("kind")
                .and_then(Value::as_array)
                .unwrap_or_default();
            if kinds.iter().any(|k| k.as_str() == Some("custom-build")) {
                return Err("Build scripts need explicit audit".into());
            }
            let source = PathBuf::from(
                target
                    .get("src_path")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
            if !source.ends_with("main.rs") && !source.ends_with("lib.rs") {
                continue;
            }
            let text =
                fs::read_to_string(&source).map_err(|e| format!("{}: {e}", source.display()))?;
            let denied_worker =
                text.contains("#![deny(unsafe_code)]") && text.contains(WORKER_ENTRY);
            if !text.contains("#![forbid(unsafe_code)]") && !denied_worker {
                return Err(format!(
                    "Missing crate-level unsafe prohibition: {}",
                    source.display()
                ));
            }
        }
    }
    // The library crates are scanned as text as well; xtask relies on its
    // `forbid` attribute because its own tests spell out the patterns. A worker
    // binary's C entry point is the one unsafe item admitted.
    for source in walk::files(&root.join("crates"), &|rel| {
        rel.extension().is_some_and(|e| e == "rs")
    }) {
        let text = fs::read_to_string(&source).map_err(|e| format!("{}: {e}", source.display()))?;
        let rest = without_worker_entry(&text).map_err(|e| format!("{}: {e}", source.display()))?;
        if has_unsafe_syntax(&rest) {
            return Err(format!(
                "Unsafe syntax requires investigation: {}",
                source.display()
            ));
        }
    }
    Ok(format!(
        "Strict-safe workspace: {} owned packages; no third-party packages, build scripts or unsafe syntax but each worker's C entry point",
        packages.len()
    ))
}

/// The C entry point of a worker binary, the one item its `deny(unsafe_code)` allows.
const WORKER_ENTRY: &str = "#[allow(unsafe_code)]\n#[unsafe(no_mangle)]\nextern \"C\" fn main(";

/// `text` without its worker entry point. A file may hold one, in a crate root that
/// denies unsafe code and has no `main` of Rust's own outside its tests.
fn without_worker_entry(text: &str) -> Result<String, String> {
    match text.matches(WORKER_ENTRY).count() {
        0 => Ok(text.to_owned()),
        1 if text.contains("#![deny(unsafe_code)]")
            && text.contains("#![cfg_attr(not(test), no_main)]") =>
        {
            Ok(text.replace(WORKER_ENTRY, ""))
        }
        count => Err(format!(
            "{count} worker entry points; a worker binary has one, with #![deny(unsafe_code)] and #![cfg_attr(not(test), no_main)]"
        )),
    }
}

/// The lint tables of a Cargo manifest whose headers start with `prefix`, without
/// comments or blank lines.
fn lint_table(manifest: &str, prefix: &str) -> String {
    let mut table = String::new();
    let mut inside = false;
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            inside = line.starts_with(prefix);
        }
        if inside && !line.is_empty() && !line.starts_with('#') {
            table.push_str(line);
            table.push('\n');
        }
    }
    table
}

/// `unsafe` as a word followed by a block, `impl`, `trait`, `fn`, `extern` or an
/// attribute's parenthesis.
pub fn has_unsafe_syntax(text: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices("unsafe").any(|(at, _)| {
        if text[..at].chars().next_back().is_some_and(word) {
            return false;
        }
        let rest = text[at + "unsafe".len()..].trim_start();
        ["{", "impl", "trait", "fn", "extern", "("]
            .iter()
            .any(|next| rest.starts_with(next))
    })
}

#[cfg(test)]
mod tests;
