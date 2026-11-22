//! Prepare the reproducible subtitle carriers without recording reference folds.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::harness::{features, report};
use crate::{fetch, sha256};

const CORPUS: &str = ".research/subtitle-corpus";
const DECODER_BIN: &str = ".research/reference-decode/install/bin";

/// Carriers whose exact bytes require an archived authoring input or output.
pub(crate) fn private_input(path: &Path) -> bool {
    let Ok(path) = path.strip_prefix(CORPUS) else {
        return false;
    };
    path.starts_with("cc")
        || path.starts_with("dvb/research")
        || path == Path::new("pgs/fate-pgs_sub.mkv")
        || (path.starts_with("dvb")
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("recorded-")
                        && (name.ends_with("--matroska-segments.ts")
                            || name.ends_with("--pes-data-field.ts"))
                }))
}

const AUTHOR: &str = r"
from pathlib import Path
import shutil
import sys
from harness.subtitles import dvd_streams as dvd, dvb_streams as dvb
from harness.subtitles import pgs_streams as pgs, teletext_streams as teletext

stage = Path(sys.argv[1])
corpus = stage / '.research/subtitle-corpus'
for name in ('dvd', 'dvb', 'dvb/authored'):
    (corpus / name).mkdir(parents=True, exist_ok=True)
dvd.generate(str(corpus / 'dvd'))
dvb.main([str(corpus / 'dvb')])
for name, data in dvb.recorded(str(Path.cwd())).items():
    (corpus / 'dvb' / (name + '.ts')).write_bytes(data)
for source in map(Path, dvb.AUTHORED_INPUTS):
    shutil.copyfile(source, corpus / 'dvb/authored' / source.name)
teletext.write_streams(str(corpus / 'teletext'))

# The PGS author records a manifest as well. Keep it in the workspace
# used for generation, away from the committed pins.
(stage / 'harness/features/subtitle-pgs').mkdir(parents=True)
pgs.ROOT = str(stage)
pgs.REFERENCE = str(stage / 'no-reference-worker')
pgs.FATE = sys.argv[2]
pgs.main()
";

pub(super) fn prepare(root: &Path) -> Result<(), String> {
    let mut files = BTreeMap::new();
    for input in features::required_inputs(root)? {
        let (Ok(path), Some(pin)) = (input.path.strip_prefix(root), input.sha256) else {
            continue;
        };
        if !path.starts_with(CORPUS)
            || path.starts_with(".research/subtitle-corpus/fate")
            || private_input(path)
        {
            continue;
        }
        if !path.components().all(|p| matches!(p, Component::Normal(_))) {
            return Err(format!("{} escapes its corpus", path.display()));
        }
        if files
            .insert(path.to_path_buf(), pin.clone())
            .is_some_and(|old| old != pin)
        {
            return Err(format!("{}: conflicting subtitle pins", path.display()));
        }
    }
    if files.keys().all(|path| root.join(path).is_file()) {
        return verify_and_publish(root, root, &files);
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?;
    let stage = root.join(".research").join(format!(
        "subtitle-fixtures-{}-{}",
        std::process::id(),
        stamp.as_nanos()
    ));
    report::create_output(&stage)?;
    let path = std::env::join_paths(std::iter::once(root.join(DECODER_BIN)).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .map_err(|e| e.to_string())?;
    let result = crate::process::run(
        Command::new(crate::python::interpreter())
            .args(["-B", "-c", AUTHOR])
            .arg(&stage)
            .arg(root.join(CORPUS).join("fate/sub/pgs_sub.sup"))
            .env("PATH", path)
            .current_dir(root),
    )
    .and_then(|()| verify_and_publish(root, &stage, &files));
    let cleaned = fs::remove_dir_all(&stage).map_err(|e| format!("{}: {e}", stage.display()));
    result.and(cleaned)
}

fn verify_and_publish(
    root: &Path,
    stage: &Path,
    files: &BTreeMap<std::path::PathBuf, String>,
) -> Result<(), String> {
    for (path, pin) in files {
        let source = stage.join(path);
        let bytes = fs::read(&source).map_err(|e| format!("{}: {e}", source.display()))?;
        if sha256::hex(&bytes) != *pin {
            return Err(format!("{} differs from its pin", path.display()));
        }
        if root != stage {
            let out = root.join(path);
            let parent = out.parent().ok_or("subtitle fixture without a directory")?;
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
            fetch::publish(&out, &bytes)?;
        }
    }
    println!("Verified {} authored subtitle inputs", files.len());
    Ok(())
}
