//! The tables HarfBuzz 14.6.0 compiles, by the rules of its generators
//! (`gen-ucd-table.py`, `gen-emoji-table.py` and `gen-arabic-table.py`, read as
//! evidence of what they produce).

use std::collections::{BTreeMap, HashMap};

use super::emit::{Writer, hex};
use super::pack::pack;
use super::source::{self, Record};
use super::{Generated, Loaded};

/// General_Category values in the order of `hb_unicode_general_category_t`, which is
/// also the order of `GeneralCategory`.
const GENERAL_CATEGORIES: [&str; 30] = [
    "Cc", "Cf", "Cn", "Co", "Cs", "Ll", "Lm", "Lo", "Lt", "Lu", "Mc", "Me", "Mn", "Nd", "Nl", "No",
    "Pc", "Pd", "Pe", "Pf", "Pi", "Po", "Ps", "Sc", "Sk", "Sm", "So", "Zl", "Zp", "Zs",
];

/// Hangul syllables decompose and compose algorithmically.
const HANGUL_SYLLABLES: std::ops::Range<u32> = 0xAC00..0xAC00 + 11172;

/// The ligatures `gen-arabic-table.py` keeps for fallback shaping.
const LIGATURES: [u32; 67] = [
    0xF2EE, 0xFC08, 0xFC0E, 0xFC12, 0xFC32, 0xFC3F, 0xFC40, 0xFC41, 0xFC42, 0xFC43, 0xFC44, 0xFC4E,
    0xFC5E, 0xFC60, 0xFC61, 0xFC62, 0xFC6A, 0xFC6D, 0xFC6F, 0xFC70, 0xFC73, 0xFC75, 0xFC86, 0xFC8F,
    0xFC91, 0xFC94, 0xFC9C, 0xFC9D, 0xFC9E, 0xFC9F, 0xFCA1, 0xFCA2, 0xFCA3, 0xFCA4, 0xFCA8, 0xFCAA,
    0xFCAC, 0xFCB0, 0xFCC9, 0xFCCA, 0xFCCB, 0xFCCC, 0xFCCD, 0xFCCE, 0xFCCF, 0xFCD0, 0xFCD1, 0xFCD2,
    0xFCD3, 0xFCD5, 0xFCDA, 0xFCDB, 0xFCDC, 0xFCDD, 0xFD30, 0xFD88, 0xFEF5, 0xFEF6, 0xFEF7, 0xFEF8,
    0xFEF9, 0xFEFA, 0xFEFB, 0xFEFC, 0xF201, 0xF211, 0xF2EE,
];

/// Private-use ligatures `gen-arabic-table.py` adds to UnicodeData.txt.
const PUA_LIGATURES: &str = "\
F201;PUA ARABIC LIGATURE LELLAH ISOLATED FORM;Lo;0;AL;<isolated> 0644 0644 0647;;;;N;;;;;
F211;PUA ARABIC LIGATURE LAM WITH MEEM WITH JEEM INITIAL FORM;Lo;0;AL;<initial> 0644 0645 062C;;;;N;;;;;
F2EE;PUA ARABIC LIGATURE SHADDA WITH FATHATAN ISOLATED FORM;Lo;0;AL;<isolated> 0020 064B 0651;;;;N;;;;;";

/// Presentation forms, in the column order of the generated shaping table (the
/// order of `ArabicForm`).
const FORMS: [&str; 4] = ["initial", "medial", "final", "isolated"];

pub fn generate(ucd: &Loaded) -> Result<Generated, String> {
    let records = source::unicode_data(ucd.text("UnicodeData.txt"))?;
    let expanded = source::expanded(&records)?;
    let mut w = Writer::new(&ucd.header(
        "The Unicode properties of HarfBuzz 14.6.0: its default (UCD) Unicode functions, \
         emoji and Arabic shaping tables",
    ));
    w.raw(
        "\nuse super::trie::Trie;\n\
         use super::{ArabicJoining, ArabicLigature, ArabicLigatureSet};\n\
         use crate::Script;\n",
    );
    general_category(&mut w, &expanded)?;
    combining_class(&mut w, &expanded);
    script(&mut w, ucd)?;
    mirroring(&mut w, ucd)?;
    normalization(&mut w, ucd, &records)?;
    extended_pictographic(&mut w, ucd)?;
    arabic_joining(&mut w, ucd)?;
    arabic_shaping(&mut w, &records)?;
    Ok(w.finish())
}

fn general_category(
    w: &mut Writer,
    expanded: &[(std::ops::RangeInclusive<u32>, &Record<'_>)],
) -> Result<(), String> {
    let index = |gc: &str| {
        GENERAL_CATEGORIES
            .iter()
            .position(|&name| name == gc)
            .map(|i| i as u16)
            .ok_or_else(|| format!("unknown General_Category {gc}"))
    };
    let mut values = source::filled(index("Cn")?);
    for (range, record) in expanded {
        source::set(&mut values, range.clone(), &index(record.general_category)?);
    }
    w.trie(
        "General_Category (UnicodeData.txt; unlisted code points are Cn), as\n\
         `GeneralCategory` discriminants.",
        "GENERAL_CATEGORY",
        &pack(&values, index("Cn")?),
    );
    Ok(())
}

fn combining_class(w: &mut Writer, expanded: &[(std::ops::RangeInclusive<u32>, &Record<'_>)]) {
    let mut values = source::filled(0u16);
    for (range, record) in expanded {
        source::set(
            &mut values,
            range.clone(),
            &u16::from(record.combining_class),
        );
    }
    w.trie(
        "Canonical_Combining_Class (UnicodeData.txt).",
        "COMBINING_CLASS",
        &pack(&values, 0),
    );
}

fn script(w: &mut Writer, ucd: &Loaded) -> Result<(), String> {
    let short: HashMap<&str, &str> = source::fields(ucd.text("PropertyValueAliases.txt"))
        .filter(|f| f.len() >= 3 && f[0] == "sc")
        .map(|f| (f[2], f[1]))
        .collect();
    let mut tags = source::filled("Zzzz");
    for f in source::fields(ucd.text("Scripts.txt")) {
        let tag = short
            .get(f[1])
            .ok_or_else(|| format!("Scripts.txt: unknown script {}", f[1]))?;
        source::set(&mut tags, source::range(f[0])?, tag);
    }
    let mut palette: Vec<&str> = tags.clone();
    palette.sort_unstable();
    palette.dedup();
    let index: HashMap<&str, u16> = palette
        .iter()
        .enumerate()
        .map(|(i, &t)| (t, i as u16))
        .collect();
    let values: Vec<u16> = tags.iter().map(|t| index[t]).collect();
    w.trie(
        "Script (Scripts.txt; unlisted code points are Zzzz), as indices into [`SCRIPTS`].",
        "SCRIPT",
        &pack(&values, index["Zzzz"]),
    );
    let items: Vec<String> = palette
        .iter()
        .map(|t| format!("Script::from_code(*b\"{t}\")"))
        .collect();
    w.array(
        "The values of [`SCRIPT`]: ISO 15924 codes.",
        "SCRIPTS",
        "Script",
        &items,
        4,
    );
    Ok(())
}

/// A property given as signed offsets from each code point, stored as indices into
/// a list of the distinct offsets with 0 first.
fn deltas(w: &mut Writer, doc: &str, name: &str, offsets: &[i32]) {
    let mut palette = vec![0];
    palette.extend(offsets.iter().copied().filter(|&d| d != 0));
    palette[1..].sort_unstable();
    palette.dedup();
    let values: Vec<u16> = offsets
        .iter()
        .map(|d| palette.iter().position(|p| p == d).expect("listed") as u16)
        .collect();
    w.trie(
        &format!("{doc}, as indices into [`{name}_DELTAS`]."),
        name,
        &pack(&values, 0),
    );
    let items: Vec<String> = palette.iter().map(i32::to_string).collect();
    w.array(
        &format!("The offsets of [`{name}`]; 0 first."),
        &format!("{name}_DELTAS"),
        "i32",
        &items,
        4,
    );
}

fn mirroring(w: &mut Writer, ucd: &Loaded) -> Result<(), String> {
    let mut offsets = source::filled(0i32);
    for f in source::fields(ucd.text("BidiMirroring.txt")) {
        let (cp, mirror) = (source::code_point(f[0])?, source::code_point(f[1])?);
        offsets[cp as usize] = mirror as i32 - cp as i32;
    }
    deltas(
        w,
        "Bidi_Mirroring_Glyph (BidiMirroring.txt) as an offset",
        "MIRRORING",
        &offsets,
    );
    Ok(())
}

/// Canonical decompositions and the primary compositions HarfBuzz finds.
fn normalization(w: &mut Writer, ucd: &Loaded, records: &[Record<'_>]) -> Result<(), String> {
    let mut excluded = source::filled(false);
    for f in source::fields(ucd.text("DerivedNormalizationProps.txt")) {
        if f.get(1) == Some(&"Full_Composition_Exclusion") {
            source::set(&mut excluded, source::range(f[0])?, &true);
        }
    }
    let mut singles = BTreeMap::new();
    let mut pairs = BTreeMap::new();
    let mut compositions = BTreeMap::new();
    for record in records {
        let cp = record.code_point;
        let Some(mapping) = record.canonical_decomposition()? else {
            continue;
        };
        if HANGUL_SYLLABLES.contains(&cp) {
            continue;
        }
        match mapping[..] {
            [a] => {
                singles.insert(cp, a);
            }
            [a, b] => {
                pairs.insert(cp, (a, b));
                if let Some(composite) =
                    composition(cp, a, b, excluded[cp as usize], record.combining_class)
                    && compositions.insert((a, b), composite).is_some()
                {
                    return Err(format!("two compositions of U+{a:04X} U+{b:04X}"));
                }
            }
            _ => {
                return Err(format!(
                    "U+{cp:04X}: decomposition of {} characters",
                    mapping.len()
                ));
            }
        }
    }
    let mut single_list: Vec<u32> = singles.values().copied().collect();
    single_list.sort_unstable();
    single_list.dedup();
    let mut pair_list: Vec<(u32, u32)> = pairs.values().copied().collect();
    pair_list.sort_unstable();
    pair_list.dedup();
    let mut values = source::filled(0u16);
    for (&cp, a) in &singles {
        values[cp as usize] = 1 + single_list.binary_search(a).expect("listed") as u16;
    }
    for (&cp, pair) in &pairs {
        values[cp as usize] =
            (1 + single_list.len() + pair_list.binary_search(pair).expect("listed")) as u16;
    }
    w.trie(
        "Canonical decompositions (UnicodeData.txt) other than Hangul syllables: 0 for none,\n\
         `1 + i` for [`SINGLE_DECOMPOSITIONS`]`[i]`, then `1 + SINGLE_DECOMPOSITIONS.len() + i`\n\
         for [`PAIR_DECOMPOSITIONS`]`[i]`.",
        "DECOMPOSITION",
        &pack(&values, 0),
    );
    let items: Vec<String> = single_list.iter().map(|&a| hex(a)).collect();
    w.array(
        "Decompositions to one character.",
        "SINGLE_DECOMPOSITIONS",
        "u32",
        &items,
        4,
    );
    let items: Vec<String> = pair_list
        .iter()
        .map(|&(a, b)| format!("({}, {})", hex(a), hex(b)))
        .collect();
    w.array(
        "Decompositions to two characters.",
        "PAIR_DECOMPOSITIONS",
        "(u32, u32)",
        &items,
        8,
    );
    let items: Vec<String> = compositions
        .iter()
        .map(|(&(a, b), &c)| format!("({}, {}, {})", hex(a), hex(b), hex(c)))
        .collect();
    w.array(
        "`(a, b, composite)`, sorted by `(a, b)`: the pairs that compose, other than Hangul.",
        "COMPOSITIONS",
        "(u32, u32, u32)",
        &items,
        12,
    );
    Ok(())
}

/// What HarfBuzz's composition of `a` and `b` finds for `cp`, which decomposes to
/// them. Characters excluded from composition and those with a nonzero combining
/// class store no composite. A pair of `a < U+0800` and `b` in U+0300..U+037F is
/// searched only in HarfBuzz's 32-bit list, which keeps 14 bits of the composite;
/// an entry whose composite does not fit that list is not found.
fn composition(cp: u32, a: u32, b: u32, excluded: bool, combining_class: u8) -> Option<u32> {
    let composite = if excluded || combining_class != 0 {
        0
    } else {
        cp
    };
    let small_pair = a & 0xFFFF_F800 == 0 && b & 0xFFFF_FF80 == 0x0300;
    let composite = match (small_pair, composite & 0xFFF0_C000 == 0) {
        (true, true) => composite & 0x3FFF,
        (true, false) => 0,
        (false, _) => composite,
    };
    (composite != 0).then_some(composite)
}

fn extended_pictographic(w: &mut Writer, ucd: &Loaded) -> Result<(), String> {
    let mut values = source::filled(0u16);
    for f in source::fields(ucd.text("emoji/emoji-data.txt")) {
        if f.get(1) == Some(&"Extended_Pictographic") {
            source::set(&mut values, source::range(f[0])?, &1);
        }
    }
    w.trie(
        "Extended_Pictographic (emoji-data.txt): 1 or 0.",
        "EXTENDED_PICTOGRAPHIC",
        &pack(&values, 0),
    );
    Ok(())
}

/// ArabicShaping.txt's joining types, with the Syriac ALAPH and DALATH RISH groups
/// distinguished and join-causing taken as dual-joining.
fn arabic_joining(w: &mut Writer, ucd: &Loaded) -> Result<(), String> {
    const JOININGS: [&str; 8] = [
        "None",
        "Some(ArabicJoining::NonJoining)",
        "Some(ArabicJoining::LeftJoining)",
        "Some(ArabicJoining::RightJoining)",
        "Some(ArabicJoining::DualJoining)",
        "Some(ArabicJoining::Alaph)",
        "Some(ArabicJoining::DalathRish)",
        "Some(ArabicJoining::Transparent)",
    ];
    let mut values = source::filled(0u16);
    for f in source::fields(ucd.text("ArabicShaping.txt")) {
        let value = match (f[3], f[2]) {
            ("ALAPH", _) => 5,
            ("DALATH RISH", _) => 6,
            (_, "U") => 1,
            (_, "L") => 2,
            (_, "R") => 3,
            (_, "D" | "C") => 4,
            (_, "T") => 7,
            (_, other) => return Err(format!("ArabicShaping.txt: joining type {other}")),
        };
        source::set(&mut values, source::range(f[0])?, &value);
    }
    w.trie(
        "The joining types of ArabicShaping.txt, as indices into [`ARABIC_JOININGS`].",
        "ARABIC_JOINING",
        &pack(&values, 0),
    );
    let items: Vec<String> = JOININGS.iter().map(|&j| j.to_owned()).collect();
    w.array(
        "The values of [`ARABIC_JOINING`]; `None` for characters not listed.",
        "ARABIC_JOININGS",
        "Option<ArabicJoining>",
        &items,
        1,
    );
    Ok(())
}

/// A ligature's components, the form each takes, and its presentation form.
type Ligatures = Vec<(Vec<u32>, Vec<(Option<usize>, u32)>)>;

/// The presentation forms of single characters and the fallback ligatures, as
/// `gen-arabic-table.py` builds them from compatibility decompositions.
fn arabic_shaping(w: &mut Writer, records: &[Record<'_>]) -> Result<(), String> {
    let extra = source::unicode_data(PUA_LIGATURES)?;
    let mut shapes: BTreeMap<u32, [u32; 4]> = BTreeMap::new();
    // In order of first occurrence, as the generator's dictionaries keep them.
    let mut ligatures: Ligatures = Vec::new();
    for record in records.iter().chain(&extra) {
        let Some((tag, mut items)) = record.tagged_decomposition()? else {
            continue;
        };
        let Some(form) = FORMS.iter().position(|&f| f == tag) else {
            continue;
        };
        let cp = record.code_point;
        if let [single] = items[..] {
            shapes.entry(single).or_default()[form] = cp;
            continue;
        }
        let mut mark = false;
        if items[0] == 0x20 {
            // A mark ligature: a space, then the marks in visual order.
            items = items[1..].iter().rev().copied().collect();
            mark = true;
        }
        if !LIGATURES.contains(&cp) {
            continue;
        }
        let shape = (!mark).then_some(form);
        let position = ligatures.iter().position(|(key, _)| *key == items);
        let index = position.unwrap_or_else(|| {
            ligatures.push((items, Vec::new()));
            ligatures.len() - 1
        });
        let entry = &mut ligatures[index].1;
        match entry.iter_mut().find(|(s, _)| *s == shape) {
            Some(slot) => slot.1 = cp,
            None => entry.push((shape, cp)),
        }
    }

    let first = *shapes.keys().next().ok_or("no Arabic presentation forms")?;
    let last = *shapes.keys().next_back().expect("nonempty");
    w.raw(&format!(
        "\n/// The first character of [`ARABIC_SHAPING`].\npub(super) const ARABIC_SHAPING_FIRST: u32 = {};\n",
        hex(first)
    ));
    let rows: Vec<String> = (first..=last)
        .map(|u| {
            let forms = shapes.get(&u).copied().unwrap_or_default();
            format!("[{}]", forms.map(hex).join(", "))
        })
        .collect();
    w.array(
        "The presentation forms of the characters from [`ARABIC_SHAPING_FIRST`] on, in\n\
         `ArabicForm` order (initial, medial, final, isolated); 0 for none.",
        "ARABIC_SHAPING",
        "[u16; 4]",
        &rows,
        8,
    );

    let form = |cp: u32, form: usize| {
        shapes
            .get(&cp)
            .map(|forms| forms[form])
            .filter(|&f| f != 0)
            .ok_or_else(|| format!("U+{cp:04X} has no {} form", FORMS[form]))
    };
    // An isolated or initial ligature starts with an initial letter, an isolated
    // or final one ends with a final letter; every other letter is medial.
    let (initial, medial, final_, isolated) = (0, 1, 2, 3);
    let mut pairs: BTreeMap<u32, Vec<(Vec<u32>, u32)>> = BTreeMap::new();
    let mut marks: BTreeMap<u32, Vec<(Vec<u32>, u32)>> = BTreeMap::new();
    let mut triples: BTreeMap<u32, Vec<(Vec<u32>, u32)>> = BTreeMap::new();
    for (key, entries) in &ligatures {
        for &(shape, ligature) in entries {
            let (table, components) = match (key.len(), shape) {
                (2, None) => (&mut marks, key.clone()),
                (2 | 3, Some(shape)) => {
                    let last = key.len() - 1;
                    let components = key
                        .iter()
                        .enumerate()
                        .map(|(i, &letter)| {
                            let wanted = if i == 0 && (shape == isolated || shape == initial) {
                                initial
                            } else if i == last && (shape == isolated || shape == final_) {
                                final_
                            } else {
                                medial
                            };
                            form(letter, wanted)
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    (
                        if key.len() == 2 {
                            &mut pairs
                        } else {
                            &mut triples
                        },
                        components,
                    )
                }
                _ => return Err(format!("unexpected ligature U+{ligature:04X}")),
            };
            table
                .entry(components[0])
                .or_default()
                .push((components[1..].to_vec(), ligature));
        }
    }
    let sets = [
        (
            "ARABIC_LIGATURES",
            1,
            "The two-letter ligatures, by the forms of their letters.",
            &pairs,
        ),
        (
            "ARABIC_MARK_LIGATURES",
            1,
            "The ligatures of two marks.",
            &marks,
        ),
        (
            "ARABIC_LIGATURES_3",
            2,
            "The three-letter ligatures, by the forms of their letters.",
            &triples,
        ),
    ];
    for (name, rest, doc, table) in sets {
        let items: Vec<String> = table
            .iter()
            .map(|(&first, ligatures)| {
                let entries: Vec<String> = ligatures
                    .iter()
                    .map(|(components, ligature)| {
                        let components: Vec<String> = components.iter().map(|&c| hex(c)).collect();
                        format!(
                            "ArabicLigature {{ components: [{}], ligature: {} }}",
                            components.join(", "),
                            hex(*ligature)
                        )
                    })
                    .collect();
                format!(
                    "ArabicLigatureSet {{ first: {}, ligatures: &[{}] }}",
                    hex(first),
                    entries.join(", ")
                )
            })
            .collect();
        let entries: usize = table.values().map(Vec::len).sum();
        w.array(
            &format!("{doc}\nSorted by first component, each set in HarfBuzz's order."),
            name,
            &format!("ArabicLigatureSet<{rest}>"),
            &items,
            24,
        );
        w.add_data(entries * 4 * (rest + 1));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composition_follows_harfbuzz_storage() {
        // U+00C0 = A + grave: stored in the 32-bit list.
        assert_eq!(composition(0xC0, 0x41, 0x300, false, 0), Some(0xC0));
        // Excluded and non-starter decompositions do not compose.
        assert_eq!(composition(0x958, 0x915, 0x93C, true, 0), None);
        assert_eq!(composition(0x344, 0x308, 0x301, true, 230), None);
        // A composite of 14 bits or more with a small pair is not found...
        assert_eq!(composition(0x4000, 0x41, 0x300, false, 0), None);
        // ...and one with only bits 16..20 set beyond 14 loses them.
        assert_eq!(composition(0x1_0041, 0x41, 0x300, false, 0), Some(0x41));
        // Other pairs keep the full composite.
        assert_eq!(
            composition(0x1_109A, 0x1_1099, 0x1_10BA, false, 0),
            Some(0x1_109A)
        );
    }
}
