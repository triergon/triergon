//! The shaping features' manifests, generated. Text cases are a font of the font
//! corpus (`harness/fonts/corpus.json`) with the texts of one script and one set of
//! segment properties and features; bidi, simple and Unicode cases are generated
//! text files (`.research/shape/texts`) with the settings of each feature. The
//! work each case must fold is pinned in `harness/features/<id>/pins.json`: the
//! HarfBuzz and FriBidi reference's folds (see [`crate::lanes`]).

use std::path::Path;

use super::texts::{self, ALPHABETS, Rng, SAMPLES};
use crate::json::Value;
use crate::lanes::Lane;

type R<T> = Result<T, String>;

/// Where the text cases' fonts and the generated text files are.
const FONT_DIRECTORY: &str = crate::fonts::CORPUS_FILES;
pub const TEXT_DIRECTORY: &str = ".research/shape/texts";

/// The files case rows are generated from, besides the pins.
pub const ROW_SOURCES: [&str; 3] = [
    "harness/fonts/corpus.json",
    "xtask/src/shape/manifest.rs",
    "xtask/src/shape/texts.rs",
];

/// The cases `cargo xtask check` mutates, by feature: fonts with layout tables of
/// every kind and with none, and the random text files.
pub const SMOKE_CASES: [(&str, &[&str]); 4] = [
    (
        "shape-text",
        &[
            "arimo-regular-ttf-latn-libass",
            "notosansarabic-regular-ttf-arab-libass",
            "notosansdevanagari-regular-ttf-deva-libass",
            "authored-cmap-formats-ttf-latn-libass",
        ],
    ),
    (
        "shape-libass",
        &[
            "arimo-regular-ttf-latn-libass",
            "notosansarabic-regular-ttf-arab-libass",
            "notosans-regular-otf-latn-libass",
        ],
    ),
    ("shape-bidi", &["bidi-random-lines-on-brackets"]),
    ("shape-simple", &["simple-random-rtl"]),
];

/// The features libass enables, in its order (`init_features` and
/// `set_run_features` in ass_shaper.c), for horizontal text.
const LIBASS: &str = "vert=0,vkna=0,kern=1,liga=1,clig=1";

/// One way of shaping a script's texts with a face.
#[derive(Clone, Copy)]
enum Variant {
    /// libass's run: the script's direction and language and libass's features,
    /// with items inside longer texts as well.
    Libass,
    /// `\kern0`.
    NoKerning,
    /// Non-standard letter spacing (`\fsp`).
    NoLigatures,
    /// A vertical (`@`) font.
    Vertical,
    /// The direction opposite to the script's.
    Opposite,
    /// HarfBuzz's default language in the C locale.
    CLanguage,
    /// No features at all.
    Plain,
    /// A track language overriding the script's.
    Language(&'static str),
}

impl Variant {
    fn name(self) -> String {
        match self {
            Self::Libass => "libass".into(),
            Self::NoKerning => "no-kerning".into(),
            Self::NoLigatures => "no-ligatures".into(),
            Self::Vertical => "vertical".into(),
            Self::Opposite => "opposite".into(),
            Self::CLanguage => "c".into(),
            Self::Plain => "plain".into(),
            Self::Language(language) => format!("language-{language}"),
        }
    }

    /// Direction, language and features.
    fn settings(self, script: &str) -> [&'static str; 3] {
        let (direction, language) = (texts::direction(script), texts::language(script));
        match self {
            Self::Libass => [direction, language, LIBASS],
            Self::NoKerning => [direction, language, "vert=0,vkna=0,kern=0,liga=1,clig=1"],
            Self::NoLigatures => [direction, language, "vert=0,vkna=0,kern=1,liga=0,clig=0"],
            Self::Vertical => [direction, language, "vert=1,vkna=1,kern=1,liga=1,clig=1"],
            Self::Opposite => [texts::opposite(direction), language, LIBASS],
            Self::CLanguage => [direction, "c", LIBASS],
            Self::Plain => [direction, language, "-"],
            Self::Language(language) => [direction, language, LIBASS],
        }
    }
}

/// The scripts each corpus font covers, which get every variant; other fonts and
/// scripts get libass's run only.
const COVERAGE: &[(&str, &[&str])] = &[
    (
        "Arimo-Regular.ttf",
        &["Latn", "Grek", "Cyrl", "Hebr", "Zyyy"],
    ),
    ("Arimo-BoldItalic.ttf", &["Latn", "Grek", "Cyrl"]),
    ("Tinos-Regular.ttf", &["Latn", "Grek", "Cyrl", "Hebr"]),
    ("Cousine-Regular.ttf", &["Latn", "Grek", "Cyrl", "Hebr"]),
    (
        "DejaVuSans.ttf",
        &["Latn", "Grek", "Cyrl", "Hebr", "Arab", "Zyyy"],
    ),
    (
        "DejaVuSans-Bold.ttf",
        &["Latn", "Grek", "Cyrl", "Hebr", "Arab"],
    ),
    (
        "DejaVuSansMono.ttf",
        &["Latn", "Grek", "Cyrl", "Hebr", "Arab"],
    ),
    ("DejaVuSerif.ttf", &["Latn", "Grek", "Cyrl"]),
    ("NotoSans-Regular.ttf", &["Latn", "Grek", "Cyrl", "Zyyy"]),
    ("NotoSans-Regular.otf", &["Latn", "Grek", "Cyrl"]),
    ("NotoSans-BoldItalic.otf", &["Latn", "Grek", "Cyrl"]),
    ("NotoSansJP-Regular.otf", &["Hani", "Hira", "Kana"]),
    ("NotoSansJP-VF.otf", &["Hani", "Hira", "Kana"]),
    ("NotoSansCJK-Regular.ttc", &["Hani", "Hira", "Kana", "Hang"]),
    ("NotoSansArabic-Regular.ttf", &["Arab"]),
    ("NotoNaskhArabic-Regular.ttf", &["Arab"]),
    ("NotoNastaliqUrdu-Regular.ttf", &["Arab"]),
    ("NotoSansHebrew-Regular.ttf", &["Hebr"]),
    ("NotoSansDevanagari-Regular.ttf", &["Deva"]),
    ("NotoSansBengali-Regular.ttf", &["Beng"]),
    ("NotoSansTamil-Regular.ttf", &["Taml"]),
    ("NotoSansGujarati-Regular.ttf", &["Gujr"]),
    ("NotoSansGurmukhi-Regular.ttf", &["Guru"]),
    ("NotoSansKannada-Regular.ttf", &["Knda"]),
    ("NotoSansMalayalam-Regular.ttf", &["Mlym"]),
    ("NotoSansTelugu-Regular.ttf", &["Telu"]),
    ("NotoSansSinhala-Regular.ttf", &["Sinh"]),
    ("NotoSansThai-Regular.ttf", &["Thai"]),
    ("NotoSansKhmer-Regular.ttf", &["Khmr"]),
    ("NotoSansMyanmar-Regular.ttf", &["Mymr"]),
    ("NotoEmoji-wght.ttf", &["Zyyy"]),
];

/// The required text cases: every shaper and script group with a font that
/// covers it, each outline format, and the paths for fonts without layout
/// tables (`kern` only, no GDEF, no GPOS, no Arabic forms).
const REQUIRED: &[(&str, &[&str])] = &[
    (
        "Arimo-Regular.ttf",
        &["Latn", "Grek", "Cyrl", "Hebr", "Arab", "Deva"],
    ),
    ("NotoSans-Regular.otf", &["Latn", "Grek", "Cyrl"]),
    ("DejaVuSans.ttf", &["Arab", "Hebr"]),
    ("DejaVuMathTeXGyre.ttf", &["Latn"]),
    ("NotoSansJP-Regular.otf", &["Hani", "Hira", "Kana"]),
    ("NotoSansJP-VF.otf", &["Hira"]),
    ("NotoSansCJK-Regular.ttc", &["Hang"]),
    ("NotoSansArabic-Regular.ttf", &["Arab"]),
    ("NotoNastaliqUrdu-Regular.ttf", &["Arab"]),
    ("NotoSansHebrew-Regular.ttf", &["Hebr"]),
    ("NotoSansDevanagari-Regular.ttf", &["Deva"]),
    ("NotoSansBengali-Regular.ttf", &["Beng"]),
    ("NotoSansTamil-Regular.ttf", &["Taml"]),
    ("NotoSansGujarati-Regular.ttf", &["Gujr"]),
    ("NotoSansGurmukhi-Regular.ttf", &["Guru"]),
    ("NotoSansKannada-Regular.ttf", &["Knda"]),
    ("NotoSansMalayalam-Regular.ttf", &["Mlym"]),
    ("NotoSansTelugu-Regular.ttf", &["Telu"]),
    ("NotoSansSinhala-Regular.ttf", &["Sinh"]),
    ("NotoSansThai-Regular.ttf", &["Thai"]),
    ("NotoSansKhmer-Regular.ttf", &["Khmr"]),
    ("NotoSansMyanmar-Regular.ttf", &["Mymr"]),
    ("NotoEmoji-wght.ttf", &["Zyyy"]),
    ("authored-cmap-formats.ttf", &["Latn"]),
    ("authored-cff-features.otf", &["Latn"]),
];

/// Random strings per script: in the required cases, and in the whole sweep.
const RANDOM_REQUIRED: usize = 8;
const RANDOM_ALL: usize = 48;
const RANDOM_BIDI: usize = 2000;
const RANDOM_SIMPLE: usize = 1000;

/// libass's font size with hinting off: 256 pixels in 26.6.
const LIBASS_SIZE: u32 = 256 << 6;

/// What a feature runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Load {
    /// Text cases: the required fonts and scripts, or every font with every script.
    Text {
        all: bool,
    },
    /// The required text cases through libass's font functions at a 26.6 pixel
    /// size.
    Libass {
        size: u32,
    },
    Bidi,
    Simple,
    Unicode,
}

pub const FEATURES: [&str; 6] = [
    "shape-text",
    "shape-libass",
    "shape-text-all",
    "shape-bidi",
    "shape-simple",
    "shape-unicode",
];

pub const LANE: Lane = Lane {
    command: "shape",
    features: &FEATURES,
    reference: "harfbuzz-native",
    pin_note: "HarfBuzz 14.6.0's and FriBidi 1.0.17's folds, recorded by `cargo xtask shape manifest --record`; never edited by hand.",
    report_directory: ".research/shape-report",
    manifest_directory: ".research/shape/manifests",
    row_sources: &ROW_SOURCES,
    rows,
    note,
    directory,
    write_inputs,
    check_inputs,
    smoke: &SMOKE_CASES,
};

fn load(id: &str) -> R<Load> {
    match id {
        "shape-text" => Ok(Load::Text { all: false }),
        "shape-libass" => Ok(Load::Libass { size: LIBASS_SIZE }),
        "shape-text-all" => Ok(Load::Text { all: true }),
        "shape-bidi" => Ok(Load::Bidi),
        "shape-simple" => Ok(Load::Simple),
        "shape-unicode" => Ok(Load::Unicode),
        _ => Err(format!("{id}: not a shaping feature")),
    }
}

/// Upper-case hexadecimal code points separated by single spaces.
fn hex(text: &[char]) -> String {
    text.iter()
        .map(|&c| format!("{:04X}", u32::from(c)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The text arguments of one script and variant: the script's authored texts (or
/// those of one track language), its random strings and, for libass's run, items
/// inside the longer texts.
fn text_arguments(script: &str, variant: Variant, random: usize) -> Vec<String> {
    let samples = SAMPLES.iter().filter(|s| s.script == script);
    let mut arguments = Vec::new();
    if let Variant::Language(language) = variant {
        for sample in samples.filter(|s| s.language == Some(language)) {
            arguments.push(hex(&sample.text.chars().collect::<Vec<_>>()));
        }
        return arguments;
    }
    let authored: Vec<Vec<char>> = samples.map(|s| s.text.chars().collect()).collect();
    arguments.extend(authored.iter().map(|t| hex(t)));
    if let Some(alphabet) = ALPHABETS.iter().find(|a| a.script == script) {
        // Each script's strings come from its own seeded generator, so the
        // required strings are the first of the whole sweep's.
        let seed = script
            .bytes()
            .fold(0x5348_4150_4531_u64, |s, b| s.rotate_left(8) ^ u64::from(b));
        let mut rng = Rng::new(seed);
        for _ in 0..random {
            let length = 1 + rng.below(32);
            arguments.push(hex(&texts::random_text(alphabet, length, &mut rng)));
        }
    }
    if matches!(variant, Variant::Libass) {
        for text in authored.iter().filter(|t| t.len() >= 6) {
            let (len, third) = (text.len(), text.len() / 3);
            for (start, length) in [(third, third), (0, len / 2), (len / 2, len - len / 2)] {
                arguments.push(format!("{start},{length}:{}", hex(text)));
            }
        }
    }
    arguments
}

/// The variants of a script on a face that covers it.
fn variants(script: &str) -> Vec<Variant> {
    let mut variants = vec![
        Variant::Libass,
        Variant::NoKerning,
        Variant::NoLigatures,
        Variant::Opposite,
        Variant::CLanguage,
        Variant::Plain,
    ];
    if matches!(script, "Hani" | "Hira" | "Kana" | "Hang") {
        variants.push(Variant::Vertical);
    }
    let mut languages: Vec<&'static str> = SAMPLES
        .iter()
        .filter(|s| s.script == script)
        .filter_map(|s| s.language)
        .collect();
    languages.sort_unstable();
    languages.dedup();
    variants.extend(languages.into_iter().map(Variant::Language));
    variants
}

/// Every script the authored texts are written in, in first-use order.
fn scripts() -> Vec<&'static str> {
    let mut scripts: Vec<&'static str> = Vec::new();
    for sample in SAMPLES {
        if !scripts.contains(&sample.script) {
            scripts.push(sample.script);
        }
    }
    scripts
}

fn covers(name: &str, script: &str) -> bool {
    COVERAGE
        .iter()
        .any(|(file, scripts)| *file == name && scripts.contains(&script))
}

/// The text case rows of a feature, in corpus order; with a `size`, each row's
/// arguments start with it.
fn text_rows(root: &Path, all: bool, size: Option<u32>) -> R<Vec<Value>> {
    let mut rows = Vec::new();
    for (source, files) in crate::fonts::corpus(root)? {
        if !crate::fonts::serves(&source, "shape") {
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
            let stem = name.replace('.', "-").to_lowercase();
            let wanted: Vec<&'static str> = if all {
                scripts()
            } else {
                REQUIRED
                    .iter()
                    .filter(|(f, _)| *f == name)
                    .flat_map(|(_, scripts)| scripts.iter().copied())
                    .collect()
            };
            for script in wanted {
                let runs = if covers(name, script) {
                    variants(script)
                } else {
                    vec![Variant::Libass]
                };
                let random = if all { RANDOM_ALL } else { RANDOM_REQUIRED };
                for variant in runs {
                    let mut arguments: Vec<String> = size.iter().map(ToString::to_string).collect();
                    arguments.extend([variant.settings(script)[0].to_owned(), script.to_owned()]);
                    arguments.extend(
                        variant.settings(script)[1..]
                            .iter()
                            .map(|s| (*s).to_owned()),
                    );
                    arguments.extend(text_arguments(script, variant, random));
                    let id = format!("{stem}-{}-{}", script.to_lowercase(), variant.name());
                    let mut fields = vec![
                        ("id", Value::str(id)),
                        ("profile", Value::str(script.to_lowercase())),
                        ("file", Value::str(&file.path)),
                        ("sha256", Value::str(&file.sha256)),
                    ];
                    fields.extend(rights.iter().cloned());
                    fields.push(("arguments", Value::strings(arguments)));
                    rows.push(Value::object(fields));
                }
            }
        }
    }
    Ok(rows)
}

/// The generated text files: name and contents.
pub fn inputs() -> Vec<(&'static str, String)> {
    let line = |text: &[char]| hex(text) + "\n";
    let mut authored: Vec<Vec<char>> = SAMPLES.iter().map(|s| s.text.chars().collect()).collect();
    authored.extend(texts::nesting_texts().into_iter().map(|(_, text)| text));
    let mut rng = Rng::new(0x4249_4449);
    let random: Vec<Vec<char>> = (0..RANDOM_BIDI)
        .map(|_| {
            let length = 1 + rng.below(48);
            texts::random_bidi_text(length, &mut rng)
        })
        .collect();
    // Up to three line starts in 1..len, from their own generator.
    let mut breaks = Rng::new(0x4C49_4E45);
    let mut with_lines = |texts: &[Vec<char>]| -> String {
        texts
            .iter()
            .map(|text| {
                if text.len() < 2 {
                    return line(text);
                }
                let mut at: Vec<usize> = (0..1 + breaks.below(3))
                    .map(|_| 1 + breaks.below(text.len() - 1))
                    .collect();
                at.sort_unstable();
                at.dedup();
                let at: Vec<String> = at.iter().map(ToString::to_string).collect();
                format!("{}|{}\n", hex(text), at.join(","))
            })
            .collect()
    };
    let authored_lines = with_lines(&authored);
    let random_lines = with_lines(&random);
    let all = |texts: &[Vec<char>]| texts.iter().map(|t| line(t)).collect::<String>();
    let mut arabic: Vec<Vec<char>> = SAMPLES
        .iter()
        .filter(|s| s.script == "Arab")
        .map(|s| s.text.chars().collect())
        .collect();
    let mut rng = Rng::new(0x5349_4D50);
    arabic.extend((0..RANDOM_SIMPLE).map(|_| {
        let length = 1 + rng.below(24);
        texts::random_arabic_text(length, &mut rng)
    }));
    let arabic_authored = SAMPLES.iter().filter(|s| s.script == "Arab").count();
    vec![
        ("bidi-authored.txt", all(&authored)),
        ("bidi-authored-lines.txt", authored_lines),
        ("bidi-random.txt", all(&random)),
        ("bidi-random-lines.txt", random_lines),
        ("simple-authored.txt", all(&arabic[..arabic_authored])),
        ("simple-random.txt", all(&arabic[arabic_authored..])),
        // Every scalar value; surrogates have no properties of their own.
        ("unicode-ranges.txt", "0 D7FF\nE000 10FFFF\n".into()),
    ]
}

/// Writes the generated text files that differ from what is on disk.
pub fn write_inputs(root: &Path) -> R<()> {
    let directory = root.join(TEXT_DIRECTORY);
    std::fs::create_dir_all(&directory).map_err(|e| format!("{}: {e}", directory.display()))?;
    for (name, contents) in inputs() {
        let path = directory.join(name);
        if std::fs::read(&path).ok().as_deref() != Some(contents.as_bytes()) {
            crate::fetch::publish(&path, contents.as_bytes())?;
        }
    }
    Ok(())
}

/// The rows of a bidi, simple or Unicode feature: each generated file with each
/// setting.
fn text_file_rows(load: Load) -> Vec<Value> {
    let inputs = inputs();
    let (prefix, settings): (&str, Vec<(String, Vec<&str>)>) = match load {
        Load::Bidi => (
            "bidi",
            ["ltr", "rtl", "on"]
                .iter()
                .flat_map(|base| {
                    [("", "0"), ("-brackets", "1")]
                        .map(|(name, flag)| (format!("{base}{name}"), vec![*base, flag]))
                })
                .collect(),
        ),
        Load::Simple => (
            "simple",
            ["ltr", "rtl", "on"]
                .iter()
                .map(|base| ((*base).to_owned(), vec![*base]))
                .collect(),
        ),
        _ => ("unicode", vec![(String::new(), Vec::new())]),
    };
    let mut rows = Vec::new();
    for (name, contents) in inputs.iter().filter(|(n, _)| n.starts_with(prefix)) {
        let stem = name.trim_end_matches(".txt");
        for (setting, arguments) in &settings {
            let id = if setting.is_empty() {
                stem.to_owned()
            } else {
                format!("{stem}-{setting}")
            };
            let profile = if stem.contains("random") {
                "random"
            } else {
                "authored"
            };
            rows.push(Value::object([
                ("id", Value::str(id)),
                ("profile", Value::str(profile)),
                ("file", Value::str(*name)),
                (
                    "sha256",
                    Value::str(crate::sha256::hex(contents.as_bytes())),
                ),
                ("rights", Value::str("authored-synthetic")),
                (
                    "arguments",
                    Value::strings(arguments.iter().map(|a| (*a).to_owned())),
                ),
            ]));
        }
    }
    rows
}

fn rows(root: &Path, id: &str) -> R<Vec<Value>> {
    match load(id)? {
        Load::Text { all } => text_rows(root, all, None),
        Load::Libass { size } => text_rows(root, false, Some(size)),
        load => Ok(text_file_rows(load)),
    }
}

fn note(id: &str) -> &'static str {
    match id {
        "shape-text" => {
            "A font of every outline format and of every script group the shapers cover, and fonts without layout tables, each with the authored texts and eight seeded random strings of a script under libass's run and the variants a covering face gets."
        }
        "shape-libass" => {
            "The cases of shape-text through libass's font functions at 256 pixels with hinting off: its charmap, and FreeType's default-load advances, extents and contour points and its kerning at that size and ppem."
        }
        "shape-text-all" => {
            "Every font of the corpus with the texts of every script: libass's run for all, every variant for the scripts a face covers, 48 seeded random strings per script."
        }
        "shape-bidi" => {
            "The authored texts, nesting past the depth limit and 2000 random strings over every bidi class, on one line and with line breaks, with each base direction, with and without bracket pairs."
        }
        "shape-simple" => {
            "The Arabic texts and 1000 random Arabic strings with each base direction."
        }
        _ => "Every Unicode scalar value.",
    }
}

fn directory(id: &str) -> &'static str {
    if matches!(load(id), Ok(Load::Text { .. } | Load::Libass { .. })) {
        FONT_DIRECTORY
    } else {
        TEXT_DIRECTORY
    }
}

/// The authored texts' labels are unique.
fn check_inputs(_root: &Path) -> Vec<String> {
    texts::duplicate_label()
        .map(|label| format!("xtask/src/shape/texts.rs: two samples are labelled {label}"))
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_required_cases_cover_every_shaper_and_bidi_class() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let rows = text_rows(root, false, None).unwrap();
        let scripts: Vec<&str> = rows
            .iter()
            .filter_map(|r| r.get("profile").and_then(Value::as_str))
            .collect();
        for script in [
            "latn", "grek", "cyrl", "hebr", "arab", "deva", "beng", "taml", "gujr", "guru", "knda",
            "mlym", "telu", "sinh", "thai", "khmr", "mymr", "hani", "hira", "kana", "hang", "zyyy",
        ] {
            assert!(scripts.contains(&script), "{script}");
        }
        let inputs = inputs();
        let random = &inputs
            .iter()
            .find(|(n, _)| *n == "bidi-random.txt")
            .unwrap()
            .1;
        // Every class's characters occur in the random bidi texts.
        for code in [
            "0627", "05D0", "202E", "2068", "0300", "2029", "0009", "FF09",
        ] {
            assert!(random.contains(code), "{code}");
        }
        // libass's cases are the text cases with the size first.
        let libass = text_rows(root, false, Some(LIBASS_SIZE)).unwrap();
        assert_eq!(libass.len(), rows.len());
        let arguments = |row: &Value| -> Vec<String> {
            let list = row.get("arguments").and_then(Value::as_array).unwrap();
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        };
        for (sized, text) in libass.iter().zip(&rows) {
            assert_eq!(sized.get("id"), text.get("id"));
            assert_eq!(arguments(sized)[0], "16384");
            assert_eq!(arguments(sized)[1..], arguments(text)[..]);
        }
        assert_eq!(text_file_rows(Load::Bidi).len(), 4 * 6);
        assert_eq!(text_file_rows(Load::Simple).len(), 2 * 3);
        assert_eq!(text_file_rows(Load::Unicode).len(), 1);
    }

    #[test]
    fn generated_inputs_are_deterministic() {
        assert_eq!(inputs(), inputs());
        let rows = text_file_rows(Load::Bidi);
        assert_eq!(
            rows[0].get("id").and_then(Value::as_str),
            Some("bidi-authored-ltr")
        );
    }
}
