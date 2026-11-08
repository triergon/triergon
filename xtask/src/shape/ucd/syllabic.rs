//! The character categories of HarfBuzz 14.6.0's syllabic shapers, by the rules of
//! `gen-indic-table.py` (Indic, Khmer and Myanmar), `gen-use-table.py` (the
//! Universal Shaping Engine) and `gen-vowel-constraints.py`, read as evidence of
//! what they produce, and the scripts of the shapers: the ones whose letters
//! join, by the rule of `gen-arabic-joining-list.py`, and the ones HarfBuzz gives
//! the Universal Shaping Engine, which it lists by hand in `hb-ot-shaper.hh`.
//!
//! As in HarfBuzz, the USE table takes Default_Ignorable_Code_Point from Unicode
//! 17.0.0 and everything else from 18.0.0, with Microsoft's override files from
//! HarfBuzz's `src/ms-use/`.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use super::emit::{Writer, hex};
use super::pack::pack;
use super::source;
use super::{FILE_LIMIT, Generated, Loaded};
use crate::{fetch, sha256};

use crate::shape::reference::{CACHE, HARFBUZZ};

/// The files of HarfBuzz 14.6.0's tree the tables are read from, as it ships
/// them: Microsoft's override and constraint files, the shaper choice and the
/// script tags.
const TREE_FILES: [(&str, &str); 5] = [
    (
        "src/ms-use/IndicSyllabicCategory-Additional.txt",
        "b9472e3e72d5fba8cb3f2e0578e73012aaae786db25779de0f1a5a5ab69b84a6",
    ),
    (
        "src/ms-use/IndicPositionalCategory-Additional.txt",
        "2baa1c1efe5a5f108c304b1e27d0d97864c806764eb2b0a1bd91db80ae26b5b8",
    ),
    (
        "src/ms-use/IndicShapingInvalidCluster.txt",
        "02024d4289864665721e14ec99eb320ce187f289514b793449b4f6a8ddaf5944",
    ),
    (
        "src/hb-ot-shaper.hh",
        "3eb84b0a89a8c4dbee0930e66bd071bb374d76e9f2748ac441d84e79dc34cce3",
    ),
    (
        "src/hb-script-list.h",
        "898de25adaf9253471d9f1351a6c90e3547e1470dd2f468ab19a86561868182b",
    ),
];

/// The files of [`TREE_FILES`], verified against their pinned hashes.
pub struct TreeFiles {
    syllabic: String,
    positional: String,
    invalid_clusters: String,
    shaper: String,
    scripts: String,
}

impl TreeFiles {
    pub fn load(root: &Path) -> Result<Self, String> {
        let mut texts = Vec::new();
        for (path, hash) in TREE_FILES {
            let path = root.join(CACHE).join(HARFBUZZ.directory).join(path);
            if !path.exists() {
                return Err(format!(
                    "{}: missing; `cargo xtask shape prepare` unpacks the HarfBuzz release",
                    path.display()
                ));
            }
            let data = fetch::read_bounded(&path, FILE_LIMIT)?;
            if sha256::hex(&data) != hash {
                return Err(format!("{}: differs from HarfBuzz 14.6.0", path.display()));
            }
            texts.push(
                String::from_utf8(data).map_err(|_| format!("{}: not UTF-8", path.display()))?,
            );
        }
        let [syllabic, positional, invalid_clusters, shaper, scripts] =
            <[String; 5]>::try_from(texts).map_err(|_| "five HarfBuzz tree files")?;
        Ok(Self {
            syllabic,
            positional,
            invalid_clusters,
            shaper,
            scripts,
        })
    }
}

/// The output's header: the generator, then every source with its hash.
fn header(ucd: &Loaded, derived: &Loaded) -> String {
    let mut header = ucd.header(
        "The character categories of HarfBuzz 14.6.0's Indic, Khmer, Myanmar and \
         Universal Shaping Engine shapers and its vowel constraints",
    );
    let derived = derived.header("Default_Ignorable_Code_Point");
    header.extend(derived.lines().skip(1).map(|line| format!("{line}\n")));
    header.push_str(&format!(
        "// and Microsoft's USE data and the shapers' scripts in {}\n//     sha256 {}:\n",
        HARFBUZZ.url, HARFBUZZ.sha256
    ));
    for (path, hash) in TREE_FILES {
        header.push_str(&format!("//   {path}\n//     sha256 {hash}\n"));
    }
    header
}

pub fn generate(ucd: &Loaded, derived: &Loaded, tree: &TreeFiles) -> Result<Generated, String> {
    let mut w = Writer::new(&header(ucd, derived));
    w.raw(
        "\nuse super::syllabic::{IndicCategory, IndicPosition, UseCategory};\n\
         use super::trie::Trie;\n\
         use crate::Script;\n",
    );
    indic(&mut w, ucd)?;
    universal(&mut w, ucd, derived, tree)?;
    vowel_constraints(&mut w, ucd, tree)?;
    joining_scripts(&mut w, ucd)?;
    use_scripts(&mut w, tree)?;
    Ok(w.finish())
}

/// Every code point a semicolon-separated file lists, with the value of field
/// `field`, in file order; ranges are expanded.
fn listed(text: &str, field: usize) -> Result<Vec<(u32, &str)>, String> {
    let mut values = Vec::new();
    for f in source::fields(text) {
        let value = *f.get(field).ok_or("missing field")?;
        for cp in source::range(f[0])? {
            values.push((cp, value));
        }
    }
    Ok(values)
}

/// The block of every code point Blocks.txt lists.
fn blocks(ucd: &Loaded) -> Result<HashMap<u32, &str>, String> {
    Ok(listed(ucd.text("Blocks.txt"), 1)?.into_iter().collect())
}

// Indic, Khmer and Myanmar.

const ALLOWED_SINGLES: [u32; 2] = [0x00A0, 0x25CC];
const ALLOWED_BLOCKS: [&str; 20] = [
    "Basic Latin",
    "Latin-1 Supplement",
    "Devanagari",
    "Bengali",
    "Gurmukhi",
    "Gujarati",
    "Oriya",
    "Tamil",
    "Telugu",
    "Kannada",
    "Malayalam",
    "Myanmar",
    "Khmer",
    "Vedic Extensions",
    "General Punctuation",
    "Superscripts and Subscripts",
    "Devanagari Extended",
    "Myanmar Extended-B",
    "Myanmar Extended-A",
    "Myanmar Extended-C",
];

/// Indic_Syllabic_Category to the shapers' category.
fn indic_category(name: &str) -> Result<&'static str, String> {
    Ok(match name {
        "Other" | "Consonant_Prefixed" | "Modifying_Letter" | "Tone_Letter" => "X",
        "Avagraha" => "Symbol",
        "Bindu" | "Gemination_Mark" | "Syllable_Modifier" | "Visarga" => "SM",
        "Brahmi_Joining_Number" | "Consonant_Placeholder" | "Number" | "Number_Joiner" => {
            "PLACEHOLDER"
        }
        "Cantillation_Mark" => "A",
        "Consonant"
        | "Consonant_Dead"
        | "Consonant_Head_Letter"
        | "Consonant_Initial_Postfixed" => "C",
        "Consonant_Final"
        | "Consonant_Medial"
        | "Consonant_Subjoined"
        | "Consonant_Succeeding_Repha" => "CM",
        "Consonant_Killer" | "Pure_Killer" | "Vowel_Dependent" => "M",
        "Consonant_Preceding_Repha" => "Repha",
        "Consonant_With_Stacker" => "CS",
        "Invisible_Stacker" | "Virama" => "H",
        "Joiner" => "ZWJ",
        "Non_Joiner" => "ZWNJ",
        "Nukta" | "Tone_Mark" => "N",
        "Register_Shifter" => "RS",
        "Vowel" | "Vowel_Independent" => "V",
        _ => return Err(format!("unknown Indic_Syllabic_Category {name}")),
    })
}

/// Indic_Positional_Category to the shapers' position.
fn indic_position(name: &str) -> Result<&'static str, String> {
    Ok(match name {
        "Not_Applicable" => "END",
        "Left" => "PRE_C",
        "Top" | "Top_And_Left" => "ABOVE_C",
        "Bottom" | "Top_And_Bottom" | "Top_And_Bottom_And_Left" => "BELOW_C",
        "Right"
        | "Bottom_And_Right"
        | "Left_And_Right"
        | "Top_And_Bottom_And_Right"
        | "Top_And_Left_And_Right"
        | "Top_And_Right" => "POST_C",
        "Overstruck" => "AFTER_MAIN",
        "Visual_order_left" => "PRE_M",
        _ => return Err(format!("unknown Indic_Positional_Category {name}")),
    })
}

/// HarfBuzz's category overrides, applied after the mapping.
const INDIC_CATEGORY_OVERRIDES: &[(&[u32], &str)] = &[
    (
        &[
            0xFE00, 0xFE01, 0xFE02, 0xFE03, 0xFE04, 0xFE05, 0xFE06, 0xFE07,
        ],
        "VS",
    ),
    (
        &[
            0xFE08, 0xFE09, 0xFE0A, 0xFE0B, 0xFE0C, 0xFE0D, 0xFE0E, 0xFE0F,
        ],
        "VS",
    ),
    (
        &[0x2015, 0x2022, 0x25FB, 0x25FC, 0x25FD, 0x25FE],
        "PLACEHOLDER",
    ),
    (
        &[
            0x0930, 0x09B0, 0x09F0, 0x0A30, 0x0AB0, 0x0B30, 0x0BB0, 0x0C30, 0x0CB0, 0x0D30,
        ],
        "Ra",
    ),
    (&[0x0953, 0x0954], "SM"),
    (&[0x0A40], "MPst"),
    (&[0x0A72, 0x0A73], "C"),
    (
        &[
            0x1CE2, 0x1CE3, 0x1CE4, 0x1CE5, 0x1CE6, 0x1CE7, 0x1CE8, 0x1CED,
        ],
        "A",
    ),
    (&[0xA8F2, 0xA8F3, 0xA8F4, 0xA8F5, 0xA8F6, 0xA8F7], "Symbol"),
    (
        &[
            0x1CE9, 0x1CEA, 0x1CEB, 0x1CEC, 0x1CEE, 0x1CEF, 0x1CF0, 0x1CF1,
        ],
        "Symbol",
    ),
    (&[0x0A51], "M"),
    (&[0x11301, 0x11302, 0x11303], "SM"),
    (&[0x1133B, 0x1133C, 0x0AFB, 0x0B55], "N"),
    (&[0x09FC, 0x0C80, 0x0D04], "PLACEHOLDER"),
    (&[0x25CC], "DOTTEDCIRCLE"),
    // Khmer.
    (&[0x179A], "Ra"),
    (&[0x17CC, 0x17C9, 0x17CA], "Robatic"),
    (
        &[0x17C6, 0x17CB, 0x17CD, 0x17CE, 0x17CF, 0x17D0, 0x17D1],
        "Xgroup",
    ),
    (&[0x17C7, 0x17C8, 0x17DD, 0x17D3], "Ygroup"),
    (&[0x17D9], "PLACEHOLDER"),
    // Myanmar.
    (&[0x104E], "C"),
    (&[0x1004, 0x101B, 0x105A], "Ra"),
    (&[0x1032, 0x1036], "A"),
    (&[0x103A], "As"),
    (&[0x103E], "MH"),
    (&[0x1060], "ML"),
    (&[0x103C], "MR"),
    (&[0x103D, 0x1082], "MW"),
    (&[0x103B, 0x105E, 0x105F], "MY"),
    (
        &[
            0x1063, 0x1064, 0x1069, 0x106A, 0x106B, 0x106C, 0x106D, 0xAA7B,
        ],
        "PT",
    ),
    (
        &[
            0x1038, 0x1087, 0x1088, 0x1089, 0x108A, 0x108B, 0x108C, 0x108D, 0x108F, 0x109A, 0x109B,
            0x109C,
        ],
        "SM",
    ),
    (&[0x104A], "PLACEHOLDER"),
];

/// HarfBuzz's position overrides, applied last.
const INDIC_POSITION_OVERRIDES: [(u32, &str); 2] = [(0x0A51, "BELOW_C"), (0x0B01, "BEFORE_SUB")];

/// Where a matra goes relative to the consonants, by its position and block.
fn matra_position(cp: u32, position: &str, block: &str) -> Result<&'static str, String> {
    Ok(match (position, block) {
        ("PRE_C", _) => "PRE_M",
        ("POST_C", "Devanagari") => "AFTER_SUB",
        ("POST_C", "Bengali" | "Gurmukhi" | "Gujarati" | "Oriya" | "Tamil" | "Malayalam") => {
            "AFTER_POST"
        }
        ("POST_C", "Telugu") if cp <= 0x0C42 => "BEFORE_SUB",
        ("POST_C", "Kannada") if !(0x0CC3..=0x0CD6).contains(&cp) => "BEFORE_SUB",
        ("POST_C", _) => "AFTER_SUB",
        ("ABOVE_C", "Gurmukhi") => "AFTER_POST",
        ("ABOVE_C", "Oriya") => "AFTER_MAIN",
        ("ABOVE_C", "Telugu" | "Kannada") => "BEFORE_SUB",
        ("ABOVE_C", _) => "AFTER_SUB",
        ("BELOW_C", "Gurmukhi" | "Gujarati" | "Tamil" | "Malayalam") => "AFTER_POST",
        ("BELOW_C", "Telugu" | "Kannada") => "BEFORE_SUB",
        ("BELOW_C", _) => "AFTER_SUB",
        _ => return Err(format!("U+{cp:04X}: matra at {position}")),
    })
}

/// The category of a Khmer or Myanmar dependent vowel, by its position.
fn vowel_category(cp: u32, position: &str) -> Result<&'static str, String> {
    Ok(match position {
        "PRE_C" => "VPre",
        "ABOVE_C" => "VAbv",
        "BELOW_C" => "VBlw",
        "POST_C" => "VPst",
        _ => return Err(format!("U+{cp:04X}: vowel at {position}")),
    })
}

/// The Rust name of a category or position value.
fn variant(value: &str) -> String {
    let name = match value {
        "END" => "End",
        "PLACEHOLDER" => "Placeholder",
        "DOTTEDCIRCLE" => "DottedCircle",
        "ZWNJ" => "Zwnj",
        "ZWJ" => "Zwj",
        "BASE_C" => "BaseC",
        "ABOVE_C" => "AboveC",
        "BELOW_C" => "BelowC",
        "POST_C" => "PostC",
        "PRE_C" => "PreC",
        "PRE_M" => "PreM",
        "AFTER_MAIN" => "AfterMain",
        "AFTER_SUB" => "AfterSub",
        "BEFORE_SUB" => "BeforeSub",
        "AFTER_POST" => "AfterPost",
        "SMVD" => "Smvd",
        other => other,
    };
    name.to_owned()
}

fn indic(w: &mut Writer, ucd: &Loaded) -> Result<(), String> {
    let blocks = blocks(ucd)?;
    // (Indic_Syllabic_Category, Indic_Positional_Category, block) of every code
    // point either category file lists.
    let mut combined: BTreeMap<u32, (&str, &str, &str)> = BTreeMap::new();
    for (cp, value) in listed(ucd.text("IndicSyllabicCategory.txt"), 1)? {
        combined
            .entry(cp)
            .or_insert(("Other", "Not_Applicable", "No_Block"))
            .0 = value;
    }
    for (cp, value) in listed(ucd.text("IndicPositionalCategory.txt"), 1)? {
        combined
            .entry(cp)
            .or_insert(("Other", "Not_Applicable", "No_Block"))
            .1 = value;
    }
    for (cp, entry) in &mut combined {
        if let Some(block) = blocks.get(cp) {
            entry.2 = block;
        }
    }
    combined.retain(|cp, entry| ALLOWED_SINGLES.contains(cp) || ALLOWED_BLOCKS.contains(&entry.2));

    let mut data: BTreeMap<u32, (&str, &str, &str)> = BTreeMap::new();
    for (&cp, &(category, position, block)) in &combined {
        let mut category = indic_category(category)?;
        if category == "SM" && position == "Not_Applicable" {
            category = "SMPst";
        }
        data.insert(cp, (category, indic_position(position)?, block));
    }
    let block_of = |cp: u32| {
        blocks
            .get(&cp)
            .copied()
            .ok_or_else(|| format!("U+{cp:04X}: no block"))
    };
    for &(cps, category) in INDIC_CATEGORY_OVERRIDES {
        for &cp in cps {
            let position = data.get(&cp).map_or("END", |entry| entry.1);
            data.insert(cp, (category, position, block_of(cp)?));
        }
    }
    for (&cp, entry) in &mut data {
        let (category, position, block) = *entry;
        let mut position = if ["CM", "SM", "RS", "H", "M", "MPst"].contains(&category) {
            position
        } else {
            "END"
        };
        let mut category = category;
        if ["C", "CS", "Ra", "CM", "V", "PLACEHOLDER", "DOTTEDCIRCLE"].contains(&category) {
            position = "BASE_C";
        } else if ["M", "MPst"].contains(&category) {
            if block.starts_with("Khmer") || block.starts_with("Myanmar") {
                category = vowel_category(cp, position)?;
            } else {
                position = matra_position(cp, position, block)?;
            }
        } else if ["SM", "SMPst", "VD", "A", "Symbol"].contains(&category) {
            position = "SMVD";
        }
        *entry = (category, position, block);
    }
    for (cp, position) in INDIC_POSITION_OVERRIDES {
        let category = data.get(&cp).map_or("X", |entry| entry.0);
        data.insert(cp, (category, position, block_of(cp)?));
    }

    let mut palette = vec![("X", "END")];
    for &(category, position, _) in data.values() {
        if (category, position) != ("X", "END") {
            palette.push((category, position));
        }
    }
    palette[1..].sort_unstable();
    palette.dedup();
    let index: HashMap<(&str, &str), u16> = palette
        .iter()
        .enumerate()
        .map(|(i, &pair)| (pair, i as u16))
        .collect();
    let mut values = source::filled(0u16);
    for (&cp, &(category, position, _)) in &data {
        values[cp as usize] = index[&(category, position)];
    }
    w.trie(
        "Indic_Syllabic_Category and Indic_Positional_Category as the Indic, Khmer and\n\
         Myanmar shapers see them, as indices into [`INDIC_PROPERTIES`].",
        "INDIC",
        &pack(&values, 0),
    );
    let items: Vec<String> = palette
        .iter()
        .map(|&(category, position)| {
            format!(
                "(IndicCategory::{}, IndicPosition::{})",
                variant(category),
                variant(position)
            )
        })
        .collect();
    w.array(
        "The values of [`INDIC`]; no category at no position first.",
        "INDIC_PROPERTIES",
        "(IndicCategory, IndicPosition)",
        &items,
        2,
    );
    Ok(())
}

// The Universal Shaping Engine.

/// Scripts `gen-use-table.py` leaves out.
const USE_DISABLED_SCRIPTS: [&str; 5] = ["Arabic", "Lao", "Samaritan", "Syriac", "Thai"];

/// The properties a USE category is derived from.
struct UseProperties<'a> {
    cp: u32,
    syllabic: &'a str,
    positional: &'a str,
    joining: &'a str,
    ignorable: bool,
    general: &'a str,
}

impl UseProperties<'_> {
    fn is(&self, values: &[&str]) -> bool {
        values.contains(&self.syllabic)
    }

    fn base(&self) -> bool {
        self.is(&[
            "Number",
            "Consonant",
            "Consonant_Head_Letter",
            "Tone_Letter",
            "Vowel_Independent",
        ]) || (["C", "D", "L", "R"].contains(&self.joining) && self.syllabic != "Joiner")
            || (self.general == "Lo"
                && self.is(&[
                    "Avagraha",
                    "Bindu",
                    "Consonant_Final",
                    "Consonant_Medial",
                    "Consonant_Subjoined",
                    "Vowel",
                    "Vowel_Dependent",
                ]))
    }

    fn base_other(&self) -> bool {
        self.syllabic == "Consonant_Placeholder"
            || [0x2015, 0x2022, 0x25FB, 0x25FC, 0x25FD, 0x25FE].contains(&self.cp)
    }

    fn cgj(&self) -> bool {
        self.syllabic == "Joiner" || (self.ignorable && ["Mc", "Me", "Mn"].contains(&self.general))
    }

    fn word_joiner(&self) -> bool {
        const HANGUL_FILLERS: [u32; 8] = [
            0x115F, 0x1160, 0x3164, 0xFFA0, 0x1BCA0, 0x1BCA1, 0x1BCA2, 0x1BCA3,
        ];
        (self.ignorable
            && !HANGUL_FILLERS.contains(&self.cp)
            && self.syllabic == "Other"
            && !self.cgj())
            || self.general == "Cn"
    }

    fn other(&self) -> bool {
        (self.general == "Po"
            || self.is(&["Consonant_Dead", "Joiner", "Modifying_Letter", "Other"]))
            && !self.base()
            && !self.base_other()
            && !self.cgj()
            && self.syllabic != "Symbol_Modifier"
            && !self.word_joiner()
    }

    /// Every USE category whose rule holds; exactly one should.
    fn categories(&self) -> Vec<&'static str> {
        let not_lo = self.general != "Lo";
        let rules: [(&str, bool); 29] = [
            ("B", self.base()),
            ("N", self.syllabic == "Brahmi_Joining_Number"),
            ("GB", self.base_other()),
            ("CGJ", self.cgj()),
            (
                "F",
                (self.syllabic == "Consonant_Final" && not_lo)
                    || self.syllabic == "Consonant_Succeeding_Repha",
            ),
            ("FM", self.syllabic == "Syllable_Modifier"),
            (
                "M",
                (self.syllabic == "Consonant_Medial" && not_lo)
                    || self.syllabic == "Consonant_Initial_Postfixed",
            ),
            (
                "CM",
                self.is(&["Nukta", "Gemination_Mark", "Consonant_Killer"]),
            ),
            ("SUB", self.syllabic == "Consonant_Subjoined" && not_lo),
            ("CS", self.syllabic == "Consonant_With_Stacker"),
            ("H", self.syllabic == "Virama" && self.cp != 0x0DCA),
            ("HVM", self.cp == 0x0DCA),
            ("HN", self.syllabic == "Number_Joiner"),
            (
                "IS",
                self.syllabic == "Invisible_Stacker" && self.cp != 0x1A60,
            ),
            ("G", self.syllabic == "Hieroglyph"),
            ("HM", self.syllabic == "Hieroglyph_Modifier"),
            ("HR", self.syllabic == "Hieroglyph_Mirror"),
            ("J", self.syllabic == "Hieroglyph_Joiner"),
            (
                "SB",
                self.is(&["Hieroglyph_Mark_Begin", "Hieroglyph_Segment_Begin"]),
            ),
            (
                "SE",
                self.is(&["Hieroglyph_Mark_End", "Hieroglyph_Segment_End"]),
            ),
            ("ZWNJ", self.syllabic == "Non_Joiner"),
            ("O", self.other()),
            ("RK", self.syllabic == "Reordering_Killer"),
            (
                "R",
                self.is(&["Consonant_Preceding_Repha", "Consonant_Prefixed"]),
            ),
            ("Sk", self.cp == 0x1A60),
            ("SM", self.syllabic == "Symbol_Modifier"),
            (
                "V",
                self.syllabic == "Pure_Killer"
                    || (not_lo && self.is(&["Vowel", "Vowel_Dependent"])),
            ),
            (
                "VM",
                self.is(&[
                    "Tone_Mark",
                    "Cantillation_Mark",
                    "Register_Shifter",
                    "Visarga",
                ]) || (not_lo && self.syllabic == "Bindu"),
            ),
            ("WJ", self.word_joiner()),
        ];
        rules
            .iter()
            .filter(|(_, holds)| *holds)
            .map(|&(name, _)| name)
            .collect()
    }

    /// The position suffixes of a category that takes one, with the
    /// Indic_Positional_Category values of each.
    fn positions(category: &str) -> Option<&'static [(&'static str, &'static [&'static str])]> {
        Some(match category {
            "F" => &[("Abv", &["Top"]), ("Blw", &["Bottom"]), ("Pst", &["Right"])],
            "M" => &[
                ("Abv", &["Top"]),
                ("Blw", &["Bottom", "Bottom_And_Left", "Bottom_And_Right"]),
                ("Pst", &["Right"]),
                ("Pre", &["Left", "Top_And_Bottom_And_Left"]),
            ],
            "CM" => &[("Abv", &["Top"]), ("Blw", &["Bottom", "Overstruck"])],
            "V" => &[
                (
                    "Abv",
                    &[
                        "Top",
                        "Top_And_Bottom",
                        "Top_And_Bottom_And_Right",
                        "Top_And_Right",
                    ],
                ),
                ("Blw", &["Bottom", "Overstruck", "Bottom_And_Right"]),
                ("Pst", &["Right"]),
                (
                    "Pre",
                    &[
                        "Left",
                        "Top_And_Left",
                        "Top_And_Left_And_Right",
                        "Left_And_Right",
                    ],
                ),
            ],
            "VM" => &[
                ("Abv", &["Top"]),
                ("Blw", &["Bottom", "Overstruck"]),
                ("Pst", &["Right"]),
                ("Pre", &["Left"]),
            ],
            "SM" => &[("Abv", &["Top"]), ("Blw", &["Bottom"])],
            "FM" => &[
                ("Abv", &["Top"]),
                ("Blw", &["Bottom"]),
                ("Pst", &["Not_Applicable"]),
            ],
            _ => return None,
        })
    }

    fn category(&self) -> Result<String, String> {
        let describe = || {
            format!(
                "U+{:04X} ({} {} {} {} {})",
                self.cp, self.syllabic, self.positional, self.general, self.joining, self.ignorable
            )
        };
        let categories = self.categories();
        let [category] = categories[..] else {
            return Err(format!("{}: USE categories {categories:?}", describe()));
        };
        let positional = if [0x11302, 0x11303, 0x114C1].contains(&self.cp) {
            "Top"
        } else {
            self.positional
        };
        let Some(positions) = Self::positions(category) else {
            // These take no position suffix, whatever their position.
            const UNPOSITIONED: [&str; 9] = ["B", "H", "HM", "HR", "HVM", "IS", "R", "RK", "SUB"];
            if !["Not_Applicable", "Visual_Order_Left"].contains(&positional)
                && self.cp != 0x0F7F
                && !UNPOSITIONED.contains(&category)
            {
                return Err(format!("{}: {category} has a position", describe()));
            }
            return Ok(category.to_owned());
        };
        let suffixes: Vec<&str> = positions
            .iter()
            .filter(|(_, values)| values.contains(&positional))
            .map(|&(suffix, _)| suffix)
            .collect();
        match suffixes[..] {
            [suffix] => Ok(format!("{category}{suffix}")),
            _ => Err(format!("{}: {category} positions {suffixes:?}", describe())),
        }
    }
}

fn universal(
    w: &mut Writer,
    ucd: &Loaded,
    derived: &Loaded,
    tree: &TreeFiles,
) -> Result<(), String> {
    // Field values per code point, as gen-use-table.py merges its files: entries
    // start with the syllabic, positional, joining or ignorable files, and
    // UnicodeData.txt (its range ends unexpanded), Scripts.txt and the
    // additional files only fill in or override.
    struct Entry<'a> {
        syllabic: &'a str,
        positional: &'a str,
        joining: &'a str,
        ignorable: bool,
        general: &'a str,
        script: &'a str,
    }
    impl Default for Entry<'_> {
        fn default() -> Self {
            Self {
                syllabic: "Other",
                positional: "Not_Applicable",
                joining: "X",
                ignorable: false,
                general: "Cn",
                script: "Unknown",
            }
        }
    }
    let mut entries: BTreeMap<u32, Entry<'_>> = BTreeMap::new();
    for (cp, value) in listed(ucd.text("IndicSyllabicCategory.txt"), 1)? {
        entries.entry(cp).or_default().syllabic = value;
    }
    for (cp, value) in listed(&tree.syllabic, 1)? {
        entries.entry(cp).or_default().syllabic = if value == "Consonant_Final_Modifier" {
            "Syllable_Modifier"
        } else {
            value
        };
    }
    for (cp, value) in listed(ucd.text("IndicPositionalCategory.txt"), 1)? {
        entries.entry(cp).or_default().positional = value;
    }
    for (cp, value) in listed(&tree.positional, 1)? {
        entries.entry(cp).or_default().positional = if value == "NA" {
            "Not_Applicable"
        } else {
            value
        };
    }
    for (cp, value) in listed(ucd.text("ArabicShaping.txt"), 2)? {
        entries.entry(cp).or_default().joining = value;
    }
    for (cp, value) in listed(derived.text("DerivedCoreProperties.txt"), 1)? {
        if value == "Default_Ignorable_Code_Point" {
            entries.entry(cp).or_default().ignorable = true;
        }
    }
    for f in source::fields(ucd.text("UnicodeData.txt")) {
        if let Some(e) = entries.get_mut(&source::code_point(f[0])?) {
            e.general = f[2];
        }
    }
    for (cp, value) in listed(ucd.text("Scripts.txt"), 1)? {
        if let Some(e) = entries.get_mut(&cp) {
            e.script = value;
        }
    }

    let mut categories = BTreeMap::new();
    for (&cp, e) in &entries {
        if USE_DISABLED_SCRIPTS.contains(&e.script) {
            continue;
        }
        let syllabic = match cp {
            0x1CE2..=0x1CE8 => "Cantillation_Mark",
            0x0F18..=0x0F19 | 0x0F3E..=0x0F3F => "Vowel_Dependent",
            0x1CED => "Tone_Mark",
            _ => e.syllabic,
        };
        let properties = UseProperties {
            cp,
            syllabic,
            positional: e.positional,
            joining: e.joining,
            ignorable: e.ignorable,
            general: e.general,
        };
        categories.insert(cp, properties.category()?);
    }

    let mut palette: Vec<&str> = vec!["O"];
    palette.extend(
        categories
            .values()
            .map(String::as_str)
            .filter(|&c| c != "O"),
    );
    palette[1..].sort_unstable();
    palette.dedup();
    let mut values = source::filled(0u16);
    for (&cp, category) in &categories {
        values[cp as usize] = palette.iter().position(|p| p == category).expect("listed") as u16;
    }
    w.trie(
        "The Universal Shaping Engine category of every character, as indices into\n\
         [`USE_CATEGORIES`].",
        "USE",
        &pack(&values, 0),
    );
    let items: Vec<String> = palette
        .iter()
        .map(|&category| {
            // Spelled as Rust variants: capitals only for initials of words.
            let name = match category {
                "CGJ" => "Cgj",
                "HVM" => "Hvm",
                "SUB" => "Sub",
                "ZWNJ" => "Zwnj",
                other => other,
            };
            format!("UseCategory::{name}")
        })
        .collect();
    w.array(
        "The values of [`USE`]; other first.",
        "USE_CATEGORIES",
        "UseCategory",
        &items,
        1,
    );
    Ok(())
}

// Vowel constraints.

/// The prohibited sequences of one script as `gen-vowel-constraints.py` builds
/// them: a run of characters, or the sequences after each possible character.
/// Adding a sequence that starts like a stored one keeps only the shorter.
enum Constraints {
    Run(Vec<u32>),
    Next(BTreeMap<u32, Constraints>),
}

impl Constraints {
    fn add(&mut self, sequence: &[u32]) {
        let Some((&first, rest)) = sequence.split_first() else {
            return;
        };
        if let Self::Run(run) = self {
            if run.starts_with(sequence) {
                *run = sequence.to_vec();
            } else if !sequence.starts_with(run) {
                let next = Self::Run(run[1..].to_vec());
                *self = Self::Next(BTreeMap::from([(run[0], next)]));
            }
        }
        if let Self::Next(next) = self {
            match next.get_mut(&first) {
                Some(constraints) => constraints.add(rest),
                None => {
                    next.insert(first, Self::Run(rest.to_vec()));
                }
            }
        }
    }

    /// Every stored sequence, after `prefix`. HarfBuzz's generated matcher puts
    /// the dotted circle before the last character and stays within the buffer
    /// only for sequences of two characters and those of three whose first two
    /// no other sequence shares; anything else is rejected.
    fn sequences(&self, prefix: &mut Vec<u32>, out: &mut Vec<Vec<u32>>) -> Result<(), String> {
        match self {
            Self::Run(run) => {
                let fits = match prefix.len() {
                    0 => run.len() == 2,
                    1 => matches!(run.len(), 1 | 2),
                    2 => run.is_empty(),
                    _ => false,
                };
                if !fits {
                    return Err(format!("prohibited sequence {prefix:X?} then {run:X?}"));
                }
                out.push([&prefix[..], run].concat());
            }
            Self::Next(next) => {
                if prefix.len() > 1 {
                    return Err(format!("prohibited sequences after {prefix:X?} branch"));
                }
                for (&c, constraints) in next {
                    prefix.push(c);
                    constraints.sequences(prefix, out)?;
                    prefix.pop();
                }
            }
        }
        Ok(())
    }
}

/// The prohibited sequences of each script, by its ISO 15924 code: a dotted
/// circle goes before the last character of each.
fn vowel_constraints(w: &mut Writer, ucd: &Loaded, tree: &TreeFiles) -> Result<(), String> {
    let short = script_codes(ucd);
    let scripts: HashMap<u32, &str> = listed(ucd.text("Scripts.txt"), 1)?.into_iter().collect();
    let mut constraints: BTreeMap<&str, Constraints> = BTreeMap::new();
    let mut lines = tree.invalid_clusters.lines();
    lines
        .by_ref()
        .find(|line| line.trim() == "#")
        .ok_or("IndicShapingInvalidCluster.txt: no end of header")?;
    for line in lines {
        let data = line.split('#').next().unwrap_or("");
        let sequence = data
            .split(';')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .map(source::code_point)
            .collect::<Result<Vec<u32>, _>>()?;
        if sequence.is_empty() {
            continue;
        }
        if sequence.len() < 2 {
            return Err(format!("prohibited sequence {sequence:X?} is too short"));
        }
        let name = scripts
            .get(&sequence[0])
            .ok_or_else(|| format!("U+{:04X}: no script", sequence[0]))?;
        let tag = short
            .get(name)
            .ok_or_else(|| format!("unknown script {name}"))?;
        match constraints.get_mut(tag) {
            Some(set) => set.add(&sequence),
            None => {
                constraints.insert(tag, Constraints::Run(sequence));
            }
        }
    }
    let mut items = Vec::new();
    for (tag, set) in &constraints {
        let mut sequences = Vec::new();
        set.sequences(&mut Vec::new(), &mut sequences)?;
        w.add_data(sequences.iter().map(|s| 4 * s.len() + 16).sum());
        let list: Vec<String> = sequences
            .iter()
            .map(|s| {
                let cps: Vec<String> = s.iter().map(|&c| hex(c)).collect();
                format!("&[{}]", cps.join(", "))
            })
            .collect();
        items.push(format!(
            "(Script::from_code(*b\"{tag}\"), &[{}])",
            list.join(", ")
        ));
    }
    w.array(
        "Vowel sequences that look like another vowel, from Microsoft's\n\
         IndicShapingInvalidCluster.txt, by script code: a dotted circle goes before\n\
         the last character of each.",
        "VOWEL_CONSTRAINTS",
        "(Script, &[&[u32]])",
        &items,
        20,
    );
    Ok(())
}

/// The ISO 15924 code of every script, by its long name.
fn script_codes(ucd: &Loaded) -> HashMap<&str, &str> {
    source::fields(ucd.text("PropertyValueAliases.txt"))
        .filter(|f| f.len() >= 3 && f[0] == "sc")
        .map(|f| (f[2], f[1]))
        .collect()
}

/// `scripts` (ISO 15924 codes) as a sorted array of `Script` values named
/// `name`.
fn script_array(w: &mut Writer, doc: &str, name: &str, mut scripts: Vec<String>) {
    scripts.sort_unstable();
    scripts.dedup();
    let items: Vec<String> = scripts
        .iter()
        .map(|code| format!("Script::from_code(*b\"{code}\")"))
        .collect();
    w.array(doc, name, "Script", &items, 4);
}

/// The scripts whose letters join as Arabic letters do, as
/// `gen-arabic-joining-list.py` finds them: those of the characters
/// ArabicShaping.txt gives a joining type other than T and U, but Common and
/// Inherited.
fn joining_scripts(w: &mut Writer, ucd: &Loaded) -> Result<(), String> {
    let codes = script_codes(ucd);
    let scripts: HashMap<u32, &str> = listed(ucd.text("Scripts.txt"), 1)?.into_iter().collect();
    let mut joining = Vec::new();
    for f in source::fields(ucd.text("ArabicShaping.txt")) {
        let kind = *f.get(2).ok_or("ArabicShaping.txt: missing joining type")?;
        if matches!(kind, "T" | "U") {
            continue;
        }
        let cp = source::code_point(f[0])?;
        let name = *scripts
            .get(&cp)
            .ok_or_else(|| format!("U+{cp:04X}: no script"))?;
        if matches!(name, "Common" | "Inherited") {
            continue;
        }
        let code = codes
            .get(name)
            .ok_or_else(|| format!("unknown script {name}"))?;
        joining.push((*code).to_owned());
    }
    script_array(
        w,
        "Scripts whose letters join as Arabic letters do (ArabicShaping.txt gives\n\
         them a joining type other than T and U), sorted.",
        "JOINING_SCRIPTS",
        joining,
    );
    Ok(())
}

/// The scripts `hb_ot_shaper_categorize` gives the Universal Shaping Engine,
/// by their tags in `hb-script-list.h`.
fn use_scripts(w: &mut Writer, tree: &TreeFiles) -> Result<(), String> {
    let tags = script_tags(&tree.scripts)?;
    let mut scripts = Vec::new();
    for name in use_script_names(&tree.shaper)? {
        let tag = tags
            .get(name)
            .ok_or_else(|| format!("hb-script-list.h: no HB_SCRIPT_{name}"))?;
        scripts.push(tag.clone());
    }
    script_array(
        w,
        "Scripts HarfBuzz gives the Universal Shaping Engine, sorted.",
        "USE_SCRIPTS",
        scripts,
    );
    Ok(())
}

/// The ISO 15924 code of each `HB_SCRIPT_NAME = HB_TAG ('A','b','c','d')`, by
/// name.
fn script_tags(text: &str) -> Result<HashMap<&str, String>, String> {
    let mut tags = HashMap::new();
    for line in text.lines() {
        let Some((name, value)) = line.trim().split_once('=') else {
            continue;
        };
        let (Some(name), Some(value)) = (
            name.trim().strip_prefix("HB_SCRIPT_"),
            value.trim().strip_prefix("HB_TAG ("),
        ) else {
            continue;
        };
        let letters = value.split(')').next().unwrap_or_default();
        let code: String = letters
            .split(',')
            .map(|letter| letter.trim().trim_matches('\''))
            .collect();
        if code.len() != 4 || !code.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(format!("HB_SCRIPT_{name}: unexpected tag ({letters})"));
        }
        tags.insert(name, code);
    }
    Ok(tags)
}

/// The script names (`HB_SCRIPT_` stripped) of the case group of
/// `hb_ot_shaper_categorize`'s switch whose last statement returns the
/// Universal Shaping Engine. (The Indic group returns it too, but returns the
/// Indic shaper last.)
fn use_script_names(shaper: &str) -> Result<Vec<&str>, String> {
    let start = shaper
        .find("hb_ot_shaper_categorize (")
        .ok_or("hb-ot-shaper.hh: no hb_ot_shaper_categorize")?;
    // Each group: its labels and the shaper its last statement returns.
    let mut groups: Vec<(Vec<&str>, Option<&str>)> = Vec::new();
    let mut in_labels = false;
    for line in shaper[start..].lines().skip(1) {
        let line = line.trim();
        if line == "}" {
            break;
        }
        if let Some(label) = line
            .strip_prefix("case HB_SCRIPT_")
            .and_then(|rest| rest.strip_suffix(':'))
        {
            if !in_labels {
                groups.push((Vec::new(), None));
                in_labels = true;
            }
            if let Some((labels, _)) = groups.last_mut() {
                labels.push(label);
            }
            continue;
        }
        if line.is_empty() || line.starts_with("/*") || line.starts_with('#') {
            continue;
        }
        in_labels = false;
        if let Some(returned) = line
            .strip_prefix("return &_hb_ot_shaper_")
            .and_then(|rest| rest.strip_suffix(';'))
            && let Some((_, last)) = groups.last_mut()
        {
            *last = Some(returned);
        }
    }
    let mut use_groups = groups
        .into_iter()
        .filter(|(_, last)| *last == Some("use"))
        .map(|(labels, _)| labels);
    match (use_groups.next(), use_groups.next()) {
        (Some(labels), None) if !labels.is_empty() => Ok(labels),
        _ => Err("hb-ot-shaper.hh: not one case group ending with the USE shaper".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_use_scripts_are_the_group_that_returns_the_use_shaper_last() {
        let shaper = "hb_ot_shaper_categorize (hb_script_t script)
{
  switch ((hb_tag_t) script)
  {
    default:
      return &_hb_ot_shaper_default;
    case HB_SCRIPT_BENGALI:
      if (x) return &_hb_ot_shaper_default;
      else if (y) return &_hb_ot_shaper_use;
      else return &_hb_ot_shaper_indic;
#ifndef HB_NO_X
    case HB_SCRIPT_TIBETAN:

    /* Unicode-3.0 additions */
    case HB_SCRIPT_SINHALA:
      if (x)
	return &_hb_ot_shaper_default;
      else
	return &_hb_ot_shaper_use;
#endif
  }
}";
        assert_eq!(use_script_names(shaper), Ok(vec!["TIBETAN", "SINHALA"]));
        let scripts = "  HB_SCRIPT_TIBETAN\t\t\t= HB_TAG ('T','i','b','t'), /*2.0*/\n\
                       HB_SCRIPT_INVALID = HB_TAG_NONE,";
        let tags = script_tags(scripts).unwrap();
        assert_eq!(tags.get("TIBETAN").map(String::as_str), Some("Tibt"));
        assert_eq!(tags.len(), 1);
    }

    #[test]
    fn constraints_keep_the_shorter_of_overlapping_sequences() {
        let mut set = Constraints::Run(vec![1, 2]);
        for sequence in [&[1, 3][..], &[1, 2, 4], &[5, 6, 7], &[1, 3, 8]] {
            set.add(sequence);
        }
        let mut sequences = Vec::new();
        set.sequences(&mut Vec::new(), &mut sequences).unwrap();
        assert_eq!(sequences, [vec![1, 2], vec![1, 3], vec![5, 6, 7]]);

        let mut set = Constraints::Run(vec![1, 2, 3]);
        set.add(&[1, 2, 4]);
        assert!(set.sequences(&mut Vec::new(), &mut Vec::new()).is_err());
    }

    #[test]
    fn matra_positions_follow_the_block() {
        assert_eq!(matra_position(0x093F, "PRE_C", "Devanagari"), Ok("PRE_M"));
        assert_eq!(matra_position(0x0C41, "POST_C", "Telugu"), Ok("BEFORE_SUB"));
        assert_eq!(matra_position(0x0C43, "POST_C", "Telugu"), Ok("AFTER_SUB"));
        assert_eq!(matra_position(0x0CC3, "POST_C", "Kannada"), Ok("AFTER_SUB"));
        assert!(matra_position(0x0900, "END", "Devanagari").is_err());
    }
}
