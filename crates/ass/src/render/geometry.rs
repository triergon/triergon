//! The renderer's settings and the frame geometry they give a script: the
//! content area, the pixel aspect, the layout resolution and the mapping from
//! script to frame coordinates (libass's `ass_reconfigure`, `ass_layout_res`,
//! the PAR correction of `ass_start_frame`, `x2scr_*`, `y2scr_*` and
//! `init_font_scale`).

use triergon_core::{Error, Result};

use crate::{Margins, Script};

/// A size in whole pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Size {
    /// The width.
    pub width: u32,
    /// The height.
    pub height: u32,
}

impl Size {
    /// A size of `width` × `height` pixels.
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Both sides as `i32`, when both are positive and their product fits
    /// (libass's `ass_set_frame_size` and `ass_set_storage_size` checks).
    fn checked(self) -> Option<(i32, i32)> {
        let width = i32::try_from(self.width).ok().filter(|&w| w > 0)?;
        let height = i32::try_from(self.height).ok().filter(|&h| h > 0)?;
        width.checked_mul(height)?;
        Some((width, height))
    }
}

/// Margins around the content area of the frame, in frame pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FrameMargins {
    /// Above the content area.
    pub top: i32,
    /// Below it.
    pub bottom: i32,
    /// Left of it.
    pub left: i32,
    /// Right of it.
    pub right: i32,
}

/// How much a [`Renderer`](super::Renderer) keeps between frames: outlines
/// and font descriptions by count, glyph bitmaps and composited runs by
/// bytes. [`CacheLimits::LIBASS`], the default, is libass's;
/// [`CacheLimits::new`] gives what `ass_set_cache_limits` sets. Each frame
/// cuts the caches to them as libass does, never what the last frame's
/// images were drawn from, which stays until nothing holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CacheLimits {
    /// Outlines kept, at most: glyphs, drawings, stroked borders and the
    /// unit square of opaque boxes.
    pub outlines: usize,
    /// Bytes of glyph bitmaps kept, at most, counted as libass counts them:
    /// pixels, records and the points of the outline drawn.
    pub bitmap_bytes: usize,
    /// Bytes of composited runs kept, at most, counted as libass counts
    /// them: pixels and records.
    pub composite_bytes: usize,
    /// Font descriptions (family, weight, slant, orientation) kept with the
    /// fonts made for them, at most. libass keeps them all; past this many,
    /// those asked for least recently go, never one the last frame asked
    /// for, and a font asked for again starts over from its first face. A
    /// caller that wants libass's behaviour sets it to `usize::MAX`.
    pub fonts: usize,
}

impl CacheLimits {
    /// The share of the megabytes that composites get: one part in
    /// `COMPOSITE_RATIO + 1` (`COMPOSITE_CACHE_RATIO`).
    const COMPOSITE_RATIO: usize = 2;

    /// The font descriptions kept, which libass does not bound: far more
    /// than a script names.
    pub const FONTS: usize = 10_000;

    /// libass's limits before `ass_set_cache_limits`: 10,000 outlines
    /// (`GLYPH_CACHE_MAX`), 128 MiB of glyph bitmaps and 64 MiB of
    /// composites, and [`CacheLimits::FONTS`] font descriptions.
    pub const LIBASS: Self = Self {
        outlines: 10_000,
        bitmap_bytes: 128 << 20,
        composite_bytes: (128 << 20) / Self::COMPOSITE_RATIO,
        fonts: Self::FONTS,
    };

    /// The limits `ass_set_cache_limits(glyphs, megabytes)` sets: `glyphs`
    /// outlines, and `megabytes` MiB split two thirds to glyph bitmaps and
    /// one third to composites. libass reads 0 as its default for either
    /// value; here 0 keeps nothing beyond what the last frame holds. Font
    /// descriptions keep their bound, [`CacheLimits::FONTS`].
    pub const fn new(glyphs: usize, megabytes: usize) -> Self {
        let total = megabytes.saturating_mul(1 << 20);
        let composite_bytes = total / (Self::COMPOSITE_RATIO + 1);
        Self {
            outlines: glyphs,
            bitmap_bytes: total - composite_bytes,
            composite_bytes,
            fonts: Self::FONTS,
        }
    }
}

impl Default for CacheLimits {
    fn default() -> Self {
        Self::LIBASS
    }
}

/// How a [`Renderer`](super::Renderer) maps scripts onto its frame, as
/// libass's renderer settings do. [`Settings::new`] gives libass's defaults
/// for a frame size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    /// The size of the frame images are placed in.
    pub frame: Size,
    /// The size of the video the subtitles belong to, before any scaling to
    /// the frame (`ass_set_storage_size`); it sets the layout resolution and
    /// the pixel aspect unless the script gives a layout resolution.
    pub storage: Option<Size>,
    /// Margins around the content area: the video's place in the frame.
    pub margins: FrameMargins,
    /// Whether events placed by their margins may use the frame margins
    /// (`ass_set_use_margins`).
    pub use_margins: bool,
    /// The pixel aspect ratio of the frame (`ass_set_pixel_aspect`); `None`
    /// derives it from the storage size.
    pub pixel_aspect: Option<f64>,
    /// Scales every font, border, shadow and blur of events that are not
    /// positioned explicitly (`ass_set_font_scale`).
    pub font_scale: f64,
    /// Extra space between lines in frame pixels (`ass_set_line_spacing`).
    pub line_spacing: f64,
    /// Moves bottom-aligned events up by a percentage of the frame, 0 to 100
    /// (`ass_set_line_position`).
    pub line_position: f64,
    /// What is kept between frames.
    pub cache: CacheLimits,
    /// Work units bounding one frame as a whole, per frame and not per
    /// event: every shown event's evaluation, font selection and fallback,
    /// shaping, outlines, layout and drawing (glyph bitmaps, composites,
    /// vector clips), and collision handling. A glyph bitmap or composite
    /// found in the renderer's caches charges a lookup (a unit per bitmap,
    /// and per glyph of a composite) instead of the work of drawing it, so
    /// a frame drawn again from the caches spends little. Running out is
    /// `Error::Policy`, where libass would go on drawing;
    /// [`Settings::DEFAULT_WORK`] by default, and a caller that wants libass's
    /// unbounded behaviour raises it.
    pub work: u64,
}

impl Settings {
    /// The default work bound of a frame, 2²⁸ units.
    pub const DEFAULT_WORK: u64 = 1 << 28;

    /// libass's defaults for a frame of `frame` pixels: no storage size, no
    /// margins, a derived pixel aspect, font scale 1.
    pub const fn new(frame: Size) -> Self {
        Self {
            frame,
            storage: None,
            margins: FrameMargins {
                top: 0,
                bottom: 0,
                left: 0,
                right: 0,
            },
            use_margins: false,
            pixel_aspect: None,
            font_scale: 1.0,
            line_spacing: 0.0,
            line_position: 0.0,
            cache: CacheLimits::LIBASS,
            work: Self::DEFAULT_WORK,
        }
    }
}

/// The settings, held once, and what libass derives from them: the frame and
/// storage sides as it keeps them, the content area and the area that area
/// fits in without margins.
#[derive(Debug)]
pub(super) struct Canvas {
    pub settings: Settings,
    pub width: i32,
    pub height: i32,
    pub storage: Option<(i32, i32)>,
    /// `frame_content_width` and `frame_content_height`.
    pub content: (i32, i32),
    /// `fit_width` and `fit_height`.
    pub fit: (f64, f64),
}

impl Canvas {
    /// The settings as libass applies them.
    ///
    /// # Errors
    ///
    /// `Invalid` for a frame side of 0 or a frame whose pixel count overflows,
    /// which libass renders nothing for.
    pub(super) fn new(settings: &Settings) -> Result<Self> {
        let (width, height) = settings
            .frame
            .checked()
            .ok_or(Error::Invalid("frame size"))?;
        let m = settings.margins;
        let content = (
            width.wrapping_sub(m.left).wrapping_sub(m.right),
            height.wrapping_sub(m.top).wrapping_sub(m.bottom),
        );
        let (cw, ch) = (i64::from(content.0), i64::from(content.1));
        let (w, h) = (i64::from(width), i64::from(height));
        let fit = (
            if cw * h >= ch * w {
                f64::from(width)
            } else {
                f64::from(content.0) * f64::from(height) / f64::from(content.1)
            },
            if cw * h <= ch * w {
                f64::from(height)
            } else {
                f64::from(content.1) * f64::from(width) / f64::from(content.0)
            },
        );
        Ok(Self {
            settings: *settings,
            width,
            height,
            storage: settings.storage.and_then(Size::checked),
            content,
            fit,
        })
    }

    /// The pixel aspect set by hand: libass clamps a negative one to 0 and
    /// reads 0 as none; NaN passes its clamp.
    fn pixel_aspect(&self) -> Option<f64> {
        self.settings
            .pixel_aspect
            .filter(|&par| par > 0.0 || par.is_nan())
    }

    /// The geometry of `script` in this frame.
    pub(super) fn place(&self, script: &Script) -> Geometry<'_> {
        let info = script.info();
        let play_res = info.play_res();
        let layout_res = self.layout_res(play_res, info.layout_res());
        // The PAR correction: a pixel aspect is derived from the content's
        // aspect and the layout resolution's whenever the script names a
        // layout resolution, or none is set and the storage size is known.
        let (cw, ch) = self.content;
        let par = match self.pixel_aspect() {
            Some(par) if info.layout_res().is_none() => par,
            _ if cw != 0 && ch != 0 && (info.layout_res().is_some() || self.storage.is_some()) => {
                let dar = f64::from(cw) / f64::from(ch);
                let sar = f64::from(layout_res.0) / f64::from(layout_res.1);
                dar / sar
            }
            _ => 1.0,
        };
        Geometry {
            canvas: self,
            play_res,
            layout_res,
            par,
        }
    }

    /// `ass_layout_res`: the script's layout resolution, else the storage
    /// size, else the play resolution corrected by a pixel aspect set by hand.
    fn layout_res(&self, play_res: (i32, i32), script: Option<(i32, i32)>) -> (i32, i32) {
        if let Some(layout) = script {
            return layout;
        }
        if let Some(storage) = self.storage {
            return storage;
        }
        let (cw, ch) = self.content;
        #[allow(clippy::float_cmp)] // libass's test for exactly square pixels
        let Some(par) = self
            .pixel_aspect()
            .filter(|&par| par != 1.0 && cw != 0 && ch != 0)
        else {
            return play_res;
        };
        // Integer products and quotients first, as libass computes them.
        if par > 1.0 {
            let x = play_res.1.wrapping_mul(cw).wrapping_div(ch);
            (((f64::from(x) / par).round() as i32).max(1), play_res.1)
        } else {
            let y = play_res.0.wrapping_mul(ch).wrapping_div(cw);
            (play_res.0, ((f64::from(y) * par).round() as i32).max(1))
        }
    }
}

/// One frame's geometry for one script.
#[derive(Clone, Copy, Debug)]
pub(super) struct Geometry<'c> {
    pub canvas: &'c Canvas,
    /// The script's play resolution after libass's defaults.
    pub play_res: (i32, i32),
    /// `ass_layout_res`.
    pub layout_res: (i32, i32),
    /// `par_scale_x`: x is laid out divided by it and stretched by it again
    /// when glyphs are drawn.
    pub par: f64,
}

/// The scales of one event (`init_font_scale`).
#[derive(Clone, Copy, Debug)]
pub(super) struct Scales {
    /// Script pixels to frame pixels, before the pixel aspect.
    pub screen: [f64; 2],
    /// Of `\blur`, and of y for the perspective distance.
    pub blur: [f64; 2],
    /// Of borders and shadows.
    pub border: [f64; 2],
}

impl Geometry<'_> {
    /// The play resolution as `f64`.
    fn res(&self) -> (f64, f64) {
        (f64::from(self.play_res.0), f64::from(self.play_res.1))
    }

    /// Whether an event uses the margin mapping: one placed by its margins
    /// when the settings let it use the frame margins.
    pub(super) fn margin_mode(&self, explicit: bool) -> bool {
        !explicit && self.canvas.settings.use_margins
    }

    pub(super) fn x_pos(&self, x: f64) -> f64 {
        x * f64::from(self.canvas.content.0) / self.par / self.res().0
            + f64::from(self.canvas.settings.margins.left)
    }

    pub(super) fn x_left(&self, x: f64, explicit: bool) -> f64 {
        if self.margin_mode(explicit) {
            x * self.canvas.fit.0 / self.par / self.res().0
        } else {
            self.x_pos(x)
        }
    }

    pub(super) fn x_right(&self, x: f64, explicit: bool) -> f64 {
        if self.margin_mode(explicit) {
            x * self.canvas.fit.0 / self.par / self.res().0
                + (f64::from(self.canvas.width) - self.canvas.fit.0)
        } else {
            self.x_pos(x)
        }
    }

    /// The width an event's lines wrap at and are aligned in, in pixels: the
    /// play resolution less its left and right margins.
    pub(super) fn text_width(&self, margins: Margins, explicit: bool) -> f64 {
        self.x_right(
            f64::from(self.play_res.0.wrapping_sub(margins.right)),
            explicit,
        ) - self.x_left(f64::from(margins.left), explicit)
    }

    /// Without the pixel aspect: clip rectangles.
    pub(super) fn x_pos_scaled(&self, x: f64) -> f64 {
        x * f64::from(self.canvas.content.0) / self.res().0
            + f64::from(self.canvas.settings.margins.left)
    }

    pub(super) fn y_pos(&self, y: f64) -> f64 {
        y * f64::from(self.canvas.content.1) / self.res().1
            + f64::from(self.canvas.settings.margins.top)
    }

    pub(super) fn y_mid(&self, y: f64, explicit: bool) -> f64 {
        if self.margin_mode(explicit) {
            y * self.canvas.fit.1 / self.res().1
                + (f64::from(self.canvas.height) - self.canvas.fit.1) * 0.5
        } else {
            self.y_pos(y)
        }
    }

    pub(super) fn y_top(&self, y: f64, explicit: bool) -> f64 {
        if self.margin_mode(explicit) {
            y * self.canvas.fit.1 / self.res().1
        } else {
            self.y_pos(y)
        }
    }

    pub(super) fn y_sub(&self, y: f64, explicit: bool) -> f64 {
        if self.margin_mode(explicit) {
            y * self.canvas.fit.1 / self.res().1
                + (f64::from(self.canvas.height) - self.canvas.fit.1)
        } else {
            self.y_pos(y)
        }
    }

    /// The scales of an event: libass multiplies them by the font scale
    /// unless the event is explicit (its default selective-style setting).
    pub(super) fn scales(&self, explicit: bool, scaled_border: bool) -> Scales {
        let (w, h) = if self.margin_mode(explicit) {
            self.canvas.fit
        } else {
            (
                f64::from(self.canvas.content.0),
                f64::from(self.canvas.content.1),
            )
        };
        let (rx, ry) = self.res();
        let screen = [w / rx, h / ry];
        let blur = [
            w / f64::from(self.layout_res.0),
            h / f64::from(self.layout_res.1),
        ];
        let border = if scaled_border { screen } else { blur };
        let mut scales = Scales {
            screen,
            blur,
            border,
        };
        if !explicit {
            let k = self.canvas.settings.font_scale;
            for v in [&mut scales.screen, &mut scales.blur, &mut scales.border] {
                *v = v.map(|s| s * k);
            }
        }
        scales
    }
}

#[cfg(test)]
mod tests {
    use super::{Canvas, Settings, Size};

    #[test]
    fn cache_limits_split_as_libass_splits_them() {
        use super::CacheLimits;
        let default = CacheLimits::default();
        assert_eq!(
            (
                default.outlines,
                default.bitmap_bytes,
                default.composite_bytes
            ),
            (10_000, 128 << 20, 64 << 20)
        );
        let player = CacheLimits::new(1000, 24);
        assert_eq!(
            (player.outlines, player.bitmap_bytes, player.composite_bytes),
            (1000, 16 << 20, 8 << 20)
        );
    }

    #[test]
    fn frames_fit_their_content_and_reject_empty_sides() {
        assert!(Canvas::new(&Settings::new(Size::new(0, 720))).is_err());
        let mut settings = Settings::new(Size::new(1920, 1080));
        settings.margins.top = 100;
        settings.margins.bottom = 100;
        let frame = Canvas::new(&settings).unwrap();
        assert_eq!(frame.content, (1920, 880));
        // The content keeps its aspect: a 1920x880 area fills the width.
        assert!((frame.fit.0 - 1920.0).abs() < 1e-9);
        assert!((frame.fit.1 - 880.0).abs() < 1e-9);
    }
}
