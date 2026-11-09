//! Rendering a script's events at a time into positioned alpha images, as
//! libass 0.17.5's `ass_render_frame` does with hinting off and complex
//! shaping, its defaults.
//!
//! An event passes through stages, each reading the ones before it and
//! filling its own storage, kept from event to event: its text evaluated
//! ([`text`]), shaped into clusters ([`shaping`]), laid out in lines
//! ([`layout`], [`wrap`]), placed in the frame ([`placement`]), and drawn
//! ([`bitmaps`]) into images ([`images`]).

mod bitmaps;
mod cache;
mod collisions;
mod fonts;
mod geometry;
mod images;
mod layout;
mod outlines;
mod placement;
mod shaping;
mod text;
mod units;
mod wrap;

use std::sync::atomic::{AtomicU32, Ordering};

use triergon_core::{Budget, BudgetVec, Result, WorkBudget};
use triergon_raster::{Clip as ClipRect, ClipFrame, FrameScale, Matrix, Rect};

use crate::{Context, EventState, Script, charge};
use bitmaps::{Combined, Painter};
use collisions::EventBox;
pub(crate) use collisions::Placed;
pub use fonts::Fonts;
use fonts::Library;
pub use geometry::{CacheLimits, FrameMargins, Settings, Size};
use geometry::{Canvas, Geometry, Scales};
pub use images::{Change, Image, ImageKind};
use images::{ClipMask, Clipping};
use layout::{Input, Layout, Room, Sources};
use outlines::Outlines;
use placement::{Anchor, Placer, Runs, apply_fade};
use shaping::{Shaped, Shaper, Shaping};
use text::{DrawingText, Evaluated, Text};

/// One rendered frame.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'r> {
    /// The images, in drawing order.
    pub images: &'r [Image],
    /// How they differ from the previous frame's (`ass_render_frame`'s
    /// `detect_change`).
    pub change: Change,
}

/// The spare capacity, in bytes, a stage's store keeps after it is filled.
const SPARE_BYTES: usize = 128;

/// Fits a stage's store to what the event filled it with: storage is kept
/// from event to event, but spare capacity beyond [`SPARE_BYTES`] is given
/// back, so an event drawn after a larger one does not hold the larger
/// one's storage, and events of about one size reallocate nothing.
#[inline]
pub(super) fn fit<T>(store: &mut BudgetVec<T>) -> Result<()> {
    let spare = SPARE_BYTES / size_of::<T>().max(1);
    if store.capacity() - store.len() > spare {
        store.try_shrink_to(store.len() + spare)?;
    }
    Ok(())
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
    canvas: Canvas,
    id: u32,
    pipeline: Pipeline,
    /// The images of the last frame rendered, which [`Frame`] shows.
    last_frame: BudgetVec<Image>,
    /// The frame being assembled, then the one before it.
    next_frame: BudgetVec<Image>,
    boxes: BudgetVec<EventBox>,
    placed: BudgetVec<Placed>,
    used: BudgetVec<collisions::Rect>,
    /// The work the last call to [`Renderer::render`] spent.
    #[cfg(feature = "work-probe")]
    work_spent: u64,
}

/// What renders one event: the font library, the outline cache, the shaper
/// and the painter, which keep their state between frames, and the storage
/// of every stage, which each event fills again.
#[derive(Debug)]
struct Pipeline {
    budget: Budget,
    library: Library,
    outlines: Outlines,
    shaper: Shaper,
    painter: Painter,
    text: Text,
    shaped: Shaped,
    layout: Layout,
    runs: Runs,
    /// The composited runs of the event being drawn.
    combined: BudgetVec<Combined>,
    /// The images of every event rendered in the frame, event by event.
    event_images: BudgetVec<Image>,
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
            canvas: Canvas::new(settings)?,
            id: RENDERERS.fetch_add(1, Ordering::Relaxed),
            pipeline: Pipeline {
                budget: budget.clone(),
                library: Library::new(budget, fonts)?,
                outlines: Outlines::new(budget),
                shaper: Shaper::new(budget),
                painter: Painter::new(budget, triergon_raster::Limits::default()),
                text: Text::new(budget),
                shaped: Shaped::new(budget),
                layout: Layout::new(budget),
                runs: Runs::new(budget),
                combined: BudgetVec::new(budget),
                event_images: BudgetVec::new(budget),
            },
            last_frame: BudgetVec::new(budget),
            next_frame: BudgetVec::new(budget),
            boxes: BudgetVec::new(budget),
            placed: BudgetVec::new(budget),
            used: BudgetVec::new(budget),
            #[cfg(feature = "work-probe")]
            work_spent: 0,
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
            #[cfg(feature = "work-probe")]
            {
                self.work_spent = 0;
            }
            return Ok(Frame {
                images: &[],
                change: Change::Content,
            });
        }
        // One budget bounds the whole frame: every event's evaluation, font
        // selection, shaping, layout and drawing, and collision handling.
        let mut work = WorkBudget::new(self.canvas.settings.work);
        let change = self.render_frame(script, now, &mut work);
        #[cfg(feature = "work-probe")]
        {
            self.work_spent = work.spent();
        }
        Ok(Frame {
            images: &self.last_frame,
            change: change?,
        })
    }

    /// The work units the last call to [`Renderer::render`] spent: 0 for a
    /// script without events, and for a frame refused past
    /// [`Settings::work`] what it spent before the charge it refused. A
    /// development aid, with the `work-probe` feature.
    #[cfg(feature = "work-probe")]
    pub fn work_spent(&self) -> u64 {
        self.work_spent
    }

    /// Renders every shown event, handles collisions and makes the result
    /// the last frame; how it differs from the one before.
    fn render_frame(
        &mut self,
        script: &mut Script,
        now: i64,
        work: &mut WorkBudget,
    ) -> Result<Change> {
        let settings = &self.canvas.settings;
        let pipeline = &mut self.pipeline;
        pipeline
            .library
            .register_embedded(script.id(), script.fonts())?;
        pipeline.library.begin_frame(settings.cache.fonts)?;
        // The stages let go of what the last event drew and the last frame
        // was assembled from, so only the last frame's images keep the
        // values they show from the cut: composites, then bitmaps, then
        // outlines, as libass cuts its caches when a frame starts.
        pipeline.layout.release();
        pipeline.combined.clear();
        pipeline.event_images.clear();
        pipeline.painter.cut(&settings.cache);
        pipeline.outlines.cut(settings.cache.outlines);
        let geometry = self.canvas.place(script);
        let boxes = &mut self.boxes;
        boxes.clear();
        for index in 0..script.events().len() {
            if script.events()[index].is_shown_at(now)
                && let Some(event) = pipeline.render_event(script, index, now, &geometry, work)?
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
        let placed = &mut self.placed;
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
            charge(work, taking_part.saturating_mul(taking_part))?;
            collisions::fix(
                layer,
                group,
                pipeline.event_images.as_mut_slice(),
                &mut self.used,
                self.id,
                self.canvas.height,
            )?;
        }
        let next = &mut self.next_frame;
        next.clear();
        for b in boxes.as_slice() {
            for image in &pipeline.event_images[b.images.clone()] {
                next.try_push(image.clone()).map_err(|e| e.error)?;
            }
        }
        for (b, record) in boxes.as_slice().iter().zip(self.placed.as_slice()) {
            script.events_mut()[b.event].placed = *record;
        }
        images::drop_transparent(next);
        let change = Change::between(&self.last_frame, next);
        std::mem::swap(&mut self.last_frame, next);
        // The previous frame's images go; what they were drawn from may now
        // leave the caches.
        next.clear();
        Ok(change)
    }
}

impl Pipeline {
    /// `ass_render_event`: the event's images appended to the frame's and
    /// its box; `None` when libass renders nothing for it.
    fn render_event(
        &mut self,
        script: &Script,
        index: usize,
        now: i64,
        geometry: &Geometry<'_>,
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
        let scales = geometry.scales(
            context.state().explicit,
            script.info().scaled_border_and_shadow,
        );
        let Some(evaluated) = self.text.read(
            &mut context,
            event.text(),
            &mut self.library,
            &scales,
            geometry.par,
            work,
        )?
        else {
            return Ok(None);
        };
        if self.text.chars().is_empty() {
            return Ok(None);
        }
        let room = Room {
            geometry,
            scales: &scales,
            margins: event.margins.or(evaluated.style.margins),
            elapsed: now.wrapping_sub(event.start),
        };
        let area = self.draw(script, &evaluated, &room, work)?;
        let begin = self.event_images.len();
        self.event_images(script, &evaluated, &room, area, work)?;
        // The event's runs and clip are let go or held by its images.
        self.combined.clear();
        self.painter.settle();
        self.outlines.settle();
        Ok(Some(EventBox::new(
            index,
            event,
            &evaluated.state,
            area,
            begin..self.event_images.len(),
        )))
    }

    /// Shapes, lays out, places and draws the evaluated text into its
    /// composited runs; its box for collision handling.
    fn draw(
        &mut self,
        script: &Script,
        evaluated: &Evaluated<'_>,
        room: &Room<'_>,
        work: &mut WorkBudget,
    ) -> Result<Rect> {
        let text = &self.text;
        // Shaping and layout pass over the characters a fixed number of
        // times; font fallback, shaping, outlines and the line rebalancing
        // charge their own work.
        charge(work, text.chars().len().saturating_mul(16))?;
        let shaping = Shaping::new(evaluated.last.encoding, script.info());
        self.shaper
            .shape(text, &mut self.library, &shaping, work, &mut self.shaped)?;
        let input = Input {
            text,
            evaluated,
            shaped: &self.shaped,
            shaping: &shaping,
        };
        let sources = Sources {
            library: &self.library,
            outlines: &mut self.outlines,
            limits: script.limits(),
        };
        self.layout.lay_out(&input, sources, room, work)?;
        let geometry = room.geometry;
        let anchor = Anchor::new(geometry, &evaluated.state, room.margins, &self.layout);
        self.runs.place(text, &self.shaped, &self.layout)?;
        let placer = Placer::new(
            text,
            &self.shaped,
            &self.layout,
            &anchor,
            geometry,
            room.scales,
        );
        let frame = FrameScale {
            blur: room.scales.blur,
            border: room.scales.border,
            pixel_aspect: geometry.par,
        };
        self.painter.draw(
            &self.runs,
            &placer,
            &frame,
            &mut self.outlines,
            &mut self.combined,
            work,
        )?;
        Ok(anchor.event_box(&self.layout, geometry.par))
    }

    /// The event's images from its composited runs: the box of a
    /// `BorderStyle` 4 event, then its shadows, borders and fills, clipped.
    fn event_images(
        &mut self,
        script: &Script,
        evaluated: &Evaluated<'_>,
        room: &Room<'_>,
        area: Rect,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let geometry = room.geometry;
        let opaque_background = evaluated.last.border_style.0 == 4;
        if opaque_background
            && let Some(image) = background(geometry, evaluated, room.scales, area, &self.budget)?
        {
            self.event_images.try_push(image).map_err(|e| e.error)?;
        }
        let clipping = clipping(geometry, &evaluated.state);
        let mask = self.vector_clip(geometry, &evaluated.state, room.scales, script, work)?;
        images::render_text(
            &self.combined,
            &clipping,
            !opaque_background,
            mask.as_ref(),
            &self.budget,
            &mut self.event_images,
        )
    }

    /// The vector clip drawn in the frame, if the event has one that draws.
    fn vector_clip(
        &mut self,
        geometry: &Geometry<'_>,
        state: &EventState<'_>,
        scales: &Scales,
        script: &Script,
        work: &mut WorkBudget,
    ) -> Result<Option<ClipMask>> {
        let Some(clip) = state.vector_clip else {
            return Ok(None);
        };
        let drawing = DrawingText {
            text: clip.drawing,
            scale: clip.scale,
            baseline: 0,
        };
        let Some(outline) = self.outlines.drawing(&drawing, script.limits(), work)? else {
            return Ok(None);
        };
        let w = drawing.pixel_scale();
        let margins = geometry.canvas.settings.margins;
        let matrix = Matrix([
            [
                scales.screen[0] * w,
                0.0,
                f64::from(margins.left.wrapping_mul(64)),
            ],
            [
                0.0,
                scales.screen[1] * w,
                f64::from(margins.top.wrapping_mul(64)),
            ],
            [0.0, 0.0, 1.0],
        ]);
        Ok(self
            .painter
            .draw_shape(&outline, &matrix, work)?
            .map(|bitmap| ClipMask {
                bitmap,
                inverse: clip.inverse,
            }))
    }
}

/// `add_background`: the box of a `BorderStyle` 4 event around its
/// collision box and positive shadow, in the faded back colour.
fn background(
    geometry: &Geometry<'_>,
    evaluated: &Evaluated<'_>,
    scales: &Scales,
    event: Rect,
    budget: &Budget,
) -> Result<Option<Image>> {
    let size = |shadow: f64, scale: f64| {
        if shadow > 0.0 {
            (shadow * scale).round() as i32
        } else {
            0
        }
    };
    let last = &evaluated.last;
    let (sx, sy) = (
        size(last.shadow_x, scales.border[0]),
        size(last.shadow_y, scales.border[1]),
    );
    let area = Rect::new(
        event.x_min.wrapping_sub(sx),
        event.y_min.wrapping_sub(sy),
        event.x_max.wrapping_add(sx),
        event.y_max.wrapping_add(sy),
    );
    let colour = apply_fade(last.colours.back, evaluated.state.fade);
    let canvas = geometry.canvas;
    images::background(area, (canvas.width, canvas.height), colour, budget)
}

/// The event's clip rectangle in the frame.
fn clipping(geometry: &Geometry<'_>, state: &EventState<'_>) -> Clipping {
    let canvas = geometry.canvas;
    let explicit = state.explicit;
    let clip = state.clip;
    let mut rect = if explicit || !canvas.settings.use_margins {
        let x = |v: i32| geometry.x_pos_scaled(f64::from(v)).round() as i32;
        let y = |v: i32| geometry.y_pos(f64::from(v)).round() as i32;
        let mut r = Rect::new(x(clip.x0), y(clip.y0), x(clip.x1), y(clip.y1));
        if explicit {
            let margins = canvas.settings.margins;
            let (zx, zy) = (margins.left, margins.top);
            r.x_min = r.x_min.max(zx);
            r.y_min = r.y_min.max(zy);
            r.x_max = r.x_max.min(zx.wrapping_add(canvas.content.0));
            r.y_max = r.y_max.min(zy.wrapping_add(canvas.content.1));
        }
        r
    } else {
        Rect::new(0, 0, canvas.width, canvas.height)
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
            width: canvas.width,
            height: canvas.height,
            content: Rect::new(
                geometry.x_pos_scaled(0.0) as i32,
                geometry.y_pos(0.0) as i32,
                geometry.x_pos_scaled(f64::from(px)) as i32,
                geometry.y_pos(f64::from(py)) as i32,
            ),
        },
    }
}
