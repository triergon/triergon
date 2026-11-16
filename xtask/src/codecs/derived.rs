//! `codecs derive`: the elementary streams of the audio correctness lanes that are
//! made from FATE samples rather than served by FATE as they are. Each comes from
//! one FATE source, fetched by its SHA-256, through the decode profile's `ffmpeg`
//! with the arguments [`RECIPES`] records, or as an unchanged copy, and must equal
//! the correctness manifest's pin for its vector byte for byte. FFmpeg is a pinned
//! tool here: what it writes is checked, never interpreted.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::json::Value;
use crate::{fetch, sha256};

/// How each derived stream is made.
pub const RECIPES: &str = "harness/codecs/derived-streams.json";
const SCHEMA: &str = "triergon.codecs.derived-streams.v1";
/// The manifest whose vectors pin the derived streams' paths and hashes.
pub const MANIFEST: &str = "harness/codecs/correctness-manifest.json";
/// Where the derived streams go: every manifest vector below it is derived.
pub const DIRECTORY: &str = ".research/codec-derived/";
/// Where the FATE sources go, at their paths in the FATE suite.
const FATE: &str = ".research/fate-samples";

/// One derived stream: its manifest vector and how it is made.
#[derive(Debug)]
pub struct Stream {
    pub name: String,
    /// The vector's repository-relative path and pinned SHA-256.
    pub path: String,
    pub sha256: String,
    /// The FATE source, relative to the suite's root, and its SHA-256.
    source: String,
    source_sha256: String,
    /// The `ffmpeg` arguments between the input and the output; none for a copy.
    ffmpeg: Option<Vec<String>>,
}

/// Every derived stream, joined with its manifest vector. Each recipe names a
/// vector below [`DIRECTORY`], and each such vector has exactly one recipe.
pub fn streams(root: &Path) -> Result<Vec<Stream>, String> {
    let recipes = crate::checks::read_json(&root.join(RECIPES))?;
    if recipes.get("schema").and_then(Value::as_str) != Some(SCHEMA) {
        return Err(format!("{RECIPES}: not {SCHEMA}"));
    }
    let manifest = crate::checks::read_json(&root.join(MANIFEST))?;
    let vectors = manifest
        .get("vectors")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{MANIFEST}: no vectors"))?;
    let text = |value: &Value, key: &str| -> Result<String, String> {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("{RECIPES}: a stream without {key}"))
    };
    let mut streams: Vec<Stream> = Vec::new();
    for recipe in recipes
        .get("streams")
        .and_then(Value::as_array)
        .unwrap_or_default()
    {
        let name = text(recipe, "name")?;
        if streams.iter().any(|s| s.name == name) {
            return Err(format!("{RECIPES}: {name} listed twice"));
        }
        let mut found = vectors.iter().filter(|v| {
            v.get("name").and_then(Value::as_str) == Some(&name)
                && v.get("path")
                    .and_then(Value::as_str)
                    .is_some_and(|p| p.starts_with(DIRECTORY))
        });
        let vector = found
            .next()
            .ok_or_else(|| format!("{name}: no vector of {MANIFEST} below {DIRECTORY}"))?;
        if found.next().is_some() {
            return Err(format!(
                "{name}: two vectors of {MANIFEST} below {DIRECTORY}"
            ));
        }
        let source_sha256 = text(recipe, "source_sha256")?;
        if source_sha256.len() != 64 || !source_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("{name}: invalid source_sha256"));
        }
        let ffmpeg = match recipe.get("ffmpeg") {
            None => None,
            Some(args) => Some(
                args.as_array()
                    .filter(|a| !a.is_empty())
                    .ok_or_else(|| format!("{name}: ffmpeg is a list of arguments"))?
                    .iter()
                    .map(|a| a.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| format!("{name}: ffmpeg is a list of arguments"))?,
            ),
        };
        streams.push(Stream {
            path: text(vector, "path")?,
            sha256: text(vector, "sha256")?,
            name,
            source: text(recipe, "source")?,
            source_sha256,
            ffmpeg,
        });
    }
    let unmade: Vec<&str> = vectors
        .iter()
        .filter(|v| {
            v.get("path")
                .and_then(Value::as_str)
                .is_some_and(|p| p.starts_with(DIRECTORY))
        })
        .filter_map(|v| v.get("name").and_then(Value::as_str))
        .filter(|name| !streams.iter().any(|s| s.name == *name))
        .collect();
    if !unmade.is_empty() {
        return Err(format!(
            "vectors below {DIRECTORY} without a recipe in {RECIPES}: {}",
            unmade.join(", ")
        ));
    }
    Ok(streams)
}

/// `codecs derive --ffmpeg FF`: make every derived stream at its manifest path.
/// A stream that does not equal its pin is reported with what was made and left
/// out; the command fails after trying them all.
pub fn run(root: &Path, ffmpeg: &Path) -> Result<(), String> {
    let streams = streams(root)?;
    let mut differ = Vec::new();
    for stream in &streams {
        fetch::fate_sample(&root.join(FATE), &stream.source, &stream.source_sha256)?;
        let source = root.join(FATE).join(&stream.source);
        let out = root.join(&stream.path);
        let parent = out.parent().ok_or("a derived stream without a directory")?;
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        let mut partial = out.clone().into_os_string();
        partial.push(".partial");
        let partial = PathBuf::from(partial);
        match &stream.ffmpeg {
            Some(args) => {
                let status = Command::new(ffmpeg)
                    .args(["-nostdin", "-v", "error", "-y", "-i"])
                    .arg(&source)
                    .args(args)
                    .arg(&partial)
                    .status()
                    .map_err(|e| format!("{}: {e}", ffmpeg.display()))?;
                if !status.success() {
                    let _ = fs::remove_file(&partial);
                    return Err(format!("{}: ffmpeg {status}", stream.name));
                }
            }
            None => {
                fs::copy(&source, &partial).map_err(|e| format!("{}: {e}", partial.display()))?;
            }
        }
        let made = sha256::file_hex(&partial).map_err(|e| format!("{}: {e}", partial.display()))?;
        let bytes = fs::metadata(&partial).map_or(0, |m| m.len());
        if made == stream.sha256 {
            fs::rename(&partial, &out).map_err(|e| format!("{}: {e}", out.display()))?;
            println!("ok {} {bytes} bytes from {}", stream.path, stream.source);
        } else {
            let _ = fs::remove_file(&partial);
            println!(
                "DIFFERS {}: made {bytes} bytes, SHA-256 {made}; pinned {}",
                stream.path, stream.sha256
            );
            differ.push(stream.name.as_str());
        }
    }
    if differ.is_empty() {
        println!("{} streams equal their pins", streams.len());
        Ok(())
    } else {
        Err(format!(
            "{} of {} streams differ from their pins: {}",
            differ.len(),
            streams.len(),
            differ.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_derived_vector_has_one_recipe() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let streams = streams(root).unwrap();
        assert_eq!(streams.len(), 71);
        for stream in &streams {
            assert!(stream.source_sha256.len() == 64, "{}", stream.name);
            assert!(!stream.source.starts_with('/'), "{}", stream.name);
        }
        let copies = streams.iter().filter(|s| s.ffmpeg.is_none()).count();
        assert_eq!(copies, 17);
    }
}
