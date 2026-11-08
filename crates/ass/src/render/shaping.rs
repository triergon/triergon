//! The second stage: an event's text shaped as libass's complex shaper does
//! (`ass_shaper_find_runs`, `ass_shaper_shape`): each character's script and
//! face, runs of one face, script and style, bidi levels per paragraph, and
//! OpenType shaping of each run at the drawn size, which gives each
//! character a cluster of glyphs. The bidi levels give the visual order of
//! each line once lines are known (`ass_shaper_reorder`).

use std::collections::HashMap;
use std::ops::Range;

use triergon_core::{Budget, BudgetVec, Reservation, Result, WorkBudget, round_ties_even_i32};
use triergon_font::GlyphId;
use triergon_raster::{Vector, pixels_26_6};
use triergon_shape::bidi::{BaseDirection, BidiClass, Level, Resolver, reorder_line};
use triergon_shape::{
    Buffer, Direction, Face, Feature, Language, Plan, Script as WritingScript, SizedMetrics, Tag,
};

use super::fonts::Library;
use super::text::{DRAWN_SIZE, Text};
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
    /// The face of the character's font its glyphs come from.
    pub(super) face: usize,
    /// Its glyphs in [`Shaped::glyphs`]: always one at least, its own glyph
    /// from the character map when shaping gave it none.
    pub(super) glyphs: Range<usize>,
    /// The sum of the glyphs' advances.
    pub(super) advance: Vector,
    /// Shaping gave the character a glyph; a drawing always has one. A
    /// character a ligature took or HarfBuzz hid is not shown (libass's
    /// `skip`).
    pub(super) visible: bool,
}

/// An event shaped: what the next stage lays out.
#[derive(Debug)]
pub(super) struct Shaped {
    /// Each character's cluster, in text order.
    pub(super) clusters: BudgetVec<Cluster>,
    /// The glyphs of every cluster, cluster by cluster.
    pub(super) glyphs: BudgetVec<ShapedGlyph>,
}

impl Shaped {
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

/// The shaper's reusable state: a buffer, a bidi resolver, the plans built
/// so far, and storage for the event being shaped.
#[derive(Debug)]
pub(super) struct Shaper {
    budget: Budget,
    buffer: Buffer,
    resolver: Resolver,
    plans: HashMap<PlanKey, Plan>,
    charge: Reservation,
    /// The event's text as HarfBuzz reads it.
    text: BudgetVec<char>,
    /// What finding runs gave each character.
    items: BudgetVec<Itemised>,
    /// The glyphs shaping gave, in the order it gave them.
    output: BudgetVec<Output>,
}

/// What finding runs gives a character (`ass_shaper_find_runs`).
#[derive(Clone, Copy, Debug)]
struct Itemised {
    script: WritingScript,
    /// The face of its font it is shaped with.
    face: usize,
    /// Its glyph in that face, as the character map gives it.
    glyph: GlyphId,
    /// The run of one face, script and style it is shaped in.
    run: u32,
}

/// One glyph shaping gave, for the character `char`.
#[derive(Clone, Copy, Debug)]
struct Output {
    char: usize,
    /// Its place in the output, which orders a cluster's glyphs.
    order: usize,
    glyph: ShapedGlyph,
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
            buffer: Buffer::new(budget),
            resolver: Resolver::new(budget),
            plans: HashMap::new(),
            charge: Reservation::empty(budget),
            text: BudgetVec::new(budget),
            items: BudgetVec::new(budget),
            output: BudgetVec::new(budget),
        }
    }

    /// Shapes `text`: its clusters, and the bidi levels its lines are later
    /// reordered by.
    pub(super) fn shape(
        &mut self,
        text: &Text<'_>,
        library: &mut Library,
        shaping: &Shaping<'_>,
        work: &mut WorkBudget,
    ) -> Result<(Shaped, Bidi)> {
        self.find_runs(text, library, shaping.whole_text, work)?;
        self.text.clear();
        self.text.try_reserve_exact(text.chars.len())?;
        for c in text.chars.iter() {
            self.text.try_push(scalar(c.symbol)).map_err(|e| e.error)?;
        }
        let bidi = self.resolve_levels(text, shaping)?;
        self.shape_runs(text, library, shaping, &bidi.levels, work)?;
        Ok((self.clusters(text)?, bidi))
    }

    /// `ass_shaper_find_runs`: scripts, faces and shape runs.
    fn find_runs(
        &mut self,
        text: &Text<'_>,
        library: &mut Library,
        whole_text: bool,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let chars = text.chars.as_slice();
        let items = &mut self.items;
        items.clear();
        items.try_reserve_exact(chars.len())?;
        for c in chars {
            // HarfBuzz gives no script to what is not a scalar value.
            let script = char::from_u32(c.symbol).map_or(WritingScript::UNKNOWN, WritingScript::of);
            items
                .try_push(Itemised {
                    script,
                    face: 0,
                    glyph: GlyphId(0),
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
                (items[i].face, items[i].glyph) = library.glyph(c.font, c.symbol, work)?;
            }
            if i > 0 {
                let (previous, last) = (&chars[i - 1], items[i - 1]);
                if previous.font != c.font
                    || (!ignorable && last.face != items[i].face)
                    || last.script != items[i].script
                    || c.starts_run
                    || (!whole_text && c.style.spacing != 0.0)
                    || previous.style.decorations != c.style.decorations
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
    fn resolve_levels(&mut self, text: &Text<'_>, shaping: &Shaping<'_>) -> Result<Bidi> {
        let len = self.text.len();
        let mut bidi = Bidi {
            classes: BudgetVec::new(&self.budget),
            levels: BudgetVec::new(&self.budget),
            directions: BudgetVec::new(&self.budget),
        };
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
        let chars = text.chars.as_slice();
        let mut start = 0;
        for i in 0..len {
            let ends = i == len - 1
                || bidi.classes[i] == BidiClass::B
                || (!shaping.whole_text
                    && (chars[i + 1].starts_run || chars[i].style.spacing != 0.0));
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
        Ok(bidi)
    }

    /// `shape_harfbuzz`: each item of one shape run and bidi level shaped,
    /// its glyphs kept in the order shaping gives them.
    fn shape_runs(
        &mut self,
        text: &Text<'_>,
        library: &Library,
        shaping: &Shaping<'_>,
        levels: &[Level],
        work: &mut WorkBudget,
    ) -> Result<()> {
        let chars = text.chars.as_slice();
        let len = chars.len();
        self.output.clear();
        let mut i = 0;
        while i < len {
            if chars[i].drawing.is_some() {
                i += 1;
                continue;
            }
            let offset = i;
            let (run, level) = (self.items[offset].run, levels[offset]);
            while i + 1 < len && self.items[i + 1].run == run && levels[i + 1] == level {
                i += 1;
            }
            let item = offset..i + 1;
            let first = &chars[offset];
            let font = library.get(first.font);
            let face_font = &font.faces()[self.items[offset].face];
            let direction = level.direction();
            let script = self.items[offset].script;
            let language = language_code(shaping.language, script);
            // Ligatures only without letter spacing, vertical forms for `@`
            // fonts.
            let features = [font.vertical(), shaping.kerning, first.style.spacing == 0.0];
            let context = if shaping.whole_text {
                0..len
            } else {
                let lead = usize::from(
                    offset > 0
                        && !chars[offset].starts_run
                        && is_shaping_control(chars[offset - 1].symbol),
                );
                let trail = usize::from(
                    i + 1 < len
                        && !chars[i + 1].starts_run
                        && is_shaping_control(chars[i + 1].symbol),
                );
                offset - lead..i + 1 + trail
            };
            // A call shapes the item and reads at most `CONTEXT` characters of
            // the context on either side (HarfBuzz's buffer context), however
            // much of the text is handed to it.
            let read = item.len()
                + (item.start - context.start).min(CONTEXT)
                + (context.end - item.end).min(CONTEXT);
            charge(work, read.saturating_add(RUN_UNITS))?;
            let sized = face_font.font.at_size(pixels_26_6(DRAWN_SIZE))?;
            let metrics = SizedMetrics::new(sized)?;
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
                let code = language.split(|&b| b == 0).next().unwrap_or_default();
                let plan = Plan::new(
                    &shaper_face,
                    direction,
                    script,
                    &Language::new(std::str::from_utf8(code).unwrap_or("c"))?,
                    &plan_features(features),
                    &self.budget,
                )?;
                self.plans.insert(key, plan);
                self.charge
                    .resize_to(self.plans.len().saturating_mul(PLAN_ENTRY_BYTES))?;
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
            self.output.try_reserve_exact(self.buffer.len())?;
            for (shaped, position) in self.buffer.glyphs() {
                let char = base + shaped.cluster as usize;
                let Some(c) = chars.get(char) else { continue };
                let [sx, sy] = c.style.scale;
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
                let order = self.output.len();
                self.output
                    .try_push(Output { char, order, glyph })
                    .map_err(|e| e.error)?;
            }
            i += 1;
        }
        Ok(())
    }

    /// Each character's cluster: the glyphs shaping gave it, in the order it
    /// gave them, else its own glyph.
    fn clusters(&mut self, text: &Text<'_>) -> Result<Shaped> {
        let chars = text.chars.as_slice();
        let output = self.output.as_mut_slice();
        output.sort_unstable_by_key(|o| (o.char, o.order));
        let mut shaped = Shaped {
            clusters: BudgetVec::new(&self.budget),
            glyphs: BudgetVec::new(&self.budget),
        };
        shaped.clusters.try_reserve_exact(chars.len())?;
        shaped
            .glyphs
            .try_reserve_exact(chars.len().max(output.len()))?;
        let mut output = output.iter().peekable();
        for (i, (c, item)) in chars.iter().zip(self.items.iter()).enumerate() {
            let start = shaped.glyphs.len();
            let mut advance = Vector::default();
            while let Some(o) = output.next_if(|o| o.char == i) {
                advance.x = advance.x.wrapping_add(o.glyph.advance.x);
                advance.y = advance.y.wrapping_add(o.glyph.advance.y);
                shaped.glyphs.try_push(o.glyph).map_err(|e| e.error)?;
            }
            let visible = c.drawing.is_some() || shaped.glyphs.len() > start;
            if shaped.glyphs.len() == start {
                let own = ShapedGlyph {
                    id: item.glyph,
                    ..ShapedGlyph::default()
                };
                shaped.glyphs.try_push(own).map_err(|e| e.error)?;
            }
            shaped
                .clusters
                .try_push(Cluster {
                    face: item.face,
                    glyphs: start..shaped.glyphs.len(),
                    advance,
                    visible,
                })
                .map_err(|e| e.error)?;
        }
        Ok(shaped)
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
    /// `ass_shaper_reorder`: the character shown at each position, each line
    /// reordered on its own and, without whole-text layout, each run and
    /// letter-spaced character within it; `starts_run` tells where runs start
    /// once lines are trimmed.
    pub(super) fn visual_order(
        mut self,
        text: &Text<'_>,
        lines: &[Line],
        starts_run: impl Fn(usize) -> bool,
        shaping: &Shaping<'_>,
    ) -> Result<BudgetVec<usize>> {
        let chars = text.chars.as_slice();
        let mut order = BudgetVec::new(self.classes.budget());
        order.try_extend_with(chars.len(), |i| i)?;
        let mut paragraph = 0;
        for line in lines {
            let mut start = line.chars.start;
            for i in line.chars.clone() {
                let class = self.classes[i];
                let ends = i + 1 == line.chars.end
                    || class == BidiClass::B
                    || (!shaping.whole_text
                        && (starts_run(i + 1) || chars[i].style.spacing != 0.0));
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
                    &mut self.levels.as_mut_slice()[start..=i],
                    &mut order.as_mut_slice()[start..=i],
                )?;
                start = i + 1;
                if shaping.whole_text && class == BidiClass::B {
                    paragraph += 1;
                }
            }
        }
        Ok(order)
    }
}
