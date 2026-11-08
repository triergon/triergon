//! The character properties a glyph carries through shaping.

use crate::buffer::{CharFlag, Info, Scratch, UnicodeProps};
use crate::unicode::{GeneralCategory, code_point};

/// Whether HarfBuzz treats `c` as default ignorable: invisible unless the font
/// has a glyph a lookup uses.
pub(crate) fn is_default_ignorable(c: u32) -> bool {
    match c >> 16 {
        0 => match c >> 8 {
            0x00 => c == 0x00AD,
            0x03 => c == 0x034F,
            0x06 => c == 0x061C,
            0x17 => matches!(c, 0x17B4..=0x17B5),
            0x18 => matches!(c, 0x180B..=0x180F),
            0x20 => matches!(c, 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F),
            0xFE => matches!(c, 0xFE00..=0xFE0F | 0xFEFF),
            0xFF => matches!(c, 0xFFF0..=0xFFF8),
            _ => false,
        },
        0x01 => matches!(c, 0x1D173..=0x1D17A),
        0x0E => matches!(c, 0xE0000..=0xE0FFF),
        _ => false,
    }
}

/// HarfBuzz's reordering class for a mark: the canonical combining class, with
/// Hebrew, Arabic, Syriac, Telugu, Thai, Lao and Tibetan classes remapped so that
/// marks sort the way fonts expect.
pub(crate) fn modified_combining_class(c: u32) -> u8 {
    match c {
        0x1A60 | 0x0FC6 => return 254,
        0x0F39 => return 127,
        _ => {}
    }
    match code_point::combining_class(c) {
        10 => 22,
        11 => 15,
        12 => 16,
        13 => 17,
        14 => 23,
        15 => 18,
        16 => 19,
        17 => 20,
        18 => 21,
        19 => 14,
        20 => 24,
        21 => 12,
        22 => 25,
        23 => 13,
        24 => 10,
        25 => 11,
        26 => 26,
        27 => 28,
        28 => 29,
        29 => 30,
        30 => 31,
        31 => 32,
        32 => 33,
        33 => 27,
        34 => 34,
        35 => 35,
        36 => 36,
        84 => 4,
        91 => 5,
        103 => 3,
        130 => 132,
        132 => 131,
        class => class,
    }
}

/// Sets the properties of the character `info.glyph`, noting in `scratch` what
/// later steps must look for.
pub(crate) fn set_unicode_props(info: &mut Info, scratch: &mut Scratch) {
    let c = info.glyph;
    let category = code_point::general_category(c);
    let mut props = UnicodeProps::new(category);
    if c >= 0x80 {
        if is_default_ignorable(c) {
            scratch.default_ignorables = true;
            props.set(CharFlag::Ignorable, true);
            match c {
                0x200C => props.set(CharFlag::Zwnj, true),
                0x200D => props.set(CharFlag::Zwj, true),
                // Mongolian free variation selectors and tags are hidden but take
                // part in shaping.
                0x180B..=0x180D | 0x180F | 0xE0020..=0xE007F => {
                    props.set(CharFlag::Hidden, true);
                }
                0x034F => {
                    scratch.cgj = true;
                    props.set(CharFlag::Hidden, true);
                }
                _ => {}
            }
        }
        if category.is_mark() {
            scratch.continuations = true;
            props.set(CharFlag::Continuation, true);
            props.set_high(modified_combining_class(c));
        }
    }
    info.unicode = props;
}

/// [`set_unicode_props`] for every character, with the grapheme continuations
/// HarfBuzz recognises: emoji modifiers, regional indicator pairs, ZWJ sequences,
/// halfwidth voicing marks and tags.
pub(crate) fn set_all(infos: &mut [Info], scratch: &mut Scratch, global_mask: u32) {
    let mut i = 0;
    while i < infos.len() {
        let info = &mut infos[i];
        info.mask = global_mask;
        set_unicode_props(info, scratch);
        let c = info.glyph;
        if c < 0x80 {
            i += 1;
            continue;
        }
        let category = info.category();
        if matches!(
            category,
            GeneralCategory::LowercaseLetter
                | GeneralCategory::UppercaseLetter
                | GeneralCategory::TitlecaseLetter
                | GeneralCategory::OtherLetter
                | GeneralCategory::SpaceSeparator
        ) {
            i += 1;
            continue;
        }
        let regional = |c: u32| matches!(c, 0x1F1E6..=0x1F1FF);
        if category == GeneralCategory::ModifierSymbol && matches!(c, 0x1F3FB..=0x1F3FF) {
            continuation(&mut infos[i], scratch);
        } else if i > 0 && regional(c) {
            if regional(infos[i - 1].glyph) && !infos[i - 1].is_continuation() {
                continuation(&mut infos[i], scratch);
            }
        } else if infos[i].is_zwj() {
            continuation(&mut infos[i], scratch);
            if i + 1 < infos.len() && code_point::is_extended_pictographic(infos[i + 1].glyph) {
                i += 1;
                infos[i].mask = global_mask;
                set_unicode_props(&mut infos[i], scratch);
                continuation(&mut infos[i], scratch);
            }
        } else if matches!(c, 0xFF9E..=0xFF9F | 0xE0020..=0xE007F) {
            continuation(&mut infos[i], scratch);
        } else if c == 0x2044 {
            scratch.fraction_slash = true;
        }
        i += 1;
    }
}

fn continuation(info: &mut Info, scratch: &mut Scratch) {
    scratch.continuations = true;
    info.unicode.set(CharFlag::Continuation, true);
}
