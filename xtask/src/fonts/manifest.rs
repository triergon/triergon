//! The font features' corpus manifests, generated. Every file of the font corpus
//! (`harness/fonts/corpus.json`) becomes cases of each feature's load settings; the
//! work each case must fold is pinned in `harness/features/<id>/pins.json`, the
//! FreeType reference's folds (see [`crate::generators`]).

use std::path::Path;

use super::{CORPUS, corpus};
use crate::generators::{self, Generator};
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

pub const GENERATOR: Generator = Generator {
    command: "fonts",
    main: super::main,
    candidate: "font-worker",
    pin_note: (
        "freetype-native",
        "FreeType 2.15.0's folds, recorded by `cargo xtask fonts manifest --record`; never edited by hand.",
    ),
    report_directory: ".research/font-report",
    manifest_directory: ".research/fonts/manifests",
    row_sources: &ROW_SOURCES,
    rows,
    note,
    directory: |_| DIRECTORY,
    write_inputs: generators::no_inputs,
    check_inputs: generators::no_checks,
    smoke: &[
        ("font-outlines", &SMOKE_CASES),
        ("font-hinting-none", &SMOKE_CASES),
    ],
};

fn load(id: &str) -> R<Load> {
    match id {
        "font-outlines" => Ok(Load::Faces),
        "font-hinting-none" => Ok(Load::Sized(&[UNHINTED_HEIGHT])),
        "font-hinting-modes" => Ok(Load::Sized(&HINTING_HEIGHTS)),
        _ => Err(format!("{id}: not a font feature")),
    }
}

/// The case rows of a feature, without expected work, in corpus order.
fn rows(root: &Path, id: &str) -> R<Vec<Value>> {
    let load = load(id)?;
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
            match load {
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

fn note(id: &str) -> &'static str {
    match id {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_font_feature_generates_its_pinned_cases() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        assert!(
            GENERATOR.check(root).is_empty(),
            "{:?}",
            GENERATOR.check(root)
        );
        let count = |id| {
            GENERATOR
                .manifest(root, id)
                .unwrap()
                .get("cases")
                .and_then(Value::as_array)
                .unwrap()
                .len()
        };
        assert_eq!(
            ["font-outlines", "font-hinting-none", "font-hinting-modes"].map(|f| (f, count(f))),
            [
                ("font-outlines", 51),
                ("font-hinting-none", 102),
                ("font-hinting-modes", 918)
            ]
        );
        let none = GENERATOR.manifest(root, "font-hinting-none").unwrap();
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
        assert!(GENERATOR.check(&root).is_empty());
        let pins = std::fs::read_to_string(feature.join("pins.json")).unwrap();
        let other_reference = pins.replacen("freetype-native", "other-reference", 1);
        std::fs::write(feature.join("pins.json"), other_reference).unwrap();
        assert!(GENERATOR.check(&root)[0].contains("pin reference differs"));
        std::fs::write(feature.join("pins.json"), pins.replace("\n  ", "\n ")).unwrap();
        assert!(
            GENERATOR.check(&root)[0].contains("not in the form"),
            "hand formatting"
        );
        let renamed = pins.replacen("arimo-bold-ttf", "arimo-bold", 1);
        std::fs::write(feature.join("pins.json"), renamed).unwrap();
        assert!(
            GENERATOR.check(&root)[0].contains("pinned as"),
            "a case id differs"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
