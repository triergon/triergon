//! Font selection and fallback as libass does it without a system font
//! provider: the attached and embedded fonts are matched by name, weight and
//! slant (`ass_font_select`), then the default family, then the default font
//! file; each requested font keeps the faces it was given and adds one for a
//! character none of them maps (`ass_font_get_index`), and each of its faces
//! keeps the charmap the last such lookup left it on.

use std::collections::HashMap;
use std::sync::Arc;

use triergon_core::{Budget, BudgetVec, Bytes, Reservation, Result, WorkBudget};
use triergon_font::{Charmap, FaceInfo, Font, GlyphId, Limits, Request, select};

use crate::{EmbeddedFont, FontDesc, charge};

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
///
/// libass opens a face anew for each font that adds it and looks characters
/// up through the charmap the face has selected, which a lookup can change
/// (`ass_font_get_index`), so each font's copy keeps its own.
#[derive(Clone, Debug)]
pub(super) struct Face {
    pub(super) font: Arc<Font>,
    pub(super) uid: u32,
    /// The selected charmap's index in [`Font::charmaps`]: the one libass
    /// selects on opening the face (`ass_charmap_magic`) until a lookup
    /// leaves it on another.
    charmap: Option<usize>,
}

impl Face {
    fn new(font: Font, uid: u32) -> Self {
        let charmap = font.text_charmap_index();
        Self {
            font: Arc::new(font),
            uid,
            charmap,
        }
    }

    /// The charmap the face has selected.
    pub(super) fn charmap(&self) -> Option<Charmap<'_>> {
        self.font.charmap(self.charmap?)
    }

    /// The glyph `symbol` maps to through the selected charmap, as libass
    /// looks it up (`ass_font_index_magic`, then `FT_Get_Char_Index`).
    fn glyph(&self, symbol: u32) -> Result<Option<GlyphId>> {
        match self.charmap() {
            Some(charmap) => Ok(self.font.text_lookup(charmap)?.glyph(symbol)),
            None => Ok(None),
        }
    }

    /// libass's last resort for a face selected for `symbol` that does not
    /// map it ("broken font? Trying all charmaps", `ass_font.c:681-692`):
    /// each charmap is selected in turn, in FreeType's order, and `symbol`
    /// looked up through it. The face keeps the one that maps it, or the
    /// last one. A charmap is charged to `work` as it is tried.
    fn try_every_charmap(&mut self, symbol: u32, work: &mut WorkBudget) -> Result<Option<GlyphId>> {
        for (index, charmap) in self.font.charmaps().enumerate() {
            charge(work, 1)?;
            self.charmap = Some(index);
            if let Some(glyph) = self.font.text_lookup(charmap)?.glyph(symbol) {
                return Ok(Some(glyph));
            }
        }
        Ok(None)
    }
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

/// A requested font, by an identity the [`Library`] never gives another
/// font, so what is keyed by it (outlines) cannot be taken for another font
/// once the font leaves the library.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct FontId(u64);

/// One description of a family the library was asked for: its weight, slant
/// and orientation, the font made for it (`None` when no face could be
/// found, which is remembered, as libass remembers it), and the frame it was
/// last asked for in.
#[derive(Debug)]
struct Requested {
    weight: u32,
    slant: u32,
    vertical: bool,
    font: Option<FontId>,
    used: u64,
}

impl Requested {
    fn is(&self, desc: &FontDesc<'_>) -> bool {
        self.weight == desc.weight && self.slant == desc.slant && self.vertical == desc.vertical
    }
}

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

/// Bytes charged for each requested font, description and registered face
/// beyond the storage that charges itself and the family names, which are
/// charged by length: the map slots and handles.
const ENTRY_BYTES: usize = 128;

/// The faces fonts are selected from and the fonts requested so far, cut to
/// the font limit of the renderer's [`CacheLimits`](super::CacheLimits) at
/// each frame: the descriptions asked for least recently go first, never one
/// the last frame asked for. libass keeps every font it made; a font asked
/// for again after it left the library starts over from the face selected
/// for it, without the faces it had gathered for other characters.
#[derive(Debug)]
pub(super) struct Library {
    budget: Budget,
    limits: Limits,
    faces: BudgetVec<Face>,
    next_uid: u32,
    /// How many of each script's embedded fonts are registered, by the
    /// script's identity.
    embedded: HashMap<u64, usize>,
    default_family: Option<Box<[u8]>>,
    default_font: DefaultFont,
    fonts: HashMap<FontId, RequestedFont>,
    next_font: u64,
    /// The descriptions asked for, by family name, so a lookup borrows the
    /// name it is given.
    by_family: HashMap<Box<[u8]>, Vec<Requested>>,
    /// How many descriptions `by_family` holds.
    requested: usize,
    /// The bytes of the family names held: the map's keys and the fonts'.
    name_bytes: usize,
    /// The current frame, which descriptions are stamped with when asked for.
    frame: u64,
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
            embedded: HashMap::new(),
            default_family: fonts.default_family.as_deref().map(Box::from),
            default_font: fonts
                .default_font
                .clone()
                .map_or(DefaultFont::None, DefaultFont::Unopened),
            fonts: HashMap::new(),
            next_font: 0,
            by_family: HashMap::new(),
            requested: 0,
            name_bytes: 0,
            frame: 0,
            charge: Reservation::empty(budget),
        };
        for file in &fonts.attached {
            library.register(file)?;
        }
        Ok(library)
    }

    /// Registers the embedded fonts of `script` that are not yet, as libass
    /// does at the start of each frame. libass keeps every track's embedded
    /// fonts in the library the tracks share and registers those added since
    /// the last frame, whichever track the frame renders; here each script's
    /// are registered when a frame of it is rendered, after those of the
    /// scripts rendered before, which stay selectable as in libass.
    pub(super) fn register_embedded(&mut self, script: u64, fonts: &[EmbeddedFont]) -> Result<()> {
        let registered = self.embedded.get(&script).copied().unwrap_or(0);
        if registered >= fonts.len() {
            return Ok(());
        }
        for font in &fonts[registered..] {
            self.register(&font.data)?;
        }
        self.embedded.insert(script, fonts.len());
        self.charge_entries()
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
            let face = Face::new(font, self.next_uid());
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
        let entries = self.faces.len() + self.requested + self.fonts.len() + self.embedded.len();
        self.charge.resize_to(
            entries
                .saturating_mul(ENTRY_BYTES)
                .saturating_add(self.name_bytes),
        )
    }

    /// Starts a frame: cuts the fonts to `limit` descriptions, least recently
    /// asked for first, keeping those the last frame asked for.
    pub(super) fn begin_frame(&mut self, limit: usize) -> Result<()> {
        self.frame += 1;
        let excess = self.requested.saturating_sub(limit);
        if excess == 0 {
            return Ok(());
        }
        let last = self.frame - 1;
        let mut stamps = BudgetVec::new(&self.budget);
        stamps.try_reserve_exact(self.requested)?;
        for requested in self.by_family.values().flatten() {
            if requested.used < last {
                stamps.try_push(requested.used).map_err(|e| e.error)?;
            }
        }
        if stamps.is_empty() {
            return Ok(());
        }
        stamps.as_mut_slice().sort_unstable();
        // Frames stamp many descriptions alike: the oldest frames go whole.
        let oldest = stamps.as_slice()[excess.min(stamps.len()) - 1];
        let fonts = &mut self.fonts;
        let (mut removed, mut freed) = (0, 0);
        self.by_family.retain(|family, list| {
            list.retain(|requested| {
                let keep = requested.used > oldest;
                if !keep {
                    removed += 1;
                    if let Some(font) = requested.font.and_then(|id| fonts.remove(&id)) {
                        freed += font.key.family.len();
                    }
                }
                keep
            });
            if list.is_empty() {
                freed += family.len();
            }
            !list.is_empty()
        });
        self.requested -= removed;
        self.name_bytes -= freed;
        self.charge_entries()
    }

    /// The requested font for `desc` (`ass_font_new`): built and given its
    /// first face on first request; `None` when no face can be found for it,
    /// which makes an event that uses it fail.
    pub(super) fn font(
        &mut self,
        desc: &FontDesc<'_>,
        work: &mut WorkBudget,
    ) -> Result<Option<FontId>> {
        let frame = self.frame;
        if let Some(requested) = self
            .by_family
            .get_mut(desc.family)
            .and_then(|list| list.iter_mut().find(|r| r.is(desc)))
        {
            requested.used = frame;
            return Ok(requested.font);
        }
        let key = Key {
            family: desc.family.into(),
            weight: desc.weight,
            slant: desc.slant,
            vertical: desc.vertical,
        };
        let id = match self.choose(&key, 0, work)? {
            Some(face) => {
                let mut faces = BudgetVec::new(&self.budget);
                faces.try_push(face).map_err(|e| e.error)?;
                let id = FontId(self.next_font);
                self.next_font += 1;
                self.name_bytes += key.family.len();
                self.fonts.insert(id, RequestedFont { key, faces });
                Some(id)
            }
            None => None,
        };
        let requested = Requested {
            weight: desc.weight,
            slant: desc.slant,
            vertical: desc.vertical,
            font: id,
            used: frame,
        };
        if let Some(list) = self.by_family.get_mut(desc.family) {
            list.push(requested);
        } else {
            self.name_bytes += desc.family.len();
            self.by_family.insert(desc.family.into(), vec![requested]);
        }
        self.requested += 1;
        self.charge_entries()?;
        Ok(id)
    }

    pub(super) fn get(&self, id: FontId) -> &RequestedFont {
        &self.fonts[&id]
    }

    /// The face and glyph of `symbol` in font `id` (`ass_font_get_index`):
    /// the first face that maps it through the charmap it has selected, else
    /// a face selected for it and added to the font (or found there), which
    /// is tried through each of its charmaps if it does not, else glyph 0 of
    /// that face or of the first.
    ///
    /// # Errors
    ///
    /// `Unsupported` where libass would look `symbol` up in a way this crate
    /// does not: through a charmap whose codes libass converts
    /// ([`Font::text_lookup`]).
    pub(super) fn glyph(
        &mut self,
        id: FontId,
        symbol: u32,
        work: &mut WorkBudget,
    ) -> Result<(usize, GlyphId)> {
        if symbol < 0x20 {
            return Ok((0, GlyphId(0)));
        }
        let font = &self.fonts[&id];
        // Each face the font has gathered is tried.
        charge(work, font.faces.len())?;
        for (index, face) in font.faces().iter().enumerate() {
            if let Some(glyph) = face.glyph(symbol)? {
                return Ok((index, glyph));
            }
        }
        if font.faces.len() == MAX_FACES {
            return Ok((0, GlyphId(0)));
        }
        let key = font.key.clone();
        let Some(face) = self.choose(&key, symbol, work)? else {
            return Ok((0, GlyphId(0)));
        };
        let Some(font) = self.fonts.get_mut(&id) else {
            return Ok((0, GlyphId(0)));
        };
        let index = if let Some(index) = font.faces().iter().position(|f| f.uid == face.uid) {
            index
        } else {
            font.faces.try_push(face).map_err(|e| e.error)?;
            font.faces.len() - 1
        };
        let face = &mut font.faces[index];
        let glyph = match face.glyph(symbol)? {
            Some(glyph) => Some(glyph),
            None => face.try_every_charmap(symbol, work)?,
        };
        Ok((index, glyph.unwrap_or(GlyphId(0))))
    }

    /// `ass_font_select` for `font` and `code` (any face for 0): the face to
    /// add, opening the default font file if it is chosen.
    fn choose(&mut self, key: &Key, code: u32, work: &mut WorkBudget) -> Result<Option<Face>> {
        // The requested and the default family are each matched against
        // every registered face.
        charge(work, self.faces.len().saturating_mul(2))?;
        Ok(match self.select(key, code) {
            Some(Choice::Registered(index)) => Some(self.faces.as_slice()[index].clone()),
            Some(Choice::Default) => self.default_face(),
            None => None,
        })
    }

    /// The requested family, then the default family, each for a face that
    /// maps `code`, then the default font file.
    fn select(&self, key: &Key, code: u32) -> Option<Choice> {
        let character = (code != 0).then_some(code);
        // What matching reads of a face is three fields of the parsed font, so
        // each search reads them as it reaches the face, without storage.
        let infos = || self.faces.iter().map(|face| FaceInfo::new(&face.font));
        let by_family = |family: &[u8]| {
            let family = std::str::from_utf8(family).ok()?;
            let request = Request {
                family,
                weight: key.weight,
                slant: key.slant,
                character,
            };
            select(infos(), &request).map(Choice::Registered)
        };
        let requested = Some(&*key.family)
            .filter(|family| !family.is_empty())
            .and_then(by_family);
        let default = matches!(
            self.default_font,
            DefaultFont::Unopened(_) | DefaultFont::Opened(_)
        );
        requested
            .or_else(|| self.default_family.as_deref().and_then(by_family))
            .or_else(|| default.then_some(Choice::Default))
    }

    /// The default font file's first face, opened once; `None` when it does
    /// not open.
    fn default_face(&mut self) -> Option<Face> {
        if let DefaultFont::Unopened(file) = &self.default_font {
            self.default_font = match Font::new(file.clone(), 0, &self.budget, self.limits) {
                Ok(font) => DefaultFont::Opened(Face::new(font, self.next_uid())),
                Err(_) => DefaultFont::None,
            };
        }
        match &self.default_font {
            DefaultFont::Opened(face) => Some(face.clone()),
            _ => None,
        }
    }
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
        let mut work = triergon_core::WorkBudget::new(1 << 20);
        assert_eq!(library.font(&desc, &mut work), Ok(None));
        // The failure is remembered.
        assert_eq!(library.font(&desc, &mut work), Ok(None));
    }

    #[test]
    fn names_are_charged_and_the_least_recent_leave() {
        let budget = Budget::new(16 << 20).unwrap();
        let mut library = Library::new(&budget, &Fonts::default()).unwrap();
        let mut work = triergon_core::WorkBudget::new(1 << 20);
        let empty = budget.used();
        let names: Vec<Vec<u8>> = (0..20).map(|i| format!("{i:04000}").into_bytes()).collect();
        let desc = |family| crate::FontDesc {
            family,
            vertical: false,
            weight: 400,
            slant: 0,
        };
        for (frame, name) in names.iter().enumerate() {
            library.begin_frame(4).unwrap();
            // Each frame asks for a new name and the one before it again.
            library.font(&desc(name), &mut work).unwrap();
            if frame > 0 {
                library.font(&desc(&names[frame - 1]), &mut work).unwrap();
            }
            assert!(library.requested <= 5, "{}", library.requested);
            // The names are charged by length.
            assert!(budget.used() >= empty + library.requested * 4000);
        }
        // The last frame's names stay; the oldest went.
        let kept = |library: &Library, name: &[u8]| library.by_family.contains_key(name);
        assert!(kept(&library, &names[19]) && kept(&library, &names[18]));
        assert!(!kept(&library, &names[0]));
        // With no room, only what the last frame asked for stays.
        library.begin_frame(0).unwrap();
        assert!(kept(&library, &names[19]) && kept(&library, &names[18]));
        assert!(!kept(&library, &names[17]));
    }
}
