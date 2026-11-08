//! Rendering a script's events at a time into positioned alpha images, as
//! libass 0.17.5's `ass_render_frame` does with hinting off and complex
//! shaping, its defaults.

mod bitmaps;
mod collisions;
mod fonts;
mod geometry;
mod images;
mod layout;
mod outlines;
mod shaping;
mod text;

use std::sync::atomic::{AtomicU32, Ordering};

use triergon_core::{Budget, BudgetVec, Result, WorkBudget};
use triergon_raster::{Clip as ClipRect, ClipFrame, FrameScale, Matrix, Rect, Vector, pixels_26_6};

use crate::{
    Context, EventState, GlyphState, HAlign, Justify, KaraokeGlyph, Margins,
    Placement as EventPlacement, Script, ScrollDirection, VAlign, charge,
};
use bitmaps::{Combined, Painter, apply_fade};
use collisions::EventBox;
pub(crate) use collisions::Placed;
pub use fonts::Fonts;
use fonts::Library;
pub use geometry::{CacheLimits, FrameMargins, Settings, Size};
use geometry::{Canvas, Geometry};
pub use images::{Change, Image, ImageKind};
use images::{ClipMask, Clipping};
use layout::{Lines, TextBox};
use outlines::Outlines;
use shaping::{Shaper, Shaping};
use text::Glyphs;

/// One rendered frame.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'r> {
    /// The images, in drawing order.
    pub images: &'r [Image],
    /// How they differ from the previous frame's (`ass_render_frame`'s
    /// `detect_change`).
    pub change: Change,
}

/// Identities of renderers, which collision records are tied to.
static RENDERERS: AtomicU32 = AtomicU32::new(1);

/// Renders frames of scripts as libass's renderer does.
///
/// ```no_run
/// use triergon_ass::{Fonts, Options, Renderer, Script, Settings, Size};
/// use triergon_core::{Budget, Bytes};
///
/// let budget = Budget::new(256 << 20)?;
/// let font = Bytes::copy_from_slice(&budget, &std::fs::read("Arimo-Regular.ttf")?)?;
/// let fonts = Fonts { attached: vec![font], ..Fonts::default() };
/// let mut settings = Settings::new(Size::new(1920, 1080));
/// settings.storage = Some(Size::new(1280, 720));
/// let mut renderer = Renderer::new(&budget, &settings, &fonts)?;
/// let mut script = Script::parse(&budget, &std::fs::read("subtitles.ass")?, &Options::default())?;
/// for image in renderer.render(&mut script, 1500)?.images {
///     let _ = (image.kind, image.x, image.y, image.colour, image.rows());
/// }
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct Renderer {
    budget: Budget,
    frame: Canvas,
    id: u32,
    library: Library,
    outlines: Outlines,
    shaper: Shaper,
    painter: Painter,
    images: BudgetVec<Image>,
    scratch: Scratch,
}

/// Storage reused from event to event.
#[derive(Debug)]
struct Scratch {
    runs: BudgetVec<Combined>,
    map: BudgetVec<usize>,
    karaoke: BudgetVec<KaraokeGlyph>,
    lines: Lines,
    images: BudgetVec<Image>,
    boxes: BudgetVec<EventBox>,
    placed: BudgetVec<Placed>,
    /// The frame being assembled.
    frame: BudgetVec<Image>,
    used: BudgetVec<collisions::Rect>,
}

impl Renderer {
    /// A renderer for `settings` drawing with `fonts`, whose storage is
    /// charged to `budget`.
    ///
    /// # Errors
    ///
    /// `Invalid` for a frame with a side of 0, which libass draws nothing in.
    pub fn new(budget: &Budget, settings: &Settings, fonts: &Fonts) -> Result<Self> {
        Ok(Self {
            budget: budget.clone(),
            frame: Canvas::new(settings)?,
            id: RENDERERS.fetch_add(1, Ordering::Relaxed),
            library: Library::new(budget, fonts)?,
            outlines: Outlines::new(budget),
            shaper: Shaper::new(budget),
            painter: Painter::new(budget, triergon_raster::Limits::default()),
            images: BudgetVec::new(budget),
            scratch: Scratch {
                runs: BudgetVec::new(budget),
                map: BudgetVec::new(budget),
                karaoke: BudgetVec::new(budget),
                lines: Lines {
                    lines: BudgetVec::new(budget),
                    height: 0.0,
                    border_top: 0,
                    border_bottom: 0,
                    border_x: 0,
                },
                images: BudgetVec::new(budget),
                boxes: BudgetVec::new(budget),
                placed: BudgetVec::new(budget),
                frame: BudgetVec::new(budget),
                used: BudgetVec::new(budget),
            },
        })
    }

    /// The frame of `script` at `now` milliseconds: its images, in the order
    /// libass lists them (event by event in layer and read order, each
    /// event's background, shadows, borders and fills; fully transparent
    /// images left out), and how they differ from the previous frame's.
    /// Collision handling remembers each event's place in `script` for later
    /// frames.
    ///
    /// A script without events gives no images and [`Change::Content`], as
    /// libass gives when it renders nothing; the next frame is then compared
    /// with the last one that was rendered.
    ///
    /// # Errors
    ///
    /// The budget's errors, `Policy` past a work limit, and `Unsupported`
    /// for what the renderer does not draw: glyphs of `@` fonts and
    /// libass's simple shaping. The previous frame is then kept for
    /// comparison.
    pub fn render(&mut self, script: &mut Script, now: i64) -> Result<Frame<'_>> {
        if script.events().is_empty() {
            return Ok(Frame {
                images: &[],
                change: Change::Content,
            });
        }
        self.library
            .register_embedded(script.id(), script.fonts())?;
        self.library.begin_frame(self.frame.cache.fonts)?;
        self.outlines.trim(self.frame.cache.outlines)?;
        let geometry = self.frame.place(script);
        self.scratch.images.clear();
        // One budget bounds the whole frame: every event's evaluation, font
        // selection, shaping, layout and drawing, and collision handling.
        let mut work = WorkBudget::new(self.frame.work);
        let mut boxes = std::mem::replace(&mut self.scratch.boxes, BudgetVec::new(&self.budget));
        boxes.clear();
        for index in 0..script.events().len() {
            if script.events()[index].is_shown_at(now)
                && let Some(event) = self.render_event(script, index, now, &geometry, &mut work)?
            {
                boxes.try_push(event).map_err(|e| e.error)?;
            }
        }
        // libass sorts by layer and read order with qsort, which leaves the
        // order of equal keys unspecified, and a ReadOrder repeats (0 for
        // every event of a whole script): equal keys keep the script's order.
        // Boxes are collected in that order, so the event index completes the
        // key and the sort needs no scratch.
        boxes
            .as_mut_slice()
            .sort_unstable_by_key(|b| (b.layer, b.read_order, b.event));
        // Collision records are worked on in scratch and kept with the
        // events only once the whole frame has been made: a refused frame
        // leaves them as they were.
        let placed = &mut self.scratch.placed;
        placed.clear();
        placed.try_reserve_exact(boxes.len())?;
        for b in boxes.as_slice() {
            placed
                .try_push(script.events()[b.event].placed)
                .map_err(|e| e.error)?;
        }
        let mut records = placed.as_mut_slice();
        for layer in boxes.as_mut_slice().chunk_by_mut(|a, b| a.layer == b.layer) {
            let (group, rest) = records.split_at_mut(layer.len());
            records = rest;
            // Each event that takes part is fitted against those before it.
            let taking_part = layer.iter().filter(|b| b.takes_part()).count();
            charge(&mut work, taking_part.saturating_mul(taking_part))?;
            collisions::fix(
                layer,
                group,
                self.scratch.images.as_mut_slice(),
                &mut self.scratch.used,
                self.id,
                self.frame.height,
            )?;
        }
        let images = self.scratch.images.as_slice();
        let next = &mut self.scratch.frame;
        next.clear();
        for b in boxes.as_slice() {
            for image in &images[b.images.clone()] {
                next.try_push(image.clone()).map_err(|e| e.error)?;
            }
        }
        for (b, record) in boxes.as_slice().iter().zip(self.scratch.placed.as_slice()) {
            script.events_mut()[b.event].placed = *record;
        }
        self.scratch.boxes = boxes;
        images::drop_transparent(next);
        let change = Change::between(self.images.as_slice(), next.as_slice());
        std::mem::swap(&mut self.images, next);
        // The previous frame's images go; what they were drawn from may now
        // leave the caches.
        next.clear();
        Ok(Frame {
            images: self.images.as_slice(),
            change,
        })
    }

    /// `ass_render_event`: the event's images appended to the scratch list
    /// and its box; `None` when libass renders nothing for it.
    fn render_event(
        &mut self,
        script: &Script,
        index: usize,
        now: i64,
        geometry: &Geometry,
        work: &mut WorkBudget,
    ) -> Result<Option<EventBox>> {
        let event = &script.events()[index];
        if event.style >= script.styles().len() {
            return Ok(None);
        }
        // Evaluation passes over the effect and the text once.
        charge(
            work,
            event
                .effect()
                .map_or(0, <[u8]>::len)
                .saturating_add(event.text().len()),
        )?;
        let mut context = Context::new(script, event, now, geometry.layout_res)?;
        let explicit = context.state().explicit;
        let scales = geometry.scales(explicit, script.info().scaled_border_and_shadow);
        let Some(mut glyphs) = Glyphs::read(
            &self.budget,
            &mut context,
            &mut self.library,
            &scales,
            geometry.par,
            work,
        )?
        else {
            return Ok(None);
        };
        if glyphs.len == 0 {
            return Ok(None);
        }
        let laid = Laid {
            state: *context.state(),
            last: *context.glyph_state(),
            scales,
            explicit,
            margins: event.margins.or(context.style().margins),
        };
        let bbox = self.lay_out(
            script,
            now.wrapping_sub(event.start),
            geometry,
            &laid,
            &mut glyphs,
            work,
        )?;
        let lines = &self.scratch.lines;
        let device = device_position(geometry, &laid, &bbox, lines, self.frame.line_position);
        set_shifts(&mut glyphs, geometry, &laid, &bbox, device);
        let frame_scale = FrameScale {
            blur: scales.blur,
            border: scales.border,
            pixel_aspect: geometry.par,
        };
        self.painter.render(
            &mut glyphs,
            device,
            &frame_scale,
            self.frame.margins.left,
            &mut self.scratch.runs,
            work,
        )?;

        // The event's box for collisions: VSFilter's biased roundings.
        let (device_x, device_y) = device;
        let par = geometry.par;
        let first_asc = lines.lines.as_slice()[0].asc;
        let border = (lines.border_top, lines.border_bottom, lines.border_x);
        let top = (device_y - first_asc - f64::from(border.0)) as i32;
        let height = (lines.height + f64::from(border.1) + f64::from(border.0)) as i32;
        let left = ((device_x + bbox.x_min) * par - f64::from(border.2) + 0.5) as i32;
        let width = ((bbox.x_max - bbox.x_min) * par + 2.0 * f64::from(border.2) + 0.5) as i32;

        let begin = self.scratch.images.len();
        let opaque_background = laid.last.border_style.0 == 4;
        if opaque_background {
            self.background(
                &laid,
                Rect::new(
                    left,
                    top,
                    left.wrapping_add(width),
                    top.wrapping_add(height),
                ),
            )?;
        }
        let clipping = self.clipping(geometry, &laid.state, explicit);
        let mask = self.vector_clip(&laid.state, &scales, script, work)?;
        images::render_text(
            self.scratch.runs.as_slice(),
            &clipping,
            !opaque_background,
            mask.as_ref(),
            &self.budget,
            &mut self.scratch.images,
        )?;
        Ok(Some(EventBox {
            event: index,
            layer: event.layer,
            read_order: event.read_order,
            top,
            height,
            left,
            width,
            detect_collisions: laid.state.detect_collisions,
            moves_up: laid.state.alignment.vertical == VAlign::Bottom,
            images: begin..self.scratch.images.len(),
        }))
    }

    /// Shapes and lays out an event's glyphs, `elapsed` milliseconds into
    /// the event, from its style runs to the baseline shear: the text box.
    fn lay_out(
        &mut self,
        script: &Script,
        elapsed: i64,
        geometry: &Geometry,
        laid: &Laid<'_>,
        glyphs: &mut Glyphs<'_>,
        work: &mut WorkBudget,
    ) -> Result<TextBox> {
        let info = script.info();
        // Every stage below passes over the glyphs once; font fallback,
        // shaping, outlines and the line rebalancing charge their own work.
        charge(work, glyphs.list.len().saturating_mul(16))?;
        glyphs.split_style_runs(&self.library);
        let (base, whole_text) = Shaping::base_for(laid.last.encoding);
        let shaping = Shaping {
            base,
            whole_text,
            kerning: info.kerning,
            language: info.language(),
        };
        self.shaper
            .find_runs(glyphs, &mut self.library, shaping.whole_text, work)?;
        let mut bidi = self.shaper.shape(glyphs, &self.library, &shaping, work)?;
        layout::retrieve(
            glyphs,
            &self.library,
            &mut self.outlines,
            script.limits(),
            &laid.scales,
            geometry.par,
            work,
        )?;
        layout::preliminary(glyphs);

        let explicit = laid.explicit;
        let max_text_width = geometry.x_right(
            f64::from(geometry.play_res.0.wrapping_sub(laid.margins.right)),
            explicit,
        ) - geometry.x_left(f64::from(laid.margins.left), explicit);
        let state = &laid.state;
        let lines = &mut self.scratch.lines;
        let spacing = self.frame.line_spacing;
        layout::wrap(
            glyphs,
            max_text_width,
            state.wrap_style,
            spacing,
            &laid.scales,
            lines,
            work,
        )?;
        layout::karaoke(glyphs, elapsed, &mut self.scratch.karaoke)?;
        shaping::reorder(glyphs, &mut bidi, &shaping, &mut self.scratch.map)?;
        let map = self.scratch.map.as_slice();
        layout::reorder(glyphs, map, lines, spacing);
        // A banner's lines start at the left and are justified as aligned.
        let (halign, justify) = if state.scroll.is_some_and(|s| s.is_horizontal()) {
            let justify = match state.alignment.horizontal {
                HAlign::Left => Justify::Left,
                HAlign::Center => Justify::Center,
                HAlign::Right => Justify::Right,
                HAlign::None => Justify::Auto,
            };
            (HAlign::Left, justify)
        } else {
            (state.alignment.horizontal, state.justify)
        };
        layout::align(glyphs, halign, justify, max_text_width);
        let bbox = layout::text_box(glyphs, lines);
        layout::shear(glyphs, map, false);
        Ok(bbox)
    }

    /// `add_background`: the box of a `BorderStyle` 4 event around its
    /// collision box and positive shadow, in the faded back colour.
    fn background(&mut self, laid: &Laid<'_>, event: Rect) -> Result<()> {
        let size = |shadow: f64, scale: f64| {
            if shadow > 0.0 {
                (shadow * scale).round() as i32
            } else {
                0
            }
        };
        let (sx, sy) = (
            size(laid.last.shadow_x, laid.scales.border[0]),
            size(laid.last.shadow_y, laid.scales.border[1]),
        );
        let area = Rect::new(
            event.x_min.wrapping_sub(sx),
            event.y_min.wrapping_sub(sy),
            event.x_max.wrapping_add(sx),
            event.y_max.wrapping_add(sy),
        );
        let colour = apply_fade(laid.last.colours.back, laid.state.fade);
        let frame = (self.frame.width, self.frame.height);
        if let Some(image) = images::background(area, frame, colour, &self.budget)? {
            self.scratch.images.try_push(image).map_err(|e| e.error)?;
        }
        Ok(())
    }

    /// The event's clip rectangle in the frame.
    fn clipping(&self, geometry: &Geometry, state: &EventState<'_>, explicit: bool) -> Clipping {
        let frame = &self.frame;
        let clip = state.clip;
        let mut rect = if explicit || !frame.use_margins {
            let x = |v: i32| geometry.x_pos_scaled(f64::from(v)).round() as i32;
            let y = |v: i32| geometry.y_pos(f64::from(v)).round() as i32;
            let mut r = Rect::new(x(clip.x0), y(clip.y0), x(clip.x1), y(clip.y1));
            if explicit {
                let (zx, zy) = (frame.margins.left, frame.margins.top);
                r.x_min = r.x_min.max(zx);
                r.y_min = r.y_min.max(zy);
                r.x_max = r.x_max.min(zx.wrapping_add(frame.content.0));
                r.y_max = r.y_max.min(zy.wrapping_add(frame.content.1));
            }
            r
        } else {
            Rect::new(0, 0, frame.width, frame.height)
        };
        if let Some(scroll) = state.scroll
            && let Some((y0, y1)) = scroll.band
        {
            rect.y_min = rect.y_min.max(geometry.y_pos(f64::from(y0)).round() as i32);
            rect.y_max = rect.y_max.min(geometry.y_pos(f64::from(y1)).round() as i32);
        }
        let (px, py) = geometry.play_res;
        Clipping {
            clip: ClipRect {
                rect,
                inverse: clip.inverse,
            },
            frame: ClipFrame {
                width: frame.width,
                height: frame.height,
                content: Rect::new(
                    geometry.x_pos_scaled(0.0) as i32,
                    geometry.y_pos(0.0) as i32,
                    geometry.x_pos_scaled(f64::from(px)) as i32,
                    geometry.y_pos(f64::from(py)) as i32,
                ),
            },
        }
    }

    /// The vector clip drawn in the frame, if the event has one that draws.
    fn vector_clip(
        &mut self,
        state: &EventState<'_>,
        scales: &geometry::Scales,
        script: &Script,
        work: &mut WorkBudget,
    ) -> Result<Option<ClipMask>> {
        let Some(clip) = state.vector_clip else {
            return Ok(None);
        };
        let drawing = text::DrawingText {
            text: clip.drawing,
            scale: clip.scale,
            baseline: 0,
        };
        let Some(outline) = self.outlines.drawing(&drawing, script.limits(), work)? else {
            return Ok(None);
        };
        let base = 1_i32.wrapping_shl(clip.scale.wrapping_sub(1) as u32);
        let w = if base > 0 { 1.0 / f64::from(base) } else { 0.0 };
        let matrix = Matrix([
            [
                scales.screen[0] * w,
                0.0,
                f64::from(self.frame.margins.left.wrapping_mul(64)),
            ],
            [
                0.0,
                scales.screen[1] * w,
                f64::from(self.frame.margins.top.wrapping_mul(64)),
            ],
            [0.0, 0.0, 1.0],
        ]);
        Ok(self
            .painter
            .draw_shape(&outline.shape, &matrix, work)?
            .map(|bitmaps| ClipMask {
                bitmaps,
                inverse: clip.inverse,
            }))
    }
}

/// What an event's walk left and what follows from it.
#[derive(Clone, Copy, Debug)]
struct Laid<'s> {
    /// The event-wide state after the text.
    state: EventState<'s>,
    /// The per-glyph state after the text: the last `\fe`, border style,
    /// shadow and back colour apply to the whole event.
    last: GlyphState<'s>,
    scales: geometry::Scales,
    explicit: bool,
    /// The event's margins, else its style's.
    margins: Margins,
}

/// `calculate_rotation_params`: every glyph's offset from the rotation
/// origin (`\org`, else the alignment point), displaced by its shadow.
fn set_shifts(
    glyphs: &mut Glyphs<'_>,
    geometry: &Geometry,
    laid: &Laid<'_>,
    bbox: &TextBox,
    (device_x, device_y): (f64, f64),
) {
    let state = &laid.state;
    let center = if let Some((x, y)) = state.origin {
        (geometry.x_pos(x), geometry.y_pos(y))
    } else {
        let (bx, by) = base_point(bbox, state.alignment.horizontal, state.alignment.vertical);
        (device_x + bx, device_y + by)
    };
    let (border, par) = (laid.scales.border, geometry.par);
    for i in 0..glyphs.list.len() {
        let g = glyphs.get_mut(i);
        g.shift = Vector::new(
            g.pos.x.wrapping_add(pixels_26_6(
                device_x - center.0 + g.shadow[0] * border[0] / par,
            )),
            g.pos
                .y
                .wrapping_add(pixels_26_6(device_y - center.1 + g.shadow[1] * border[1])),
        );
    }
}

/// `get_base_point`: the point of the box the alignment anchors.
fn base_point(bbox: &TextBox, halign: HAlign, valign: VAlign) -> (f64, f64) {
    let x = match halign {
        HAlign::Left => bbox.x_min,
        HAlign::Center => (bbox.x_max + bbox.x_min) / 2.0,
        HAlign::Right => bbox.x_max,
        HAlign::None => 0.0,
    };
    let y = match valign {
        VAlign::Top => bbox.y_min,
        VAlign::Middle => (bbox.y_max + bbox.y_min) / 2.0,
        VAlign::Bottom => bbox.y_max,
        VAlign::TopAndMiddle => 0.0,
    };
    (x, y)
}

/// The event's position in pre-aspect frame pixels: by `\pos` or `\move`,
/// by a scroll, or by its alignment and margins.
fn device_position(
    geometry: &Geometry,
    laid: &Laid<'_>,
    bbox: &TextBox,
    lines: &Lines,
    line_position: f64,
) -> (f64, f64) {
    let (state, explicit) = (&laid.state, laid.explicit);
    let (mut x, mut y) = (0.0, 0.0);
    let positioned = match state.placement {
        EventPlacement::Positioned(px, py) => {
            let (bx, by) = base_point(bbox, state.alignment.horizontal, state.alignment.vertical);
            x = geometry.x_pos(px) - bx;
            y = geometry.y_pos(py) - by;
            true
        }
        EventPlacement::Margins => false,
    };
    let (play_x, play_y) = geometry.play_res;
    match state.scroll {
        Some(s) if s.direction == ScrollDirection::RightToLeft => {
            x = geometry.x_pos(f64::from(play_x) - s.shift);
        }
        Some(s) if s.direction == ScrollDirection::LeftToRight => {
            x = geometry.x_pos(s.shift) - (bbox.x_max - bbox.x_min);
        }
        _ if !positioned => x = geometry.x_left(f64::from(laid.margins.left), explicit),
        _ => {}
    }
    let first_asc = lines.lines.as_slice()[0].asc;
    match state.scroll.and_then(|s| s.band.map(|band| (s, band))) {
        Some((s, (y0, _))) if s.direction == ScrollDirection::TopToBottom => {
            y = geometry.y_mid(f64::from(y0) + s.shift, explicit) - bbox.y_max;
        }
        Some((s, (_, y1))) if s.direction == ScrollDirection::BottomToTop => {
            y = geometry.y_mid(f64::from(y1) - s.shift, explicit) - bbox.y_min;
        }
        _ if !positioned => match state.alignment.vertical {
            VAlign::Top => {
                y = geometry.y_top(f64::from(laid.margins.vertical), explicit) + first_asc;
            }
            VAlign::Middle => {
                let scr_y = geometry.y_mid(f64::from(play_y) / 2.0, explicit);
                y = scr_y - (bbox.y_max + bbox.y_min) / 2.0;
            }
            VAlign::Bottom | VAlign::TopAndMiddle => {
                let line_pos = if explicit { 0.0 } else { line_position };
                let bottom = geometry.y_sub(
                    f64::from(play_y.wrapping_sub(laid.margins.vertical)),
                    explicit,
                );
                let top = geometry.y_top(0.0, explicit);
                y = bottom + (top - bottom) * line_pos / 100.0;
                y -= lines.height;
                y += first_asc;
                let y0 = top + first_asc;
                if y < y0 && line_pos > 0.0 {
                    y = y0;
                }
            }
        },
        _ => {}
    }
    (x, y)
}
