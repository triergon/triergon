//! Codec and reader lanes scheduled by the gate.

use std::collections::BTreeSet;
use std::path::Path;

use crate::harness::bounds::{Bound, Unmeasured};
use crate::json::{self, Value};

pub(crate) const FILE: &str = "harness/codecs/gate-lanes.json";

pub(super) fn planned() -> Value {
    json::parse(include_str!("../../../harness/codecs/gate-lanes.json"))
        .expect("codec gate lane registry")
}

pub(super) fn item<'a>(registry: &'a Value, name: &str) -> impl Iterator<Item = &'a str> {
    registry
        .get("items")
        .and_then(|items| items.get(name))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

pub(super) fn video_workloads() -> String {
    video_workload_names(include_str!("../../../harness/codecs/workloads.json"))
}

pub(super) fn video_workload_names(list: &str) -> String {
    let list = json::parse(list).expect("video workload list");
    workloads(&list)
        .iter()
        .filter(|row| {
            row.get("operation")
                .and_then(Value::as_str)
                .is_some_and(|operation| operation.starts_with("video-"))
        })
        .filter_map(|row| row.get("path").and_then(Value::as_str))
        .filter_map(|path| path.rsplit('/').next())
        .collect::<Vec<_>>()
        .join(",")
}

fn workload_path(name: &str) -> String {
    let filename = match name {
        "codec-workloads" => "workloads",
        "codec-file-workloads" => "file-workloads",
        other => other,
    };
    format!("harness/codecs/{filename}.json")
}

pub(crate) fn design_errors(root: &Path, registry: &Value) -> Vec<String> {
    match validate(root, registry) {
        Ok(()) => Vec::new(),
        Err(error) => vec![format!("{FILE}: {error}")],
    }
}

fn validate(root: &Path, registry: &Value) -> Result<(), String> {
    if registry.get("schema") != Some(&Value::int(1)) {
        return Err("expected schema 1".into());
    }
    let Some(Value::Object(items)) = registry.get("items") else {
        return Err("missing items".into());
    };
    let Some(Value::Object(lanes)) = registry.get("lanes") else {
        return Err("missing lanes".into());
    };
    let mut scheduled = BTreeSet::new();
    for (name, list) in items {
        let list = list
            .as_array()
            .filter(|list| !list.is_empty())
            .ok_or_else(|| format!("{name}: expected a nonempty lane list"))?;
        for lane in list {
            let lane = lane
                .as_str()
                .ok_or_else(|| format!("{name}: expected a lane name"))?;
            if !lanes.iter().any(|(registered, _)| registered == lane) {
                return Err(format!("{name}: unknown lane {lane}"));
            }
            if !scheduled.insert(lane) {
                return Err(format!("{lane}: scheduled more than once"));
            }
        }
    }
    let manifest = Value::object([(
        "cases",
        Value::Array(
            lanes
                .iter()
                .map(|(lane, _)| Value::object([("id", Value::str(lane))]))
                .collect(),
        ),
    )]);
    let unmeasured = super::TARGETS
        .into_iter()
        .map(|target| Unmeasured::read(registry, &manifest, target).map(|bounds| (target, bounds)))
        .collect::<Result<Vec<_>, _>>()?;
    for (lane, definition) in lanes {
        let grade_lane = match lane.as_str() {
            "aac-default" => "aac",
            "hevc-simd" => "hevc",
            lane => lane,
        };
        if !scheduled.contains(lane.as_str())
            || !(crate::codecs::correctness::is_lane(grade_lane)
                || crate::codecs::readers::format(lane).is_some()
                || matches!(lane.as_str(), "reader-ogg-seek" | "imdct"))
        {
            return Err(format!("{lane}: unrecognized or unscheduled lane"));
        }
        if !matches!(
            definition.get("build").and_then(Value::as_str),
            Some("stable" | "simd" | "session")
        ) {
            return Err(format!("{lane}: unknown build"));
        }
        match definition.get("measured_by") {
            Some(Value::Null) => {
                for (target, unmeasured) in &unmeasured {
                    for bound in Bound::ALL {
                        if !unmeasured.has(lane, bound) {
                            return Err(format!("{lane}: missing unmeasured {bound} on {target}"));
                        }
                    }
                }
            }
            Some(Value::String(list)) => {
                let path = if list == "aac-access-units" {
                    "harness/features/aac-access-units/corpus.json".into()
                } else {
                    workload_path(list)
                };
                if !root.join(&path).is_file() {
                    return Err(format!("{lane}: missing workload list {path}"));
                }
            }
            _ => return Err(format!("{lane}: missing measured_by")),
        }
    }
    validate_subset(root, "aac-default-workloads", "audio-aac")?;
    validate_subset(root, "hevc-simd-workloads", "video-hevc")?;
    validate_workload_vectors(root)?;
    Ok(())
}

fn workloads(list: &Value) -> &[Value] {
    list.get("workloads")
        .and_then(Value::as_array)
        .unwrap_or_default()
}

fn validate_subset(root: &Path, list: &str, operation: &str) -> Result<(), String> {
    let origin = json::read(&root.join(workload_path("codec-workloads")))?;
    let target = json::read(&root.join(workload_path(list)))?;
    let mut expected = workloads(&origin)
        .iter()
        .filter(|row| row.get("operation").and_then(Value::as_str) == Some(operation))
        .peekable();
    if expected.peek().is_none() || !expected.eq(workloads(&target)) {
        return Err(format!(
            "{list} differs from {operation} in codec-workloads"
        ));
    }
    Ok(())
}

fn validate_workload_vectors(root: &Path) -> Result<(), String> {
    let manifest = json::read(&root.join(crate::codecs::derived::MANIFEST))?;
    let vectors = manifest
        .get("vectors")
        .and_then(Value::as_array)
        .ok_or("correctness manifest has no vectors")?;
    for list in ["codec-workloads", "codec-file-workloads"] {
        let list = json::read(&root.join(workload_path(list)))?;
        for row in workloads(&list) {
            let path = row
                .get("path")
                .and_then(Value::as_str)
                .ok_or("workload without path")?;
            let operation = row
                .get("operation")
                .and_then(Value::as_str)
                .ok_or("workload without operation")?;
            let (_, lane) = operation
                .split_once('-')
                .ok_or("workload operation without a lane")?;
            let layer_two = Path::new(path)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("mp2"));
            let lane = if lane == "mp3" && layer_two {
                "mp2"
            } else {
                lane
            };
            if !vectors.iter().any(|vector| {
                vector.get("lane").and_then(Value::as_str) == Some(lane)
                    && vector.get("path") == row.get("path")
                    && vector.get("sha256") == row.get("sha256")
            }) {
                return Err(format!("{path}: no pinned {lane} correctness vector"));
            }
        }
    }
    Ok(())
}
