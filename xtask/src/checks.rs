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
    let research = root.join("research");
    for path in walk::files(&research, &|rel| {
        rel.extension().is_some_and(|e| e == "json")
    }) {
        let rel = path.strip_prefix(root).expect("walked under root");
        if let Err(error) = read_json(&path) {
            errors.push(format!("{}: {error}", rel.display()));
        }
    }
    let gates = match (
        read_json(&root.join("harness/policy.json")),
        read_json(&root.join("harness/module-gates.json")),
    ) {
        (Ok(_), Ok(gates)) => gates,
        (Err(error), _) => return Err(vec![format!("harness/policy.json: {error}")]),
        (_, Err(error)) => return Err(vec![format!("harness/module-gates.json: {error}")]),
    };
    errors.extend(runtime_mentions(root));
    errors.extend(contract_fields(root));
    // Generated manifests: the case ids and the pins match, in the recorded form.
    for generator in crate::generators::GENERATORS {
        errors.extend(generator.check(root));
    }
    let modules = gates
        .get("modules")
        .and_then(Value::as_array)
        .unwrap_or_default();
    if modules
        .iter()
        .any(|m| m.get("accepted").is_some_and(Value::truthy))
    {
        errors.push("a module was marked accepted without implementation evidence".into());
    }
    let fate = match read_json(&root.join("harness/fate/index.json")) {
        Ok(fate) => fate,
        Err(error) => return Err(vec![format!("harness/fate/index.json: {error}")]),
    };
    let cases = fate
        .get("cases")
        .and_then(Value::as_array)
        .unwrap_or_default();
    let mapped = |c: &Value| {
        c.get("status").and_then(Value::as_str) != Some("unmapped")
            || c.get("rights").and_then(Value::as_str) != Some("unreviewed")
    };
    if cases.iter().any(mapped) {
        errors.push("FATE index misrepresents coverage/rights".into());
    }
    if errors.is_empty() {
        Ok(format!(
            "Design integrity OK: JSON, {} unaccepted module gates, {} unmapped FATE identities.\n\
             This is not a feature-parity, performance, memory or FATE pass.",
            modules.len(),
            cases.len()
        ))
    } else {
        Err(errors)
    }
}

/// The fields every feature contract (`harness/features/<id>/contract.json`) may
/// declare that the harness reads the same way for all of them: `timeout_seconds`.
fn contract_fields(root: &Path) -> Vec<String> {
    let features = root.join("harness/features");
    walk::files(&features, &|rel| rel.ends_with("contract.json"))
        .into_iter()
        .filter_map(|path| {
            let rel = path
                .strip_prefix(root)
                .expect("walked under root")
                .display();
            match read_json(&path) {
                Ok(contract) => crate::harness::worker::contract_timeout(&contract)
                    .err()
                    .map(|error| format!("{rel}: {error}")),
                Err(error) => Some(format!("{rel}: {error}")),
            }
        })
        .collect()
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
        let Some(runtime) = leading_ratio(&cell) else {
            continue;
        };
        let above = runtime.parse().is_ok_and(|r: f64| r > gate.ceiling);
        let claims_every_case = pass_count(&cell).is_none_or(|(passing, cases)| passing >= cases);
        if above && claims_every_case {
            errors.push(format!(
                "docs/status.md: {area} runtime {runtime} is above the ceiling {ceiling}, \
                 but the row does not say which cases fail"
            ));
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

/// The area and runtime cell of every row of the status table.
fn status_runtimes(status: &str) -> Vec<(String, String)> {
    let cells = |line: &str| -> Vec<String> {
        line.trim()
            .trim_matches('|')
            .split('|')
            .map(|cell| cell.trim().to_owned())
            .collect()
    };
    let mut lines = status.lines();
    let Some(column) = lines.by_ref().find_map(|line| {
        let header = cells(line);
        (header.first().is_some_and(|c| c == "Area"))
            .then(|| header.iter().position(|c| c == "Runtime"))
            .flatten()
    }) else {
        return Vec::new();
    };
    lines
        .skip(1)
        .take_while(|line| line.starts_with('|'))
        .filter_map(|line| {
            let row = cells(line);
            Some((row.first()?.clone(), row.get(column)?.clone()))
        })
        .collect()
}

/// The first decimal number in `cell`, such as 1.1808 in "1.1808 (71/71 pass)".
fn leading_ratio(cell: &str) -> Option<&str> {
    let start = cell.find(|c: char| c.is_ascii_digit())?;
    let rest = &cell[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    let number = rest[..end].trim_end_matches('.');
    number.contains('.').then_some(number)
}

/// The `n/m` before "pass", such as (45, 125) in "1.3846 (45/125 pass for AVC/HEVC)".
fn pass_count(cell: &str) -> Option<(u64, u64)> {
    let words: Vec<&str> = cell
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .filter(|word| !word.is_empty())
        .collect();
    let counted = words.windows(2).find(|pair| pair[1].starts_with("pass"))?[0];
    let (passing, cases) = counted.split_once('/')?;
    Some((passing.parse().ok()?, cases.parse().ok()?))
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
    if !header.contains('|') && !delimiter.contains('|') {
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

fn pipe_cells(line: &str) -> impl Iterator<Item = &str> {
    let line = line.trim();
    let row = line
        .strip_suffix('|')
        .filter(|before| !escaped(line, before.len()))
        .unwrap_or(line);
    let row = row.strip_prefix('|').unwrap_or(row);
    let mut start = 0;
    row.match_indices('|')
        .filter_map(move |(at, _)| (!escaped(row, at)).then_some(at))
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

pub fn read_json(path: &Path) -> Result<Value, String> {
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    json::parse(&text).map_err(|e| e.to_string())
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
            if !text.contains("#![forbid(unsafe_code)]") {
                return Err(format!(
                    "Missing crate-level unsafe prohibition: {}",
                    source.display()
                ));
            }
        }
    }
    // The library crates are scanned as text as well; xtask relies on its
    // `forbid` attribute because its own tests spell out the patterns.
    for source in walk::files(&root.join("crates"), &|rel| {
        rel.extension().is_some_and(|e| e == "rs")
    }) {
        let text = fs::read_to_string(&source).map_err(|e| format!("{}: {e}", source.display()))?;
        if has_unsafe_syntax(&text) {
            return Err(format!(
                "Unsafe syntax requires investigation: {}",
                source.display()
            ));
        }
    }
    Ok(format!(
        "Strict-safe workspace: {} owned packages; no third-party packages, build scripts or unsafe syntax",
        packages.len()
    ))
}

/// `unsafe` as a word followed by a block, `impl`, `trait`, `fn` or `extern`.
pub fn has_unsafe_syntax(text: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices("unsafe").any(|(at, _)| {
        if text[..at].chars().next_back().is_some_and(word) {
            return false;
        }
        let rest = text[at + "unsafe".len()..].trim_start();
        ["{", "impl", "trait", "fn", "extern"]
            .iter()
            .any(|next| rest.starts_with(next))
    })
}

#[cfg(test)]
mod tests {
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
        let text =
            "at `` abc1234 `` and `def1234`, with ``embedded `bad1234` ticks`` and `cafe1234";
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
    fn detects_unsafe_syntax_only_at_word_starts() {
        for text in [
            "unsafe { x }",
            "unsafe impl Send",
            "pub unsafe fn f()",
            "unsafe\n  extern \"C\"",
            "unsafe trait T",
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
        fs::create_dir_all(root.join("research/sub")).unwrap();
        fs::create_dir_all(root.join("harness/fate")).unwrap();
        fs::create_dir_all(root.join("harness/features")).unwrap();
        let policy = r#"{"runtime": {"ratio_max": 1.2, "optimisation_target": 0.8}}"#;
        let gates = r#"{"modules": [{"accepted": false}, {"accepted": false}]}"#;
        fs::write(root.join("harness/policy.json"), policy).unwrap();
        fs::write(root.join("harness/module-gates.json"), gates).unwrap();
        fs::write(
            root.join("harness/fate/index.json"),
            r#"{"cases": [{"status": "unmapped", "rights": "unreviewed"}]}"#,
        )
        .unwrap();
        stated_policy(&root);
        let ok = design(&root).unwrap();
        assert!(ok.starts_with(
            "Design integrity OK: JSON, 2 unaccepted module gates, 1 unmapped FATE identities."
        ));
        fs::write(root.join("research/sub/bad.json"), "{").unwrap();
        fs::write(
            root.join("harness/policy.json"),
            policy.replace("1.2", "1.25"),
        )
        .unwrap();
        fs::write(
            root.join("harness/module-gates.json"),
            gates.replace("false}]", "true}]"),
        )
        .unwrap();
        let errors = design(&root).unwrap_err();
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors[0].starts_with("research/sub/bad.json: "));
        assert!(errors.contains(&"docs/quality.md: does not state the policy value 1.25".into()));
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
             | Framing | 1.3846 (45/125 pass for AVC/HEVC) |\n| PGS | PGS 1.11 |\n| TS | not measured |\n",
        )
        .unwrap();
        assert_eq!(
            runtime_mentions(&root),
            [
                "README.md: restates the runtime ceiling 1.20; refer to docs/quality.md",
                "docs/status.md: HLS runtime 1.2125 is above the ceiling 1.20, \
                 but the row does not say which cases fail",
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }
}
