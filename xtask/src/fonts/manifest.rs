//! The font features' corpus manifests, generated. Every file of the font corpus
//! (`harness/fonts/corpus.json`) becomes cases of each feature's load settings; the
//! work each case must fold is pinned in `harness/features/<id>/pins.json`. Pins are
//! the FreeType reference's folds, written only by [`record`] and never edited by
//! hand, so a change of pins is a change of reference output.

use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{CORPUS, corpus};
use crate::harness::manifest::canonical;
use crate::json::Value;

type R<T> = Result<T, String>;

/// The directory, relative to the repository root, that the cases' files are in.
const DIRECTORY: &str = ".research/fonts/files";

/// The files case rows are generated from, besides the pins.
pub const ROW_SOURCES: [&str; 2] = [CORPUS, "xtask/src/fonts/manifest.rs"];

/// The height libass 0.17.5 loads every glyph at by default (ASS_HINTING_NONE):
/// 256 pixels in 26.6, unhinted, with HarfBuzz advances from FT_LOAD_DEFAULT there.
const UNHINTED_HEIGHT: i32 = 16384;

/// The 26.6 heights libass requests when hinting is on: 75 and 82.5 pixels (ASS
/// sizes 20 and 22 at PlayResY 288 on 1080 lines), 50 (20 at 288 on 720), 48 (48 at
/// 1080 on 1080), 37.5 and 33.75 (10 and 9 at 288 on 1080), 13.33 (20 at 1080 on
/// 720), 24.5 (below the 25 pixels per em where kerning shrinks) and 8.4.
const HINTING_HEIGHTS: [i32; 9] = [4800, 5280, 3200, 3072, 2400, 2160, 853, 1568, 538];

/// At a size: the unhinted outlines and metrics libass draws, and the default-load
/// advances HarfBuzz receives. Each is a case of its own, so an advance the crate
/// cannot give fails only its case.
const MODES: [&str; 2] = ["outlines", "advances"];

/// The cases `cargo xtask check` mutates: one file of each outline format but the
/// collection, small enough for the dev build within the per-input timeout (the
/// CJK collection takes about a minute per load there), and the authored fonts.
pub const SMOKE_CASES: [&str; 5] = [
    "arimo-regular-ttf",
    "notosans-regular-otf",
    "notosansjp-regular-otf",
    "notosansjp-vf-otf",
    "authored-",
];

/// What a feature loads from every face.
#[derive(Clone, Copy)]
enum Load {
    /// Face data and unscaled outlines.
    Faces,
    /// Each mode at each height.
    Sized(&'static [i32]),
}

/// A font feature: its id and load settings, and whether `cargo xtask check`
/// requires it.
#[derive(Clone, Copy)]
pub struct Feature {
    pub id: &'static str,
    load: Load,
    pub required: bool,
}

pub const FEATURES: [Feature; 3] = [
    Feature {
        id: "font-outlines",
        load: Load::Faces,
        required: true,
    },
    Feature {
        id: "font-hinting-none",
        load: Load::Sized(&[UNHINTED_HEIGHT]),
        required: true,
    },
    Feature {
        id: "font-hinting-modes",
        load: Load::Sized(&HINTING_HEIGHTS),
        required: false,
    },
];

fn feature(id: &str) -> R<Feature> {
    FEATURES
        .into_iter()
        .find(|f| f.id == id)
        .ok_or_else(|| format!("{id}: not a font feature"))
}

fn pins_path(root: &Path, id: &str) -> std::path::PathBuf {
    root.join("harness/features").join(id).join("pins.json")
}

/// The case rows of a feature, without expected work, in corpus order.
fn rows(root: &Path, feature: Feature) -> R<Vec<Value>> {
    let mut rows = Vec::new();
    for (source, files) in corpus(root)? {
        if !super::serves(&source, "font") {
            continue;
        }
        let rights = if source.get("generator").and_then(Value::as_str) == Some("authored-fonts") {
            vec![("rights", Value::str("authored-synthetic"))]
        } else {
            let licence = source
                .get("licence")
                .and_then(Value::as_str)
                .ok_or("corpus source without licence")?;
            vec![
                ("rights", Value::str("reviewed-licence")),
                ("licence", Value::str(licence)),
            ]
        };
        for file in files {
            let name = file.path.rsplit('/').next().unwrap_or(&file.path);
            let (stem, extension) = name.rsplit_once('.').unwrap_or((name, ""));
            let stem = format!("{stem}-{extension}").to_lowercase();
            let row = |id: String, arguments: Vec<String>| {
                let mut fields = vec![
                    ("id", Value::str(id)),
                    ("profile", Value::str(&file.format)),
                    ("file", Value::str(&file.path)),
                    ("sha256", Value::str(&file.sha256)),
                ];
                fields.extend(rights.iter().cloned());
                fields.push(("arguments", Value::strings(arguments)));
                Value::object(fields)
            };
            match feature.load {
                Load::Faces => rows.push(row(stem.clone(), Vec::new())),
                Load::Sized(heights) => {
                    for &height in heights {
                        for mode in MODES {
                            let arguments = vec![mode.to_owned(), height.to_string()];
                            rows.push(row(format!("{stem}-{mode}-{height}"), arguments));
                        }
                    }
                }
            }
        }
    }
    Ok(rows)
}

fn note(feature: Feature) -> &'static str {
    match feature.id {
        "font-outlines" => {
            "Every file of the font corpus with every face: face data and unscaled outlines."
        }
        "font-hinting-none" => {
            "Every file of the font corpus as libass 0.17.5 loads it by default (ASS_HINTING_NONE): unhinted outlines at 256 pixels (16384 in 26.6) and the FT_LOAD_DEFAULT advances there."
        }
        _ => {
            "Every file of the font corpus at the nine other 26.6 heights libass requests when hinting is on: unhinted outlines and FT_LOAD_DEFAULT advances."
        }
    }
}

/// The pins of `id`: case id to expected work.
fn pins(root: &Path, id: &str) -> R<Vec<(String, Value)>> {
    let path = pins_path(root, id);
    let document =
        crate::checks::read_json(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    match document.get("pins") {
        Some(Value::Object(pins)) => Ok(pins.clone()),
        _ => Err(format!("{}: no pins", path.display())),
    }
}

/// The generated manifest of feature `id`: its rows with their pinned work. The
/// case ids and the pinned ids must match exactly.
pub fn manifest(root: &Path, id: &str) -> R<Value> {
    let feature = feature(id)?;
    let pins = pins(root, id)?;
    let rows = rows(root, feature)?;
    if rows.len() != pins.len() {
        return Err(format!(
            "{id}: {} cases but {} pins; re-record them",
            rows.len(),
            pins.len()
        ));
    }
    let mut cases = Vec::new();
    for (row, (pinned, expected)) in rows.into_iter().zip(pins) {
        let case = row.get("id").and_then(Value::as_str).unwrap_or_default();
        if case != pinned {
            return Err(format!(
                "{id}: case {case} is pinned as {pinned}; re-record them"
            ));
        }
        let Value::Object(mut fields) = row else {
            unreachable!("rows are objects")
        };
        fields.push(("expected".into(), expected));
        cases.push(Value::Object(fields));
    }
    Ok(Value::object([
        ("schema", Value::int(1)),
        ("note", Value::str(note(feature))),
        ("directory", Value::str(DIRECTORY)),
        ("cases", Value::Array(cases)),
    ]))
}

/// The pin file of `id` holding `pins` in case order.
fn pin_file(pins: Vec<(String, Value)>) -> Value {
    Value::object([
        ("schema", Value::int(1)),
        ("reference", Value::str("freetype-native")),
        (
            "note",
            Value::str(
                "FreeType 2.15.0's folds, recorded by `cargo xtask fonts manifest --record`; never edited by hand.",
            ),
        ),
        ("pins", Value::Object(pins)),
    ])
}

/// Every registered font feature's generated manifest builds and its pin file is
/// exactly what [`record`] writes for its cases.
pub fn check(root: &Path) -> Vec<String> {
    let mut errors = Vec::new();
    for feature in FEATURES {
        let registered = root.join("harness/features").join(feature.id);
        if !registered.join("contract.json").is_file() {
            continue;
        }
        if let Err(error) = manifest(root, feature.id) {
            errors.push(error);
            continue;
        }
        let path = pins_path(root, feature.id);
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        let rewritten = pins(root, feature.id).map(|pins| canonical(&pin_file(pins)));
        match rewritten {
            Ok(text) if text == committed => {}
            _ => errors.push(format!(
                "{}: not in the form `cargo xtask fonts manifest --record` writes",
                path.display()
            )),
        }
    }
    errors
}

/// Re-records the pins of every feature from the built FreeType reference.
pub fn record(root: &Path) -> R<()> {
    let identity = crate::checks::read_json(&root.join("target/harness/identity.json"))?;
    let reference = crate::harness::comparison::reference_named(&identity, "freetype-native")?;
    let worker = root.join(
        reference
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let built = crate::sha256::hex(
        &std::fs::read(&worker).map_err(|e| format!("{}: {e}", worker.display()))?,
    );
    if reference.get("sha256").and_then(Value::as_str) != Some(built.as_str()) {
        return Err("the FreeType reference changed since build-workers; rebuild".into());
    }
    for feature in FEATURES {
        let contract = root
            .join("harness/features")
            .join(feature.id)
            .join("contract.json");
        let contract = crate::checks::read_json(&contract)?;
        let prefix = contract
            .get("worker_operation")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{}: no worker_operation", feature.id))?;
        let rows = rows(root, feature)?;
        let folds = fold_all(root, &worker, &format!("{prefix}fold"), &rows)?;
        let pins = rows
            .iter()
            .zip(folds)
            .map(|(row, fold)| {
                let id = row.get("id").and_then(Value::as_str).unwrap_or_default();
                (id.to_owned(), fold)
            })
            .collect();
        crate::corpus::write_json(&pins_path(root, feature.id), &pin_file(pins))?;
        println!("{}: {} pins recorded", feature.id, rows.len());
    }
    Ok(())
}

/// The reference's `{packets, bytes, digest}` for every row, run in parallel.
fn fold_all(root: &Path, worker: &Path, mode: &str, rows: &[Value]) -> R<Vec<Value>> {
    let results: Mutex<Vec<Option<R<Value>>>> = Mutex::new(vec![None; rows.len()]);
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let at = next.fetch_add(1, Ordering::Relaxed);
                    let Some(row) = rows.get(at) else { break };
                    let result = fold(root, worker, mode, row);
                    results.lock().expect("results")[at] = Some(result);
                }
            });
        }
    });
    results
        .into_inner()
        .expect("results")
        .into_iter()
        .map(|r| r.unwrap_or_else(|| Err("not run".into())))
        .collect()
}

fn fold(root: &Path, worker: &Path, mode: &str, row: &Value) -> R<Value> {
    let file = row.get("file").and_then(Value::as_str).unwrap_or_default();
    let arguments = row
        .get("arguments")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str);
    let output = Command::new(worker)
        .arg(mode)
        .arg(root.join(DIRECTORY).join(file))
        .args(arguments)
        .output()
        .map_err(|e| format!("{}: {e}", worker.display()))?;
    let id = row.get("id").and_then(Value::as_str).unwrap_or_default();
    if !output.status.success() {
        return Err(format!("{id}: the reference failed: {}", output.status));
    }
    let data = crate::json::parse(&String::from_utf8_lossy(&output.stdout))
        .map_err(|e| format!("{id}: {e}"))?;
    Ok(Value::object(["packets", "bytes", "digest"].map(|key| {
        (key, data.get(key).cloned().unwrap_or(Value::Null))
    })))
}

/// Writes every generated manifest to `directory/<id>/corpus.json` for reading.
pub fn write(root: &Path, directory: &Path) -> R<()> {
    for feature in FEATURES {
        let path = directory.join(feature.id).join("corpus.json");
        let manifest = manifest(root, feature.id)?;
        let cases = manifest
            .get("cases")
            .and_then(Value::as_array)
            .map_or(0, <[_]>::len);
        crate::corpus::write_json(&path, &manifest)?;
        println!("{}: {cases} cases", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_font_feature_generates_its_pinned_cases() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        assert!(check(root).is_empty(), "{:?}", check(root));
        let count = |id| {
            manifest(root, id)
                .unwrap()
                .get("cases")
                .and_then(Value::as_array)
                .unwrap()
                .len()
        };
        assert_eq!(
            FEATURES.map(|f| (f.id, count(f.id))),
            [
                ("font-outlines", 51),
                ("font-hinting-none", 102),
                ("font-hinting-modes", 918)
            ]
        );
        let none = manifest(root, "font-hinting-none").unwrap();
        let first = &none.get("cases").and_then(Value::as_array).unwrap()[1];
        assert_eq!(
            first.get("id").and_then(Value::as_str),
            Some("arimo-bold-ttf-advances-16384")
        );
        assert_eq!(
            first.get("arguments"),
            Some(&Value::strings(["advances".to_owned(), "16384".to_owned()]))
        );
    }

    #[test]
    fn edited_or_mismatched_pins_fail_the_check() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let root = crate::walk::scratch("font-pins");
        let feature = root.join("harness/features/font-outlines");
        std::fs::create_dir_all(&feature).unwrap();
        std::fs::create_dir_all(root.join("harness/fonts")).unwrap();
        std::fs::copy(source.join(CORPUS), root.join(CORPUS)).unwrap();
        let copy = |file: &str| {
            std::fs::copy(
                source.join("harness/features/font-outlines").join(file),
                feature.join(file),
            )
            .unwrap();
        };
        copy("contract.json");
        copy("pins.json");
        assert!(check(&root).is_empty());
        let pins = std::fs::read_to_string(feature.join("pins.json")).unwrap();
        std::fs::write(feature.join("pins.json"), pins.replace("\n  ", "\n ")).unwrap();
        assert!(
            check(&root)[0].contains("not in the form"),
            "hand formatting"
        );
        let renamed = pins.replacen("arimo-bold-ttf", "arimo-bold", 1);
        std::fs::write(feature.join("pins.json"), renamed).unwrap();
        assert!(check(&root)[0].contains("pinned as"), "a case id differs");
        std::fs::remove_dir_all(root).unwrap();
    }
}
