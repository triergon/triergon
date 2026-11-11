//! The bidi and Arabic joining tables of `triergon_shape::bidi`, from the
//! Unicode 14.0.0 data files: Bidi_Class, paired brackets, mirrored glyphs,
//! joining types and the Arabic presentation forms that UnicodeData.txt
//! decomposes.
//!
//! Two properties take FriBidi's values where they differ from the UCD's, as
//! observed through the `shape-unicode` comparison over every code point:
//!
//! - Unassigned code points default to R in U+0590..U+08FF, U+FB1D..U+FB4F and
//!   U+10800..U+10FFF and to AL in U+0600..U+07BF, U+FB50..U+FDCF,
//!   U+FDF0..U+FDFF and U+FE70..U+FEFF; every other unassigned code point that
//!   DerivedBidiClass.txt gives R, AL or ET (U+0860..U+08FF, U+10D00..U+10D3F,
//!   U+10F30..U+10F6F, the Currency Symbols block and U+1E800..U+1EFFF) is L.
//! - Joining_Type is T for every Mn, Me and Cf character ArabicShaping.txt does
//!   not list, BN and the explicit formatting characters included, and G only
//!   for the BN control characters (Cc).

use std::collections::BTreeMap;

use super::emit::{Writer, hex};
use super::pack::pack;
use super::source;
use super::{Generated, Loaded};

/// Bidi_Class short names in the order of `BidiClass`'s discriminants.
const CLASSES: [&str; 23] = [
    "L", "R", "AL", "EN", "ES", "ET", "AN", "CS", "NSM", "BN", "B", "S", "WS", "ON", "LRE", "LRO",
    "RLE", "RLO", "PDF", "LRI", "RLI", "FSI", "PDI",
];

/// Joining_Type short names in the order of `JoiningType`'s discriminants; `G`
/// stands for the characters joining passes over without a type of their own.
const JOINING: [&str; 7] = ["U", "R", "D", "C", "T", "L", "G"];

/// Decomposition tags of the positional presentation forms, in the order of the
/// generated shaping table's columns.
const FORMS: [&str; 4] = ["initial", "medial", "final", "isolated"];

const LAM: u32 = 0x0644;

pub fn generate(ucd: &Loaded) -> Result<Generated, String> {
    let records = source::unicode_data(ucd.text("UnicodeData.txt"))?;
    let expanded = source::expanded(&records)?;
    let mut w = Writer::new(&ucd.header(
        "Bidi classes, paired brackets, mirrored glyphs, joining types and Arabic \
         presentation forms",
    ));
    w.raw("\nuse crate::unicode::trie::Trie;\n");
    let classes = bidi_class(&mut w, ucd, &expanded)?;
    brackets(&mut w, ucd, &records)?;
    mirrors(&mut w, ucd)?;
    joining(&mut w, ucd, &expanded, &classes)?;
    presentation_forms(&mut w, ucd, &records)?;
    Ok(w.finish())
}

fn class_index(name: &str) -> Result<u16, String> {
    CLASSES
        .iter()
        .position(|&c| c == name)
        .map(|i| i as u16)
        .ok_or_else(|| format!("unknown Bidi_Class {name}"))
}

/// The unassigned code points FriBidi gives R or AL: R in these ranges, except
/// AL in [`AL_DEFAULTS`].
const R_DEFAULTS: [std::ops::RangeInclusive<u32>; 3] =
    [0x0590..=0x08FF, 0xFB1D..=0xFB4F, 0x10800..=0x10FFF];
const AL_DEFAULTS: [std::ops::RangeInclusive<u32>; 4] = [
    0x0600..=0x07BF,
    0xFB50..=0xFDCF,
    0xFDF0..=0xFDFF,
    0xFE70..=0xFEFF,
];

/// DerivedBidiClass.txt lists every code point other than those of its
/// `@missing` default, L, including the unassigned ones with other defaults;
/// for the assigned ones it must agree with UnicodeData.txt. Unassigned code
/// points it gives R, AL or ET take FriBidi's defaults instead.
fn bidi_class(
    w: &mut Writer,
    ucd: &Loaded,
    expanded: &[(std::ops::RangeInclusive<u32>, &source::Record<'_>)],
) -> Result<Vec<u16>, String> {
    let mut values = source::filled(class_index("L")?);
    for f in source::fields(ucd.text("extracted/DerivedBidiClass.txt")) {
        source::set(&mut values, source::range(f[0])?, &class_index(f[1])?);
    }
    let mut assigned = source::filled(false);
    for (range, record) in expanded {
        let class = class_index(record.bidi_class)?;
        if let Some(cp) = range.clone().find(|&cp| values[cp as usize] != class) {
            return Err(format!(
                "U+{cp:04X}: DerivedBidiClass.txt disagrees with UnicodeData.txt"
            ));
        }
        source::set(&mut assigned, range.clone(), &true);
    }
    let strong = [class_index("R")?, class_index("AL")?, class_index("ET")?];
    for (cp, value) in values.iter_mut().enumerate() {
        if assigned[cp] || !strong.contains(value) {
            continue;
        }
        let cp = cp as u32;
        *value = if AL_DEFAULTS.iter().any(|r| r.contains(&cp)) {
            class_index("AL")?
        } else if R_DEFAULTS.iter().any(|r| r.contains(&cp)) {
            class_index("R")?
        } else {
            class_index("L")?
        };
    }
    w.trie(
        "Bidi_Class (extracted/DerivedBidiClass.txt, with FriBidi's defaults for\n\
         unassigned code points in right-to-left blocks), as `BidiClass`\n\
         discriminants.",
        "BIDI_CLASS",
        &pack(&values, class_index("L")?),
    );
    Ok(values)
}

/// Each bracket of BidiBrackets.txt with the identity its pair shares: the
/// opening bracket, or the first character of the opening bracket's
/// decomposition, canonical or compatibility. The top bit marks opening
/// brackets.
fn brackets(w: &mut Writer, ucd: &Loaded, records: &[source::Record<'_>]) -> Result<(), String> {
    let decomposed: BTreeMap<u32, u32> = records
        .iter()
        .filter_map(|r| {
            let first = match r.canonical_decomposition() {
                Ok(Some(mapping)) => mapping.first().copied(),
                Ok(None) => match r.tagged_decomposition() {
                    Ok(Some((_, mapping))) => mapping.first().copied(),
                    _ => None,
                },
                Err(_) => None,
            };
            first.map(|first| (r.code_point, first))
        })
        .collect();
    let mut items = Vec::new();
    for f in source::fields(ucd.text("BidiBrackets.txt")) {
        let cp = source::code_point(f[0])?;
        let paired = source::code_point(f[1])?;
        let (opening, opens) = match f[2] {
            "o" => (cp, true),
            "c" => (paired, false),
            other => return Err(format!("BidiBrackets.txt: unknown bracket type {other}")),
        };
        let identity = decomposed.get(&opening).copied().unwrap_or(opening);
        let flag = if opens { 0x8000_0000_u32 } else { 0 };
        items.push(format!("({}, 0x{:08X})", hex(cp), identity | flag));
    }
    w.array(
        "Paired brackets (BidiBrackets.txt) in code point order: each bracket and its\n\
         pair's identity, with the top bit set for opening brackets.",
        "BRACKETS",
        "(u32, u32)",
        &items,
        8,
    );
    Ok(())
}

/// Bidi_Mirroring_Glyph as indices into a list of the mirrored glyphs, 0 for
/// none.
fn mirrors(w: &mut Writer, ucd: &Loaded) -> Result<(), String> {
    let mut values = source::filled(0u16);
    let mut glyphs = Vec::new();
    for f in source::fields(ucd.text("BidiMirroring.txt")) {
        let cp = source::code_point(f[0])?;
        glyphs.push(hex(source::code_point(f[1])?));
        let index = u16::try_from(glyphs.len()).map_err(|_| "too many mirrored glyphs")?;
        source::set(&mut values, cp..=cp, &index);
    }
    w.trie(
        "Bidi_Mirroring_Glyph (BidiMirroring.txt), as one more than an index into\n\
         [`MIRRORED`], 0 where there is none.",
        "MIRROR",
        &pack(&values, 0),
    );
    w.array(
        "The mirrored glyphs of [`MIRROR`].",
        "MIRRORED",
        "u32",
        &glyphs,
        4,
    );
    Ok(())
}

/// Joining_Type: ArabicShaping.txt's; else T for the marks Mn and Me and the
/// format characters Cf, which ArabicShaping.txt leaves out of its list, G for
/// the other characters of class BN that UnicodeData.txt lists (controls), and
/// U.
fn joining(
    w: &mut Writer,
    ucd: &Loaded,
    expanded: &[(std::ops::RangeInclusive<u32>, &source::Record<'_>)],
    classes: &[u16],
) -> Result<(), String> {
    let index = |name: &str| {
        JOINING
            .iter()
            .position(|&j| j == name)
            .map(|i| i as u16)
            .ok_or_else(|| format!("unknown Joining_Type {name}"))
    };
    let mut values = source::filled(index("U")?);
    let bn = class_index("BN")?;
    for (range, record) in expanded {
        if matches!(record.general_category, "Mn" | "Me" | "Cf") {
            source::set(&mut values, range.clone(), &index("T")?);
        } else if classes[*range.start() as usize] == bn {
            source::set(&mut values, range.clone(), &index("G")?);
        }
    }
    for f in source::fields(ucd.text("ArabicShaping.txt")) {
        source::set(&mut values, source::range(f[0])?, &index(f[2])?);
    }
    w.trie(
        "Joining_Type (ArabicShaping.txt; of the unlisted characters, T for Mn, Me and\n\
         Cf, and G for the other characters of class BN), as `JoiningType`\n\
         discriminants.",
        "JOINING_TYPE",
        &pack(&values, index("U")?),
    );
    Ok(())
}

/// The presentation forms UnicodeData.txt decomposes to one character with a
/// positional tag, by that character, and the LAM-ALEF ligatures, which
/// decompose to LAM and a character of the ALEF joining group
/// (ArabicShaping.txt).
fn presentation_forms(
    w: &mut Writer,
    ucd: &Loaded,
    records: &[source::Record<'_>],
) -> Result<(), String> {
    let alefs: Vec<u32> = source::fields(ucd.text("ArabicShaping.txt"))
        .filter(|f| f.len() > 3 && f[3] == "ALEF")
        .map(|f| source::code_point(f[0]))
        .collect::<Result<_, _>>()?;
    let mut forms: BTreeMap<u32, [u32; 4]> = BTreeMap::new();
    let mut ligatures: BTreeMap<u32, [u32; 2]> = BTreeMap::new();
    for record in records {
        let Some((tag, mapping)) = record.tagged_decomposition()? else {
            continue;
        };
        let Some(column) = FORMS.iter().position(|&f| f == tag) else {
            continue;
        };
        match mapping[..] {
            [base] => forms.entry(base).or_default()[column] = record.code_point,
            [LAM, alef] if matches!(tag, "isolated" | "final") && alefs.contains(&alef) => {
                ligatures.entry(alef).or_default()[usize::from(tag == "final")] = record.code_point;
            }
            _ => {}
        }
    }
    let (&first, _) = forms.first_key_value().ok_or("no presentation forms")?;
    let (&last, _) = forms.last_key_value().ok_or("no presentation forms")?;
    if forms.values().flatten().any(|&f| f > 0xFFFF) {
        return Err("a presentation form beyond U+FFFF".into());
    }
    let items: Vec<String> = (first..=last)
        .map(|cp| {
            let row = forms.get(&cp).copied().unwrap_or_default();
            let row = row.map(|f| if f == 0 { "0".to_owned() } else { hex(f) });
            format!("[{}]", row.join(", "))
        })
        .collect();
    w.raw(&format!(
        "\n/// The first character of [`PRESENTATION_FORMS`].\n\
         pub(super) const PRESENTATION_FIRST: u32 = {};\n",
        hex(first)
    ));
    w.array(
        "The initial, medial, final and isolated presentation forms of each character\n\
         from [`PRESENTATION_FIRST`] on (UnicodeData.txt's `<initial>`, `<medial>`,\n\
         `<final>` and `<isolated>` decompositions to that character), 0 for none.",
        "PRESENTATION_FORMS",
        "[u16; 4]",
        &items,
        8,
    );
    let items: Vec<String> = ligatures
        .iter()
        .map(|(&alef, [isolated, last])| {
            format!("({}, {}, {})", hex(alef), hex(*isolated), hex(*last))
        })
        .collect();
    w.array(
        "The ligatures of LAM with each ALEF (UnicodeData.txt's `<isolated>` and\n\
         `<final>` decompositions to U+0644 and a character of the ALEF joining\n\
         group): the ALEF, the isolated ligature and the final one.",
        "LAM_ALEF",
        "(u32, u32, u32)",
        &items,
        12,
    );
    Ok(())
}
