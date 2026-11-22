//! Public regression samples used by the required views.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Component, Path};

use crate::fetch::{self, Network, Source};
use crate::harness::{features, manifest, preflight};

const FATE: &str = ".research/fate-samples";
const SUBTITLE_FATE: &str = ".research/subtitle-corpus/fate";
const FLAC: &str = ".research/flac-test-files";
const FLAC_RELEASE: &str = "aa7b0c6cf32994c106ae517a08134c28a96ff5b2";

/// Fetch exactly the public files named by required contracts, using the
/// corpus pins for both the download cache and the upstream files.
pub(super) fn prepare(root: &Path) -> Result<(), String> {
    let mut args = Vec::new();
    for contract in features::required(root)? {
        if let Some(directory) = contract
            .document
            .get("corpus_directory")
            .and_then(crate::json::Value::as_str)
        {
            args.extend(["--corpus".into(), format!("{}={directory}", contract.id)]);
        }
        args.extend(["--feature".into(), contract.id]);
    }
    let mut files = BTreeMap::new();
    for input in preflight::inputs(root, &args, &|path| {
        manifest::read(root, path).map(|(document, _)| document)
    })? {
        let Some(pin) = input.sha256 else {
            continue;
        };
        for directory in [FATE, SUBTITLE_FATE, FLAC] {
            let Ok(relative) = input.path.strip_prefix(root.join(directory)) else {
                continue;
            };
            if !relative
                .components()
                .all(|p| matches!(p, Component::Normal(_)))
            {
                return Err(format!("{} escapes its corpus", input.path.display()));
            }
            let name = relative.to_str().ok_or("non-UTF-8 corpus path")?;
            if files
                .insert((directory, name.to_owned()), pin.clone())
                .is_some_and(|old| old != pin)
            {
                return Err(format!("{directory}/{name}: conflicting corpus pins"));
            }
        }
    }
    let mut failures = Vec::new();
    for ((directory, name), pin) in &files {
        let directory_path = root.join(directory);
        let result = if *directory == FLAC {
            flac(&directory_path, name, pin)
        } else {
            fetch::fate_sample_bounded(&directory_path, name, pin, 16 * 1024 * 1024)
        };
        if let Err(error) = result {
            failures.push(format!("{directory}/{name}: {error}"));
        }
    }
    if !failures.is_empty() {
        return Err(failures.join("\n"));
    }
    println!("Verified {} public media inputs", files.len());
    Ok(())
}

fn flac(directory: &Path, file: &str, pin: &str) -> Result<(), String> {
    let path = directory.join(file);
    if path.is_file() {
        return if crate::harness::linux::file_sha(&path)? == pin {
            Ok(())
        } else {
            Err("input hash differs from its pin".into())
        };
    }
    let mut encoded = String::new();
    for byte in file.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            write!(&mut encoded, "%{byte:02X}").expect("writing to a string succeeds");
        }
    }
    let url = format!(
        "https://raw.githubusercontent.com/ietf-wg-cellar/flac-test-files/{FLAC_RELEASE}/{encoded}"
    );
    let bytes = fetch::pinned(
        &Network,
        &[Source::Url(&url)],
        pin,
        96 * 1024 * 1024,
        "triergon-required-test-vectors",
    )?;
    let parent = path.parent().ok_or("FLAC test file without a directory")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    fetch::publish(&path, &bytes)
}
