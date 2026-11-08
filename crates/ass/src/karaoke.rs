//! Karaoke timing over laid-out glyphs, libass's `ass_process_karaoke_effects`.

use std::ops::Range;

use crate::c_long::{lrint, to_int};
use crate::{Karaoke, KaraokeMark};

/// What karaoke needs of one laid-out glyph.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct KaraokeGlyph {
    /// The glyph starts a run (libass's `starts_new_run` after shaping and
    /// wrapping); the first glyph always does.
    pub starts_new_run: bool,
    /// The karaoke tags before the glyph.
    pub mark: KaraokeMark,
    /// Invisible (libass's `skip`), so not part of a syllable's extent.
    pub skip: bool,
    /// Pen position in 26.6 fixed point.
    pub x: i32,
    /// Advance in 26.6 fixed point.
    pub advance: i32,
    /// `\frz` of the glyph, in degrees.
    pub rotate_z: f64,
}

/// One karaoke syllable at one time: a run of glyphs and where its primary
/// colour gives way to the secondary. Glyphs outside every syllable keep the
/// kind and timing of their [`KaraokeMark`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Syllable {
    /// The syllable's glyphs, by index.
    pub glyphs: Range<usize>,
    /// How the syllable is sung.
    pub effect: Karaoke,
    /// The boundary between the primary (left) and secondary part in 26.6
    /// fixed point: -10⁸ before the syllable starts, 10⁸ after it ends.
    pub boundary: i32,
    /// The primary and secondary colours trade places: a filling syllable
    /// rotated between 90° and 270° fills right to left.
    pub swap_colours: bool,
}

impl Syllable {
    /// The boundary relative to a glyph's pen position, as libass stores it
    /// per glyph (`effect_timing`).
    pub const fn timing(&self, glyph: &KaraokeGlyph) -> i32 {
        self.boundary.wrapping_sub(glyph.x)
    }
}

/// The karaoke syllables of laid-out glyphs at one time, in order. A
/// syllable runs from one run start to the next; its timing accumulates as
/// VSFilter does, so `\k100\k0` without a run break advances only the next
/// syllable.
///
/// ```
/// use triergon_ass::{Karaoke, KaraokeGlyph, KaraokeMark, Syllables};
///
/// let mark = KaraokeMark { kind: Some(Karaoke::Fill), timing: 1000, ..KaraokeMark::default() };
/// let glyphs: Vec<KaraokeGlyph> = (0..4)
///     .map(|i| KaraokeGlyph {
///         starts_new_run: i == 0,
///         mark: if i == 0 { mark } else { KaraokeMark::default() },
///         x: 640 * i,
///         advance: 640,
///         ..KaraokeGlyph::default()
///     })
///     .collect();
/// // Halfway through the syllable, its fill has reached x = 1280.
/// let syllable = Syllables::new(&glyphs, 500).next().unwrap();
/// assert_eq!((syllable.glyphs, syllable.boundary), (0..4, 1280));
/// ```
#[derive(Clone, Debug)]
pub struct Syllables<'g> {
    glyphs: &'g [KaraokeGlyph],
    elapsed: i64,
    /// The next glyph index to look at; `glyphs.len()` ends the last run.
    at: usize,
    /// The start of the current run.
    run: Option<usize>,
    effect: Option<Karaoke>,
    timing: i32,
    skip_timing: i32,
    has_reset: bool,
}

impl<'g> Syllables<'g> {
    /// The syllables of `glyphs` at `elapsed` milliseconds into the event.
    pub const fn new(glyphs: &'g [KaraokeGlyph], elapsed: i64) -> Self {
        Self {
            glyphs,
            elapsed,
            at: 0,
            run: None,
            effect: None,
            timing: 0,
            skip_timing: 0,
            has_reset: false,
        }
    }

    /// The run `start..end`, a syllable once a karaoke tag was seen.
    fn syllable(&mut self, start: usize, end: usize) -> Option<Syllable> {
        let first = &self.glyphs[start];
        if first.mark.kind.is_some() {
            self.effect = first.mark.kind;
        }
        let effect = self.effect?;
        if first.mark.reset {
            self.timing = 0;
        }
        let tm_start = i64::from(self.timing.wrapping_add(first.mark.skip_timing));
        let mut tm_end = tm_start + i64::from(first.mark.timing);
        let next = if self.has_reset { 0 } else { tm_end };
        self.timing = (next + i64::from(self.skip_timing)) as i32;
        self.skip_timing = 0;
        self.has_reset = false;
        if effect != Karaoke::Fill {
            tm_end = tm_start;
        }
        let glyphs = &self.glyphs[start..end];
        let mut swap_colours = false;
        let boundary = if self.elapsed < tm_start {
            -100_000_000
        } else if self.elapsed >= tm_end {
            100_000_000
        } else {
            let last = glyphs.len() - 1;
            let first_visible = glyphs[..last].iter().position(|g| !g.skip).unwrap_or(last);
            let last_visible = glyphs[first_visible + 1..]
                .iter()
                .rposition(|g| !g.skip)
                .map_or(first_visible, |j| first_visible + 1 + j);
            let x_start = glyphs[first_visible].x;
            let last = &glyphs[last_visible];
            let x_end = last.x.wrapping_add(last.advance);
            let mut dt = (self.elapsed - tm_start) as f64 / (tm_end - tm_start) as f64;
            let rotation = first.rotate_z % 360.0;
            if rotation > 90.0 && rotation < 270.0 {
                dt = 1.0 - dt;
                swap_colours = true;
            }
            to_int(
                core::ffi::c_long::from(x_start)
                    .wrapping_add(lrint(f64::from(x_end.wrapping_sub(x_start)) * dt)),
            )
        };
        Some(Syllable {
            glyphs: start..end,
            effect,
            boundary,
            swap_colours,
        })
    }
}

impl Iterator for Syllables<'_> {
    type Item = Syllable;

    fn next(&mut self) -> Option<Syllable> {
        while self.at <= self.glyphs.len() {
            let i = self.at;
            self.at += 1;
            if let Some(glyph) = self.glyphs.get(i).filter(|g| !g.starts_new_run) {
                if glyph.mark.reset {
                    self.has_reset = true;
                    self.skip_timing = 0;
                }
                self.skip_timing = self.skip_timing.wrapping_add(glyph.mark.skip_timing);
                continue;
            }
            if let Some(start) = self.run.replace(i)
                && let Some(syllable) = self.syllable(start, i)
            {
                return Some(syllable);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{KaraokeGlyph, Syllables};
    use crate::{Karaoke, KaraokeMark};

    fn glyphs(kind: Karaoke) -> Vec<KaraokeGlyph> {
        let mark = KaraokeMark {
            kind: Some(kind),
            timing: 1000,
            ..KaraokeMark::default()
        };
        (0..4)
            .map(|i| KaraokeGlyph {
                starts_new_run: i % 2 == 0,
                mark: if i % 2 == 0 {
                    mark
                } else {
                    KaraokeMark::default()
                },
                x: 640 * i,
                advance: 640,
                ..KaraokeGlyph::default()
            })
            .collect()
    }

    #[test]
    fn a_fill_syllable_sweeps_across_its_glyphs() {
        let glyphs = glyphs(Karaoke::Fill);
        let syllables: Vec<_> = Syllables::new(&glyphs, 500).collect();
        assert_eq!(syllables.len(), 2);
        // Halfway through the first: the boundary is at x = 640.
        assert_eq!(
            (syllables[0].glyphs.clone(), syllables[0].boundary),
            (0..2, 640)
        );
        assert_eq!(syllables[0].timing(&glyphs[1]), 0);
        // The second has not started.
        assert_eq!(syllables[1].boundary, -100_000_000);
    }

    #[test]
    fn switching_syllables_change_at_their_start() {
        let glyphs = glyphs(Karaoke::Switch);
        let boundaries: Vec<_> = Syllables::new(&glyphs, 1500).map(|s| s.boundary).collect();
        assert_eq!(boundaries, [100_000_000, 100_000_000]);
    }
}
