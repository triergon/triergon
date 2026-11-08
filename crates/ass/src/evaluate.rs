//! Evaluating an event at one time: libass's render context, the state its
//! override tags change, and the walk over the event text that gives each
//! glyph a copy of it (`init_render_context`, `ass_parse_tags` and the text
//! loop of `parse_events`).
//!
//! Values are in script units: sizes, borders, shadows and positions in
//! script pixels, before the renderer's screen scaling.

use triergon_core::{Error, Result, WorkBudget};

use crate::effect::{Scroll, transition};
use crate::number::dtoi32;
use crate::tags::{Clip, Fade, FontSize, Karaoke, Move, Tag, Tags, TransformTimes, VectorClip};
use crate::text::next_char;
use crate::{
    Alignment, Bold, BorderStyle, ColourSlot, Colours, Event, Italic, Justify, Rgba, Script, Style,
    WrapStyle, charge,
};

/// The karaoke values a glyph carries from the tags just before it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct KaraokeMark {
    /// The last karaoke tag, if any.
    pub kind: Option<Karaoke>,
    /// The syllable's duration in milliseconds.
    pub timing: i32,
    /// Milliseconds before the syllable starts.
    pub skip_timing: i32,
    /// `\kt` reset the time base.
    pub reset: bool,
}

/// The per-glyph state override tags change, as each glyph copies it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlyphState<'s> {
    /// The font name, with a leading `@` for vertical text.
    pub family: &'s [u8],
    /// The weight.
    pub bold: Bold,
    /// The slant.
    pub italic: Italic,
    /// The font size in script pixels.
    pub font_size: f64,
    /// The four colours.
    pub colours: Colours,
    /// Underlined.
    pub underline: bool,
    /// Struck out.
    pub strike_out: bool,
    /// The kind of border.
    pub border_style: BorderStyle,
    /// The horizontal border width.
    pub border_x: f64,
    /// The vertical border width.
    pub border_y: f64,
    /// The horizontal scale; 1.0 is 100%.
    pub scale_x: f64,
    /// The vertical scale.
    pub scale_y: f64,
    /// Extra space between letters.
    pub spacing: f64,
    /// `\be`: passes of edge blur, 0 to 127.
    pub blur_edges: u8,
    /// `\blur`: the Gaussian blur radius, 0 to 100.
    pub blur: f64,
    /// The horizontal shadow offset.
    pub shadow_x: f64,
    /// The vertical shadow offset.
    pub shadow_y: f64,
    /// `\frx` in degrees.
    pub rotate_x: f64,
    /// `\fry` in degrees.
    pub rotate_y: f64,
    /// `\frz` in degrees.
    pub rotate_z: f64,
    /// `\fax`.
    pub shear_x: f64,
    /// `\fay`.
    pub shear_y: f64,
    /// `\fe`: the font charset.
    pub encoding: i32,
}

/// The font a glyph asks for, as libass's `ass_update_font` describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FontDesc<'s> {
    /// The family name, without the `@` of vertical text.
    pub family: &'s [u8],
    /// The family had a leading `@`.
    pub vertical: bool,
    /// The weight, [`Bold::weight`].
    pub weight: u32,
    /// The slant, [`Italic::slant`].
    pub slant: u32,
}

impl<'s> GlyphState<'s> {
    /// The font this glyph is drawn with.
    pub fn font(&self) -> FontDesc<'s> {
        let (family, vertical) = match self.family.split_first() {
            Some((b'@', rest)) => (rest, true),
            _ => (self.family, false),
        };
        FontDesc {
            family,
            vertical,
            weight: self.bold.weight(),
            slant: self.italic.slant(),
        }
    }

    /// libass's `ass_reset_render_context`: everything `\r` resets.
    fn reset(style: &'s Style) -> Self {
        Self {
            family: style.font_name(),
            bold: style.bold,
            italic: style.italic,
            font_size: style.font_size,
            colours: style.colours,
            underline: style.underline,
            strike_out: style.strike_out,
            border_style: style.border_style,
            border_x: style.outline,
            border_y: style.outline,
            scale_x: style.scale_x,
            scale_y: style.scale_y,
            spacing: style.spacing,
            blur_edges: 0,
            blur: style.blur,
            shadow_x: style.shadow,
            shadow_y: style.shadow,
            rotate_x: 0.0,
            rotate_y: 0.0,
            rotate_z: style.angle,
            shear_x: 0.0,
            shear_y: 0.0,
            encoding: style.encoding,
        }
    }
}

/// Where the event goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Placement {
    /// By alignment and margins.
    Margins,
    /// At the point `\pos` or `\move` gave.
    Positioned(f64, f64),
}

/// The event-wide state tags change.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EventState<'s> {
    /// Where the event is anchored.
    pub alignment: Alignment,
    /// How its lines are justified.
    pub justify: Justify,
    /// How its lines wrap.
    pub wrap_style: WrapStyle,
    /// Where it goes.
    pub placement: Placement,
    /// `\org`.
    pub origin: Option<(f64, f64)>,
    /// The rectangular clip, the play resolution unless a tag sets one.
    pub clip: Clip,
    /// The first vector clip.
    pub vector_clip: Option<VectorClip<'s>>,
    /// `Banner` or `Scroll` from the effect field.
    pub scroll: Option<Scroll>,
    /// Whether the event takes part in collision handling: not after `\pos`,
    /// `\move`, `\org`, `\t` or a scrolling effect.
    pub detect_collisions: bool,
    /// libass's `explicit`: a scrolling effect, or a `\pos`, `\move`,
    /// `\clip`, `\iclip`, `\org` or `\p...` anywhere in the text.
    pub explicit: bool,
    /// The alpha of `\fad`/`\fade` now, 0 to 255 (when positive it is
    /// applied over each glyph's alphas).
    pub fade: i32,
    /// The current `\p` level, 0 for text.
    pub drawing_scale: i32,
    /// The current `\pbo`.
    pub drawing_baseline: f64,
}

/// What a glyph shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Symbol<'s> {
    /// A character code; libass's lenient UTF-8 may yield any `u32`.
    Char(u32),
    /// Drawing commands, which libass shapes as U+FFFC.
    Drawing {
        /// The commands.
        text: &'s [u8],
        /// The `\p` level in force.
        scale: i32,
        /// The `\pbo` in force, truncated to an integer as libass stores it
        /// per glyph.
        baseline: i32,
    },
}

/// One glyph of an event with the state it was given.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Glyph<'s> {
    /// What it shows.
    pub symbol: Symbol<'s>,
    /// Byte offset of the symbol in the event text.
    pub offset: usize,
    /// The state of the tags before it.
    pub state: GlyphState<'s>,
    /// The karaoke tags just before it.
    pub karaoke: KaraokeMark,
    /// The event's fade alpha when the glyph was read.
    pub fade: i32,
}

/// What a walk's last scan for a `}` found.
#[derive(Clone, Copy, Debug)]
enum Brace {
    /// No scan yet.
    Unscanned,
    /// The first `}` from where the scan started.
    At(usize),
    /// None until the end of the text.
    None,
}

/// An event being evaluated at one time.
///
/// ```
/// use triergon_ass::{Context, Script, Symbol};
/// use triergon_core::Budget;
///
/// let script = b"[Script Info]\nScriptType: v4.00+\n[Events]\n\
///     Format: Layer, Start, End, Style, Text\n\
///     Dialogue: 0,0:00:00.00,0:00:02.00,Default,{\\t(\\fs40)}Hi";
/// let budget = Budget::new(1 << 20)?;
/// let script = Script::parse(&budget, script, &Default::default())?;
/// let event = &script.events()[0];
/// let mut context = Context::new(&script, event, 1000, script.info().play_res())?;
/// let glyph = context.next_glyph()?.ok_or("no glyph")?;
/// assert_eq!(glyph.symbol, Symbol::Char('H' as u32));
/// assert_eq!(glyph.state.font_size, 29.0); // halfway from 18 to 40
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct Context<'s> {
    script: &'s Script,
    event: &'s Event,
    text: &'s [u8],
    /// Milliseconds since the event start.
    elapsed: i64,
    style: &'s Style,
    glyph: GlyphState<'s>,
    karaoke: KaraokeMark,
    state: EventState<'s>,
    alignment_set: bool,
    fade_set: bool,
    at: usize,
    /// What the last scan for `}` found. The walk only moves forward, so
    /// each byte is scanned for a `}` at most once.
    close: Brace,
    work: WorkBudget,
}

impl<'s> Context<'s> {
    /// Starts `event` of `script` at `now` (milliseconds), with the work
    /// bound of the script's [`Limits`](crate::Limits). `layout_res` is the
    /// renderer's layout resolution (the play resolution unless a storage
    /// size or pixel aspect says otherwise), which scrolling effects need.
    ///
    /// # Errors
    ///
    /// `Invalid` when the event's style is not one of the script's; `Policy`
    /// when its text exceeds the script's work bound.
    pub fn new(
        script: &'s Script,
        event: &'s Event,
        now: i64,
        layout_res: (i32, i32),
    ) -> Result<Self> {
        let style = script
            .styles()
            .get(event.style)
            .ok_or(Error::Invalid("event style"))?;
        let mut work = script.limits().work();
        let info = script.info();
        let play_res = info.play_res();
        let elapsed = now.wrapping_sub(event.start);
        let text = event.text();
        // The effect field and the scan for hard overrides.
        charge(
            &mut work,
            event.effect().map_or(0, <[u8]>::len) + text.len(),
        )?;
        let scroll = event
            .effect()
            .and_then(|effect| transition(effect, elapsed, play_res, layout_res));
        let state = EventState {
            alignment: style.alignment,
            justify: style.justify,
            wrap_style: if scroll.is_some_and(|s| s.is_horizontal()) {
                WrapStyle::Manual
            } else {
                info.wrap_style
            },
            placement: Placement::Margins,
            origin: None,
            clip: Clip {
                x0: 0,
                y0: 0,
                x1: play_res.0,
                y1: play_res.1,
                inverse: false,
            },
            vector_clip: None,
            scroll,
            detect_collisions: scroll.is_none(),
            explicit: scroll.is_some() || has_hard_overrides(text),
            fade: 0,
            drawing_scale: 0,
            drawing_baseline: 0.0,
        };
        Ok(Self {
            script,
            event,
            text,
            elapsed,
            style,
            glyph: GlyphState::reset(style),
            karaoke: KaraokeMark::default(),
            state,
            alignment_set: false,
            fade_set: false,
            at: 0,
            close: Brace::Unscanned,
            work,
        })
    }

    /// The event-wide state as the tags read so far left it.
    pub const fn state(&self) -> &EventState<'s> {
        &self.state
    }

    /// The style in force: the event's, or the last `\r` style. Its margins
    /// apply where the event's are 0.
    pub const fn style(&self) -> &'s Style {
        self.style
    }

    /// The next glyph, after applying the override blocks before it; `None`
    /// at the end of the text. A `{` without a closing `}` is text.
    ///
    /// # Errors
    ///
    /// `Policy` when the walk exceeds the script's work bound.
    pub fn next_glyph(&mut self) -> Result<Option<Glyph<'s>>> {
        let text = self.text;
        loop {
            let Some(&first) = text.get(self.at) else {
                return Ok(None);
            };
            let offset = self.at;
            if first == b'{'
                && let Some(close) = self.closing_brace(offset)?
            {
                charge(&mut self.work, close - offset + 1)?;
                self.apply(Tags::new(text, offset..close), 1.0, false)?;
                self.at = close + 1;
                continue;
            }
            charge(&mut self.work, 1)?;
            let symbol = if self.state.drawing_scale != 0 {
                let start = offset + usize::from(first == b'{');
                let end = text[start..]
                    .iter()
                    .position(|&c| c == b'{')
                    .map_or(text.len(), |i| start + i);
                charge(&mut self.work, end - start)?;
                self.at = end;
                Symbol::Drawing {
                    text: &text[offset..end],
                    scale: self.state.drawing_scale,
                    baseline: self.state.drawing_baseline as i32,
                }
            } else {
                let (code, used) = next_char(&text[offset..], self.state.wrap_style);
                self.at += used;
                if code == 0 {
                    self.at = text.len();
                    return Ok(None);
                }
                Symbol::Char(code)
            };
            let glyph = Glyph {
                symbol,
                offset,
                state: self.glyph,
                karaoke: self.karaoke,
                fade: self.state.fade,
            };
            self.karaoke = KaraokeMark::default();
            return Ok(Some(glyph));
        }
    }

    /// The first `}` at or after `from`, as libass's `strchr` finds it. A
    /// `}` found earlier, or the absence of one, still answers for any later
    /// `from` up to it, so the scan and its charge stay linear in the text.
    fn closing_brace(&mut self, from: usize) -> Result<Option<usize>> {
        match self.close {
            Brace::None => return Ok(None),
            Brace::At(close) if close >= from => return Ok(Some(close)),
            _ => {}
        }
        let rest = &self.text[from..];
        let found = rest.iter().position(|&c| c == b'}');
        charge(&mut self.work, found.map_or(rest.len(), |i| i + 1))?;
        let close = found.map(|i| from + i);
        self.close = close.map_or(Brace::None, Brace::At);
        Ok(close)
    }

    /// libass's `ass_parse_tags` over `tags` with animation power `pwr`.
    /// Inside a `\t` (`nested`), a further `\t` sets the power for the tags
    /// after it; its own tags then always run to the end of the block, so
    /// they continue in place rather than recursing, and the recursion is at
    /// most one level deep.
    fn apply(&mut self, mut tags: Tags<'s>, mut pwr: f64, mut nested: bool) -> Result<()> {
        while let Some(tag) = tags.next() {
            charge(&mut self.work, 1)?;
            if let Tag::Reset(Some(_)) = tag {
                // The named style is looked up among all of them.
                charge(&mut self.work, self.script.styles().len())?;
            }
            if let Tag::Transform { times, tags: body } = tag {
                let k = self.transform_power(times);
                if nested {
                    pwr = k;
                }
                if let Some(body) = body {
                    if !nested && !tags.is_exhausted() {
                        self.apply(body, k, true)?;
                    } else {
                        (tags, pwr, nested) = (body, k, true);
                    }
                }
                continue;
            }
            self.apply_tag(tag, pwr);
        }
        Ok(())
    }

    /// The progress of a `\t` now: 0 before `t1`, 1 from `t2`, else the
    /// fraction raised to `accel`.
    fn transform_power(&mut self, times: TransformTimes) -> f64 {
        self.state.detect_collisions = false;
        let t1 = times.t1;
        let t2 = if times.t2 == 0 {
            self.event.duration as i32
        } else {
            times.t2
        };
        let t = self.elapsed as i32;
        if t < t1 {
            0.0
        } else if t >= t2 {
            1.0
        } else {
            let span = t2.wrapping_sub(t1);
            (f64::from(t.wrapping_sub(t1)) / f64::from(span)).powf(times.accel)
        }
    }

    fn apply_tag(&mut self, tag: Tag<'s>, pwr: f64) {
        let style = self.style;
        let g = &mut self.glyph;
        let mix = |old: f64, new: f64| old * (1.0 - pwr) + new * pwr;
        // C's conditional clamps, which keep a NaN.
        let at_least_zero = |v: f64| if v < 0.0 { 0.0 } else { v };
        let at_most_100 = |v: f64| if v > 100.0 { 100.0 } else { v };
        match tag {
            Tag::XBorder(v) => {
                g.border_x = v.map_or(style.outline, |v| at_least_zero(mix(g.border_x, v)));
            }
            Tag::YBorder(v) => {
                g.border_y = v.map_or(style.outline, |v| at_least_zero(mix(g.border_y, v)));
            }
            Tag::XShadow(v) => g.shadow_x = v.map_or(style.shadow, |v| mix(g.shadow_x, v)),
            Tag::YShadow(v) => g.shadow_y = v.map_or(style.shadow, |v| mix(g.shadow_y, v)),
            Tag::ShearX(v) => g.shear_x = v.map_or(0.0, |v| mix(g.shear_x, v)),
            Tag::ShearY(v) => g.shear_y = v.map_or(0.0, |v| mix(g.shear_y, v)),
            Tag::Blur(v) => g.blur = v.map_or(0.0, |v| at_most_100(at_least_zero(mix(g.blur, v)))),
            Tag::ScaleX(v) => {
                g.scale_x = v.map_or(style.scale_x, |v| at_least_zero(mix(g.scale_x, v / 100.0)));
            }
            Tag::ScaleY(v) => {
                g.scale_y = v.map_or(style.scale_y, |v| at_least_zero(mix(g.scale_y, v / 100.0)));
            }
            Tag::ScaleReset => (g.scale_x, g.scale_y) = (style.scale_x, style.scale_y),
            Tag::Spacing(v) => g.spacing = v.map_or(style.spacing, |v| mix(g.spacing, v)),
            Tag::FontSize(v) => {
                let size = match v {
                    Some(FontSize::Relative(v)) => g.font_size * (1.0 + pwr * v / 10.0),
                    Some(FontSize::Absolute(v)) => mix(g.font_size, v),
                    None => 0.0,
                };
                g.font_size = if size <= 0.0 { style.font_size } else { size };
            }
            Tag::Border(v) => {
                (g.border_x, g.border_y) = match v {
                    Some(v) => (
                        at_least_zero(mix(g.border_x, v)),
                        at_least_zero(mix(g.border_y, v)),
                    ),
                    None => (style.outline, style.outline),
                };
            }
            Tag::Shadow(v) => {
                (g.shadow_x, g.shadow_y) = match v {
                    Some(v) => (
                        at_least_zero(mix(g.shadow_x, v)),
                        at_least_zero(mix(g.shadow_y, v)),
                    ),
                    None => (style.shadow, style.shadow),
                };
            }
            Tag::RotateX(v) => g.rotate_x = v.map_or(0.0, |v| mix(g.rotate_x, v)),
            Tag::RotateY(v) => g.rotate_y = v.map_or(0.0, |v| mix(g.rotate_y, v)),
            Tag::RotateZ(v) => g.rotate_z = v.map_or(style.angle, |v| mix(g.rotate_z, v)),
            Tag::FontName(family) => g.family = family.unwrap_or(style.font_name()),
            Tag::AlphaAll(alpha) => {
                for slot in ColourSlot::ALL {
                    g.colours[slot] = match alpha {
                        Some(a) => change_alpha(g.colours[slot], a as u32, pwr),
                        None => change_alpha(g.colours[slot], style.colours[slot].a().into(), 1.0),
                    };
                }
            }
            Tag::Alpha(slot, alpha) => {
                g.colours[slot] = match alpha {
                    Some(a) => change_alpha(g.colours[slot], a as u32, pwr),
                    None => change_alpha(g.colours[slot], style.colours[slot].a().into(), 1.0),
                };
            }
            Tag::Colour(slot, colour) => {
                g.colours[slot] = match colour {
                    Some(c) => change_colour(g.colours[slot], c, pwr),
                    None => change_colour(g.colours[slot], style.colours[slot], 1.0),
                };
            }
            Tag::BlurEdges(v) => {
                g.blur_edges = v.map_or(0, |v| {
                    // VSFilter adds 0.5 even to negative values.
                    let be = dtoi32(f64::from(g.blur_edges) * (1.0 - pwr) + v * pwr + 0.5);
                    be.clamp(0, 127) as u8
                });
            }
            Tag::Bold(v) => g.bold = v.unwrap_or(style.bold),
            Tag::Italic(v) => g.italic = v.unwrap_or(style.italic),
            Tag::Underline(v) => g.underline = v.unwrap_or(style.underline),
            Tag::StrikeOut(v) => g.strike_out = v.unwrap_or(style.strike_out),
            Tag::Encoding(v) => g.encoding = v.unwrap_or(style.encoding),
            Tag::Reset(name) => {
                let named = name
                    .and_then(|name| self.script.styles().iter().rev().find(|s| s.name() == name));
                self.reset(named);
            }
            Tag::KaraokeTime(v) => {
                self.karaoke.skip_timing = dtoi32(v.map_or(0.0, |v| v * 10.0));
                self.karaoke.timing = 0;
                self.karaoke.reset = true;
            }
            Tag::Karaoke(kind, v) => {
                let k = &mut self.karaoke;
                k.kind = Some(kind);
                k.skip_timing = k.skip_timing.wrapping_add(k.timing);
                k.timing = dtoi32(v.unwrap_or(100.0) * 10.0);
            }
            tag => self.apply_event_tag(tag, pwr),
        }
    }

    /// The tags that change the event rather than the glyphs.
    fn apply_event_tag(&mut self, tag: Tag<'s>, pwr: f64) {
        let s = &mut self.state;
        let duration = self.event.duration as i32;
        match tag {
            Tag::Clip(rect) => {
                let mix = |old: i32, new: i32| {
                    (f64::from(old) * (1.0 - pwr) + f64::from(new) * pwr) as i32
                };
                let c = &mut s.clip;
                (c.x0, c.y0) = (mix(c.x0, rect.x0), mix(c.y0, rect.y0));
                (c.x1, c.y1) = (mix(c.x1, rect.x1), mix(c.y1, rect.y1));
                c.inverse = rect.inverse;
            }
            Tag::VectorClip(clip) => {
                s.vector_clip.get_or_insert(clip);
            }
            Tag::Move(m) => {
                let (x, y) = move_position(m, self.elapsed as i32, duration);
                if s.placement == Placement::Margins {
                    s.placement = Placement::Positioned(x, y);
                    s.detect_collisions = false;
                }
            }
            Tag::Pos(x, y) => {
                if s.placement == Placement::Margins {
                    s.placement = Placement::Positioned(x, y);
                    s.detect_collisions = false;
                }
            }
            Tag::Origin(x, y) => {
                if s.origin.is_none() {
                    s.origin = Some((x, y));
                    s.detect_collisions = false;
                }
            }
            Tag::Fade(fade) => {
                if !self.fade_set {
                    s.fade = fade_alpha(fade, self.elapsed, duration);
                    self.fade_set = true;
                }
            }
            Tag::Alignment(alignment) => {
                if !self.alignment_set {
                    s.alignment = alignment.unwrap_or(self.style.alignment);
                    self.alignment_set = true;
                }
            }
            Tag::WrapStyle(v) => s.wrap_style = v.unwrap_or(self.script.info().wrap_style),
            Tag::Drawing(level) => s.drawing_scale = level,
            Tag::DrawingBaseline(offset) => s.drawing_baseline = offset,
            _ => {}
        }
    }

    /// `\r`: back to `style`, or to the event's style.
    fn reset(&mut self, style: Option<&'s Style>) {
        let style = style.unwrap_or_else(|| &self.script.styles()[self.event.style]);
        self.style = style;
        self.glyph = GlyphState::reset(style);
    }
}

/// libass's `calc_anim_int32`: `(1 - pwr) * old + new * pwr` truncated by
/// `dtoi32`.
fn anim(new: u32, old: u32, pwr: f64) -> i32 {
    dtoi32((1.0 - pwr) * f64::from(old) + f64::from(new) * pwr)
}

/// libass's `change_alpha`: the low byte of the animated value.
fn change_alpha(colour: Rgba, new: u32, pwr: f64) -> Rgba {
    colour.with_alpha(anim(new, colour.a().into(), pwr) as u8)
}

/// libass's `change_color`: each of red, green and blue animated in its own
/// byte position, alpha kept.
fn change_colour(colour: Rgba, new: Rgba, pwr: f64) -> Rgba {
    let (old, new) = (colour.0.swap_bytes(), new.0.swap_bytes());
    let mixed = [0xff_0000_u32, 0xff00, 0xff]
        .into_iter()
        .fold(0, |acc, mask| {
            acc | (anim(new & mask, old & mask, pwr) as u32 & mask)
        });
    Rgba((mixed & 0xff_ffff).swap_bytes() | u32::from(colour.a()))
}

/// The position of a `\move` at `t` milliseconds into an event of `duration`.
fn move_position(m: Move, t: i32, duration: i32) -> (f64, f64) {
    let (t1, t2) = if m.t1 <= 0 && m.t2 <= 0 {
        (0, duration)
    } else {
        (m.t1, m.t2)
    };
    let k = if t <= t1 {
        0.0
    } else if t >= t2 {
        1.0
    } else {
        f64::from(t.wrapping_sub(t1)) / f64::from(t2.wrapping_sub(t1))
    };
    (
        k * (m.to.0 - m.from.0) + m.from.0,
        k * (m.to.1 - m.from.1) + m.from.1,
    )
}

/// libass's `interpolate_alpha` for `\fad`/`\fade` at `now` milliseconds into
/// an event of `duration`.
fn fade_alpha(fade: Fade, now: i64, duration: i32) -> i32 {
    let (a1, a2, a3, t1, t2, t3, t4) = match fade {
        Fade::InOut { fade_in, fade_out } => (255, 0, 255, -1, fade_in, fade_out, -1),
        Fade::Full {
            a1,
            a2,
            a3,
            t1,
            t2,
            t3,
            t4,
        } => (a1, a2, a3, t1, t2, t3, t4),
    };
    // Both outer times -1: the inner ones are durations from the event's
    // start and back from its end.
    let (t1, t3, t4) = if t1 == -1 && t4 == -1 {
        (0, duration.wrapping_sub(t3), duration)
    } else {
        (t1, t3, t4)
    };
    let fraction = |from: i32, to: i32| {
        f64::from((now as u32).wrapping_sub(from as u32) as i32)
            / f64::from((to as u32).wrapping_sub(from as u32) as i32)
    };
    let between = |a: i32, b: i32, cf: f64| (f64::from(a) * (1.0 - cf) + f64::from(b) * cf) as i32;
    if now < t1.into() {
        a1
    } else if now < t2.into() {
        between(a1, a2, fraction(t1, t2))
    } else if now < t3.into() {
        a2
    } else if now < t4.into() {
        between(a2, a3, fraction(t3, t4))
    } else {
        a3
    }
}

/// libass's `ass_event_has_hard_overrides`: whether a block holds `\pos`,
/// `\move`, `\clip`, `\iclip`, `\org` or any tag starting with `p`, which
/// exempts an event from selective style overrides.
fn has_hard_overrides(text: &[u8]) -> bool {
    const HARD: [&[u8]; 7] = [b"pos", b"move", b"clip", b"iclip", b"org", b"pbo", b"p"];
    let mut at = 0;
    while at < text.len() {
        match text[at] {
            b'\\' if at + 1 < text.len() => at += 2,
            b'{' => {
                at += 1;
                while at < text.len() && text[at] != b'}' {
                    if text[at] == b'\\' && HARD.iter().any(|t| text[at + 1..].starts_with(t)) {
                        return true;
                    }
                    at += 1;
                }
            }
            _ => at += 1,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{Fade, Rgba, change_alpha, change_colour, fade_alpha, has_hard_overrides};

    #[test]
    fn colours_animate_per_channel() {
        let red = Rgba(0xff00_0000);
        let blue = Rgba(0x0000_ff80);
        assert_eq!(change_colour(red, blue, 0.5), Rgba(0x7f00_7f00));
        assert_eq!(change_alpha(red, 0xff, 1.0), Rgba(0xff00_00ff));
        // A negative alpha converts as a huge unsigned value: dtoi32 gives
        // i32::MIN, whose low byte is 0.
        assert_eq!(change_alpha(red, -1_i32 as u32, 1.0), red);
    }

    #[test]
    fn fades_are_piecewise_linear() {
        let fad = Fade::InOut {
            fade_in: 100,
            fade_out: 100,
        };
        assert_eq!(fade_alpha(fad, 0, 1000), 255);
        assert_eq!(fade_alpha(fad, 50, 1000), 127);
        assert_eq!(fade_alpha(fad, 500, 1000), 0);
        assert_eq!(fade_alpha(fad, 1000, 1000), 255);
        let full = Fade::Full {
            a1: 255,
            a2: 0,
            a3: 255,
            t1: -1,
            t2: 100,
            t3: 100,
            t4: -1,
        };
        assert_eq!(fade_alpha(full, 50, 1000), 127);
    }

    #[test]
    fn hard_overrides_are_found_in_blocks_only() {
        assert!(has_hard_overrides(br"{\pos(1,2)}a"));
        assert!(has_hard_overrides(br"{\b1\p1}m"));
        assert!(!has_hard_overrides(br"\pos {\b1}"));
    }
}
