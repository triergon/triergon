//! Choosing a face for an ASS style's font name, bold and italic values, with
//! libass's scoring.

use crate::Font;
use crate::name::{NameRecord, id, platform};

/// What an ASS style or override asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request<'a> {
    /// The font name, without a leading `@` (vertical) marker.
    pub family: &'a str,
    /// The ASS bold value: 1 or -1 is bold (weight 700), 0 or less regular (400),
    /// and other values are weights.
    pub bold: i32,
    /// The ASS italic value: 1 is italic, 0 or less upright; other positive values
    /// are also treated as italic.
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

    /// The requested slant: 110 for italic, else the value, never negative.
    pub const fn slant(&self) -> u32 {
        match self.italic {
            1 => 110,
            ..=0 => 0,
            slant => slant.unsigned_abs(),
        }
    }
}

/// What matching knows about one face.
#[derive(Clone, Copy, Debug)]
pub struct FaceInfo<'a> {
    font: &'a Font,
    weight: u32,
    slant: u32,
}

impl<'a> FaceInfo<'a> {
    pub fn new(font: &'a Font) -> Self {
        let style = font.style();
        let weight = match font.os2().map_or(0, |o| o.weight_class) {
            0 if style.bold => 700,
            0 => 400,
            1 => 100,
            2 => 200,
            3 => 300,
            4 => 350,
            5 => 400,
            6 => 600,
            7 => 700,
            8 => 800,
            9 => 900,
            weight => u32::from(weight),
        };
        Self {
            font,
            weight,
            slant: if style.italic { 110 } else { 0 },
        }
    }

    pub const fn font(&self) -> &'a Font {
        self.font
    }

    /// The weight libass assigns: `OS/2.usWeightClass`, with classes 1 to 9 read
    /// as 100 to 900 (4 as 350), and 400 or 700 by the bold flag when it is zero.
    pub const fn weight(&self) -> u32 {
        self.weight
    }

    /// 110 when the face's style is italic, else 0.
    pub const fn slant(&self) -> u32 {
        self.slant
    }

    /// Whether a Windows-platform family name (ID 1) equals `name` ignoring ASCII
    /// case. A face without any uses FreeType's family name in its ASCII form.
    pub fn has_family(&self, name: &str) -> bool {
        let mut any = false;
        for record in self.windows(id::FAMILY) {
            any = true;
            if record.eq_ignore_ascii_case(name) {
                return true;
            }
        }
        !any && self.font.family_name().is_some_and(|r| ascii_eq(r, name))
    }

    /// Whether a Windows-platform full name (ID 4) or the PostScript name equals
    /// `name` ignoring ASCII case.
    pub fn has_full_name(&self, name: &str) -> bool {
        self.windows(id::FULL_NAME)
            .any(|r| r.eq_ignore_ascii_case(name))
            || self
                .font
                .postscript_name()
                .is_some_and(|r| postscript_eq(r, name))
    }

    fn windows(&self, name_id: u16) -> impl Iterator<Item = NameRecord<'a>> + 'a {
        self.font
            .names()
            .filter(move |r| r.platform == platform::WINDOWS && r.name_id == name_id)
    }

    /// The distance from `request`: 1 for an upright face asked to be italic, 4 for
    /// an italic face asked to be upright, plus 73/256 of the weight difference.
    pub fn score(&self, request: &Request<'_>) -> u32 {
        let (want, have) = (request.slant(), self.slant);
        let slant = if want > 0 && have == 0 {
            1
        } else if want == 0 && have > 0 {
            4
        } else {
            0
        };
        slant + 73 * request.weight().abs_diff(self.weight) / 256
    }
}

/// The face for `request` among `faces`, by index.
///
/// A face whose family name matches scores [`FaceInfo::score`]; one whose full or
/// PostScript name matches instead scores zero. The lowest score wins, the
/// earlier face on a tie, and a face that lacks `request.character` is passed
/// over. A score of zero ends the search.
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
            let covered = request
                .character
                .is_none_or(|c| face.font.glyph_index(c).is_some());
            if !covered {
                continue;
            }
            best = Some((score, index));
        }
        if best.is_some_and(|(lowest, _)| lowest == 0) {
            break;
        }
    }
    best.map(|(_, index)| index)
}

/// A record's code units: bytes for Macintosh records, UTF-16 units otherwise.
fn units<'a>(record: &NameRecord<'a>) -> impl Iterator<Item = u32> + 'a {
    let width = if record.platform == platform::MACINTOSH {
        1
    } else {
        2
    };
    record.bytes.chunks_exact(width).map(move |u| {
        if width == 1 {
            u32::from(u[0])
        } else {
            u32::from(u16::from_be_bytes([u[0], u[1]]))
        }
    })
}

fn same(mut text: impl Iterator<Item = char>, name: &str) -> bool {
    name.chars()
        .all(|c| text.next().is_some_and(|t| t.eq_ignore_ascii_case(&c)))
        && text.next().is_none()
}

/// FreeType's ASCII form of a name compared with `name`: it ends at NUL and turns
/// every unit outside 32..=127 into `?`.
fn ascii_eq(record: NameRecord<'_>, name: &str) -> bool {
    let text = units(&record).take_while(|&u| u != 0).map(|u| {
        u8::try_from(u)
            .ok()
            .filter(|b| (32..=127).contains(b))
            .map_or('?', char::from)
    });
    same(text, name)
}

/// FreeType's PostScript form, which keeps printable ASCII other than `[](){}<>/%`.
fn postscript_eq(record: NameRecord<'_>, name: &str) -> bool {
    let text = units(&record)
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
        assert_eq!(request(0, 1).slant(), 110);
        assert_eq!(request(0, -1).slant(), 0);
        assert_eq!(request(0, 7).slant(), 7);
    }
}
