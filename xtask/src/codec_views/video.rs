//! Video view corpora from the decoder's pinned conformance inputs.

use std::fs;

use std::path::Path;

use crate::json::{self, Value};

use super::blocks::{Cases, string};

pub(super) fn fixtures(root: &Path, id: &str, write: bool) -> Result<(), String> {
    let mut cases = Cases::new(root, id);
    let grade = json::read(&root.join("harness/codecs/correctness-manifest.json"))?;
    let vectors = grade
        .get("vectors")
        .and_then(Value::as_array)
        .ok_or("missing correctness vectors")?;
    for row in vectors {
        let codec = string(row, "lane")?;
        let name = string(row, "name")?;
        let path = string(row, "path")?;
        let pin = string(row, "sha256")?;
        if matches!(codec, "h264" | "hevc") && path.starts_with(".research/fate-samples/") {
            add(
                &mut cases,
                id,
                name,
                codec,
                path,
                pin,
                "fetched-not-redistributed",
            );
        }
    }
    let list = json::read(&root.join("harness/codecs/workloads.json"))?;
    for row in list
        .get("workloads")
        .and_then(Value::as_array)
        .ok_or("missing workload list")?
    {
        if let Some(codec @ ("h264" | "hevc")) = string(row, "operation")?.strip_prefix("video-")
            && matches!(
                id,
                "hevc-access-units" | "picture-properties" | "video-nal-input"
            )
        {
            add(
                &mut cases,
                id,
                string(row, "id")?,
                codec,
                string(row, "path")?,
                string(row, "sha256")?,
                "authored-synthetic",
            );
        }
    }
    if id == "video-nal-input" || id == "hevc-access-units" {
        authored(root, id, &mut cases, write)?;
    }
    cases.finish(write)
}

fn add(
    cases: &mut Cases<'_>,
    id: &str,
    name: &str,
    codec: &str,
    path: &str,
    pin: &str,
    rights: &str,
) {
    match id {
        "hevc-access-units" if codec == "hevc" => {
            for window in ["whole", "4096"] {
                cases.pinned(
                    &format!("{name}-{window}"),
                    codec,
                    path,
                    pin,
                    rights,
                    &[window],
                );
            }
        }
        "picture-properties" => cases.pinned(name, codec, path, pin, rights, &[codec]),
        "h264-stream-inspection" if codec == "h264" => {
            cases.pinned(name, codec, path, pin, rights, &[]);
        }
        "video-nal-input" => {
            cases.pinned(
                &format!("{name}-nal"),
                codec,
                path,
                pin,
                rights,
                &[codec, "nal"],
            );
            if codec == "h264" {
                cases.pinned(
                    &format!("{name}-nals-split"),
                    codec,
                    path,
                    pin,
                    rights,
                    &[codec, "nals-split"],
                );
            }
        }
        _ => {}
    }
}

fn authored(root: &Path, feature: &str, cases: &mut Cases<'_>, write: bool) -> Result<(), String> {
    for (codec, ext, aud_type, end_types, damaged) in [
        ("h264", "h264", 9, [10, 11], &[0, 0, 1, 0x41, 0x40][..]),
        ("hevc", "hevc", 35, [36, 37], &[0, 0, 1, 2, 1, 0x40][..]),
    ] {
        if codec == "h264" && feature == "hevc-access-units" {
            continue;
        }
        let fixture = |name: &str| format!("crates/framing/fixtures/raw-annex-b/{name}");
        let aud = fixture(&format!(
            "{}-aud1.{ext}",
            if codec == "h264" { "avc" } else { "hevc" }
        ));
        let bytes = fs::read(root.join(&aud)).map_err(|error| error.to_string())?;
        add(
            cases,
            feature,
            &format!("{codec}-every-aud"),
            codec,
            &aud,
            &crate::sha256::hex(&bytes),
            "authored-synthetic",
        );
        if codec == "hevc" {
            let end = fixture("hevc-eos.hevc");
            let bytes = fs::read(root.join(&end)).map_err(|error| error.to_string())?;
            add(
                cases,
                feature,
                "hevc-end-of-sequence",
                codec,
                &end,
                &crate::sha256::hex(&bytes),
                "authored-synthetic",
            );
        }
        if feature == "hevc-access-units" {
            continue;
        }

        let starts: Vec<_> = bytes
            .windows(3)
            .enumerate()
            .filter(|(index, prefix)| *prefix == [0, 0, 1] && index + 3 < bytes.len())
            .map(|(index, _)| {
                let begin = index - usize::from(index > 0 && bytes[index - 1] == 0);
                let header = bytes[index + 3];
                let kind = if codec == "h264" {
                    header & 31
                } else {
                    header >> 1 & 63
                };
                (begin, kind)
            })
            .collect();
        let mut no_eos = Vec::new();
        for (i, &(at, kind)) in starts.iter().enumerate() {
            let end = starts.get(i + 1).map_or(bytes.len(), |&(end, _)| end);
            if !end_types.contains(&kind) {
                no_eos.extend_from_slice(&bytes[at..end]);
            }
        }
        let midpoint = starts
            .iter()
            .filter(|(_, kind)| *kind == aud_type)
            .nth(1)
            .ok_or("the authored stream lacks the second delimiter")?
            .0;
        let eos = if codec == "h264" {
            &[0, 0, 1, 10, 0x80][..]
        } else {
            &[0, 0, 1, 72, 1, 0x80][..]
        };
        for (variant, data) in [
            ("no-eos", no_eos),
            (
                "mid-eos",
                [&bytes[..midpoint], eos, &bytes[midpoint..]].concat(),
            ),
            (
                "damaged-slice",
                [&bytes[..midpoint], damaged, &bytes[midpoint..]].concat(),
            ),
        ] {
            let name = format!("{codec}-{variant}");
            let (path, pin) = cases.authored(&format!("{name}.{ext}"), &data, write)?;
            add(
                cases,
                feature,
                &name,
                codec,
                &path,
                &pin,
                "authored-synthetic",
            );
        }
    }
    Ok(())
}
