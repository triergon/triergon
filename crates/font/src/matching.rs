//! Choosing a face for an ASS style's font name, bold and italic values, with
//! libass's scoring.

use crate::name::{NameId, NameRecord, Platform};
use crate::{Font, OutlineFormat};

/// What an ASS style or override asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request<'a> {
    /// The font name, without a leading `@` (vertical) marker.
    pub family: &'a str,
    /// The ASS bold value: 1 or -1 is bold (weight 700), 0 or less regular (400),
    /// and other values are weights.
    pub bold: i32,
    /// The ASS italic value: any positive value asks for an italic face.
    pub italic: i32,
    /// A character the face must map, or `None`.
    pub character: Option<char>,
}

impl Request<'_> {
    /// The requested weight.
    pub const fn weight(&self) -> u32 {
        match self.bold {
            1 | -1 => 700,
            ..=0 => 400,
            weight => weight.unsigned_abs(),
        }
    }

    /// The requested slant as libass passes it on: 100 for the ASS value 1, 0 for
    /// values of 0 or less, and other values unchanged.
    pub const fn slant(&self) -> u32 {
        match self.italic {
            1 => 100,
            ..=0 => 0,
            slant => slant.unsigned_abs(),
        }
    }

    /// Whether an italic face is requested.
    pub const fn italic(&self) -> bool {
        self.italic > 0
    }
}

/// What matching knows about one face.
#[derive(Clone, Copy, Debug)]
pub struct FaceInfo<'a> {
    font: &'a Font,
    weight: u32,
    bold: bool,
    italic: bool,
}

impl<'a> FaceInfo<'a> {
    pub fn new(font: &'a Font) -> Self {
        let freetype = font.style();
        let weight = font.weight();
        // With OS/2, only fsSelection's italic and bold bits count, as GDI reads them.
        let (bold, italic) = match font.os2() {
            Some(os2) => (os2.selection.bold(), os2.selection.italic()),
            None => (freetype.bold, freetype.italic),
        };
        Self {
            font,
            weight,
            bold,
            italic,
        }
    }

    pub const fn font(&self) -> &'a Font {
        self.font
    }

    /// The weight libass assigns: `OS/2.usWeightClass`, with classes 1 to 9 read
    /// as 100 to 900 (4 as 350), and 400 or 700 by FreeType's bold flag when it is
    /// zero or the table is absent.
    pub const fn weight(&self) -> u32 {
        self.weight
    }

    /// The italic flag: `fsSelection` bit 0, or FreeType's flag without `OS/2`.
    pub const fn is_italic(&self) -> bool {
        self.italic
    }

    /// The bold flag: `fsSelection` bit 5, or FreeType's flag without `OS/2`.
    pub const fn is_bold(&self) -> bool {
        self.bold
    }

    /// Whether a Windows-platform family name (ID 1) equals `name` ignoring ASCII
    /// case. A face without any uses FreeType's family name in its ASCII form.
    pub fn has_family(&self, name: &str) -> bool {
        let mut any = false;
        for record in self.windows(NameId::Family) {
            any = true;
            if record.eq_ignore_ascii_case(name) {
                return true;
            }
        }
        !any && self.font.family_name().is_some_and(|r| ascii_eq(r, name))
    }

    /// Whether `name` is the face's full name (Windows ID 4) or PostScript name,
    /// ignoring ASCII case. When exactly one of the two matches, a face with CFF
    /// outlines goes by its PostScript name and any other by its full name.
    pub fn has_full_name(&self, name: &str) -> bool {
        let full = self
            .windows(NameId::FullName)
            .any(|r| r.eq_ignore_ascii_case(name));
        let postscript = self
            .font
            .postscript_name()
            .is_some_and(|r| postscript_eq(r, name));
        if full == postscript {
            full
        } else if self.font.outline_format() == OutlineFormat::Cff {
            postscript
        } else {
            full
        }
    }

    fn windows(&self, name_id: NameId) -> impl Iterator<Item = NameRecord<'a>> + 'a {
        self.font
            .names()
            .filter(move |r| r.platform == Platform::Windows && r.name_id == name_id)
    }

    /// The distance from `request`: 1 for an upright face asked to be italic, 4 for
    /// an italic face asked to be upright, plus 73/256 of the weight difference. A
    /// face not flagged bold counts as 120 heavier when the request is more than 150
    /// heavier, since the renderer emboldens it.
    pub fn score(&self, request: &Request<'_>) -> u32 {
        let slant = match (request.italic(), self.italic) {
            (true, false) => 1,
            (false, true) => 4,
            _ => 0,
        };
        let wanted = request.weight();
        let weight = if wanted > self.weight + 150 && !self.bold {
            self.weight + 120
        } else {
            self.weight
        };
        slant + 73 * wanted.abs_diff(weight) / 256
    }

    /// Whether the face maps `c` in its text charmap, looked up unchanged.
    fn covers(&self, c: Option<char>) -> bool {
        c.is_none_or(|c| {
            self.font
                .text_charmap()
                .is_some_and(|charmap| charmap.glyph(u32::from(c)).is_some())
        })
    }
}

/// The face for `request` among `faces`, by index.
///
/// A face whose family name matches scores [`FaceInfo::score`]; one whose full or
/// PostScript name matches instead scores zero. The lowest score wins, the
/// earlier face on a tie, and a face that does not map `request.character` is
/// passed over. A face scoring zero ends the search.
pub fn select(faces: &[FaceInfo<'_>], request: &Request<'_>) -> Option<usize> {
    let mut best: Option<(u32, usize)> = None;
    for (index, face) in faces.iter().enumerate() {
        let score = if face.has_family(request.family) {
            face.score(request)
        } else if face.has_full_name(request.family) {
            0
        } else {
            continue;
        };
        if best.is_none_or(|(lowest, _)| score < lowest) {
            if !face.covers(request.character) {
                continue;
            }
            best = Some((score, index));
        }
        if score == 0 {
            break;
        }
    }
    best.map(|(_, index)| index)
}

fn same(mut text: impl Iterator<Item = char>, name: &str) -> bool {
    name.chars()
        .all(|c| text.next().is_some_and(|t| t.eq_ignore_ascii_case(&c)))
        && text.next().is_none()
}

/// FreeType's ASCII form of a name compared with `name`: it ends at NUL and turns
/// every unit outside 32..=127 into `?`.
fn ascii_eq(record: NameRecord<'_>, name: &str) -> bool {
    let text = record
        .code_units()
        .map(u32::from)
        .take_while(|&u| u != 0)
        .map(|u| {
            u8::try_from(u)
                .ok()
                .filter(|b| (32..=127).contains(b))
                .map_or('?', char::from)
        });
    same(text, name)
}

/// FreeType's PostScript form, which keeps printable ASCII other than `[](){}<>/%`.
fn postscript_eq(record: NameRecord<'_>, name: &str) -> bool {
    let text = record
        .code_units()
        .map(u32::from)
        .filter_map(|u| u8::try_from(u).ok())
        .filter(|&b| (33..=126).contains(&b) && !b"[](){}<>/%".contains(&b))
        .map(char::from);
    same(text, name)
}

#[cfg(test)]
mod tests {
    use super::Request;

    #[test]
    fn requests_map_ass_values() {
        let request = |bold, italic| Request {
            family: "Arimo",
            bold,
            italic,
            character: None,
        };
        assert_eq!(request(1, 0).weight(), 700);
        assert_eq!(request(-1, 0).weight(), 700);
        assert_eq!(request(0, 0).weight(), 400);
        assert_eq!(request(-5, 0).weight(), 400);
        assert_eq!(request(300, 0).weight(), 300);
        assert!(request(0, 1).italic() && request(0, 7).italic());
        assert_eq!((request(0, 1).slant(), request(0, 7).slant(), request(0, -3).slant()), (100, 7, 0));
        assert!(!request(0, -1).italic() && !request(0, 0).italic());
    }
}
