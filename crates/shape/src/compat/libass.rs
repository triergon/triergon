//! Shaping as libass sets it up for ASS subtitles: the font functions it
//! installs on the HarfBuzz font of a face at a size, and the rotation
//! decisions of its glyph metrics cache for `@` fonts.
//!
//! Reference: libass 0.17.5, with FreeType 2.15.0 and HarfBuzz 14.6.0.

mod rotations;

pub use rotations::{FaceRotations, GlyphRotations, rotates};

use crate::metrics::{Extents, Metrics, Scale, unless_unloadable};
use rotations::Decide;
use std::cell::RefCell;
use std::hash::Hash;
use triergon_core::{Error, Result, WorkBudget};
use triergon_font::compat::libass::{TextLookup, harfbuzz_scale, text_charmap, text_lookup};
use triergon_font::{
    CharEncoding, Charmap, ControlBox, GlyphId, GlyphMetrics, Load, Outline, Platform, SizedFont,
    Variant, VariationSequences,
};

/// The font functions libass 0.17.5 installs on the HarfBuzz font of a face at a
/// size: the face's character map looked up as libass looks codes up
/// ([`text_lookup`]), FreeType's variation sequences
/// ([`Font::variation_sequences`](triergon_font::Font::variation_sequences)),
/// default-load advances ([`SizedFont::default_advance`]) and default
/// kerning ([`SizedFont::kerning`]), in 26.6 pixels at the size's
/// [`harfbuzz_scale`] and pixels per em, so that hinting device tables apply.
///
/// For a face of an `@` font ([`Self::rotating`]), the advance of a glyph
/// libass's metrics cache keeps rotated ([`GlyphRotations`]) is the default
/// load's vertical advance ([`SizedFont::default_vertical_advance`]).
///
/// libass reads glyph extents and contour points from FreeType's default load
/// ([`SizedFont::load`] with [`Load::Default`]): the extents are its grid-fitted
/// bearings and size, the contour points its outline's. A glyph without points
/// has empty extents in every face; for other glyphs, the default load's
/// `Unsupported` (instructed TrueType faces, which need FreeType's TrueType
/// interpreter, and the glyphs FreeType's auto-hinter gives a CJK or Indic
/// style) is the functions', as is an advance `default_advance` cannot give.
pub struct SizedMetrics<'a> {
    sized: SizedFont<'a>,
    lookup: Option<TextLookup<'a>>,
    /// A Unicode charmap, which FreeType needs for variation sequences.
    unicode: bool,
    /// FreeType's variation sequences.
    variations: Option<VariationSequences<'a>>,
    scale: Scale,
    rotations: Option<&'a dyn Decide>,
    /// The caller's budget for the default loads of extents and contour
    /// points.
    work: RefCell<&'a mut WorkBudget>,
    /// The outline those loads fill.
    scratch: RefCell<Outline>,
}

impl std::fmt::Debug for SizedMetrics<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SizedMetrics")
            .field("sized", &self.sized)
            .field("lookup", &self.lookup)
            .field("unicode", &self.unicode)
            .field("scale", &self.scale)
            .field("rotating", &self.rotations.is_some())
            .finish_non_exhaustive()
    }
}

impl<'a> SizedMetrics<'a> {
    /// libass's font functions over `sized`, through the charmap libass
    /// selects when it opens the face ([`text_charmap`]), with `work`
    /// bounding what they load.
    ///
    /// # Errors
    ///
    /// As [`Self::with_charmap`].
    pub fn new(sized: SizedFont<'a>, work: &'a mut WorkBudget) -> Result<Self> {
        Self::with_charmap(sized, text_charmap(sized.font()), work)
    }

    /// libass's font functions over `sized`, through `charmap`, the one the
    /// face has selected: libass leaves a face on another charmap when only
    /// that one maps a character.
    ///
    /// # Errors
    ///
    /// `Unsupported` when libass would convert codes for the charmap, which
    /// this crate does not ([`text_lookup`]).
    pub fn with_charmap(
        sized: SizedFont<'a>,
        charmap: Option<Charmap<'a>>,
        work: &'a mut WorkBudget,
    ) -> Result<Self> {
        let lookup = charmap
            .map(|charmap| text_lookup(sized.font(), charmap))
            .transpose()?;
        let unicode = charmap.is_some_and(|c| {
            c.platform() == Platform::Unicode
                || matches!(
                    c.encoding(),
                    CharEncoding::Unicode | CharEncoding::UnicodeFull
                )
        });
        let (x, y) = harfbuzz_scale(&sized);
        let size = sized.metrics();
        Ok(Self {
            sized,
            lookup,
            unicode,
            variations: sized.font().variation_sequences(),
            scale: Scale {
                x,
                y,
                x_ppem: u32::from(size.x_ppem),
                y_ppem: u32::from(size.y_ppem),
            },
            rotations: None,
            work: RefCell::new(work),
            scratch: RefCell::new(Outline::new(sized.font().data().budget())),
        })
    }

    /// The font functions of a face of an `@` font, whose glyph metrics
    /// cache keeps `rotations`: every glyph a lookup loads is recorded there,
    /// and a glyph it keeps rotated advances by its vertical advance.
    #[must_use]
    pub fn rotating<K: Copy + Eq + Hash>(self, rotations: &'a FaceRotations<'a, K>) -> Self {
        Self {
            rotations: Some(rotations),
            ..self
        }
    }

    /// Records a glyph a lookup loads in the metrics cache: through the
    /// character `c`, or `None` for an advance or extents. Whether its cached
    /// metrics are rotated.
    fn loaded(&self, glyph: GlyphId, c: Option<char>) -> bool {
        self.rotations.is_some_and(|r| r.decide(glyph, c))
    }

    /// Records a glyph a character lookup returned in the metrics cache.
    fn looked_up(&self, glyph: Option<GlyphId>, c: char) -> Option<GlyphId> {
        if let Some(glyph) = glyph {
            self.loaded(glyph, Some(c));
        }
        glyph
    }

    /// FreeType's default load of `glyph` into the scratch outline; `None`
    /// for a glyph that does not load, which libass answers with nothing.
    fn default_load(&self, glyph: GlyphId) -> Result<Option<GlyphMetrics>> {
        let loaded = self.sized.load(
            glyph,
            Load::Default,
            &mut self.scratch.borrow_mut(),
            &mut self.work.borrow_mut(),
        );
        unless_unloadable(loaded.map(Some), None)
    }
}

impl Metrics for SizedMetrics<'_> {
    fn scale(&self) -> Scale {
        self.scale
    }

    fn nominal_glyph(&self, c: char) -> Option<GlyphId> {
        self.looked_up(self.lookup?.glyph(u32::from(c)), c)
    }

    fn variation_glyph(&self, c: char, selector: char) -> Option<GlyphId> {
        let lookup = self.lookup.filter(|_| self.unicode)?;
        let code = lookup.code(u32::from(c))?;
        let glyph = match self.variations?.variant(code, u32::from(selector)) {
            Variant::Default => lookup.charmap().glyph(code),
            Variant::Glyph(glyph) => Some(glyph),
            Variant::None => None,
        };
        self.looked_up(glyph, c)
    }

    fn h_advance(&self, glyph: GlyphId) -> Result<i32> {
        let advance = if self.loaded(glyph, None) {
            self.sized.default_vertical_advance(glyph)
        } else {
            self.sized.default_advance(glyph)
        };
        unless_unloadable(advance, 0)
    }

    fn extents(&self, glyph: GlyphId) -> Result<Option<Extents>> {
        // Rotated or not, the cached extents are the default load's.
        self.loaded(glyph, None);
        match self.default_load(glyph) {
            Ok(loaded) => Ok(loaded.map(|m| Extents {
                x_bearing: m.bearing_x,
                y_bearing: m.bearing_y,
                width: m.width,
                height: m.height.wrapping_neg(),
            })),
            // A glyph without points has an empty box however FreeType loads
            // it, so its extents need no hinting this crate lacks.
            Err(Error::Unsupported(what)) => {
                let font = self.sized.font();
                match font.control_box(glyph, &mut self.work.borrow_mut()) {
                    Ok(ControlBox::Empty) => Ok(Some(Extents::default())),
                    _ => Err(Error::Unsupported(what)),
                }
            }
            Err(error) => Err(error),
        }
    }

    fn contour_point(&self, glyph: GlyphId, point: u16) -> Result<Option<(i32, i32)>> {
        if self.default_load(glyph)?.is_none() {
            return Ok(None);
        }
        let outline = self.scratch.borrow();
        let point = outline.points().get(usize::from(point));
        Ok(point.map(|p| (p.point.x, p.point.y)))
    }

    fn has_h_kerning(&self) -> bool {
        true
    }

    fn h_kerning(&self, left: GlyphId, right: GlyphId) -> Result<i32> {
        Ok(self.sized.kerning(left, right))
    }

    fn finish(&self) -> Result<()> {
        self.rotations
            .and_then(Decide::take_refusal)
            .map_or(Ok(()), Err)
    }
}
