//! The renderer's settings and the frame geometry they give a script: the
//! content area, the pixel aspect, the layout resolution and the mapping from
//! script to frame coordinates (libass's `ass_reconfigure`, `ass_layout_res`,
//! the PAR correction of `ass_start_frame`, `x2scr_*`, `y2scr_*` and
//! `init_font_scale`).

use triergon_core::{Error, Result};

use crate::Script;

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
}

impl Settings {
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
        }
    }
}

/// The settings checked once: the frame and storage sides as libass keeps
/// them, the content area and the area it fits in without margins.
#[derive(Clone, Copy, Debug)]
pub(super) struct Frame {
    pub width: i32,
    pub height: i32,
    pub storage: Option<(i32, i32)>,
    pub margins: FrameMargins,
    pub use_margins: bool,
    pub pixel_aspect: Option<f64>,
    pub font_scale: f64,
    pub line_spacing: f64,
    pub line_position: f64,
    /// `frame_content_width` and `frame_content_height`.
    pub content: (i32, i32),
    /// `fit_width` and `fit_height`.
    pub fit: (f64, f64),
}

impl Frame {
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
        let line_position = settings.line_position;
        Ok(Self {
            width,
            height,
            storage: settings.storage.and_then(Size::checked),
            margins: m,
            use_margins: settings.use_margins,
            pixel_aspect: settings
                .pixel_aspect
                .map(|par| if par < 0.0 { 0.0 } else { par }),
            font_scale: settings.font_scale,
            line_spacing: settings.line_spacing,
            line_position,
            content,
            fit,
        })
    }

    /// The geometry of `script` in this frame.
    pub(super) fn place(&self, script: &Script) -> Geometry {
        let info = script.info();
        let play_res = info.play_res();
        let layout_res = self.layout_res(play_res, info.layout_res());
        // The PAR correction: a pixel aspect is derived from the content's
        // aspect and the layout resolution's whenever the script names a
        // layout resolution, or none is set and the storage size is known.
        let (cw, ch) = self.content;
        let user = self.pixel_aspect.unwrap_or(0.0);
        let par = if user == 0.0 || info.layout_res().is_some() {
            if cw != 0 && ch != 0 && (info.layout_res().is_some() || self.storage.is_some()) {
                let dar = f64::from(cw) / f64::from(ch);
                let sar = f64::from(layout_res.0) / f64::from(layout_res.1);
                dar / sar
            } else {
                1.0
            }
        } else {
            user
        };
        Geometry {
            frame: *self,
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
        let par = self.pixel_aspect.unwrap_or(0.0);
        let (cw, ch) = self.content;
        #[allow(clippy::float_cmp)] // libass's test for exactly square pixels
        if par <= 0.0 || par == 1.0 || cw == 0 || ch == 0 {
            return play_res;
        }
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
pub(super) struct Geometry {
    pub frame: Frame,
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

impl Geometry {
    /// The play resolution as `f64`.
    fn res(&self) -> (f64, f64) {
        (f64::from(self.play_res.0), f64::from(self.play_res.1))
    }

    /// Whether an event uses the margin mapping: one placed by its margins
    /// when the settings let it use the frame margins.
    pub(super) fn margin_mode(&self, explicit: bool) -> bool {
        !explicit && self.frame.use_margins
    }

    pub(super) fn x_pos(&self, x: f64) -> f64 {
        x * f64::from(self.frame.content.0) / self.par / self.res().0
            + f64::from(self.frame.margins.left)
    }

    pub(super) fn x_left(&self, x: f64, explicit: bool) -> f64 {
        if self.margin_mode(explicit) {
            x * self.frame.fit.0 / self.par / self.res().0
        } else {
            self.x_pos(x)
        }
    }

    pub(super) fn x_right(&self, x: f64, explicit: bool) -> f64 {
        if self.margin_mode(explicit) {
            x * self.frame.fit.0 / self.par / self.res().0
                + (f64::from(self.frame.width) - self.frame.fit.0)
        } else {
            self.x_pos(x)
        }
    }

    /// Without the pixel aspect: clip rectangles.
    pub(super) fn x_pos_scaled(&self, x: f64) -> f64 {
        x * f64::from(self.frame.content.0) / self.res().0 + f64::from(self.frame.margins.left)
    }

    pub(super) fn y_pos(&self, y: f64) -> f64 {
        y * f64::from(self.frame.content.1) / self.res().1 + f64::from(self.frame.margins.top)
    }

    pub(super) fn y_mid(&self, y: f64, explicit: bool) -> f64 {
        if self.margin_mode(explicit) {
            y * self.frame.fit.1 / self.res().1
                + (f64::from(self.frame.height) - self.frame.fit.1) * 0.5
        } else {
            self.y_pos(y)
        }
    }

    pub(super) fn y_top(&self, y: f64, explicit: bool) -> f64 {
        if self.margin_mode(explicit) {
            y * self.frame.fit.1 / self.res().1
        } else {
            self.y_pos(y)
        }
    }

    pub(super) fn y_sub(&self, y: f64, explicit: bool) -> f64 {
        if self.margin_mode(explicit) {
            y * self.frame.fit.1 / self.res().1 + (f64::from(self.frame.height) - self.frame.fit.1)
        } else {
            self.y_pos(y)
        }
    }

    /// The scales of an event: libass multiplies them by the font scale
    /// unless the event is explicit (its default selective-style setting).
    pub(super) fn scales(&self, explicit: bool, scaled_border: bool) -> Scales {
        let (w, h) = if self.margin_mode(explicit) {
            self.frame.fit
        } else {
            (
                f64::from(self.frame.content.0),
                f64::from(self.frame.content.1),
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
            let k = self.frame.font_scale;
            for v in [&mut scales.screen, &mut scales.blur, &mut scales.border] {
                *v = v.map(|s| s * k);
            }
        }
        scales
    }
}

#[cfg(test)]
mod tests {
    use super::{Frame, Settings, Size};

    #[test]
    fn frames_fit_their_content_and_reject_empty_sides() {
        assert!(Frame::new(&Settings::new(Size::new(0, 720))).is_err());
        let mut settings = Settings::new(Size::new(1920, 1080));
        settings.margins.top = 100;
        settings.margins.bottom = 100;
        let frame = Frame::new(&settings).unwrap();
        assert_eq!(frame.content, (1920, 880));
        // The content keeps its aspect: a 1920x880 area fills the width.
        assert!((frame.fit.0 - 1920.0).abs() < 1e-9);
        assert!((frame.fit.1 - 880.0).abs() < 1e-9);
    }
}
