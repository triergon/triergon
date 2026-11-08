//! The tables of the FriBidi 1.0.17 release, by the rules of the FriBidi 1.0.12
//! generators that produced them (`gen.tab/gen-*-tab.c`, read as evidence of what
//! they produce; nothing of FriBidi is reproduced here).
//!
//! FriBidi reads UnicodeData.txt line by line: the code points between a
//! `<..., First>` and `<..., Last>` line keep their default values.

use std::collections::{BTreeMap, HashMap};
use std::ops::RangeInclusive;

use super::emit::{Writer, hex};
use super::pack::pack;
use super::source::{self, Record};
use super::{Generated, Loaded};

/// Bidi_Class values with their `BidiClass` variants.
const BIDI_CLASSES: [(&str, &str); 23] = [
    ("L", "L"),
    ("R", "R"),
    ("AL", "Al"),
    ("EN", "En"),
    ("ES", "Es"),
    ("ET", "Et"),
    ("AN", "An"),
    ("CS", "Cs"),
    ("NSM", "Nsm"),
    ("BN", "Bn"),
    ("B", "B"),
    ("S", "S"),
    ("WS", "Ws"),
    ("ON", "On"),
    ("LRE", "Lre"),
    ("LRO", "Lro"),
    ("RLE", "Rle"),
    ("RLO", "Rlo"),
    ("PDF", "Pdf"),
    ("LRI", "Lri"),
    ("RLI", "Rli"),
    ("FSI", "Fsi"),
    ("PDI", "Pdi"),
];

/// The classes of BMP code points UnicodeData.txt does not list, in the order
/// FriBidi applies them; everything else is L. Then the last two code points of
/// every plane are BN, and then [`SUPPLEMENTARY_BIDI_CLASSES`] apply.
const DEFAULT_BIDI_CLASSES: [(RangeInclusive<u32>, &str); 10] = [
    (0x0590..=0x05FF, "R"),
    (0x07C0..=0x08FF, "R"),
    (0xFB1D..=0xFB4F, "R"),
    (0x0600..=0x07BF, "AL"),
    (0xFB50..=0xFDCF, "AL"),
    (0xFDF0..=0xFDFF, "AL"),
    (0xFE70..=0xFEFF, "AL"),
    (0x2060..=0x206F, "BN"),
    (0xFDD0..=0xFDEF, "BN"),
    (0xFFF0..=0xFFF8, "BN"),
];

const SUPPLEMENTARY_BIDI_CLASSES: [(RangeInclusive<u32>, &str); 2] =
    [(0x1_0800..=0x1_0FFF, "R"), (0xE_0000..=0xE_0FFF, "BN")];

/// Joining types with their `JoiningType` variants.
const JOINING_TYPES: [(&str, &str); 7] = [
    ("U", "NonJoining"),
    ("R", "RightJoining"),
    ("D", "DualJoining"),
    ("C", "JoinCausing"),
    ("T", "Transparent"),
    ("L", "LeftJoining"),
    ("G", "Ignored"),
];

/// Presentation-form tags, in the column order of the generated table (the order
/// of `ArabicForm`).
const FORMS: [&str; 4] = ["initial", "medial", "final", "isolated"];

pub fn generate(ucd: &Loaded) -> Result<Generated, String> {
    let records = source::unicode_data(ucd.text("UnicodeData.txt"))?;
    let mut w = Writer::new(&ucd.header(
        "The character properties of FriBidi 1.0.17: bidi types, brackets, mirroring, \
         joining types and Arabic presentation forms",
    ));
    w.raw(
        "\nuse super::{BidiClass, BracketKind, JoiningType};\n\
         use crate::unicode::trie::Trie;\n",
    );
    bidi_class(&mut w, &records)?;
    bracket(&mut w, ucd, &records)?;
    mirror(&mut w, ucd)?;
    joining_type(&mut w, ucd, &records)?;
    arabic_shaping(&mut w, ucd, &records)?;
    Ok(w.finish())
}

fn bidi_class(w: &mut Writer, records: &[Record<'_>]) -> Result<(), String> {
    let index = |class: &str| {
        BIDI_CLASSES
            .iter()
            .position(|&(name, _)| name == class)
            .map(|i| i as u16)
            .ok_or_else(|| format!("unknown Bidi_Class {class}"))
    };
    let mut values = source::filled(index("L")?);
    for (range, class) in &DEFAULT_BIDI_CLASSES {
        source::set(&mut values, range.clone(), &index(class)?);
    }
    for plane in 0..17 {
        let last = (plane << 16) | 0xFFFF;
        source::set(&mut values, last - 1..=last, &index("BN")?);
    }
    for (range, class) in &SUPPLEMENTARY_BIDI_CLASSES {
        source::set(&mut values, range.clone(), &index(class)?);
    }
    for record in records {
        values[record.code_point as usize] = index(record.bidi_class)?;
    }
    w.trie(
        "FriBidi's bidi types, as indices into [`BIDI_CLASSES`].",
        "BIDI_CLASS",
        &pack(&values, index("L")?),
    );
    let variants: Vec<String> = BIDI_CLASSES
        .iter()
        .map(|(_, variant)| format!("BidiClass::{variant}"))
        .collect();
    w.array(
        "The values of [`BIDI_CLASS`].",
        "BIDI_CLASSES",
        "BidiClass",
        &variants,
        1,
    );
    Ok(())
}

/// A bracket's identity is the opening bracket of its pair, replaced by the first
/// character of that bracket's decomposition if it has one, so equivalent brackets
/// pair (U+2329 and U+3008 both have identity U+3008). FriBidi 1.0.12's generator
/// folds compatibility decompositions as well, so U+FF08 FULLWIDTH LEFT
/// PARENTHESIS has identity U+0028; the generator of later releases excludes them,
/// but the tables FriBidi 1.0.17 ships still fold them.
fn bracket(w: &mut Writer, ucd: &Loaded, records: &[Record<'_>]) -> Result<(), String> {
    let mut equivalent = HashMap::new();
    for record in records {
        let mapping = match record.tagged_decomposition()? {
            Some((_, mapping)) => Some(mapping),
            None => record.canonical_decomposition()?,
        };
        if let Some(mapping) = mapping {
            equivalent.insert(record.code_point, mapping[0]);
        }
    }
    let mut offsets = source::filled(None);
    for f in source::fields(ucd.text("BidiBrackets.txt")) {
        let cp = source::code_point(f[0])?;
        let (opening, kind) = match f[2] {
            "o" => (cp, "Open"),
            "c" => (source::code_point(f[1])?, "Close"),
            other => return Err(format!("BidiBrackets.txt: bracket type {other}")),
        };
        let identity = equivalent.get(&opening).copied().unwrap_or(opening);
        offsets[cp as usize] = Some((identity as i32 - cp as i32, kind));
    }
    let mut palette: Vec<Option<(i32, &str)>> = offsets.clone();
    palette.sort_unstable();
    palette.dedup();
    let values: Vec<u16> = offsets
        .iter()
        .map(|o| palette.binary_search(o).expect("listed") as u16)
        .collect();
    w.trie(
        "Paired brackets (BidiBrackets.txt), as indices into [`BRACKETS`].",
        "BRACKET",
        &pack(&values, 0),
    );
    let items: Vec<String> = palette
        .iter()
        .map(|entry| match entry {
            None => "None".to_owned(),
            Some((offset, kind)) => format!("Some(({offset}, BracketKind::{kind}))"),
        })
        .collect();
    w.array(
        "The values of [`BRACKET`]: the offset from a bracket to its identity, and its kind.",
        "BRACKETS",
        "Option<(i32, BracketKind)>",
        &items,
        8,
    );
    Ok(())
}

fn mirror(w: &mut Writer, ucd: &Loaded) -> Result<(), String> {
    let mut offsets = source::filled(0i32);
    for f in source::fields(ucd.text("BidiMirroring.txt")) {
        let (cp, mirror) = (source::code_point(f[0])?, source::code_point(f[1])?);
        offsets[cp as usize] = mirror as i32 - cp as i32;
    }
    let mut palette = vec![0];
    palette.extend(offsets.iter().copied().filter(|&d| d != 0));
    palette[1..].sort_unstable();
    palette.dedup();
    let values: Vec<u16> = offsets
        .iter()
        .map(|d| palette.iter().position(|p| p == d).expect("listed") as u16)
        .collect();
    w.trie(
        "Bidi_Mirroring_Glyph (BidiMirroring.txt) as an offset, as indices into\n\
         [`MIRROR_DELTAS`].",
        "MIRROR",
        &pack(&values, 0),
    );
    let items: Vec<String> = palette.iter().map(i32::to_string).collect();
    w.array(
        "The offsets of [`MIRROR`]; 0 first.",
        "MIRROR_DELTAS",
        "i32",
        &items,
        4,
    );
    Ok(())
}

/// Ignored for the explicit formatting classes and BN, transparent for marks and
/// format characters, then as ArabicShaping.txt lists them; non-joining otherwise.
fn joining_type(w: &mut Writer, ucd: &Loaded, records: &[Record<'_>]) -> Result<(), String> {
    const IGNORED: [&str; 10] = [
        "BN", "LRE", "RLE", "LRO", "RLO", "PDF", "LRI", "RLI", "FSI", "PDI",
    ];
    let index = |name: &str| {
        JOINING_TYPES
            .iter()
            .position(|&(t, _)| t == name)
            .map(|i| i as u16)
            .ok_or_else(|| format!("unknown joining type {name}"))
    };
    let mut values = source::filled(index("U")?);
    for record in records {
        let slot = &mut values[record.code_point as usize];
        if IGNORED.contains(&record.bidi_class) {
            *slot = index("G")?;
        }
        if matches!(record.general_category, "Mn" | "Me" | "Cf") {
            *slot = index("T")?;
        }
    }
    for f in source::fields(ucd.text("ArabicShaping.txt")) {
        source::set(&mut values, source::range(f[0])?, &index(f[2])?);
    }
    w.trie(
        "FriBidi's joining types, as indices into [`JOINING_TYPES`].",
        "JOINING_TYPE",
        &pack(&values, index("U")?),
    );
    let variants: Vec<String> = JOINING_TYPES
        .iter()
        .map(|(_, variant)| format!("JoiningType::{variant}"))
        .collect();
    w.array(
        "The values of [`JOINING_TYPE`].",
        "JOINING_TYPES",
        "JoiningType",
        &variants,
        1,
    );
    Ok(())
}

/// The presentation forms of single characters, and the mandatory ligatures of LAM
/// with an ALEF, both from compatibility decompositions.
fn arabic_shaping(w: &mut Writer, ucd: &Loaded, records: &[Record<'_>]) -> Result<(), String> {
    let mut forms: BTreeMap<u32, [u32; 4]> = BTreeMap::new();
    let mut ligatures = Vec::new();
    for record in records {
        let Some((tag, mapping)) = record.tagged_decomposition()? else {
            continue;
        };
        let Some(form) = FORMS.iter().position(|&f| f == tag) else {
            continue;
        };
        match mapping[..] {
            // The last form listed wins.
            [base] => forms.entry(base).or_default()[form] = record.code_point,
            [0x0644, alef] => ligatures.push((form, alef, record.code_point)),
            _ => {}
        }
    }
    let first = *forms.keys().next().ok_or("no Arabic presentation forms")?;
    let last = *forms.keys().next_back().expect("nonempty");
    w.raw(&format!(
        "\n/// The first character of [`PRESENTATION_FORMS`].\n\
         pub(super) const PRESENTATION_FIRST: u32 = {};\n",
        hex(first)
    ));
    let rows: Vec<String> = (first..=last)
        .map(|u| {
            format!(
                "[{}]",
                forms
                    .get(&u)
                    .copied()
                    .unwrap_or_default()
                    .map(hex)
                    .join(", ")
            )
        })
        .collect();
    w.array(
        "The presentation forms of the characters from [`PRESENTATION_FIRST`] on, in\n\
         `ArabicForm` order (initial, medial, final, isolated); 0 for none.",
        "PRESENTATION_FORMS",
        "[u16; 4]",
        &rows,
        8,
    );

    let alefs: Vec<u32> = source::fields(ucd.text("ArabicShaping.txt"))
        .filter(|f| f.get(3) == Some(&"ALEF"))
        .map(|f| source::code_point(f[0]))
        .collect::<Result<_, _>>()?;
    let form_of = |cp: u32, form: usize| {
        forms
            .get(&cp)
            .map(|f| f[form])
            .filter(|&f| f != 0)
            .ok_or_else(|| format!("U+{cp:04X} has no {} form", FORMS[form]))
    };
    let (initial, medial, final_, isolated) = (0, 1, 2, 3);
    let mut pairs = Vec::new();
    for (form, alef, ligature) in ligatures {
        if !alefs.contains(&alef) {
            continue;
        }
        let lam = match form {
            f if f == isolated => form_of(0x0644, initial)?,
            f if f == final_ => form_of(0x0644, medial)?,
            _ => return Err(format!("U+{ligature:04X}: unexpected LAM-ALEF form")),
        };
        pairs.push((lam, form_of(alef, final_)?, ligature));
    }
    pairs.sort_unstable();
    let items: Vec<String> = pairs
        .iter()
        .map(|&(lam, alef, ligature)| format!("({}, {}, {})", hex(lam), hex(alef), hex(ligature)))
        .collect();
    w.array(
        "`(lam, alef, ligature)`, sorted: the presentation forms of LAM and an ALEF that\n\
         form a mandatory ligature.",
        "LAM_ALEF_LIGATURES",
        "(u32, u32, u32)",
        &items,
        12,
    );
    Ok(())
}
