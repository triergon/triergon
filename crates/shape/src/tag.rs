//! Scripts, languages, directions and OpenType tags.

use std::fmt;

/// A four-byte OpenType tag, such as a feature (`liga`), script (`latn`) or
/// language system (`ENG `) tag. Tags order as their big-endian numbers do.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Tag(u32);

impl Tag {
    /// The tag spelt by `bytes`.
    #[must_use]
    pub const fn new(bytes: &[u8; 4]) -> Self {
        Self(u32::from_be_bytes(*bytes))
    }

    /// The tag whose big-endian number is `value`, as OpenType stores tags and
    /// HarfBuzz's `hb_tag_t` holds them.
    #[must_use]
    pub const fn from_u32(value: u32) -> Self {
        Self(value)
    }

    /// The tag's four bytes.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; 4] {
        self.0.to_be_bytes()
    }

    /// The tag as a big-endian number.
    #[must_use]
    pub const fn to_u32(self) -> u32 {
        self.0
    }
}

impl fmt::Debug for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.to_bytes()))
    }
}

/// An ISO 15924 script code, such as `Latn` or `Arab`, as HarfBuzz names scripts.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Script([u8; 4]);

impl Script {
    /// `Zyyy`: characters shared by several scripts.
    pub const COMMON: Self = Self(*b"Zyyy");
    /// `Zinh`: characters that take the script of the preceding character.
    pub const INHERITED: Self = Self(*b"Zinh");
    /// `Zzzz`: unassigned code points.
    pub const UNKNOWN: Self = Self(*b"Zzzz");

    /// The script whose canonical ISO 15924 code is `code`, unchecked: for
    /// codes already in the canonical form, such as the generated tables'.
    pub(crate) const fn from_code(code: [u8; 4]) -> Self {
        Self(code)
    }

    /// The Script property of `c`, as `hb_unicode_script` gives it: [`COMMON`]
    /// and [`INHERITED`] for characters of several scripts, [`UNKNOWN`] for
    /// unassigned code points.
    ///
    /// [`COMMON`]: Self::COMMON
    /// [`INHERITED`]: Self::INHERITED
    /// [`UNKNOWN`]: Self::UNKNOWN
    #[must_use]
    pub fn of(c: char) -> Self {
        crate::unicode::code_point::script(u32::from(c))
    }

    /// The four letters of the script's ISO 15924 code, such as `*b"Arab"`.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; 4] {
        self.0
    }

    /// The script an ISO 15924 code names, canonicalised as
    /// `hb_script_from_iso15924_tag` does: case is normalised, a few variant codes
    /// map to their script, and anything that is not four letters is `Zzzz`.
    pub fn from_iso15924(code: [u8; 4]) -> Self {
        let tag = u32::from_be_bytes(code);
        if tag == 0 {
            return Self::UNKNOWN;
        }
        let canonical = ((tag & 0xdfdf_dfdf) | 0x0020_2020).to_be_bytes();
        let script = match &canonical {
            b"Aran" => *b"Arab",
            b"Cyrs" => *b"Cyrl",
            b"Geok" => *b"Geor",
            b"Hans" | b"Hant" => *b"Hani",
            b"Jamo" => *b"Hang",
            b"Latf" | b"Latg" => *b"Latn",
            b"Syre" | b"Syrj" | b"Syrn" => *b"Syrc",
            _ if u32::from_be_bytes(canonical) & 0xe0e0_e0e0 == 0x4060_6060 => canonical,
            _ => return Self::UNKNOWN,
        };
        Self(script)
    }

    /// The direction the script is written in horizontally, or `None` for scripts
    /// written in either (Old Hungarian, Old Italic, Runic, Tifinagh).
    pub fn horizontal_direction(self) -> Option<Direction> {
        const RIGHT_TO_LEFT: [&[u8; 4]; 36] = [
            b"Arab", b"Hebr", b"Syrc", b"Thaa", b"Cprt", b"Khar", b"Phnx", b"Nkoo", b"Lydi",
            b"Avst", b"Armi", b"Phli", b"Prti", b"Sarb", b"Orkh", b"Samr", b"Mand", b"Merc",
            b"Mero", b"Mani", b"Mend", b"Nbat", b"Narb", b"Palm", b"Phlp", b"Hatr", b"Adlm",
            b"Rohg", b"Sogo", b"Sogd", b"Elym", b"Chrs", b"Yezi", b"Ougr", b"Gara", b"Sidt",
        ];
        const EITHER: [&[u8; 4]; 4] = [b"Hung", b"Ital", b"Runr", b"Tfng"];
        if RIGHT_TO_LEFT.contains(&&self.0) {
            Some(Direction::RightToLeft)
        } else if EITHER.contains(&&self.0) {
            None
        } else {
            Some(Direction::LeftToRight)
        }
    }

    /// The OpenType script tags to look for, in order of preference, as
    /// `hb_ot_tags_from_script_and_language` lists them: the third and second
    /// Indic tag versions first, then the original tag.
    pub(crate) fn ot_tags(self) -> ([Tag; 3], usize) {
        let mut tags = [Tag::default(); 3];
        let mut count = 0;
        let new = match &self.0 {
            b"Beng" => Some(*b"bng2"),
            b"Deva" => Some(*b"dev2"),
            b"Gujr" => Some(*b"gjr2"),
            b"Guru" => Some(*b"gur2"),
            b"Knda" => Some(*b"knd2"),
            b"Mlym" => Some(*b"mlm2"),
            b"Orya" => Some(*b"ory2"),
            b"Taml" => Some(*b"tml2"),
            b"Telu" => Some(*b"tel2"),
            b"Mymr" => Some(*b"mym2"),
            _ => None,
        };
        if let Some(new) = new {
            if &new != b"mym2" {
                let mut third = new;
                third[3] = b'3';
                tags[count] = Tag::new(&third);
                count += 1;
            }
            tags[count] = Tag::new(&new);
            count += 1;
        }
        let old = match &self.0 {
            b"Hira" | b"Kana" | b"Hrkt" => *b"kana",
            b"Laoo" => *b"lao ",
            b"Yiii" => *b"yi  ",
            b"Nkoo" => *b"nko ",
            b"Vaii" => *b"vai ",
            b"Zmth" => *b"math",
            other => {
                let mut lower = *other;
                lower[0] |= 0x20;
                lower
            }
        };
        tags[count] = Tag::new(&old);
        (tags, count + 1)
    }
}

impl fmt::Debug for Script {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.0))
    }
}

/// The direction a run is shaped in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Left to right, as Latin is written.
    LeftToRight,
    /// Right to left, as Arabic and Hebrew are written.
    RightToLeft,
}

impl Direction {
    /// Whether glyphs come in the reverse of logical order: right to left.
    #[must_use]
    pub const fn is_backward(self) -> bool {
        matches!(self, Self::RightToLeft)
    }

    /// The other direction.
    #[must_use]
    pub const fn reverse(self) -> Self {
        match self {
            Self::LeftToRight => Self::RightToLeft,
            Self::RightToLeft => Self::LeftToRight,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Direction, Script, Tag};

    #[test]
    fn scripts_canonicalise_like_harfbuzz() {
        assert_eq!(Script::from_iso15924(*b"latn"), Script(*b"Latn"));
        assert_eq!(Script::from_iso15924(*b"Syre"), Script(*b"Syrc"));
        assert_eq!(Script::from_iso15924(*b"ab12"), Script::UNKNOWN);
        assert_eq!(Script::from_iso15924(*b"Qaai"), Script(*b"Qaai"));
        assert_eq!(
            Script(*b"Hebr").horizontal_direction(),
            Some(Direction::RightToLeft)
        );
        let (tags, count) = Script(*b"Deva").ot_tags();
        assert_eq!(
            tags[..count],
            [Tag::new(b"dev3"), Tag::new(b"dev2"), Tag::new(b"deva")]
        );
        let (tags, count) = Script(*b"Hira").ot_tags();
        assert_eq!(tags[..count], [Tag::new(b"kana")]);
    }
}
