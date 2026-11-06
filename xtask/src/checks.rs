//! Repository integrity checks: local Markdown links, harness data files and the
//! safe-Rust, dependency-free workspace rule.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::json::{self, Value};
use crate::walk;

/// Every relative link target in a tracked-style Markdown file must exist.
/// Archived evidence snapshots are skipped: they keep the links of their time.
pub fn links(root: &Path) -> Vec<String> {
    let mut errors = Vec::new();
    for path in walk::files(root, &|rel| rel.extension().is_some_and(|e| e == "md")) {
        let rel = path.strip_prefix(root).expect("walked under root");
        if is_archived(rel) {
            continue;
        }
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
            if !path
                .parent()
                .expect("file has a parent")
                .join(file)
                .exists()
            {
                errors.push(format!("{}: missing link {target}", rel.display()));
            }
        }
    }
    errors
}

fn is_archived(rel: &Path) -> bool {
    rel.starts_with("research/qualification") || rel.starts_with("research/review")
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
    let gates = match read_json(&root.join("research/module-gates.json")) {
        Ok(gates) => gates,
        Err(error) => return Err(vec![format!("research/module-gates.json: {error}")]),
    };
    let requirements = gates.get("owner_requirements");
    if requirements
        .and_then(|r| r.get("runtime_ratio_max"))
        .and_then(Value::as_f64)
        != Some(1.2)
    {
        errors.push("delivery runtime gate differs from the policy (1.20)".into());
    }
    if requirements
        .and_then(|r| r.get("runtime_optimization_ratio_target"))
        .and_then(Value::as_f64)
        != Some(0.8)
    {
        errors.push("runtime optimization target differs from the policy (0.80)".into());
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
    fn reports_missing_links_and_skips_archives() {
        let root = crate::walk::scratch("links");
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::create_dir_all(root.join("research/review")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(
            root.join("docs/a.md"),
            "[ok](b.md#x) [web](https://x) [gone](missing.md) [self](#top)",
        )
        .unwrap();
        fs::write(root.join("docs/b.md"), "").unwrap();
        fs::write(root.join("research/review/old.md"), "[gone](missing.md)").unwrap();
        fs::write(root.join("target/t.md"), "[gone](missing.md)").unwrap();
        assert_eq!(links(&root), vec!["docs/a.md: missing link missing.md"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn design_reports_invalid_json_and_gate_changes() {
        let root = crate::walk::scratch("design");
        fs::create_dir_all(root.join("research/sub")).unwrap();
        fs::create_dir_all(root.join("harness/fate")).unwrap();
        let gates = r#"{"owner_requirements": {"runtime_ratio_max": 1.2, "runtime_optimization_ratio_target": 0.8},
                        "modules": [{"accepted": false}, {"accepted": false}]}"#;
        fs::write(root.join("research/module-gates.json"), gates).unwrap();
        fs::write(
            root.join("harness/fate/index.json"),
            r#"{"cases": [{"status": "unmapped", "rights": "unreviewed"}]}"#,
        )
        .unwrap();
        let ok = design(&root).unwrap();
        assert!(ok.starts_with(
            "Design integrity OK: JSON, 2 unaccepted module gates, 1 unmapped FATE identities."
        ));
        fs::write(root.join("research/sub/bad.json"), "{").unwrap();
        fs::write(
            root.join("research/module-gates.json"),
            gates.replace("1.2", "1.25").replace("false}]", "true}]"),
        )
        .unwrap();
        let errors = design(&root).unwrap_err();
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors[0].starts_with("research/sub/bad.json: "));
        fs::remove_dir_all(root).unwrap();
    }
}
