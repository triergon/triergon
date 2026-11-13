//! Shared checks for report fields and measurement bindings.

use std::path::{Component, Path, PathBuf};

use super::comparison::{reference_named, reference_record, rule_for};
use super::{diff, gate_build, linux, provenance};
use crate::json::Value;

pub(super) fn field<'a>(value: &'a Value, key: &str) -> Result<&'a Value, String> {
    value
        .get(key)
        .filter(|v| **v != Value::Null)
        .ok_or_else(|| format!("missing {key}"))
}

pub(super) fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    field(value, key)?
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{key} must be a nonempty string"))
}

pub(super) fn array<'a>(value: &'a Value, key: &str) -> Result<&'a [Value], String> {
    field(value, key)?
        .as_array()
        .ok_or_else(|| format!("{key} must be an array"))
}

pub(super) fn members(value: &Value) -> Result<&[(String, Value)], String> {
    match value {
        Value::Object(fields) => Ok(fields),
        _ => Err("expected a JSON object".into()),
    }
}

pub(super) fn same(a: &Value, b: &Value) -> bool {
    // Recorded booleans cannot stand in for numbers.
    diff::differences(a, b).is_empty()
}

pub(super) fn require_same(a: &Value, b: &Value, label: &str) -> Result<(), String> {
    if same(a, b) {
        Ok(())
    } else {
        Err(format!("{label} mismatch"))
    }
}

pub(super) fn put(value: &mut Value, key: &str, replacement: Value) -> Result<(), String> {
    let Value::Object(fields) = value else {
        return Err("expected a JSON object".into());
    };
    fields.retain(|(name, _)| name != key);
    fields.push((key.into(), replacement));
    Ok(())
}

pub(super) fn hash<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    let digest = text(value, key)?;
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{key} must be a SHA-256"));
    }
    Ok(digest)
}

/// Resolve hash-bound measurements without holding another case's samples.
pub(crate) fn expand_measurements(directory: &Path, row: &Value) -> Result<Value, String> {
    let directory = directory.canonicalize().map_err(|e| e.to_string())?;
    let mut expanded = row.clone();
    for kind in ["timing", "memory"] {
        let Some(record) = row.get(kind).filter(|value| **value != Value::Null) else {
            continue;
        };
        let name = text(record, "raw")?;
        if Path::new(name)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err("measurement path escapes report".into());
        }
        let path = directory.join(name);
        if provenance::file_identity(&directory, &path)?.1 != hash(record, "raw_sha256")? {
            return Err("raw measurement changed during report assembly".into());
        }
        let bytes = std::fs::read(&path).map_err(|e| format!("{name}: {e}"))?;
        // Bind the bytes actually parsed as well as the regular-file check.
        if crate::sha256::hex(&bytes) != hash(record, "raw_sha256")? {
            return Err("raw measurement changed during report assembly".into());
        }
        let raw =
            crate::json::parse_strict(std::str::from_utf8(&bytes).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        put(&mut expanded, kind, raw)?;
    }
    Ok(expanded)
}

/// A declared iteration count fixes the measured operation.
pub(super) fn iterations_valid(contract: &Value, observed: &Value) -> bool {
    let Value::Number(value) = observed else {
        return false;
    };
    let Some(count) = value
        .as_i128()
        .filter(|count| (1..=i128::from(u32::MAX)).contains(count))
    else {
        return false;
    };
    contract.get("iterations").is_none_or(
        |required| matches!(required, Value::Number(value) if value.as_i128() == Some(count)),
    )
}

/// The reference and operation recorded for a contract's exact case.
pub(super) struct SelectedOperation<'a> {
    pub rule: Option<&'a Value>,
    pub reference: Value,
    pub reference_worker: Option<&'a str>,
    /// The contract's `measurement_reference`, which its cases are timed and
    /// their memory measured against instead of `reference`.
    pub measurement_worker: Option<&'a str>,
    /// The reference the cases are timed and measured against.
    pub measured: Value,
    pub candidate_worker: Option<&'a str>,
    pub operation: Value,
}

pub(super) fn selected_operation<'a>(
    case: &Value,
    contract: &'a Value,
    identity: &'a Value,
) -> Result<SelectedOperation<'a>, String> {
    let id = contract.get("id").and_then(Value::as_str);
    let rule = rule_for(case, identity, id)?;
    let reference_worker = if rule.is_none() {
        contract.get("reference").and_then(Value::as_str)
    } else {
        None
    };
    let reference = match reference_worker {
        Some(name) => reference_named(identity, name)?,
        None => reference_record(identity, rule)?,
    };
    let measurement_worker = contract
        .get("measurement_reference")
        .and_then(Value::as_str);
    let measured = match measurement_worker {
        Some(name) => reference_named(identity, name)?,
        None => reference.clone(),
    };
    let prefix = contract.get("worker_operation").and_then(Value::as_str);
    let case_operations = contract.get("case_operations") == Some(&Value::Bool(true));
    let candidate_worker = (prefix.is_some() || case_operations).then(|| {
        contract
            .get("candidate")
            .and_then(Value::as_str)
            .unwrap_or("media-worker")
    });
    let operation = if case_operations {
        let operation = field(case, "operation")?;
        for key in ["name", "view", "prefix"] {
            text(operation, key)?;
        }
        for key in ["arguments", "candidate_options"] {
            if array(operation, key)?.iter().any(|v| v.as_str().is_none()) {
                return Err(format!("operation {key} must contain strings"));
            }
        }
        operation.clone()
    } else if let Some(prefix) = prefix {
        Value::object([
            ("name", field(contract, "operation")?.clone()),
            ("view", Value::str(format!("playback-{prefix}fold-v1"))),
        ])
    } else {
        let seek = case.get("seek_target_ns").unwrap_or(&Value::Null);
        let view = match id {
            Some("matroska-segment-metadata") => Value::str("catalogue-source-and-native-v1"),
            Some("matroska-colour") => Value::str("colour-source-and-ffmpeg9-v1"),
            _ => match rule {
                Some(rule) => field(rule, "view")?.clone(),
                None => Value::str("source-frames-ffmpeg9-clock-v1"),
            },
        };
        Value::object([
            (
                "name",
                Value::str(if *seek == Value::Null {
                    "container-demux-v1"
                } else {
                    "container-seek-preroll-v1"
                }),
            ),
            ("seek_target_ns", seek.clone()),
            ("view", view),
        ])
    };
    Ok(SelectedOperation {
        rule,
        reference,
        reference_worker,
        measurement_worker,
        measured,
        candidate_worker,
        operation,
    })
}

/// Verify the primary workers, comparison references and selected sources.
pub(super) fn current_workers(root: &Path, identity: &Value) -> Result<(PathBuf, PathBuf), String> {
    let candidate = root.join(gate_build::release_dir()?).join("media-worker");
    let reference = root.join("target/harness/ffmpeg-worker");
    for (role, path) in [("candidate", &candidate), ("reference", &reference)] {
        if linux::file_sha(path)? != hash(field(identity, "binaries")?, role)? {
            return Err(format!(
                "{role} worker changed since build identity; rebuild"
            ));
        }
    }
    let directory = root
        .join("target/harness")
        .canonicalize()
        .map_err(|e| e.to_string())?;
    for (_, record) in members(field(identity, "comparison_references")?)? {
        let path = root
            .join(text(record, "path")?)
            .canonicalize()
            .map_err(|e| e.to_string())?;
        if !path.starts_with(&directory) || linux::file_sha(&path)? != hash(record, "sha256")? {
            return Err("comparison reference changed since build identity; rebuild".into());
        }
    }
    if let Some(error) = provenance::source_identity_error(root, identity) {
        return Err(format!("{error}; rebuild"));
    }
    Ok((candidate, reference))
}

/// Bind a contract's additional candidate to its binary and selected sources.
pub(super) fn candidate_worker(
    root: &Path,
    identity: &Value,
    name: &str,
) -> Result<PathBuf, String> {
    if Path::new(name).components().count() != 1
        || !matches!(
            Path::new(name).components().next(),
            Some(Component::Normal(_))
        )
    {
        return Err("candidate must name a worker".into());
    }
    let record = field(field(identity, "additional_candidates")?, name)?;
    let path = root.join(gate_build::release_dir()?).join(name);
    let actual = linux::file_sha(&path)?;
    if actual != hash(record, "sha256")? {
        return Err(format!("{name} changed since build identity; rebuild"));
    }
    let recorded = field(record, "selected_graph")?;
    let current = super::build::candidate_graph(root, name)?;
    require_same(recorded, &current, "candidate source graph")?;
    Ok(path)
}

pub(super) fn external_contract(contract: &Value, id: &str) -> Result<Value, String> {
    let mut derived = contract.clone();
    put(&mut derived, "required_profiles", Value::strings([id]))?;
    put(
        &mut derived,
        "required_additional_vectors",
        Value::Array(vec![]),
    )?;
    Ok(derived)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::process::TempDir;

    #[test]
    fn iteration_counts_are_positive_integers_and_preserve_the_contract() {
        let contract = Value::object([("iterations", Value::int(50))]);
        assert!(iterations_valid(&contract, &Value::int(50)));
        for count in [1, 49, 51, 100] {
            assert!(!iterations_valid(&contract, &Value::int(count)));
        }
        let unspecified = Value::Object(vec![]);
        assert!(iterations_valid(&unspecified, &Value::int(1)));
        for invalid in [
            Value::int(0),
            Value::int(-1),
            Value::int(i128::from(u32::MAX) + 1),
            Value::Bool(true),
            Value::float(50.0),
            Value::str("50"),
            Value::Null,
        ] {
            assert!(!iterations_valid(&unspecified, &invalid));
            let malformed = Value::object([("iterations", invalid)]);
            assert!(!iterations_valid(&malformed, &Value::int(50)));
        }
    }

    fn built_workers() -> (TempDir, Value) {
        let directory = TempDir::new(None).unwrap();
        let root = &directory.0;
        let release = gate_build::release_dir().unwrap();
        let candidate = format!("{release}/media-worker");
        for path in [release.as_str(), "target/harness", "crates/example/src"] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        for (path, bytes) in [
            (candidate.as_str(), "candidate"),
            ("target/harness/ffmpeg-worker", "reference"),
            ("crates/example/src/lib.rs", "pub fn example() {}"),
            ("config.json", "{}"),
        ] {
            fs::write(root.join(path), bytes).unwrap();
        }
        let digest = |path: &str| Value::str(linux::file_sha(&root.join(path)).unwrap());
        let identity = Value::object([
            ("schema", Value::int(1)),
            (
                "binaries",
                Value::object([
                    ("candidate", digest(&candidate)),
                    ("reference", digest("target/harness/ffmpeg-worker")),
                ]),
            ),
            (
                "comparison_references",
                Value::object([(
                    "ffmpeg-native",
                    Value::object([
                        ("path", Value::str("target/harness/ffmpeg-worker")),
                        ("sha256", digest("target/harness/ffmpeg-worker")),
                    ]),
                )]),
            ),
            (
                "source_hashes",
                Value::object([(
                    "crates/example/src/lib.rs",
                    digest("crates/example/src/lib.rs"),
                )]),
            ),
            (
                "build_input_hashes",
                Value::object([("config.json", digest("config.json"))]),
            ),
        ]);
        (directory, identity)
    }

    #[test]
    fn current_worker_checks_reject_changed_binaries_and_sources() {
        let (directory, identity) = built_workers();
        let root = &directory.0;
        assert!(current_workers(root, &identity).is_ok());
        let candidate = format!("{}/media-worker", gate_build::release_dir().unwrap());
        for path in [
            candidate.as_str(),
            "target/harness/ffmpeg-worker",
            "crates/example/src/lib.rs",
            "config.json",
        ] {
            let original = fs::read(root.join(path)).unwrap();
            fs::write(root.join(path), b"changed").unwrap();
            assert!(current_workers(root, &identity).is_err(), "{path}");
            fs::write(root.join(path), original).unwrap();
        }
    }

    #[test]
    fn worker_checks_require_reference_records_and_safe_candidate_names() {
        let (directory, identity) = built_workers();
        let root = &directory.0;
        let mut missing = identity.clone();
        let Value::Object(fields) = &mut missing else {
            unreachable!()
        };
        fields.retain(|(key, _)| key != "comparison_references");
        assert_eq!(
            current_workers(root, &missing).unwrap_err(),
            "missing comparison_references"
        );

        for value in [Value::Null, Value::Array(vec![])] {
            let mut malformed = identity.clone();
            put(&mut malformed, "comparison_references", value).unwrap();
            assert!(current_workers(root, &malformed).is_err());
        }

        for name in ["..", "../media-worker", "/media-worker", ""] {
            assert_eq!(
                candidate_worker(root, &identity, name).unwrap_err(),
                "candidate must name a worker"
            );
        }
    }

    #[test]
    fn worker_checks_reject_malformed_hashes_and_missing_candidate_sources() {
        let (directory, identity) = built_workers();
        let root = &directory.0;
        for role in ["candidate", "reference"] {
            for value in [Value::Null, Value::Bool(true), Value::str("not-a-hash")] {
                let mut changed = identity.clone();
                let mut binaries = field(&changed, "binaries").unwrap().clone();
                put(&mut binaries, role, value).unwrap();
                put(&mut changed, "binaries", binaries).unwrap();
                assert!(current_workers(root, &changed).is_err(), "{role}");
            }
        }
        let mut changed = identity;
        let custom = root
            .join(gate_build::release_dir().unwrap())
            .join("custom-worker");
        fs::write(&custom, b"custom").unwrap();
        put(
            &mut changed,
            "additional_candidates",
            Value::object([(
                "custom-worker",
                Value::object([("sha256", Value::str(linux::file_sha(&custom).unwrap()))]),
            )]),
        )
        .unwrap();
        assert_eq!(
            candidate_worker(root, &changed, "custom-worker").unwrap_err(),
            "missing selected_graph"
        );
    }

    #[test]
    fn comparison_references_keep_hash_and_directory_bindings() {
        let (directory, mut identity) = built_workers();
        let root = &directory.0;
        fs::write(root.join("target/harness/other-worker"), b"comparison").unwrap();
        let candidate = format!("{}/media-worker", gate_build::release_dir().unwrap());
        for (path, digest) in [
            ("target/harness/other-worker", "0".repeat(64)),
            (
                candidate.as_str(),
                linux::file_sha(&root.join(&candidate)).unwrap(),
            ),
        ] {
            put(
                &mut identity,
                "comparison_references",
                Value::object([(
                    "selected",
                    Value::object([("path", Value::str(path)), ("sha256", Value::str(digest))]),
                )]),
            )
            .unwrap();
            assert_eq!(
                current_workers(root, &identity).unwrap_err(),
                "comparison reference changed since build identity; rebuild"
            );
        }
    }
}
