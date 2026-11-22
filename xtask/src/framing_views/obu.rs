//! AV1 IVF envelopes for the framing reader and wire syntax comparison.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{
    corpus::Paths,
    fetch,
    json::{self, Value},
};

use super::{Result, StreamCorpus};

pub(super) const IDS: [&str; 2] = ["av1-obu-reading", "av1-obu-wire"];
const TSV: &str = "crates/framing/fixtures/av1-obu-v1.tsv";
const SOURCES: &str = "harness/features/av1-obu-reading/sources.json";

fn ivf(payload: &[u8]) -> Vec<u8> {
    let mut output = b"DKIF\0\0\x20\0AV01\x10\0\x10\0".to_vec();
    for word in [25_u32, 1, 1, 0, payload.len() as u32] {
        output.extend(word.to_le_bytes());
    }
    output.extend(0_u64.to_le_bytes());
    output.extend(payload);
    output
}

fn cases(
    corpus: &mut StreamCorpus,
    name: &str,
    profile: &str,
    file: &str,
    sha: &str,
    rights: &str,
    [mode, exclude]: [&str; 2],
) {
    for window in ["1", "7", "4096"] {
        corpus.case(
            &format!("{name}-w{window}"),
            profile,
            file,
            sha,
            rights,
            &[mode, window, exclude],
        );
    }
}

pub(super) fn authored(paths: &Paths, corpus: &mut StreamCorpus) -> Result<()> {
    let rows = fs::read_to_string(paths.root.join(TSV)).map_err(|e| format!("{TSV}: {e}"))?;
    for line in rows.lines().skip(1) {
        let mut columns = line.split('\t');
        let id = columns.next().ok_or("missing AV1 fixture ID")?;
        let (mode, profile) = match columns.next() {
            Some("low") => ("low-overhead", "authored-low-overhead"),
            Some("one") => ("single", "authored-single"),
            _ => return Err(format!("{id}: unknown AV1 fixture mode")),
        };
        let payload = match columns.next() {
            Some("~") => Vec::new(),
            Some(wire) => crate::worker_tests::unhex(wire)?,
            None => return Err(format!("{id}: missing fixture bytes")),
        };
        let file = format!("{id}.ivf");
        let pin = corpus.write(&file, &ivf(&payload))?;
        let projection = match id {
            // These three complete OBUs deliberately carry opaque payloads.
            "three-obu-order" => "wire",
            "type-0" => "14",
            _ => "0",
        };
        cases(
            corpus,
            id,
            profile,
            &file,
            &pin,
            "authored-synthetic",
            [mode, projection],
        );
    }
    // A reduced still-picture sequence header, independent of the opaque type matrix.
    for (id, mode, profile, wire) in [
        (
            "sequence",
            "low-overhead",
            "authored-low-overhead",
            "12000a06180cffda0080",
        ),
        (
            "sequence-nonminimal",
            "low-overhead",
            "authored-low-overhead",
            "12000a8600180cffda0080",
        ),
        (
            "sequence-single",
            "single",
            "authored-single",
            "08180cffda0080",
        ),
        (
            "delimiter-extension",
            "low-overhead",
            "authored-low-overhead",
            "160800",
        ),
    ] {
        let file = format!("{id}.ivf");
        let pin = corpus.write(&file, &ivf(&crate::worker_tests::unhex(wire)?))?;
        cases(
            corpus,
            id,
            profile,
            &file,
            &pin,
            "authored-synthetic",
            [mode, "0"],
        );
    }
    Ok(())
}

fn source(root: &Path, origin: &str, pin: &str, fate: bool) -> Result<PathBuf> {
    if fate {
        let cache = root.join(".research/fate-samples");
        fetch::fate_sample(&cache, origin, pin)?;
        Ok(cache.join(origin))
    } else {
        let input = root.join(origin);
        super::verify_hash(&input, pin)?;
        Ok(input)
    }
}

pub(super) fn encoded(root: &Path, corpus: &mut StreamCorpus) -> Result<()> {
    let recipes = json::read(&root.join(SOURCES))?;
    let rows = recipes
        .get("sources")
        .and_then(Value::as_array)
        .ok_or("no AV1 sources")?;
    for entry in rows {
        let text = |key| {
            entry
                .get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{SOURCES}: missing {key}"))
        };
        let name = text("id")?;
        let file = format!("{name}.ivf");
        let pin = text("sha256")?;
        let origin = text("source")?;
        let fate = origin.starts_with("av1/");
        let output = corpus.directory.join(&file);
        if !output.is_file() {
            let input = source(root, origin, text("source_sha256")?, fate)?;
            let temporary = output.with_extension(format!("{}.partial", std::process::id()));
            let mut ffmpeg = Command::new(root.join(super::DECODER));
            ffmpeg.args(["-nostdin", "-v", "error", "-y"]);
            if let Some(format) = entry.get("format").and_then(Value::as_str) {
                ffmpeg.args(["-f", format]);
            }
            let result = crate::process::run(
                ffmpeg
                    .arg("-i")
                    .arg(input)
                    .args(["-map", "0:v:0", "-c", "copy", "-f", "ivf"])
                    .arg(&temporary),
            )
            .and_then(|()| super::verify_hash(&temporary, pin))
            .and_then(|()| fs::rename(&temporary, &output).map_err(|e| e.to_string()));
            if result.is_err() {
                let _ = fs::remove_file(&temporary);
                result?;
            }
        }
        super::verify_hash(&output, pin)?;
        let (profile, rights) = if fate {
            ("fate", "fetched-not-redistributed")
        } else {
            ("encoded-authored", "authored-synthetic")
        };
        cases(
            corpus,
            name,
            profile,
            &file,
            pin,
            rights,
            ["low-overhead", "0"],
        );
    }
    Ok(())
}
