#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_docs,
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]

mod c_long;
mod drawing;
mod effect;
mod evaluate;
mod karaoke;
mod number;
mod render;
mod scan;
mod script;
mod strings;
mod tags;
mod text;

pub use drawing::{Drawing, drawing_scale};
pub use effect::{Scroll, ScrollDirection};
pub use evaluate::{
    Context, EventState, FontDesc, Glyph, GlyphState, KaraokeMark, Placement, Symbol,
};
pub use karaoke::{KaraokeGlyph, Syllable, Syllables};
pub use render::{Fonts, FrameMargins, Image, ImageKind, Renderer, Settings, Size};
pub use script::{
    Alignment, Bold, BorderStyle, ColourSlot, Colours, EmbeddedFont, Event, HAlign, Info, Italic,
    Justify, Margins, Options, Rgba, Script, ScriptType, Style, VAlign, WrapStyle, YCbCrMatrix,
};
pub use tags::{Clip, Fade, FontSize, Karaoke, Move, Tag, Tags, TransformTimes, VectorClip};

use triergon_core::{Error, Result, WorkBudget};

/// Format-policy limits. Storage is bounded separately by the
/// [`Budget`](triergon_core::Budget) a [`Script`] and an
/// [`Outline`](triergon_raster::Outline) are charged to. Exceeding a limit is
/// [`Error::Policy`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Work units of one call: input bytes and lines, format fields, override
    /// tags, glyphs and drawing points. A [`Context`] takes its units from the
    /// limits of its script, for the whole walk over one event.
    pub work: u64,
    /// Styles of one script, the default style included.
    pub styles: usize,
    /// Events held at once.
    pub events: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            work: 1 << 28,
            styles: 1 << 16,
            events: 1 << 22,
        }
    }
}

impl Limits {
    /// The work budget of one call.
    pub(crate) fn work(&self) -> WorkBudget {
        WorkBudget::new(self.work)
    }
}

/// Charges `units` before the work is done; running out is `Error::Policy`.
#[inline]
pub(crate) fn charge(work: &mut WorkBudget, units: usize) -> Result<()> {
    match u64::try_from(units) {
        Ok(units) if work.try_charge(units) => Ok(()),
        _ => Err(Error::Policy("ass work")),
    }
}

#[cfg(test)]
mod tests {
    use super::{Limits, charge};
    use triergon_core::Error;

    #[test]
    fn work_beyond_the_limit_is_a_policy_error() {
        let limits = Limits {
            work: 3,
            ..Limits::default()
        };
        let mut work = limits.work();
        assert_eq!(charge(&mut work, 3), Ok(()));
        assert_eq!(charge(&mut work, 1), Err(Error::Policy("ass work")));
    }
}
