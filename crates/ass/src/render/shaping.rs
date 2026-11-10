//! The second stage: an event's text shaped as libass's complex shaper does
//! (`ass_shaper_find_runs`, `ass_shaper_shape`): each character's script and
//! face, runs of one face, script and style, bidi levels per paragraph, and
//! OpenType shaping of each run at the drawn size, which gives each
//! character a cluster of glyphs. The bidi levels give the visual order of
//! each line once lines are known (`ass_shaper_reorder`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;

use triergon_core::{
    Budget, BudgetVec, Error, Reservation, Result, WorkBudget, round_ties_even_i32,
};
use triergon_font::GlyphId;
use triergon_raster::{Vector, pixels_26_6};
use triergon_shape::bidi::{BaseDirection, BidiClass, Level, Resolver, reorder_line};
use triergon_shape::{
    Buffer, Direction, Face, FaceRotations, Feature, GlyphRotations, Language, Plan,
    Script as WritingScript, SizedMetrics, Tag,
};

use super::fonts::Library;
use super::text::{Char, Text};
use super::wrap::Line;
use crate::{Info, charge};

/// How an event's text is shaped and reordered.
#[derive(Clone, Copy, Debug)]
pub(super) struct Shaping<'a> {
    /// The paragraph direction: left to right, or found from the text for
    /// `\fe-1` (`ass_resolve_base_direction`).
    pub(super) base: BaseDirection,
    /// The text is shaped and reordered as a whole rather than run by run:
    /// implicitly for `\fe-1`.
    pub(super) whole_text: bool,
    /// The `Kerning` header.
    pub(super) kerning: bool,
    /// The `Language` header.
    pub(super) language: Option<&'a [u8]>,
}

impl<'a> Shaping<'a> {
    /// The settings for an event of `info` whose last `\fe` is `encoding`.
    pub(super) fn new(encoding: i32, info: &'a Info) -> Self {
        let (base, whole_text) = if encoding == -1 {
            (BaseDirection::Auto, true)
        } else {
            (BaseDirection::LeftToRight, false)
        };
        Self {
            base,
            whole_text,
            kerning: info.kerning,
            language: info.language(),
        }
    }
}

/// One glyph of a cluster, in drawn-size 26.6 units scaled by its
/// character's scale.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ShapedGlyph {
    pub(super) id: GlyphId,
    pub(super) offset: Vector,
    pub(super) advance: Vector,
}

/// What shaping gave one character: its glyphs, in the order shaping gave
/// them.
#[derive(Clone, Debug)]
pub(super) struct Cluster {
    /// Its glyphs in [`Shaped::glyphs`]: always one at least, its own glyph
    /// from the character map when shaping gave it none.
    pub(super) glyphs: Range<usize>,
    /// The sum of the glyphs' advances.
    pub(super) advance: Vector,
    /// The face of the character's font its glyphs come from.
    pub(super) face: u16,
    /// Shaping gave the character a glyph; a drawing always has one. A
    /// character a ligature took or HarfBuzz hid is not shown (libass's
    /// `skip`).
    pub(super) visible: bool,
}

/// An event shaped: what the next stage lays out. Shaped again for each
/// event, into the same storage.
#[derive(Debug)]
pub(super) struct Shaped {
    /// Each character's cluster, in text order.
    pub(super) clusters: BudgetVec<Cluster>,
    /// The glyphs of every cluster, cluster by cluster.
    pub(super) glyphs: BudgetVec<ShapedGlyph>,
    pub(super) bidi: Bidi,
}

impl Shaped {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            clusters: BudgetVec::new(budget),
            glyphs: BudgetVec::new(budget),
            bidi: Bidi {
                classes: BudgetVec::new(budget),
                levels: BudgetVec::new(budget),
                directions: BudgetVec::new(budget),
            },
        }
    }

    /// The glyphs of `cluster`.
    pub(super) fn glyphs(&self, cluster: &Cluster) -> &[ShapedGlyph] {
        &self.glyphs[cluster.glyphs.clone()]
    }
}

/// The bidi state of a shaped event: every character's class and level,
/// and the direction of each paragraph.
#[derive(Debug)]
pub(super) struct Bidi {
    classes: BudgetVec<BidiClass>,
    levels: BudgetVec<Level>,
    directions: BudgetVec<Direction>,
}

/// What one shaping plan is built for: a face, a direction, a script, a
/// language code and libass's three feature switches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct PlanKey {
    face: u32,
    direction: Direction,
    script: WritingScript,
    language: [u8; 4],
    features: [bool; 3],
}

/// The characters of context a shaping call reads on either side of its item
/// (HarfBuzz's `HB_BUFFER_CONTEXT_LENGTH`, as `triergon-shape` keeps it).
const CONTEXT: usize = 5;
/// Work units charged per shaping call beyond its characters: sizing the
/// face and finding the plan.
const RUN_UNITS: usize = 16;

/// Plans kept at most; more are built again when needed.
const MAX_PLANS: usize = 256;
/// Bytes charged per plan beyond its own storage.
const PLAN_ENTRY_BYTES: usize = 256;
/// The shaper's state: a buffer, a bidi resolver, the plans built so far,
/// and storage for the event being shaped.
#[derive(Debug)]
pub(super) struct Shaper {
    budget: Budget,
    /// libass's glyph metrics cache for `@` fonts, which it never cuts:
    /// whether each glyph's metrics are rotated, for the renderer's life.
    rotations: RefCell<GlyphRotations<RotationKey>>,
    /// The `@` font descriptions the cache has decisions for.
    vertical_fonts: VerticalFonts,
    buffer: Buffer,
    resolver: Resolver,
    plans: HashMap<PlanKey, Plan>,
    /// The plans' storage.
    charge: Reservation,
    /// The event's text as HarfBuzz reads it.
    text: BudgetVec<char>,
    /// What finding runs gave each character.
    items: BudgetVec<Itemised>,
    /// The glyphs shaping gave each character of the item being shaped,
    /// then where its cluster's next one goes.
    counts: BudgetVec<usize>,
}

/// What libass keys its glyph metrics cache by, besides the glyph: the font
/// (the number of its description among [`VerticalFonts`]), the face and the
/// size.
type RotationKey = (u32, u32, u64);

/// The descriptions of the `@` fonts asked for (family, weight and slant),
/// numbered, as libass keys its glyph metrics cache by font: sorted by
/// description and found by binary search, in storage charged to the budget
/// before it grows.
#[derive(Debug)]
struct VerticalFonts {
    /// By family name, weight and slant.
    fonts: BudgetVec<VerticalFont>,
    /// The family names, one after another.
    names: BudgetVec<u8>,
}

/// An `@` font description: where its family name is in
/// [`VerticalFonts::names`], its weight and slant, and its number.
#[derive(Clone, Copy, Debug)]
struct VerticalFont {
    name: (usize, usize),
    weight: u32,
    slant: u32,
    id: u32,
}

impl VerticalFonts {
    fn new(budget: &Budget) -> Self {
        Self {
            fonts: BudgetVec::new(budget),
            names: BudgetVec::new(budget),
        }
    }

    /// The number of the description of `family`, `weight` and `slant`.
    fn id(&mut self, family: &[u8], weight: u32, slant: u32) -> Result<u32> {
        let names = self.names.as_slice();
        let found = self.fonts.as_slice().binary_search_by(|f| {
            let (start, len) = f.name;
            (&names[start..start + len], f.weight, f.slant).cmp(&(family, weight, slant))
        });
        let at = match found {
            Ok(i) => return Ok(self.fonts.as_slice()[i].id),
            Err(at) => at,
        };
        let id = u32::try_from(self.fonts.len()).map_err(|_| Error::Overflow)?;
        let name = (self.names.len(), family.len());
        // Grown geometrically, as the descriptions accumulate for the
        // renderer's life.
        let spare = self.names.capacity() - self.names.len();
        if spare < family.len() {
            self.names
                .try_reserve_exact(family.len().max(self.names.capacity()))?;
        }
        self.names.try_extend_from_slice(family)?;
        let font = VerticalFont {
            name,
            weight,
            slant,
            id,
        };
        self.fonts.try_push(font).map_err(|e| e.error)?;
        self.fonts.as_mut_slice()[at..].rotate_right(1);
        Ok(id)
    }
}

/// What finding runs gives a character (`ass_shaper_find_runs`).
#[derive(Clone, Copy, Debug)]
struct Itemised {
    script: WritingScript,
    /// Its glyph in its face, as the character map gives it.
    glyph: GlyphId,
    /// The face of its font it is shaped with.
    face: u16,
    /// The run of one face, script and style it is shaped in.
    run: u32,
}

/// Whether a code point affects the shaping of its neighbours across runs:
/// ZWNJ and ZWJ.
const fn is_shaping_control(symbol: u32) -> bool {
    matches!(symbol, 0x200c | 0x200d)
}

/// Code points HarfBuzz replaces by an empty glyph whatever the font maps them
/// to (libass's `is_harfbuzz_ignorable`).
const fn is_ignorable(symbol: u32) -> bool {
    match symbol >> 8 {
        0x00 => symbol == 0x00ad,
        0x03 => symbol == 0x034f,
        0x06 => symbol == 0x061c,
        0x17 => matches!(symbol, 0x17b4..=0x17b5),
        0x18 => matches!(symbol, 0x180b..=0x180e),
        0x20 => matches!(symbol, 0x200b..=0x200f | 0x202a..=0x202e | 0x2060..=0x206f),
        0xfe => matches!(symbol, 0xfe00..=0xfe0f | 0xfeff),
        0xff => matches!(symbol, 0xfff0..=0xfff8),
        0x1d1 => matches!(symbol, 0x1d173..=0x1d17a),
        _ => matches!(symbol, 0xe0000..=0xe0fff),
    }
}

/// The language libass shapes a script in when the script names none
/// (`script_to_language`); `c` for the rest, the C locale's default.
fn script_language(script: WritingScript) -> &'static str {
    match &script.to_bytes() {
        b"Arab" => "ar",
        b"Armn" => "hy",
        b"Beng" => "bn",
        b"Cans" => "iu",
        b"Cher" => "chr",
        b"Copt" => "cop",
        b"Cyrl" => "ru",
        b"Deva" => "hi",
        b"Geor" => "ka",
        b"Grek" => "el",
        b"Gujr" => "gu",
        b"Guru" => "pa",
        b"Hang" => "ko",
        b"Hebr" => "he",
        b"Hira" | b"Kana" => "ja",
        b"Knda" => "kn",
        b"Laoo" => "lo",
        b"Latn" => "en",
        b"Mlym" => "ml",
        b"Mong" => "mn",
        b"Orya" => "or",
        b"Syrc" => "syr",
        b"Taml" => "ta",
        b"Telu" => "te",
        b"Thai" => "th",
        b"Tibt" => "bo",
        b"Ethi" => "am",
        b"Khmr" => "km",
        b"Mymr" => "my",
        b"Sinh" => "si",
        b"Thaa" => "dv",
        b"Buhd" => "bku",
        b"Hano" => "hnn",
        b"Tglg" => "tl",
        b"Tagb" => "tbw",
        b"Ugar" => "uga",
        b"Bugi" => "bug",
        b"Xpeo" => "peo",
        b"Sylo" => "syl",
        b"Nkoo" => "nko",
        _ => "c",
    }
}

/// The language a run is shaped in, zero padded: the `Language` header,
/// else the script's own.
fn language_code(header: Option<&[u8]>, script: WritingScript) -> [u8; 4] {
    let code = header.unwrap_or_else(|| script_language(script).as_bytes());
    let mut padded = [0; 4];
    for (to, from) in padded.iter_mut().zip(code) {
        *to = *from;
    }
    padded
}

/// A code point as a `char`, U+FFFD for what is not a scalar value, as
/// HarfBuzz reads UTF-32.
fn scalar(symbol: u32) -> char {
    char::from_u32(symbol).unwrap_or('\u{fffd}')
}

impl Shaper {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            budget: budget.clone(),
            rotations: RefCell::new(GlyphRotations::new(budget)),
            vertical_fonts: VerticalFonts::new(budget),
            buffer: Buffer::new(budget),
            resolver: Resolver::new(budget),
            plans: HashMap::new(),
            charge: Reservation::empty(budget),
            text: BudgetVec::new(budget),
            items: BudgetVec::new(budget),
            counts: BudgetVec::new(budget),
        }
    }

    /// Shapes `text` into `out`: its clusters, and the bidi levels its lines
    /// are later reordered by.
    pub(super) fn shape(
        &mut self,
        text: &Text,
        library: &mut Library,
        shaping: &Shaping<'_>,
        work: &mut WorkBudget,
        out: &mut Shaped,
    ) -> Result<()> {
        self.find_runs(text, library, shaping.whole_text, work)?;
        self.text.clear();
        self.text.try_reserve_exact(text.chars().len())?;
        for c in text.chars() {
            self.text.try_push(scalar(c.symbol)).map_err(|e| e.error)?;
        }
        self.resolve_levels(text, shaping, &mut out.bidi)?;
        out.clusters.clear();
        out.glyphs.clear();
        out.clusters.try_reserve_exact(text.chars().len())?;
        let chars = text.chars();
        let mut i = 0;
        while i < chars.len() {
            if chars[i].drawing.is_some() {
                // A drawing is drawn as itself: one glyph, sized by its outline.
                out.glyphs
                    .try_push(ShapedGlyph::default())
                    .map_err(|e| e.error)?;
                let cluster = Cluster {
                    glyphs: out.glyphs.len() - 1..out.glyphs.len(),
                    advance: Vector::default(),
                    face: self.items[i].face,
                    visible: true,
                };
                out.clusters.try_push(cluster).map_err(|e| e.error)?;
                i += 1;
                continue;
            }
            let item = self.item(chars, &out.bidi.levels, i);
            let direction = out.bidi.levels[i].direction();
            let base = self.shape_item(text, library, shaping, item.clone(), direction, work)?;
            self.push_clusters(text, item.clone(), base, out)?;
            i = item.end;
        }
        super::fit(&mut out.bidi.classes)?;
        super::fit(&mut out.bidi.levels)?;
        super::fit(&mut out.bidi.directions)?;
        super::fit(&mut out.clusters)?;
        super::fit(&mut out.glyphs)?;
        super::fit(&mut self.text)?;
        super::fit(&mut self.items)?;
        super::fit(&mut self.counts)
    }

    /// `ass_shaper_find_runs`: scripts, faces and shape runs.
    fn find_runs(
        &mut self,
        text: &Text,
        library: &mut Library,
        whole_text: bool,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let chars = text.chars();
        let items = &mut self.items;
        items.clear();
        items.try_reserve_exact(chars.len())?;
        for c in chars {
            // HarfBuzz gives no script to what is not a scalar value.
            let script = char::from_u32(c.symbol).map_or(WritingScript::UNKNOWN, WritingScript::of);
            items
                .try_push(Itemised {
                    script,
                    glyph: GlyphId(0),
                    face: 0,
                    run: 0,
                })
                .map_err(|e| e.error)?;
        }
        inherit_scripts(items);
        let items = items.as_mut_slice();
        let mut run = 0;
        for (i, c) in chars.iter().enumerate() {
            let ignorable = is_ignorable(c.symbol);
            if c.drawing.is_none() && !ignorable {
                let (face, glyph) = library.glyph(c.font, c.symbol, work)?;
                // A font holds at most `MAX_FACES` faces.
                items[i].face = face as u16;
                items[i].glyph = glyph;
            }
            if i > 0 {
                let (previous, last) = (&chars[i - 1], items[i - 1]);
                if previous.font != c.font
                    || (!ignorable && last.face != items[i].face)
                    || last.script != items[i].script
                    || c.starts_run
                    || (!whole_text && text.style(c).spacing != 0.0)
                    || text.style(previous).decorations != text.style(c).decorations
                {
                    run += 1;
                } else if ignorable {
                    items[i].face = last.face;
                }
            }
            items[i].run = run;
        }
        Ok(())
    }

    /// The bidi classes, and the levels resolved one paragraph at a time;
    /// without whole-text layout, each style run and each letter-spaced
    /// character is a paragraph.
    fn resolve_levels(
        &mut self,
        text: &Text,
        shaping: &Shaping<'_>,
        bidi: &mut Bidi,
    ) -> Result<()> {
        let len = self.text.len();
        bidi.classes.clear();
        bidi.levels.clear();
        bidi.directions.clear();
        bidi.classes.try_reserve_exact(len)?;
        bidi.levels.try_reserve_exact(len)?;
        for &c in self.text.as_slice() {
            bidi.classes
                .try_push(BidiClass::of(c))
                .map_err(|e| e.error)?;
            bidi.levels
                .try_push(Level::default())
                .map_err(|e| e.error)?;
        }
        let chars = text.chars();
        let mut start = 0;
        for i in 0..len {
            let ends = i == len - 1
                || bidi.classes[i] == BidiClass::B
                || (!shaping.whole_text
                    && (chars[i + 1].starts_run || text.style(&chars[i]).spacing != 0.0));
            if ends {
                let direction = self.resolver.resolve(
                    &bidi.classes[start..=i],
                    None,
                    shaping.base,
                    &mut bidi.levels.as_mut_slice()[start..=i],
                )?;
                if shaping.whole_text {
                    bidi.directions.try_push(direction).map_err(|e| e.error)?;
                }
                start = i + 1;
            }
        }
        Ok(())
    }

    /// The item starting at character `start`: the characters after it of
    /// its shape run and bidi level, up to a drawing (`shape_harfbuzz`).
    fn item(&self, chars: &[Char], levels: &[Level], start: usize) -> Range<usize> {
        let (run, level) = (self.items[start].run, levels[start]);
        let mut end = start + 1;
        while end < chars.len() && self.items[end].run == run && levels[end] == level {
            end += 1;
        }
        start..end
    }

    /// Shapes the characters of `item` into the buffer, reading the context
    /// libass hands HarfBuzz around it; the index in the text of the
    /// buffer's character 0.
    fn shape_item(
        &mut self,
        text: &Text,
        library: &Library,
        shaping: &Shaping<'_>,
        item: Range<usize>,
        direction: Direction,
        work: &mut WorkBudget,
    ) -> Result<usize> {
        let chars = text.chars();
        let len = chars.len();
        let first = &chars[item.start];
        let font = library.get(first.font);
        let itemised = self.items[item.start];
        let face_font = &font.faces()[usize::from(itemised.face)];
        let script = itemised.script;
        let language = language_code(shaping.language, script);
        // Ligatures only without letter spacing, vertical forms for `@`
        // fonts.
        let features = [
            font.vertical(),
            shaping.kerning,
            text.style(first).spacing == 0.0,
        ];
        let context = if shaping.whole_text {
            0..len
        } else {
            let joins = |at: usize| !chars[at].starts_run;
            let lead = usize::from(
                item.start > 0
                    && joins(item.start)
                    && is_shaping_control(chars[item.start - 1].symbol),
            );
            let trail = usize::from(
                item.end < len && joins(item.end) && is_shaping_control(chars[item.end].symbol),
            );
            item.start - lead..item.end + trail
        };
        // A call shapes the item and reads at most `CONTEXT` characters of
        // the context on either side (HarfBuzz's buffer context), however
        // much of the text is handed to it.
        let read = item.len()
            + (item.start - context.start).min(CONTEXT)
            + (context.end - item.end).min(CONTEXT);
        charge(work, read.saturating_add(RUN_UNITS))?;
        // The run's glyphs share the size their outlines load at, and the
        // face reads them through the charmap it has selected.
        let sized = face_font
            .font
            .at_size(pixels_26_6(text.style(first).size))?;
        let mut metrics = SizedMetrics::with_charmap(sized, face_font.charmap(), work)?;
        let rotations;
        if font.vertical() {
            let id = self
                .vertical_fonts
                .id(font.family(), font.weight(), font.slant())?;
            let size = text.style(first).size.to_bits();
            rotations = FaceRotations::new(&self.rotations, (id, face_font.uid, size));
            metrics = metrics.rotating(&rotations);
        }
        let shaper_face = Face::new(&face_font.font);
        let key = PlanKey {
            face: face_font.uid,
            direction,
            script,
            language,
            features,
        };
        if !self.plans.contains_key(&key) {
            if self.plans.len() >= MAX_PLANS {
                self.plans.clear();
            }
            self.charge
                .resize_to((self.plans.len() + 1).saturating_mul(PLAN_ENTRY_BYTES))?;
            let plan = Plan::new(
                &shaper_face,
                direction,
                script,
                &Language::new(language),
                &plan_features(features),
                &self.budget,
            )?;
            self.plans.insert(key, plan);
        }
        let base = context.start;
        triergon_shape::shape(
            &shaper_face,
            &metrics,
            &self.plans[&key],
            &self.text[context],
            item.start - base..item.end - base,
            &mut self.buffer,
        )?;
        Ok(base)
    }

    /// The clusters of `item`'s characters from the shaped buffer, whose
    /// character 0 is the text's `base`: each character's glyphs in the
    /// order shaping gave them, or its own glyph when it gave none.
    fn push_clusters(
        &mut self,
        text: &Text,
        item: Range<usize>,
        base: usize,
        out: &mut Shaped,
    ) -> Result<()> {
        let counts = &mut self.counts;
        counts.clear();
        counts.try_resize(item.len(), 0)?;
        let in_item = |cluster: u32| {
            (base + cluster as usize)
                .checked_sub(item.start)
                .filter(|&k| k < item.len())
        };
        for (glyph, _) in self.buffer.glyphs() {
            if let Some(k) = in_item(glyph.cluster) {
                counts[k] += 1;
            }
        }
        // Each character's cluster starts where the last ended; `counts`
        // then holds where its next glyph goes.
        let first = out.glyphs.len();
        let total: usize = counts.iter().map(|&n| n.max(1)).sum();
        // The event's glyphs grow item by item: at least double, so the
        // copies stay linear in them.
        let needed = first + total;
        if needed > out.glyphs.capacity() {
            out.glyphs
                .try_reserve_exact(needed.max(out.glyphs.capacity() * 2) - first)?;
        }
        out.glyphs.try_resize(needed, ShapedGlyph::default())?;
        let mut at = first;
        for (k, count) in counts.as_mut_slice().iter_mut().enumerate() {
            let itemised = self.items[item.start + k];
            let len = (*count).max(1);
            out.glyphs[at] = ShapedGlyph {
                id: itemised.glyph,
                ..ShapedGlyph::default()
            };
            out.clusters
                .try_push(Cluster {
                    glyphs: at..at + len,
                    advance: Vector::default(),
                    face: itemised.face,
                    visible: *count > 0,
                })
                .map_err(|e| e.error)?;
            *count = at;
            at += len;
        }
        for (shaped, position) in self.buffer.glyphs() {
            let Some(k) = in_item(shaped.cluster) else {
                continue;
            };
            let [sx, sy] = text.style(&text.chars()[item.start + k]).scale;
            let glyph = ShapedGlyph {
                id: shaped.id,
                offset: Vector::new(
                    round_ties_even_i32(f64::from(position.x_offset) * sx),
                    round_ties_even_i32(f64::from(-position.y_offset) * sy),
                ),
                advance: Vector::new(
                    round_ties_even_i32(f64::from(position.x_advance) * sx),
                    round_ties_even_i32(f64::from(-position.y_advance) * sy),
                ),
            };
            let cluster = &mut out.clusters[item.start + k];
            cluster.advance.x = cluster.advance.x.wrapping_add(glyph.advance.x);
            cluster.advance.y = cluster.advance.y.wrapping_add(glyph.advance.y);
            out.glyphs[counts[k]] = glyph;
            counts[k] += 1;
        }
        Ok(())
    }
}

/// libass's features: `vert` and `vkna` for `@` fonts, `kern` by the
/// header, `liga` and `clig` without letter spacing.
fn plan_features([vertical, kerning, ligatures]: [bool; 3]) -> [Feature; 5] {
    let feature = |tag: &[u8; 4], on: bool| Feature {
        tag: Tag::new(tag),
        value: u32::from(on),
    };
    [
        feature(b"vert", vertical),
        feature(b"vkna", vertical),
        feature(b"kern", kerning),
        feature(b"liga", ligatures),
        feature(b"clig", ligatures),
    ]
}

/// `ass_shaper_determine_script`: Common and Inherited scripts take the
/// script before them, or after them at the start.
fn inherit_scripts(items: &mut [Itemised]) {
    let inherits = |s: WritingScript| s == WritingScript::COMMON || s == WritingScript::INHERITED;
    let mut last = WritingScript::UNKNOWN;
    let mut backwards = false;
    for item in items.iter_mut() {
        if inherits(item.script) {
            if last == WritingScript::UNKNOWN {
                backwards = true;
            } else {
                item.script = last;
            }
        } else {
            last = item.script;
        }
    }
    if !backwards {
        return;
    }
    let mut last = WritingScript::UNKNOWN;
    for item in items.iter_mut().rev() {
        if inherits(item.script) {
            if last != WritingScript::UNKNOWN {
                item.script = last;
            }
        } else {
            last = item.script;
        }
    }
}

impl Bidi {
    /// `ass_shaper_reorder`: the character shown at each position into
    /// `order`, each line reordered on its own and, without whole-text
    /// layout, each run and letter-spaced character within it; `starts_run`
    /// tells where runs start once lines are trimmed. The levels are
    /// reordered in `levels`, a copy.
    pub(super) fn visual_order(
        &self,
        text: &Text,
        lines: &[Line],
        starts_run: impl Fn(usize) -> bool,
        shaping: &Shaping<'_>,
        levels: &mut BudgetVec<Level>,
        order: &mut BudgetVec<u32>,
    ) -> Result<()> {
        let chars = text.chars();
        levels.clear();
        levels.try_extend_from_slice(&self.levels)?;
        order.clear();
        // An event holds fewer characters than `u32` counts: each is
        // charged to the budget as many bytes.
        order.try_extend_with(chars.len(), |i| i as u32)?;
        let mut paragraph = 0;
        for line in lines {
            let mut start = line.chars.start;
            for i in line.chars.clone() {
                let class = self.classes[i];
                let ends = i + 1 == line.chars.end
                    || class == BidiClass::B
                    || (!shaping.whole_text
                        && (starts_run(i + 1) || text.style(&chars[i]).spacing != 0.0));
                if !ends {
                    continue;
                }
                let direction = if shaping.whole_text {
                    self.directions
                        .get(paragraph)
                        .copied()
                        .unwrap_or(Direction::LeftToRight)
                } else {
                    match shaping.base {
                        BaseDirection::RightToLeft => Direction::RightToLeft,
                        _ => Direction::LeftToRight,
                    }
                };
                reorder_line(
                    &self.classes[start..=i],
                    direction,
                    &mut levels.as_mut_slice()[start..=i],
                    &mut order.as_mut_slice()[start..=i],
                )?;
                start = i + 1;
                if shaping.whole_text && class == BidiClass::B {
                    paragraph += 1;
                }
            }
        }
        Ok(())
    }
}
