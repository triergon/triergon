//! Hangul: syllables are composed when the font has the whole syllable and
//! decomposed into jamo, which get `ljmo`, `vjmo` and `tjmo`, otherwise; a tone
//! mark after a syllable moves in front of it.
//!
//! Hangul fonts are not designed to mix precomposed syllables and jamo, so
//! normalisation does not compose for this script.

use crate::buffer::{Buffer, Info};
use crate::metrics::Scaled;
use crate::plan::{Builder, FeatureFlags, Plan};
use std::ops::Range;
use triergon_core::Result;

/// The jamo features, in the order of [`Jamo`] after `None`.
const FEATURES: [&[u8; 4]; 3] = [b"ljmo", b"vjmo", b"tjmo"];

/// The jamo feature a glyph gets, kept in the glyph's first shaper byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Jamo {
    None,
    Leading,
    Vowel,
    Trailing,
}

impl Jamo {
    fn of(info: &Info) -> Self {
        match info.shaper[0] {
            1 => Self::Leading,
            2 => Self::Vowel,
            3 => Self::Trailing,
            _ => Self::None,
        }
    }

    fn set(self, info: &mut Info) {
        info.shaper[0] = self as u8;
    }

    fn feature(self) -> Option<&'static [u8; 4]> {
        match self {
            Self::None => None,
            Self::Leading => Some(FEATURES[0]),
            Self::Vowel => Some(FEATURES[1]),
            Self::Trailing => Some(FEATURES[2]),
        }
    }
}

/// The modern jamo that compose into precomposed syllables, and the syllables.
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;
const S_BASE: u32 = 0xAC00;
const S_COUNT: u32 = L_COUNT * N_COUNT;

fn is_combining_l(c: u32) -> bool {
    (L_BASE..L_BASE + L_COUNT).contains(&c)
}

fn is_combining_v(c: u32) -> bool {
    (V_BASE..V_BASE + V_COUNT).contains(&c)
}

fn is_combining_t(c: u32) -> bool {
    (T_BASE + 1..T_BASE + T_COUNT).contains(&c)
}

fn is_syllable(c: u32) -> bool {
    (S_BASE..S_BASE + S_COUNT).contains(&c)
}

/// Leading consonant jamo, old ones included.
fn is_l(c: u32) -> bool {
    matches!(c, 0x1100..=0x115F | 0xA960..=0xA97C)
}

fn is_v(c: u32) -> bool {
    matches!(c, 0x1160..=0x11A7 | 0xD7B0..=0xD7C6)
}

fn is_t(c: u32) -> bool {
    matches!(c, 0x11A8..=0x11FF | 0xD7CB..=0xD7FB)
}

fn is_jamo(c: u32) -> bool {
    is_l(c) || is_v(c) || is_t(c)
}

fn is_tone_mark(c: u32) -> bool {
    matches!(c, 0x302E..=0x302F)
}

const DOTTED_CIRCLE: u32 = 0x25CC;

pub(super) fn collect_features(b: &mut Builder<'_>) {
    for tag in FEATURES {
        b.add_feature(tag, FeatureFlags::NONE, 1);
    }
}

/// `calt` gets a mask of its own so that it can be kept off jamo: some fonts
/// put all their jamo lookups in `calt`, which Uniscribe does not apply to Hangul.
pub(super) fn override_features(b: &mut Builder<'_>) {
    b.add_feature(b"calt", FeatureFlags::NONE, 1);
}

pub(super) fn setup_masks(plan: &Plan, buffer: &mut Buffer) {
    let masks = [Jamo::Leading, Jamo::Vowel, Jamo::Trailing]
        .map(|jamo| jamo.feature().map_or(0, |tag| plan.one_mask(tag)));
    let calt = plan.one_mask(b"calt");
    for info in buffer.infos_mut() {
        let jamo = Jamo::of(info);
        if jamo != Jamo::None {
            info.mask |= masks[jamo as usize - 1];
        }
        if is_jamo(info.glyph) {
            info.mask &= !calt;
        }
    }
}

struct Preprocess<'a, 'b> {
    font: &'a Scaled<'b>,
}

impl Preprocess<'_, '_> {
    fn has_glyph(&self, c: u32) -> bool {
        self.font.glyph(c).is_some()
    }

    /// A glyph without advance is meant to overstrike.
    fn is_zero_width(&self, c: u32) -> bool {
        self.font
            .glyph(c)
            .is_some_and(|g| self.font.advance(g) == 0)
    }

    /// A tone mark goes before the syllable it follows unless it overstrikes;
    /// without a syllable it gets a dotted circle, if the font has one.
    fn tone_mark(&self, buffer: &mut Buffer, syllable: Option<Range<usize>>) -> Result<()> {
        let tone = buffer.cur().glyph;
        if let Some(Range { start, end }) = syllable {
            buffer.next_glyph()?;
            if !self.is_zero_width(tone) {
                buffer.merge_out_clusters(start, end + 1)?;
                buffer.out_infos_mut()[start..=end].rotate_right(1);
            }
        } else if self.has_glyph(DOTTED_CIRCLE) {
            let chars = if self.is_zero_width(tone) {
                [DOTTED_CIRCLE, tone]
            } else {
                [tone, DOTTED_CIRCLE]
            };
            buffer.replace_glyphs(1, &chars)?;
        } else {
            buffer.next_glyph()?;
        }
        Ok(())
    }

    /// `<L, V>` or `<L, V, T>` at the current position, composed if the font has
    /// the syllable, else left as jamo with their features. `None` when the
    /// current character does not start one.
    fn jamo_syllable(&self, buffer: &mut Buffer) -> Result<Option<usize>> {
        let l = buffer.cur().glyph;
        let Some(v) = buffer.peek(1).map(|i| i.glyph).filter(|&v| is_v(v)) else {
            return Ok(None);
        };
        let t = buffer.peek(2).map(|i| i.glyph).filter(|&t| is_t(t));
        if is_combining_l(l) && is_combining_v(v) && t.is_none_or(is_combining_t) {
            let t_index = t.map_or(0, |t| t - T_BASE);
            let s = S_BASE + (l - L_BASE) * N_COUNT + (v - V_BASE) * T_COUNT + t_index;
            if self.has_glyph(s) {
                buffer.replace_glyphs(if t.is_some() { 3 } else { 2 }, &[s])?;
                return Ok(Some(1));
            }
        }
        let jamo: &[Jamo] = if t.is_some() {
            &[Jamo::Leading, Jamo::Vowel, Jamo::Trailing]
        } else {
            &[Jamo::Leading, Jamo::Vowel]
        };
        for &feature in jamo {
            feature.set(buffer.cur_mut());
            buffer.next_glyph()?;
        }
        Ok(Some(jamo.len()))
    }

    /// A precomposed syllable at the current position, with the trailing jamo
    /// after it composed in, or decomposed when the font lacks the syllable or a
    /// trailing jamo that does not compose follows. `None` when no syllable is
    /// left in the output.
    fn precomposed_syllable(&self, buffer: &mut Buffer) -> Result<Option<usize>> {
        let s = buffer.cur().glyph;
        let has_glyph = self.has_glyph(s);
        let n_index = (s - S_BASE) % N_COUNT;
        let t_index = n_index % T_COUNT;
        let next = buffer.peek(1).map(|i| i.glyph);
        if t_index == 0
            && let Some(t) = next.filter(|&t| is_combining_t(t))
        {
            let lvt = s + (t - T_BASE);
            if self.has_glyph(lvt) {
                buffer.replace_glyphs(2, &[lvt])?;
                return Ok(Some(1));
            }
        }
        let trailing_follows = t_index == 0 && next.is_some_and(is_t);
        if !has_glyph || trailing_follows {
            let jamo = [
                L_BASE + (s - S_BASE) / N_COUNT,
                V_BASE + n_index / T_COUNT,
                T_BASE + t_index,
            ];
            let len = if t_index == 0 { 2 } else { 3 };
            if jamo[..len].iter().all(|&c| self.has_glyph(c)) {
                let start = buffer.out_len;
                buffer.replace_glyphs(1, &jamo[..len])?;
                // A trailing jamo that did not compose joins the syllable.
                if has_glyph && t_index == 0 {
                    buffer.next_glyph()?;
                }
                let end = buffer.out_len;
                let features = [Jamo::Leading, Jamo::Vowel, Jamo::Trailing];
                for (i, feature) in (start..end).zip(features) {
                    feature.set(buffer.out_info_mut(i));
                }
                buffer.merge_out_clusters(start, end)?;
                return Ok(Some(end - start));
            }
        }
        buffer.next_glyph()?;
        Ok(has_glyph.then_some(1))
    }
}

/// Composes or decomposes each syllable as the font supports it and moves tone
/// marks, marking the jamo left for their features.
pub(super) fn preprocess_text(buffer: &mut Buffer, font: &Scaled<'_>) -> Result<()> {
    let pre = Preprocess { font };
    buffer.clear_output();
    // The output range of the syllable just seen, if the last thing seen was one.
    let mut syllable: Option<Range<usize>> = None;
    while buffer.idx < buffer.len() {
        buffer.charge(1)?;
        let c = buffer.cur().glyph;
        if is_tone_mark(c) {
            pre.tone_mark(buffer, syllable.take())?;
            continue;
        }
        let start = buffer.out_len;
        let len = if is_l(c) {
            let len = pre.jamo_syllable(buffer)?;
            if len.is_some() {
                buffer.merge_out_clusters(start, buffer.out_len)?;
            } else {
                buffer.next_glyph()?;
            }
            len
        } else if is_syllable(c) {
            pre.precomposed_syllable(buffer)?
        } else {
            buffer.next_glyph()?;
            None
        };
        syllable = len.map(|len| start..start + len);
    }
    buffer.sync()
}

#[cfg(test)]
mod tests {
    use super::{Jamo, is_combining_t, is_jamo, is_syllable, is_t, preprocess_text};
    use crate::shapers::testing::{TestFont, contents, scaled, text_buffer};
    use triergon_core::Budget;

    /// Modern jamo, the syllable 가 (LV) but not 각 (LVT), and a spacing tone mark.
    const FONT: TestFont = TestFont(&[
        (0x1100, 500),
        (0x1161, 500),
        (0x11A8, 500),
        (0xAC00, 1000),
        (0x302E, 300),
        (0x25CC, 1000),
    ]);

    fn preprocess(font: &TestFont, text: &[u32]) -> (Vec<(u32, u32)>, Vec<Jamo>) {
        let budget = Budget::new(1 << 16).expect("budget");
        let mut buffer = text_buffer(&budget, text);
        preprocess_text(&mut buffer, &scaled(font)).expect("preprocesses");
        let jamo = buffer.infos().iter().map(Jamo::of).collect();
        (contents(&buffer), jamo)
    }

    #[test]
    fn syllables_compose_when_the_font_has_them() {
        let (glyphs, jamo) = preprocess(&FONT, &[0x1100, 0x1161, 0x0020, 0x1100, 0x1161, 0x11A8]);
        assert_eq!(
            glyphs,
            [
                (0xAC00, 0),
                (0x20, 2),
                (0x1100, 3),
                (0x1161, 3),
                (0x11A8, 3)
            ]
        );
        assert_eq!(
            jamo,
            [
                Jamo::None,
                Jamo::None,
                Jamo::Leading,
                Jamo::Vowel,
                Jamo::Trailing
            ]
        );
    }

    #[test]
    fn syllables_decompose_when_the_font_lacks_them() {
        use Jamo::{Leading, Trailing, Vowel};
        // 각 is missing, so it and 가 followed by a trailing jamo become jamo.
        let (glyphs, jamo) = preprocess(&FONT, &[0xAC01, 0xAC00, 0x11A8]);
        assert_eq!(
            glyphs,
            [
                (0x1100, 0),
                (0x1161, 0),
                (0x11A8, 0),
                (0x1100, 1),
                (0x1161, 1),
                (0x11A8, 1)
            ]
        );
        assert_eq!(jamo, [Leading, Vowel, Trailing, Leading, Vowel, Trailing]);
    }

    #[test]
    fn tone_marks_precede_their_syllable_or_get_a_dotted_circle() {
        const OVERSTRIKE: TestFont = TestFont(&[(0xAC00, 1000), (0x302E, 0), (0x25CC, 1000)]);
        let (glyphs, _) = preprocess(&FONT, &[0xAC00, 0x302E, 0x302E]);
        assert_eq!(glyphs, [(0x302E, 0), (0xAC00, 0), (0x302E, 2), (0x25CC, 2)]);
        // An overstriking tone mark stays after its syllable, and comes after
        // the dotted circle.
        let (glyphs, _) = preprocess(&OVERSTRIKE, &[0xAC00, 0x302E, 0x302E]);
        assert_eq!(glyphs, [(0xAC00, 0), (0x302E, 1), (0x25CC, 2), (0x302E, 2)]);
    }

    #[test]
    fn jamo_ranges_match_unicode() {
        assert!(is_syllable(0xAC00) && is_syllable(0xD7A3) && !is_syllable(0xD7A4));
        assert!(is_combining_t(0x11A8) && is_combining_t(0x11C2) && !is_combining_t(0x11C3));
        assert!(is_t(0x11C3) && !is_t(0x11A7));
        assert!(is_jamo(0xA960) && is_jamo(0xD7B0) && is_jamo(0xD7FB) && !is_jamo(0xD7C7));
    }
}
