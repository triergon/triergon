//! Font selection and fallback as libass does it without a system font
//! provider: the attached and embedded fonts are matched by name, weight and
//! slant (`ass_font_select`), then the default family, then the default font
//! file; each requested font keeps the faces it was given and adds one for a
//! character none of them maps (`ass_font_get_index`).

use std::collections::HashMap;
use std::sync::Arc;

use triergon_core::{Budget, BudgetVec, Bytes, Reservation, Result};
use triergon_font::{FaceInfo, Font, GlyphId, Limits, Request, select};

use crate::{EmbeddedFont, FontDesc};

/// Faces libass keeps per requested font at most (`ASS_FONT_MAX_FACES`).
const MAX_FACES: usize = 10;

/// The fonts a renderer draws with, as a player configures libass without a
/// system font provider.
#[derive(Clone, Debug, Default)]
pub struct Fonts {
    /// Font files the player attaches, such as a Matroska file's attachments
    /// (`ass_add_font`), in order. Every face of each is matched by its names,
    /// before the fonts a script embeds.
    pub attached: Vec<Bytes>,
    /// The family tried when the requested one has no face for a character
    /// (`ass_set_fonts`' default family).
    pub default_family: Option<Vec<u8>>,
    /// The font file whose first face is used when neither the requested nor
    /// the default family has one for a character (`ass_set_fonts`' default
    /// font).
    pub default_font: Option<Bytes>,
}

/// A face a requested font draws with, and libass's identity of it: a font
/// holds each face once.
#[derive(Clone, Debug)]
pub(super) struct Face {
    pub(super) font: Arc<Font>,
    pub(super) uid: u32,
}

/// One requested font (libass's `ASS_Font`): its description and the faces
/// it has gathered, first the one selected for it, then one for each
/// character none of the earlier ones mapped.
#[derive(Debug)]
pub(super) struct RequestedFont {
    key: Key,
    faces: BudgetVec<Face>,
}

impl RequestedFont {
    pub(super) fn faces(&self) -> &[Face] {
        self.faces.as_slice()
    }

    pub(super) fn weight(&self) -> u32 {
        self.key.weight
    }

    pub(super) fn slant(&self) -> u32 {
        self.key.slant
    }

    pub(super) fn vertical(&self) -> bool {
        self.key.vertical
    }

    pub(super) fn family(&self) -> &[u8] {
        &self.key.family
    }
}

/// A requested font, by its index in the [`Library`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct FontId(usize);

/// libass's font description (`ASS_FontDesc`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    family: Box<[u8]>,
    weight: u32,
    slant: u32,
    vertical: bool,
}

/// What a selection picked: a registered face or the default font file.
#[derive(Clone, Copy, Debug)]
enum Choice {
    Registered(usize),
    Default,
}

/// The default font file, opened when first needed.
#[derive(Debug)]
enum DefaultFont {
    Unopened(Bytes),
    Opened(Face),
    None,
}

/// Bytes charged for each requested font and registered face beyond the
/// storage that charges itself: the map slot, key and handle.
const ENTRY_BYTES: usize = 128;

/// The faces fonts are selected from and the fonts requested so far. Like
/// libass's font cache it is never trimmed: the faces a font gathered decide
/// later lookups.
#[derive(Debug)]
pub(super) struct Library {
    budget: Budget,
    limits: Limits,
    faces: BudgetVec<Face>,
    next_uid: u32,
    /// How many of a script's embedded fonts are registered.
    embedded: usize,
    default_family: Option<Box<[u8]>>,
    default_font: DefaultFont,
    fonts: BudgetVec<RequestedFont>,
    by_key: HashMap<Key, Option<FontId>>,
    charge: Reservation,
}

impl Library {
    /// Registers every face of every attached font, in order.
    pub(super) fn new(budget: &Budget, fonts: &Fonts) -> Result<Self> {
        let mut library = Self {
            budget: budget.clone(),
            limits: Limits::default(),
            faces: BudgetVec::new(budget),
            next_uid: 1,
            embedded: 0,
            default_family: fonts.default_family.as_deref().map(Box::from),
            default_font: fonts
                .default_font
                .clone()
                .map_or(DefaultFont::None, DefaultFont::Unopened),
            fonts: BudgetVec::new(budget),
            by_key: HashMap::new(),
            charge: Reservation::empty(budget),
        };
        for file in &fonts.attached {
            library.register(file)?;
        }
        Ok(library)
    }

    /// Registers a script's embedded fonts that are not yet, as libass does
    /// at the start of each frame.
    pub(super) fn register_embedded(&mut self, fonts: &[EmbeddedFont]) -> Result<()> {
        for font in fonts.get(self.embedded..).unwrap_or_default() {
            self.register(&font.data)?;
        }
        self.embedded = self.embedded.max(fonts.len());
        Ok(())
    }

    /// Every face of `file` that opens and has a family name.
    fn register(&mut self, file: &Bytes) -> Result<()> {
        let count = Font::face_count(file).unwrap_or(1);
        for index in 0..count {
            let Ok(font) = Font::new(file.clone(), index, &self.budget, self.limits) else {
                continue;
            };
            if !FaceInfo::new(&font).has_family_name() {
                continue;
            }
            let face = Face {
                font: Arc::new(font),
                uid: self.next_uid(),
            };
            self.faces.try_push(face).map_err(|e| e.error)?;
            self.charge_entries()?;
        }
        Ok(())
    }

    fn next_uid(&mut self) -> u32 {
        let uid = self.next_uid;
        self.next_uid = self.next_uid.wrapping_add(1);
        uid
    }

    fn charge_entries(&mut self) -> Result<()> {
        let entries = self.faces.len() + self.by_key.len();
        self.charge.resize_to(entries.saturating_mul(ENTRY_BYTES))
    }

    /// The requested font for `desc` (`ass_font_new`): built and given its
    /// first face on first request; `None` when no face can be found for it,
    /// which makes an event that uses it fail.
    pub(super) fn font(&mut self, desc: &FontDesc<'_>) -> Result<Option<FontId>> {
        let key = Key {
            family: desc.family.into(),
            weight: desc.weight,
            slant: desc.slant,
            vertical: desc.vertical,
        };
        if let Some(&id) = self.by_key.get(&key) {
            return Ok(id);
        }
        let mut font = RequestedFont {
            key: key.clone(),
            faces: BudgetVec::new(&self.budget),
        };
        let id = match self.choose(&key, 0)? {
            Some(face) => {
                font.faces.try_push(face).map_err(|e| e.error)?;
                let id = FontId(self.fonts.len());
                self.fonts.try_push(font).map_err(|e| e.error)?;
                Some(id)
            }
            None => None,
        };
        self.by_key.insert(key, id);
        self.charge_entries()?;
        Ok(id)
    }

    pub(super) fn get(&self, id: FontId) -> &RequestedFont {
        &self.fonts.as_slice()[id.0]
    }

    /// The face and glyph of `symbol` in font `id` (`ass_font_get_index`):
    /// the first face that maps it, else a face selected for it and added to
    /// the font, else glyph 0 of that face or of the first.
    pub(super) fn glyph(&mut self, id: FontId, symbol: u32) -> Result<(usize, GlyphId)> {
        if symbol < 0x20 {
            return Ok((0, GlyphId(0)));
        }
        let font = &self.fonts.as_slice()[id.0];
        if let Some(found) = font
            .faces()
            .iter()
            .enumerate()
            .find_map(|(index, face)| Some((index, lookup(&face.font, symbol)?)))
        {
            return Ok(found);
        }
        if font.faces.len() == MAX_FACES {
            return Ok((0, GlyphId(0)));
        }
        let key = font.key.clone();
        let Some(face) = self.choose(&key, symbol)? else {
            return Ok((0, GlyphId(0)));
        };
        let font = &mut self.fonts.as_mut_slice()[id.0];
        let index = if let Some(index) = font.faces().iter().position(|f| f.uid == face.uid) {
            index
        } else {
            font.faces.try_push(face).map_err(|e| e.error)?;
            font.faces.len() - 1
        };
        let glyph = lookup(&font.faces()[index].font, symbol).unwrap_or(GlyphId(0));
        Ok((index, glyph))
    }

    /// `ass_font_select` for `font` and `code` (any face for 0): the face to
    /// add, opening the default font file if it is chosen.
    fn choose(&mut self, key: &Key, code: u32) -> Result<Option<Face>> {
        Ok(match self.select(key, code)? {
            Some(Choice::Registered(index)) => Some(self.faces.as_slice()[index].clone()),
            Some(Choice::Default) => self.default_face(),
            None => None,
        })
    }

    /// The requested family, then the default family, each for a face that
    /// maps `code`, then the default font file.
    fn select(&self, key: &Key, code: u32) -> Result<Option<Choice>> {
        let character = match code {
            0 => None,
            code => match char::from_u32(code) {
                Some(c) => Some(c),
                None => return Ok(None),
            },
        };
        let mut infos = BudgetVec::new(&self.budget);
        infos.try_reserve_exact(self.faces.len())?;
        for face in self.faces.as_slice() {
            infos
                .try_push(FaceInfo::new(&face.font))
                .map_err(|e| e.error)?;
        }
        let by_family = |family: &[u8]| {
            let family = std::str::from_utf8(family).ok()?;
            let request = Request {
                family,
                weight: key.weight,
                slant: key.slant,
                character,
            };
            select(infos.as_slice(), &request).map(Choice::Registered)
        };
        let requested = Some(&*key.family)
            .filter(|family| !family.is_empty())
            .and_then(by_family);
        let default = matches!(
            self.default_font,
            DefaultFont::Unopened(_) | DefaultFont::Opened(_)
        );
        Ok(requested
            .or_else(|| self.default_family.as_deref().and_then(by_family))
            .or_else(|| default.then_some(Choice::Default)))
    }

    /// The default font file's first face, opened once; `None` when it does
    /// not open.
    fn default_face(&mut self) -> Option<Face> {
        if let DefaultFont::Unopened(file) = &self.default_font {
            self.default_font = match Font::new(file.clone(), 0, &self.budget, self.limits) {
                Ok(font) => DefaultFont::Opened(Face {
                    font: Arc::new(font),
                    uid: self.next_uid(),
                }),
                Err(_) => DefaultFont::None,
            };
        }
        match &self.default_font {
            DefaultFont::Opened(face) => Some(face.clone()),
            _ => None,
        }
    }
}

/// The glyph `symbol` maps to in `font`, as libass looks it up through the
/// charmap it selects (a Symbol charmap at `0xF000 | symbol`); `None` for
/// glyph 0.
pub(super) fn lookup(font: &Font, symbol: u32) -> Option<GlyphId> {
    char::from_u32(symbol)
        .and_then(|c| font.glyph_index(c))
        .filter(|g| g.0 != 0)
}

#[cfg(test)]
mod tests {
    use super::{Fonts, Library};
    use triergon_core::Budget;

    #[test]
    fn without_fonts_nothing_is_selected() {
        let budget = Budget::new(1 << 20).unwrap();
        let mut library = Library::new(&budget, &Fonts::default()).unwrap();
        let desc = crate::FontDesc {
            family: b"Arial",
            vertical: false,
            weight: 400,
            slant: 0,
        };
        assert_eq!(library.font(&desc), Ok(None));
        // The failure is remembered.
        assert_eq!(library.font(&desc), Ok(None));
    }
}
