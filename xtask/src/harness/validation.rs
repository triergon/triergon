//! Shared checks for report fields and measurement bindings.

use std::path::{Component, Path, PathBuf};

use super::comparison::{reference_named, reference_record, rule_for};
use super::{diff, linux, provenance};
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

/// The reference and operation recorded for a contract's exact case.
pub(super) struct SelectedOperation<'a> {
    pub rule: Option<&'a Value>,
    pub reference: Value,
    pub reference_worker: Option<&'a str>,
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
    let prefix = contract.get("worker_operation").and_then(Value::as_str);
    let candidate_worker = prefix.map(|_| {
        contract
            .get("candidate")
            .and_then(Value::as_str)
            .unwrap_or("media-worker")
    });
    let operation = if let Some(prefix) = prefix {
        Value::object([
            (
                "name",
                contract.get("operation").cloned().unwrap_or(Value::Null),
            ),
            ("view", Value::str(format!("playback-{prefix}fold-v1"))),
        ])
    } else {
        let seek = case.get("seek_target_ns").unwrap_or(&Value::Null);
        let view = match id {
            Some("matroska-segment-metadata") => Value::str("catalogue-source-and-native-v1"),
            Some("matroska-colour") => Value::str("colour-source-and-ffmpeg9-v1"),
            _ => rule.map_or_else(
                || Value::str("source-frames-ffmpeg9-clock-v1"),
                |r| r.get("view").cloned().unwrap_or(Value::Null),
            ),
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
        candidate_worker,
        operation,
    })
}

/// Collection requires the exact primary binary map. Grading also requires
/// comparison references and validates recorded hashes and candidate names.
#[derive(Clone, Copy)]
pub(super) enum WorkerCheck {
    Collection,
    Grading,
}

pub(super) fn current_workers(
    root: &Path,
    identity: &Value,
    check: WorkerCheck,
) -> Result<(PathBuf, PathBuf), String> {
    let candidate = root.join("target/release/media-worker");
    let reference = root.join("target/harness/ffmpeg-worker");
    match check {
        WorkerCheck::Collection => {
            let binaries = Value::object([
                ("candidate", Value::str(linux::file_sha(&candidate)?)),
                ("reference", Value::str(linux::file_sha(&reference)?)),
            ]);
            if !identity
                .get("binaries")
                .is_some_and(|b| diff::equal(b, &binaries))
            {
                return Err("adapter changed since build identity; rebuild".into());
            }
        }
        WorkerCheck::Grading => {
            for (role, path) in [("candidate", &candidate), ("reference", &reference)] {
                if linux::file_sha(path)? != hash(field(identity, "binaries")?, role)? {
                    return Err(format!(
                        "{role} worker changed since build identity; rebuild"
                    ));
                }
            }
        }
    }
    let directory = root
        .join("target/harness")
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let references = match check {
        WorkerCheck::Collection => match identity.get("comparison_references") {
            Some(Value::Object(fields)) => fields.as_slice(),
            _ => &[],
        },
        WorkerCheck::Grading => members(field(identity, "comparison_references")?)?,
    };
    for (_, record) in references {
        let path = match check {
            WorkerCheck::Collection => record
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            WorkerCheck::Grading => text(record, "path")?,
        };
        let path = root.join(path).canonicalize().map_err(|e| e.to_string())?;
        if !path.starts_with(&directory) {
            return Err("comparison reference changed since build identity; rebuild".into());
        }
        let actual = linux::file_sha(&path)?;
        let matches = match check {
            WorkerCheck::Collection => {
                record.get("sha256").and_then(Value::as_str) == Some(actual.as_str())
            }
            WorkerCheck::Grading => actual == hash(record, "sha256")?,
        };
        if !matches {
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
    check: WorkerCheck,
) -> Result<PathBuf, String> {
    if matches!(check, WorkerCheck::Grading)
        && (Path::new(name).components().count() != 1
            || !matches!(
                Path::new(name).components().next(),
                Some(Component::Normal(_))
            ))
    {
        return Err("candidate must name a worker".into());
    }
    let record = match check {
        WorkerCheck::Collection => identity
            .get("additional_candidates")
            .and_then(|c| c.get(name))
            .ok_or_else(|| format!("{name} is not in the build identity; rebuild"))?,
        WorkerCheck::Grading => field(field(identity, "additional_candidates")?, name)?,
    };
    let path = root.join(format!("target/release/{name}"));
    let actual = linux::file_sha(&path)?;
    let matches = match check {
        WorkerCheck::Collection => {
            record.get("sha256").and_then(Value::as_str) == Some(actual.as_str())
        }
        WorkerCheck::Grading => actual == hash(record, "sha256")?,
    };
    if !matches {
        return Err(format!("{name} changed since build identity; rebuild"));
    }
    let recorded = match check {
        WorkerCheck::Collection => record.get("selected_graph"),
        WorkerCheck::Grading => Some(field(record, "selected_graph")?),
    };
    let current = provenance::selected_graph(
        root,
        &provenance::cargo_metadata(root)?,
        name,
        &provenance::matroska_inputs(),
    )?;
    match check {
        WorkerCheck::Collection if !recorded.is_some_and(|g| diff::equal(g, &current)) => Err(
            format!("{name} sources changed since build identity; rebuild"),
        ),
        WorkerCheck::Grading => {
            require_same(
                recorded.expect("checked graph"),
                &current,
                "candidate source graph",
            )?;
            Ok(path)
        }
        WorkerCheck::Collection => Ok(path),
    }
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

    fn built_workers() -> (TempDir, Value) {
        let directory = TempDir::new(None).unwrap();
        let root = &directory.0;
        for path in ["target/release", "target/harness", "crates/example/src"] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        for (path, bytes) in [
            ("target/release/media-worker", "candidate"),
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
                    ("candidate", digest("target/release/media-worker")),
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
        for check in [WorkerCheck::Collection, WorkerCheck::Grading] {
            assert!(current_workers(root, &identity, check).is_ok());
            for path in [
                "target/release/media-worker",
                "target/harness/ffmpeg-worker",
                "crates/example/src/lib.rs",
                "config.json",
            ] {
                let original = fs::read(root.join(path)).unwrap();
                fs::write(root.join(path), b"changed").unwrap();
                assert!(current_workers(root, &identity, check).is_err(), "{path}");
                fs::write(root.join(path), original).unwrap();
            }
        }
    }

    #[test]
    fn collection_and_grading_keep_their_identity_requirements() {
        let (directory, identity) = built_workers();
        let root = &directory.0;
        let mut missing = identity.clone();
        let Value::Object(fields) = &mut missing else {
            unreachable!()
        };
        fields.retain(|(key, _)| key != "comparison_references");
        assert!(current_workers(root, &missing, WorkerCheck::Collection).is_ok());
        assert_eq!(
            current_workers(root, &missing, WorkerCheck::Grading).unwrap_err(),
            "missing comparison_references"
        );

        let mut extra = identity.clone();
        let mut binaries = field(&extra, "binaries").unwrap().clone();
        put(&mut binaries, "other", Value::str("extra binary")).unwrap();
        put(&mut extra, "binaries", binaries).unwrap();
        assert!(current_workers(root, &extra, WorkerCheck::Collection).is_err());
        assert!(current_workers(root, &extra, WorkerCheck::Grading).is_ok());

        for name in ["..", "../media-worker", "/media-worker", ""] {
            assert_eq!(
                candidate_worker(root, &identity, name, WorkerCheck::Grading).unwrap_err(),
                "candidate must name a worker"
            );
        }
    }

    #[test]
    fn comparison_references_keep_hash_and_directory_bindings() {
        let (directory, mut identity) = built_workers();
        let root = &directory.0;
        fs::write(root.join("target/harness/other-worker"), b"comparison").unwrap();
        for (path, digest) in [
            ("target/harness/other-worker", "0".repeat(64)),
            (
                "target/release/media-worker",
                linux::file_sha(&root.join("target/release/media-worker")).unwrap(),
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
            for check in [WorkerCheck::Collection, WorkerCheck::Grading] {
                assert_eq!(
                    current_workers(root, &identity, check).unwrap_err(),
                    "comparison reference changed since build identity; rebuild"
                );
            }
        }
    }
}
