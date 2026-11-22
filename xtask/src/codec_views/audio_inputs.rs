//! Reproduce audio view fixtures from the selected public or authored sources.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path};
use std::process::Command;

use crate::harness::{features, linux, validation};
use crate::json::{self, Value};
use crate::{fetch, process, sha256};

const DECODER: &str = ".research/reference-decode/install/bin/ffmpeg";
const FLAC: &str = ".research/flac-generated";
const MP3: &str = ".research/mp3/vectors";
const OGG: &str = ".research/ogg-page-inputs";

pub(super) fn prepare(root: &Path) -> Result<(), String> {
    copies(root)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let stage = root.join(format!(
        ".research/audio-fixtures-{}-{stamp}",
        std::process::id()
    ));
    crate::harness::report::create_output(&stage)?;
    let result = mpeg_audio(root, &stage)
        .and_then(|()| ogg(root, &stage))
        .and_then(|()| flac(root, &stage));
    let clean = fs::remove_dir_all(&stage).map_err(|e| format!("{}: {e}", stage.display()));
    result.and(clean)
}

fn copies(root: &Path) -> Result<(), String> {
    let streams = crate::codecs::derived::streams(root)?;
    let workloads = json::read(&root.join("harness/codecs/workloads.json"))?;
    let mut count = 0;
    for row in workloads
        .get("workloads")
        .and_then(Value::as_array)
        .ok_or("missing workloads")?
    {
        let path = validation::text(row, "path")?;
        if !path.starts_with(".research/codec-inputs/")
            || !validation::text(row, "origin")?.starts_with("FATE")
        {
            continue;
        }
        let pin = validation::hash(row, "sha256")?;
        let source = streams
            .iter()
            .find(|stream| stream.sha256 == pin)
            .ok_or_else(|| format!("{path}: missing pinned stream recipe"))?;
        publish(&root.join(&source.path), &root.join(path), pin)?;
        count += 1;
    }
    println!("Verified {count} audio workload copies");
    Ok(())
}

fn mpeg_audio(root: &Path, stage: &Path) -> Result<(), String> {
    for script in ["random_frames.py", "lsf_intensity.py"] {
        process::run(
            Command::new(crate::python::interpreter())
                .arg("-B")
                .arg(root.join("harness/codecs/mp3").join(script))
                .arg(stage),
        )?;
    }
    let manifest = json::read(&root.join(crate::codecs::derived::MANIFEST))?;
    let mut pins = BTreeMap::new();
    for row in manifest
        .get("vectors")
        .and_then(Value::as_array)
        .ok_or("missing correctness vectors")?
    {
        if let Ok(file) = Path::new(validation::text(row, "path")?).strip_prefix(MP3) {
            pins.insert(file, validation::hash(row, "sha256")?);
        }
    }
    let mut count = 0;
    for entry in fs::read_dir(stage).map_err(|e| format!("{}: {e}", stage.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let file = Path::new(&name);
        let pin = pins
            .get(file)
            .ok_or_else(|| format!("{}: no MPEG audio pin", file.display()))?;
        publish(&entry.path(), &root.join(MP3).join(file), pin)?;
        count += 1;
    }
    println!("Verified {count} authored MPEG audio inputs");
    Ok(())
}

fn ogg(root: &Path, stage: &Path) -> Result<(), String> {
    let pins: BTreeMap<_, _> = features::required_inputs(root)?
        .into_iter()
        .filter_map(|input| {
            let file = input.path.strip_prefix(root.join(OGG)).ok()?;
            Some((file.to_path_buf(), input.sha256?))
        })
        .collect();
    let manifest = json::read(&root.join("harness/codecs/reader-manifest.json"))?;
    let mut count = 0;
    for row in manifest
        .get("vectors")
        .and_then(Value::as_array)
        .ok_or("missing reader vectors")?
    {
        let source = validation::text(row, "path")?;
        if validation::text(row, "lane")? != "reader-ogg"
            || !source.starts_with(".research/fate-samples/")
        {
            continue;
        }
        let name = validation::text(row, "name")?;
        let file = format!("page-a-{}", name.replace('/', "-"));
        let file = Path::new(&file);
        let Some(pin) = pins.get(file) else {
            continue;
        };
        if !matches!(
            file.components().collect::<Vec<_>>().as_slice(),
            [Component::Normal(_)]
        ) {
            return Err("invalid Ogg fixture name".into());
        }
        let source = root.join(source);
        if linux::file_sha(&source)? != validation::hash(row, "sha256")? {
            return Err(format!("{} differs from its pin", source.display()));
        }
        let output = stage.join(file);
        process::run(
            Command::new(root.join(DECODER))
                .args([
                    "-nostdin",
                    "-v",
                    "error",
                    "-y",
                    "-fflags",
                    "+bitexact",
                    "-i",
                ])
                .arg(source)
                .args(["-c", "copy", "-fflags", "+bitexact", "-f", "ogg"])
                .arg(&output),
        )?;
        publish(&output, &root.join(OGG).join(file), pin)?;
        count += 1;
    }
    println!("Verified {count} Ogg remuxes from FATE");
    Ok(())
}

fn flac(root: &Path, stage: &Path) -> Result<(), String> {
    let files: BTreeMap<_, _> = features::required_inputs(root)?
        .into_iter()
        .filter_map(|input| {
            let name = input.path.strip_prefix(root.join(FLAC)).ok()?;
            Some((name.to_path_buf(), input.sha256?))
        })
        .collect();
    let output = stage.join("flac");
    process::run(
        Command::new(crate::python::interpreter())
            .arg("-B")
            .arg(root.join("harness/codecs/flac/flac_vectors.py"))
            .arg(root.join(DECODER))
            .arg("flac")
            .arg(&output),
    )?;
    for (name, pin) in &files {
        if !matches!(
            name.components().collect::<Vec<_>>().as_slice(),
            [Component::Normal(_)]
        ) {
            return Err("invalid FLAC fixture name".into());
        }
        publish(&output.join(name), &root.join(FLAC).join(name), pin)?;
    }
    println!("Verified {} authored FLAC inputs", files.len());
    Ok(())
}

fn publish(source: &Path, output: &Path, pin: &str) -> Result<(), String> {
    let bytes = fs::read(source).map_err(|e| format!("{}: {e}", source.display()))?;
    if sha256::hex(&bytes) != pin {
        return Err(format!("{} differs from its pin", output.display()));
    }
    let parent = output.parent().ok_or("fixture without a directory")?;
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    fetch::publish(output, &bytes)
}
