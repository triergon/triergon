//! Corpus manifests: a feature's case rows and expected results.
//!
//! A feature commits either its whole manifest (`harness/features/<id>/corpus.json`)
//! or only its pins (`pins.json`: each case id with the work the reference folds),
//! in which case the rows are generated (`cargo xtask fonts manifest`) and the
//! manifest is read at that same `corpus.json` path. Either way its identity is the
//! SHA-256 of its bytes: a generated manifest's are its canonical JSON, exactly what
//! `fonts manifest` writes, so the identity is deterministic.

use std::fs;
use std::path::{Path, PathBuf};

use crate::json::{self, Value};
use crate::sha256;

/// The manifest at `path` and the SHA-256 of its bytes.
pub fn read(root: &Path, path: &Path) -> Result<(Value, String), String> {
    if path.exists() {
        let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let text = std::str::from_utf8(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        let value = json::parse(text).map_err(|e| format!("{}: {e}", path.display()))?;
        return Ok((value, sha256::hex(&bytes)));
    }
    let value =
        generated(root, path).ok_or_else(|| format!("{}: no such manifest", path.display()))??;
    Ok((value.clone(), sha256::hex(canonical(&value).as_bytes())))
}

/// The bytes a manifest or pin file is committed or written as.
pub fn canonical(value: &Value) -> String {
    json::to_python(value, Some(2)) + "\n"
}

/// The files a manifest is made from: itself, or its pins and what the rows are
/// generated from.
pub fn sources(root: &Path, path: &Path) -> Vec<PathBuf> {
    if path.exists() {
        return vec![path.to_path_buf()];
    }
    let mut files = vec![path.with_file_name("pins.json")];
    files.extend(crate::fonts::ROW_SOURCES.iter().map(|f| root.join(f)));
    files
}

/// The generated manifest for a `corpus.json` path whose directory holds pins.
fn generated(root: &Path, path: &Path) -> Option<Result<Value, String>> {
    if path.file_name().is_none_or(|n| n != "corpus.json")
        || !path.with_file_name("pins.json").is_file()
    {
        return None;
    }
    let id = path.parent().and_then(Path::file_name)?.to_str()?;
    Some(crate::fonts::manifest(root, id))
}
