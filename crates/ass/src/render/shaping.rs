//! Shaping an event's glyphs as libass's complex shaper does
//! (`ass_shaper_find_runs`, `ass_shaper_shape`, `ass_shaper_reorder`): each
//! glyph's script and face, runs of one face, script and style, bidi levels
//! per paragraph, OpenType shaping of each run at the drawn size, and the
//! visual order of each line.

use std::collections::HashMap;

use triergon_core::{Budget, BudgetVec, Reservation, Result, round_ties_even_i32};
use triergon_raster::pixels_26_6;
use triergon_shape::bidi::{BaseDirection, BidiClass, Level, Resolver, reorder_line};
use triergon_shape::{
    Buffer, Direction, Face, Feature, Language, Plan, Script as WritingScript, SizedMetrics, Tag,
};

use super::fonts::Library;
use super::text::{Break, DRAWN_SIZE, Glyphs};

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

impl Shaping<'_> {
    /// The settings for an event whose last `\fe` is `encoding`.
    pub(super) fn base_for(encoding: i32) -> (BaseDirection, bool) {
        if encoding == -1 {
            (BaseDirection::Auto, true)
        } else {
            (BaseDirection::LeftToRight, false)
        }
    }
}

/// The bidi state of a shaped event: every glyph's class and level, and the
/// direction of each paragraph.
#[derive(Debug)]
pub(super) struct Bidi {
    pub(super) classes: BudgetVec<BidiClass>,
    pub(super) levels: BudgetVec<Level>,
    pub(super) directions: BudgetVec<Direction>,
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

/// Plans kept at most; more are built again when needed.
const MAX_PLANS: usize = 256;
/// Bytes charged per plan beyond its own storage.
const PLAN_ENTRY_BYTES: usize = 256;

/// The shaper's reusable state: a buffer, a bidi resolver, the event text and
/// the plans built so far.
#[derive(Debug)]
pub(super) struct Shaper {
    budget: Budget,
    buffer: Buffer,
    resolver: Resolver,
    text: BudgetVec<char>,
    plans: HashMap<PlanKey, Plan>,
    charge: Reservation,
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
            text: BudgetVec::new(budget),
            plans: HashMap::new(),
            charge: Reservation::empty(budget),
        }
    }

    /// `ass_shaper_find_runs`: scripts, ignorables, faces and shape runs.
    pub(super) fn find_runs(
        &self,
        glyphs: &mut Glyphs<'_>,
        library: &mut Library,
        whole_text: bool,
    ) -> Result<()> {
        determine_scripts(glyphs);
        let roots = glyphs.roots_mut();
        for glyph in roots.iter_mut() {
            if is_ignorable(glyph.symbol) {
                glyph.skip = true;
            }
        }
        let mut run = 0;
        for i in 0..roots.len() {
            if !roots[i].is_drawing() && !roots[i].skip {
                let (face, glyph) = library.glyph(roots[i].font, roots[i].symbol)?;
                roots[i].face = face;
                roots[i].glyph = glyph;
            }
            if i > 0 {
                let (last, info) = (&roots[i - 1], &roots[i]);
                if last.font != info.font
                    || (!info.skip && last.face != info.face)
                    || last.script != info.script
                    || info.starts_new_run
                    || (!whole_text && info.spacing != 0.0)
                    || last.decorations != info.decorations
                {
                    run += 1;
                } else if info.skip {
                    roots[i].face = roots[i - 1].face;
                }
            }
            roots[i].shape_run = run;
        }
        Ok(())
    }

    /// `ass_shaper_shape`: the bidi levels and the complex shaping of every
    /// run. Fills each glyph's glyph index, advance and offset in drawn-size
    /// 26.6 units scaled by the glyph's scale, and adds the extra glyphs of
    /// clusters.
    pub(super) fn shape(
        &mut self,
        glyphs: &mut Glyphs<'_>,
        library: &Library,
        shaping: &Shaping<'_>,
    ) -> Result<Bidi> {
        let len = glyphs.len;
        self.text.clear();
        self.text.try_reserve_exact(len)?;
        for g in glyphs.roots() {
            self.text.try_push(scalar(g.symbol)).map_err(|e| e.error)?;
        }
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
        // Levels are resolved one paragraph at a time; without whole-text
        // layout, each run and each letter-spaced glyph is a paragraph.
        let mut start = 0;
        for i in 0..len {
            let roots = glyphs.roots();
            let ends = i == len - 1
                || bidi.classes.as_slice()[i] == BidiClass::B
                || (!shaping.whole_text
                    && (roots[i + 1].starts_new_run || roots[i].spacing != 0.0));
            if ends {
                let direction = self.resolver.resolve(
                    &bidi.classes.as_slice()[start..=i],
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
        self.shape_runs(glyphs, library, shaping, bidi.levels.as_slice())?;
        Ok(bidi)
    }

    /// `shape_harfbuzz`.
    fn shape_runs(
        &mut self,
        glyphs: &mut Glyphs<'_>,
        library: &Library,
        shaping: &Shaping<'_>,
        levels: &[Level],
    ) -> Result<()> {
        let len = glyphs.len;
        for glyph in glyphs.roots_mut() {
            glyph.skip = true;
        }
        let mut i = 0;
        while i < len {
            if glyphs.get(i).is_drawing() {
                glyphs.get_mut(i).skip = false;
                i += 1;
                continue;
            }
            let offset = i;
            let first = glyphs.get(offset);
            let (run, level) = (first.shape_run, levels[offset]);
            while i + 1 < len && glyphs.get(i + 1).shape_run == run && levels[i + 1] == level {
                i += 1;
            }
            let item = offset..i + 1;
            let first = glyphs.get(offset);
            let font = library.get(first.font);
            let face_font = &font.faces()[first.face];
            let direction = level.direction();
            let script = first.script;
            let language = language_code(shaping.language, script);
            // Ligatures only without letter spacing, vertical forms for `@`
            // fonts.
            let features = [font.vertical(), shaping.kerning, first.spacing == 0.0];
            let context = if shaping.whole_text {
                0..len
            } else {
                let roots = glyphs.roots();
                let lead = usize::from(
                    offset > 0
                        && !roots[offset].starts_new_run
                        && is_shaping_control(roots[offset - 1].symbol),
                );
                let trail = usize::from(
                    i + 1 < len
                        && !roots[i + 1].starts_new_run
                        && is_shaping_control(roots[i + 1].symbol),
                );
                offset - lead..i + 1 + trail
            };
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
            let plan = &self.plans[&key];
            let base = context.start;
            triergon_shape::shape(
                &shaper_face,
                &metrics,
                plan,
                &self.text.as_slice()[context],
                item.start - base..item.end - base,
                &mut self.buffer,
            )?;
            for (shaped, position) in self.buffer.glyphs() {
                let root = base + shaped.cluster as usize;
                let target = if glyphs.get(root).skip {
                    root
                } else {
                    glyphs.extend_cluster(root)?
                };
                let info = glyphs.get_mut(target);
                info.skip = false;
                info.glyph = shaped.id;
                let [sx, sy] = info.scale;
                info.offset.x = round_ties_even_i32(f64::from(position.x_offset) * sx);
                info.offset.y = round_ties_even_i32(f64::from(-position.y_offset) * sy);
                info.advance.x = round_ties_even_i32(f64::from(position.x_advance) * sx);
                info.advance.y = round_ties_even_i32(f64::from(-position.y_advance) * sy);
                let advance = info.advance;
                let root = glyphs.get_mut(root);
                root.cluster_advance.x = root.cluster_advance.x.wrapping_add(advance.x);
                root.cluster_advance.y = root.cluster_advance.y.wrapping_add(advance.y);
            }
            i += 1;
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

/// `ass_shaper_determine_script`: each glyph's script, with Common and
/// Inherited ones taking the script before them, or after them at the start.
fn determine_scripts(glyphs: &mut Glyphs<'_>) {
    let roots = glyphs.roots_mut();
    let inherits = |s: WritingScript| s == WritingScript::COMMON || s == WritingScript::INHERITED;
    let mut last = WritingScript::UNKNOWN;
    let mut backwards = false;
    for glyph in roots.iter_mut() {
        // HarfBuzz gives no script to what is not a scalar value.
        glyph.script =
            char::from_u32(glyph.symbol).map_or(WritingScript::UNKNOWN, WritingScript::of);
        if inherits(glyph.script) {
            if last == WritingScript::UNKNOWN {
                backwards = true;
            } else {
                glyph.script = last;
            }
        } else {
            last = glyph.script;
        }
    }
    if !backwards {
        return;
    }
    let mut last = WritingScript::UNKNOWN;
    for glyph in roots.iter_mut().rev() {
        if inherits(glyph.script) {
            if last != WritingScript::UNKNOWN {
                glyph.script = last;
            }
        } else {
            last = glyph.script;
        }
    }
}

/// `ass_shaper_reorder`: the logical index shown at each position, each line
/// (and, without whole-text layout, each run and letter-spaced glyph)
/// reordered on its own.
pub(super) fn reorder(
    glyphs: &Glyphs<'_>,
    bidi: &mut Bidi,
    shaping: &Shaping<'_>,
    map: &mut BudgetVec<usize>,
) -> Result<()> {
    let len = glyphs.len;
    map.clear();
    map.try_reserve_exact(len)?;
    for i in 0..len {
        map.try_push(i).map_err(|e| e.error)?;
    }
    let roots = glyphs.roots();
    let mut start = 0;
    let mut paragraph = 0;
    for i in 0..len {
        let class = bidi.classes.as_slice()[i];
        let ends = i == len - 1
            || roots[i + 1].linebreak != Break::None
            || class == BidiClass::B
            || (!shaping.whole_text && (roots[i + 1].starts_new_run || roots[i].spacing != 0.0));
        if !ends {
            continue;
        }
        let direction = if shaping.whole_text {
            bidi.directions
                .as_slice()
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
            &bidi.classes.as_slice()[start..=i],
            direction,
            &mut bidi.levels.as_mut_slice()[start..=i],
            &mut map.as_mut_slice()[start..=i],
        )?;
        start = i + 1;
        if shaping.whole_text && class == BidiClass::B {
            paragraph += 1;
        }
    }
    Ok(())
}
