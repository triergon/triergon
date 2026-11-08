//! The `Banner` and `Scroll` transition effects of the event effect field.

use crate::c_long::atoi;
use crate::number::ffmax;

/// Which way a transition moves the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScrollDirection {
    /// `Banner` with its left-to-right flag.
    LeftToRight,
    /// `Banner`.
    RightToLeft,
    /// `Scroll down`.
    TopToBottom,
    /// `Scroll up`.
    BottomToTop,
}

/// A scrolling event at one time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scroll {
    /// Which way the text moves.
    pub direction: ScrollDirection,
    /// The distance scrolled so far in script pixels.
    pub shift: f64,
    /// The band `(y0, y1)` of a vertical scroll, `y0 <= y1`; `None` for a banner.
    pub band: Option<(i32, i32)>,
}

impl Scroll {
    /// A banner, which moves horizontally.
    pub const fn is_horizontal(&self) -> bool {
        self.band.is_none()
    }
}

/// libass's `ass_apply_transition_effects`: `Banner;delay[;left_to_right]`,
/// `Scroll up;y0;y1;delay` and `Scroll down;...`, `elapsed` milliseconds
/// into the event. The delay per pixel is rescaled as VSFilter truncates it
/// in storage pixels, which needs the play and layout resolutions.
pub(crate) fn transition(
    effect: &[u8],
    elapsed: i64,
    play_res: (i32, i32),
    layout_res: (i32, i32),
) -> Option<Scroll> {
    let mut values = [0_i32; 4];
    let mut count = 0;
    let mut rest = effect;
    while count < 4 {
        let Some(semicolon) = rest.iter().position(|&c| c == b';') else {
            break;
        };
        rest = &rest[semicolon + 1..];
        values[count] = atoi(rest);
        count += 1;
    }
    // VSFilter keeps a delay below one storage pixel per millisecond at one,
    // truncating in storage pixels.
    let shift = |delay: i32, layout: i32, play: i32| {
        let scale = f64::from(layout) / f64::from(play);
        let delay = f64::from(ffmax(f64::from(delay) / scale, 1.0) as i32) * scale;
        elapsed as f64 / delay
    };
    if effect.starts_with(b"Banner;") {
        let direction = if count >= 2 && values[1] != 0 {
            ScrollDirection::LeftToRight
        } else {
            ScrollDirection::RightToLeft
        };
        return Some(Scroll {
            direction,
            shift: shift(values[0], layout_res.0, play_res.0),
            band: None,
        });
    }
    let direction = if effect.starts_with(b"Scroll up;") {
        ScrollDirection::BottomToTop
    } else if effect.starts_with(b"Scroll down;") {
        ScrollDirection::TopToBottom
    } else {
        return None;
    };
    if count < 3 {
        return None;
    }
    let (a, b) = (values[0], values[1]);
    Some(Scroll {
        direction,
        shift: shift(values[2], layout_res.1, play_res.1),
        band: Some((a.min(b), a.max(b))),
    })
}

#[cfg(test)]
mod tests {
    use super::{ScrollDirection, transition};

    #[test]
    fn effects_parse_as_libass() {
        let banner = transition(b"Banner;10;1", 500, (640, 480), (640, 480)).unwrap();
        assert_eq!(banner.direction, ScrollDirection::LeftToRight);
        assert_eq!(banner.shift, 50.0);
        let banner = transition(b"Banner;0", 500, (640, 480), (1280, 960)).unwrap();
        // A zero delay is one millisecond per storage pixel: two per script pixel.
        assert_eq!(banner.shift, 250.0);
        let scroll = transition(b"Scroll up;200;100;5", 100, (640, 480), (640, 480)).unwrap();
        assert_eq!(scroll.band, Some((100, 200)));
        assert_eq!(scroll.direction, ScrollDirection::BottomToTop);
        assert!(transition(b"Scroll down;1;2", 0, (640, 480), (640, 480)).is_none());
        assert!(transition(b"banner;1", 0, (640, 480), (640, 480)).is_none());
    }
}
