//! Complete M2TS program controls derived from the authored TS188 fixture.
use super::{R, program_m2ts};
use crate::{checks::read_json, corpus::write_json, json::Value, sha256};
use std::{fs, path::Path, process::Command};

const ID: &str = "m2ts-replay-avc-aac-00-program";
const INPUT: &str = "harness/fixtures/ts-program/replay-avc-aac-00.m2ts";
const ARGUMENT: &str = "m2ts192:256=avc,257=aac";

/// Append only the combined control; existing cases and pins must still match.
pub fn record(root: &Path, reference: &Path) -> R<()> {
    let reference = reference.canonicalize().map_err(|e| e.to_string())?;
    let identity = read_json(&reference.with_file_name("identity.json"))?;
    let pin = identity
        .get("comparison_references")
        .and_then(|v| v.get("ffmpeg-formats-native"))
        .ok_or("missing pinned formats reference provenance")?;
    if pin.get("sha256").and_then(Value::as_str) != Some(&hash(&reference)?)
        || pin
            .get("reference_build")
            .and_then(|v| v.get("revision"))
            .and_then(Value::as_str)
            != Some("d32b387f2b0a484599d4587d651891f0c63c4238")
    {
        return Err("formats reference differs from its pinned provenance".into());
    }
    let path = root.join("harness/features/ts-program/corpus.json");
    let mut manifest = read_json(&path)?;
    let rows = manifest
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("missing cases")?;
    for row in rows {
        let file = row
            .get("file")
            .and_then(Value::as_str)
            .ok_or("missing file")?;
        if row.get("sha256").and_then(Value::as_str) != Some(&hash(&root.join(file))?) {
            return Err(format!("input pin mismatch: {file}"));
        }
        let argument = row
            .get("arguments")
            .and_then(Value::as_array)
            .and_then(|v| v.first())
            .and_then(Value::as_str)
            .ok_or("missing selection")?;
        if row.get("expected") != Some(&observe(root, &reference, file, argument)?) {
            return Err(format!(
                "existing reference fold changed: {file} {argument}"
            ));
        }
    }
    // Use the same fixture generator, and require every physical byte to stay fixed.
    let original = fs::read(root.join(INPUT)).map_err(|e| e.to_string())?;
    let temporary =
        std::env::temp_dir().join(format!("triergon-program-{}.m2ts", std::process::id()));
    program_m2ts(
        &root.join("harness/fixtures/hls/replay-avc-aac-00.ts"),
        &temporary,
    )?;
    let generated = fs::read(&temporary).map_err(|e| e.to_string())?;
    fs::remove_file(&temporary).map_err(|e| e.to_string())?;
    if generated != original {
        return Err("authored M2TS fixture bytes changed".into());
    }
    let expected = observe(root, &reference, INPUT, ARGUMENT)?;
    let row = Value::object([
        ("id", Value::str(ID)),
        ("profile", Value::str(ID)),
        ("file", Value::str(INPUT)),
        ("sha256", Value::str(hash(&root.join(INPUT))?)),
        ("rights", Value::str("authored-synthetic")),
        ("arguments", Value::strings([ARGUMENT])),
        ("expected", expected),
    ]);
    let Value::Object(members) = &mut manifest else {
        return Err("manifest is not an object".into());
    };
    let Value::Array(rows) = members
        .iter_mut()
        .find(|(k, _)| k == "cases")
        .map(|(_, v)| v)
        .ok_or("missing cases")?
    else {
        return Err("cases is not an array".into());
    };
    if let Some(existing) = rows
        .iter()
        .find(|r| r.get("id").and_then(Value::as_str) == Some(ID))
    {
        if existing != &row {
            return Err("combined program pin changed".into());
        }
    } else {
        rows.push(row);
    }
    write_json(&path, &manifest)
}

fn observe(root: &Path, reference: &Path, file: &str, argument: &str) -> R<Value> {
    let output = Command::new(reference)
        .args(["ts-program-fold", file, argument])
        .current_dir(root)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "reference failed for {file}: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let result = crate::json::parse(
        std::str::from_utf8(&output.stdout)
            .map_err(|e| e.to_string())?
            .trim(),
    )
    .map_err(|e| e.to_string())?;
    if result.get("eof") != Some(&Value::Bool(true)) {
        return Err("reference did not reach EOF".into());
    }
    Ok(Value::object(
        ["packets", "bytes", "digest"]
            .into_iter()
            .map(|key| {
                result
                    .get(key)
                    .cloned()
                    .map(|value| (key, value))
                    .ok_or_else(|| format!("missing {key}"))
            })
            .collect::<R<Vec<_>>>()?,
    ))
}

fn hash(path: &Path) -> R<String> {
    sha256::file_hex(path).map_err(|e| e.to_string())
}
