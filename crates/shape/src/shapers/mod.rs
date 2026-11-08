//! Script shapers: what HarfBuzz does differently for some scripts around the
//! common pipeline.

mod arabic;
mod arabic_fallback;
mod hangul;
mod hebrew;
mod indic;
mod khmer;
mod myanmar;
mod syllabic;
#[cfg(test)]
mod testing;
mod thai;
mod universal;

pub(crate) use arabic::Action as ArabicAction;
pub(crate) use hangul::Jamo;

use crate::buffer::Buffer;
use crate::face::Face;
use crate::metrics::Scaled;
use crate::normalize::Mode;
use crate::plan::{Builder, Plan};
use crate::tag::{Direction, Script, Tag};
use triergon_core::Result;

/// The shaper HarfBuzz picks for a script, direction and chosen GSUB script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shaper {
    Default,
    Arabic,
    Hebrew,
    /// Thai and Lao.
    Thai,
    Hangul,
    Indic,
    /// The Universal Shaping Engine.
    Universal,
    Khmer,
    Myanmar,
    /// Myanmar in the Zawgyi encoding: no shaping of its own at all.
    Zawgyi,
}

/// Work a shaper does between two lookup stages: one of its steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pause {
    Arabic(arabic::Step),
    Indic(indic::Step),
    Universal(universal::Step),
    Khmer(khmer::Step),
    Myanmar(myanmar::Step),
}

/// When mark advances are zeroed, by their GDEF class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ZeroMarks {
    /// Before GPOS.
    Early,
    /// After GPOS.
    Late,
}

impl Shaper {
    /// `hb_ot_shaper_categorize`.
    pub(crate) fn categorize(script: Script, direction: Direction, gsub: Option<Tag>) -> Self {
        let dflt = gsub == Some(Tag::new(b"DFLT"));
        let latn = gsub == Some(Tag::new(b"latn"));
        match &script.to_bytes() {
            b"Arab" | b"Syrc" => {
                if (!dflt || &script.to_bytes() == b"Arab")
                    && matches!(direction, Direction::LeftToRight | Direction::RightToLeft)
                {
                    Self::Arabic
                } else {
                    Self::Default
                }
            }
            b"Hebr" => Self::Hebrew,
            b"Thai" | b"Laoo" => Self::Thai,
            b"Hang" => Self::Hangul,
            b"Khmr" => Self::Khmer,
            b"Qaag" => Self::Zawgyi,
            b"Beng" | b"Deva" | b"Gujr" | b"Guru" | b"Knda" | b"Mlym" | b"Orya" | b"Taml"
            | b"Telu" => {
                if dflt || latn {
                    Self::Default
                } else if gsub.is_some_and(|tag| tag.to_u32() & 0xFF == u32::from(b'3')) {
                    // Fonts for the `dev3` generation of tags use the USE.
                    Self::Universal
                } else {
                    Self::Indic
                }
            }
            b"Mymr" => {
                // `mymr` fonts predate the Myanmar shaping specification.
                if dflt || latn || gsub == Some(Tag::new(b"mymr")) {
                    Self::Default
                } else {
                    Self::Myanmar
                }
            }
            code if USE_SCRIPTS.contains(&code) => {
                if dflt || latn {
                    Self::Default
                } else {
                    Self::Universal
                }
            }
            _ => Self::Default,
        }
    }

    pub(crate) fn collect_features(self, builder: &mut Builder<'_>, script: Script) {
        match self {
            Self::Arabic => arabic::collect_features(builder, &script.to_bytes() == b"Arab"),
            Self::Hangul => hangul::collect_features(builder),
            Self::Indic => indic::collect_features(builder),
            Self::Universal => universal::collect_features(builder),
            Self::Khmer => khmer::collect_features(builder),
            Self::Myanmar => myanmar::collect_features(builder),
            _ => {}
        }
    }

    pub(crate) fn override_features(self, builder: &mut Builder<'_>) {
        match self {
            Self::Hangul => hangul::override_features(builder),
            Self::Indic => indic::override_features(builder),
            Self::Khmer => khmer::override_features(builder),
            _ => {}
        }
    }

    /// The script that GPOS must have been chosen for, if the shaper requires one.
    pub(crate) fn gpos_tag(self) -> Option<Tag> {
        (self == Self::Hebrew).then(|| Tag::new(b"hebr"))
    }

    pub(crate) fn fallback_position(self) -> bool {
        !matches!(
            self,
            Self::Thai | Self::Indic | Self::Universal | Self::Khmer | Self::Myanmar | Self::Zawgyi
        )
    }

    pub(crate) fn zero_width_marks(self) -> Option<ZeroMarks> {
        match self {
            Self::Hangul | Self::Indic | Self::Khmer | Self::Zawgyi => None,
            Self::Universal | Self::Myanmar => Some(ZeroMarks::Early),
            _ => Some(ZeroMarks::Late),
        }
    }

    pub(crate) fn normalization(self) -> Mode {
        match self {
            Self::Hangul | Self::Zawgyi => Mode::None,
            Self::Indic | Self::Universal | Self::Khmer | Self::Myanmar => {
                Mode::ComposedDiacriticsNoShortCircuit
            }
            _ => Mode::ComposedDiacritics,
        }
    }

    /// Rewrites the characters before normalisation.
    pub(crate) fn preprocess_text(
        self,
        plan: &Plan,
        buffer: &mut Buffer,
        font: &Scaled<'_>,
    ) -> Result<()> {
        match self {
            Self::Thai => thai::preprocess_text(plan, buffer, font),
            Self::Hangul => hangul::preprocess_text(buffer, font),
            Self::Indic | Self::Universal => {
                syllabic::insert_vowel_constraint_circles(plan.script, buffer)
            }
            _ => Ok(()),
        }
    }

    /// The shaper's decomposition, if it replaces Unicode's.
    pub(crate) fn decompose(self, ab: u32) -> Option<(u32, u32)> {
        match self {
            Self::Indic => indic::decompose(ab),
            Self::Khmer => khmer::decompose(ab),
            _ => crate::unicode::decompose(ab),
        }
    }

    /// The shaper's composition, if it replaces Unicode's.
    pub(crate) fn compose(self, plan: &Plan, a: u32, b: u32) -> Option<u32> {
        match self {
            Self::Hebrew => hebrew::compose(plan, a, b),
            Self::Indic => indic::compose(a, b),
            Self::Universal | Self::Khmer => universal::compose(a, b),
            _ => crate::unicode::compose(a, b),
        }
    }

    pub(crate) fn reorder_marks(self, buffer: &mut Buffer, start: usize, end: usize) -> Result<()> {
        match self {
            Self::Hebrew => hebrew::reorder_marks(buffer, start, end),
            Self::Arabic => arabic::reorder_marks(buffer, start, end),
            _ => Ok(()),
        }
    }

    pub(crate) fn setup_masks(self, plan: &Plan, buffer: &mut Buffer) {
        match self {
            Self::Arabic => arabic::setup_masks(plan, buffer),
            Self::Hangul => hangul::setup_masks(plan, buffer),
            // Khmer and Myanmar read their categories from the Indic table.
            Self::Indic | Self::Khmer | Self::Myanmar => indic::setup_masks(buffer),
            Self::Universal => universal::setup_masks(plan, buffer),
            _ => {}
        }
    }

    /// Runs a pause the shaper added; whether it changed glyphs.
    pub(crate) fn pause(
        pause: Pause,
        plan: &Plan,
        face: &Face<'_>,
        buffer: &mut Buffer,
        font: &Scaled<'_>,
    ) -> Result<bool> {
        match pause {
            Pause::Arabic(step) => arabic::pause(step, plan, face, buffer, font),
            Pause::Indic(step) => indic::pause(step, plan, face, buffer, font),
            Pause::Universal(step) => universal::pause(step, plan, buffer, font),
            Pause::Khmer(step) => khmer::pause(step, plan, buffer, font),
            Pause::Myanmar(step) => myanmar::pause(step, buffer, font),
        }
    }

    pub(crate) fn postprocess_glyphs(
        self,
        plan: &Plan,
        buffer: &mut Buffer,
        font: &Scaled<'_>,
    ) -> Result<()> {
        if self == Self::Arabic {
            arabic::postprocess_glyphs(plan, buffer, font)?;
        }
        Ok(())
    }
}

/// Scripts HarfBuzz gives the Universal Shaping Engine.
const USE_SCRIPTS: &[&[u8; 4]] = &[
    b"Tibt", b"Mong", b"Sinh", b"Buhd", b"Hano", b"Tglg", b"Tagb", b"Limb", b"Tale", b"Bugi",
    b"Khar", b"Sylo", b"Tfng", b"Bali", b"Nkoo", b"Phag", b"Cham", b"Kali", b"Lepc", b"Rjng",
    b"Saur", b"Sund", b"Egyp", b"Java", b"Kthi", b"Mtei", b"Lana", b"Tavt", b"Batk", b"Brah",
    b"Mand", b"Cakm", b"Plrd", b"Shrd", b"Takr", b"Dupl", b"Gran", b"Khoj", b"Sind", b"Mahj",
    b"Mani", b"Modi", b"Hmng", b"Phlp", b"Sidd", b"Tirh", b"Ahom", b"Mult", b"Adlm", b"Bhks",
    b"Marc", b"Newa", b"Gonm", b"Soyo", b"Zanb", b"Dogr", b"Gong", b"Rohg", b"Maka", b"Medf",
    b"Sogo", b"Sogd", b"Elym", b"Nand", b"Hmnp", b"Wcho", b"Chrs", b"Diak", b"Kits", b"Yezi",
    b"Cpmn", b"Ougr", b"Tnsa", b"Toto", b"Vith", b"Kawi", b"Nagm", b"Gara", b"Gukh", b"Krai",
    b"Onao", b"Sunu", b"Todr", b"Tutg", b"Berf", b"Sidt", b"Tayo", b"Tols", b"Jurc", b"Pcun",
    b"Seal",
];
