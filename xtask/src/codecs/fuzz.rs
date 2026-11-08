//! `codecs fuzz`: a [`crate::fuzz`] campaign over one lane's hashed vectors. Each input
//! is decoded whole by `codec-worker` in a release build and a `checked` build (release
//! optimisation with overflow checks and debug assertions), so a panic, a hang and a
//! difference between the two builds are findings; a decode error is the typed refusal.

use std::fs;
use std::path::Path;

use super::equivalence::cargo_build;
use crate::json::Value;
use crate::sha256::hex;

/// Lanes of `harness/codecs/correctness-manifest.json` and their `codec-worker` operation.
const LANES: [(&str, &str); 6] = [
    ("h264", "video-h264"),
    ("hevc", "video-hevc"),
    ("aac", "audio-aac"),
    ("ac3", "audio-ac3"),
    ("eac3", "audio-eac3"),
    ("dts", "audio-dts"),
];

const MANIFEST: &str = "harness/codecs/correctness-manifest.json";

/// The FATE suite is downloaded for research and never redistributed.
const RIGHTS: &str = "FFmpeg FATE suite sample, downloaded for research, not redistributed";

/// Run the campaign; `only` keeps the vectors whose name contains one of its texts.
pub fn run(
    root: &Path,
    lane: &str,
    only: &[String],
    campaign: &crate::fuzz::Campaign,
) -> Result<i32, String> {
    let operation = LANES
        .iter()
        .find(|(name, _)| *name == lane)
        .map(|(_, operation)| *operation)
        .ok_or_else(|| format!("unknown lane {lane}"))?;
    cargo_build(root, &["-p", "codec-worker"], false)?;
    let status = std::process::Command::new("cargo")
        .args([
            "build",
            "--locked",
            "--profile",
            "checked",
            "-p",
            "codec-worker",
        ])
        .env("CARGO_INCREMENTAL", "0")
        .current_dir(root)
        .status()
        .map_err(|e| format!("cargo: {e}"))?;
    if !status.success() {
        return Err(format!("checked codec-worker build: {status}"));
    }
    let manifest = crate::checks::read_json(&root.join(MANIFEST))?;
    let mut seeds = Vec::new();
    let mut largest = 0;
    for vector in manifest
        .get("vectors")
        .and_then(Value::as_array)
        .unwrap_or_default()
    {
        let text = |key: &str| vector.get(key).and_then(Value::as_str).unwrap_or_default();
        let name = text("name");
        if text("lane") != lane
            || !(only.is_empty() || only.iter().any(|o| name.contains(o.as_str())))
        {
            continue;
        }
        let bytes = fs::metadata(root.join(text("path")))
            .map_err(|e| format!("{}: {e}", text("path")))?
            .len() as usize;
        largest = largest.max(bytes);
        seeds.push(Value::object([
            ("id", Value::str(name)),
            ("path", Value::str(text("path"))),
            ("sha256", Value::str(text("sha256"))),
            ("rights", Value::str(RIGHTS)),
            (
                "baseline_statuses",
                Value::object([(
                    "decode",
                    Value::object([("candidate", Value::str("ok")), ("debug", Value::str("ok"))]),
                )]),
            ),
        ]));
    }
    if seeds.is_empty() {
        return Err(format!("{lane}: no vector selected"));
    }
    let worker = |profile: &str| -> Result<Value, String> {
        let path = format!("target/{profile}/codec-worker");
        let data = fs::read(root.join(&path)).map_err(|e| format!("{path}: {e}"))?;
        Ok(Value::object([
            ("path", Value::str(path)),
            ("sha256", Value::str(hex(&data))),
        ]))
    };
    let argv = Value::strings([format!("{operation}-fold"), "{input}".into()]);
    let source = fs::read(root.join(MANIFEST)).map_err(|e| format!("{MANIFEST}: {e}"))?;
    let campaign_manifest = Value::object([
        ("schema", Value::int(1)),
        (
            "workers",
            Value::object([
                ("candidate", worker("release")?),
                ("debug", worker("checked")?),
            ]),
        ),
        (
            "operations",
            Value::Array(vec![Value::object([
                ("id", Value::str("decode")),
                (
                    "roles",
                    Value::object([("candidate", argv.clone()), ("debug", argv)]),
                ),
            ])]),
        ),
        (
            "source_files",
            Value::object([(MANIFEST, Value::str(hex(&source)))]),
        ),
        ("seeds", Value::Array(seeds)),
    ]);
    let path =
        std::env::temp_dir().join(format!("triergon-codec-fuzz-{}.json", std::process::id()));
    crate::corpus::write_json(&path, &campaign_manifest)?;
    let result = crate::fuzz::campaign(
        root,
        &crate::fuzz::Campaign {
            manifest: path.clone(),
            output: campaign.output.clone(),
            maximum_input_bytes: campaign.maximum_input_bytes.max(largest),
            ..*campaign
        },
    );
    let _ = fs::remove_file(&path);
    result
}
