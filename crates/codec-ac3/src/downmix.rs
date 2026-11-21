//! Downmix metadata as FFmpeg exports it, derived from the transmitted mix level codes.

use crate::DownmixCodes;
use triergon_codec::{DownmixLevels, SurroundEncoding};

/// Preferred stereo downmix (dmixmod, Annex D Table D2.2; '11' = Pro Logic II as FFmpeg
/// reports it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownmixType {
    /// Not indicated.
    Unknown,
    /// Lo/Ro (left-only/right-only) stereo.
    LoRo,
    /// Lt/Rt (left-total/right-total) matrix-surround stereo.
    LtRt,
    /// Dolby Pro Logic II.
    DplII,
}

/// Downmix levels as linear gains, mirroring FFmpeg 9.0.2's `AVDownmixInfo` export.
/// Every field is `Some` when the info is exported.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DownmixInfo {
    /// Stereo downmix the stream prefers.
    pub preferred_downmix_type: Option<DownmixType>,
    /// Lo/Ro center mix level.
    pub center_mix_level: Option<f64>,
    /// Lt/Rt center mix level.
    pub center_mix_level_ltrt: Option<f64>,
    /// Lo/Ro surround mix level.
    pub surround_mix_level: Option<f64>,
    /// Lt/Rt surround mix level.
    pub surround_mix_level_ltrt: Option<f64>,
    /// LFE mix level (0 when the stream disables LFE mixing).
    pub lfe_mix_level: Option<f64>,
}

impl DownmixInfo {
    /// The stereo downmix FFmpeg's resampler applies: the Lt/Rt levels and surround
    /// encoding when the stream prefers an Lt/Rt (or Pro Logic II) downmix, else the
    /// Lo/Ro levels; absent levels keep the defaults.
    pub fn levels(&self) -> DownmixLevels {
        let default = DownmixLevels::default();
        let (center, surround, encoding) = match self.preferred_downmix_type {
            Some(DownmixType::LtRt) => (
                self.center_mix_level_ltrt,
                self.surround_mix_level_ltrt,
                SurroundEncoding::Dolby,
            ),
            Some(DownmixType::DplII) => (
                self.center_mix_level_ltrt,
                self.surround_mix_level_ltrt,
                SurroundEncoding::ProLogicII,
            ),
            _ => (
                self.center_mix_level,
                self.surround_mix_level,
                SurroundEncoding::None,
            ),
        };
        DownmixLevels {
            center: center.map_or(default.center, |v| v as f32),
            surround: surround.map_or(default.surround, |v| v as f32),
            lfe: self.lfe_mix_level.map_or(default.lfe, |v| v as f32),
            encoding,
        }
    }
}

/// Annex D Tables D2.3-D2.6 gains for the 3-bit mix level codes (+3 dB .. -6 dB, -inf),
/// i.e. powers of 2^(1/4); FFmpeg 9.0.2 also uses this table for the reserved
/// surround codes 0..2.
const MIX_GAINS: [f64; 8] = [
    core::f64::consts::SQRT_2,
    1.189_207_115_002_721,
    1.0,
    0.840_896_415_253_714_5,
    core::f64::consts::FRAC_1_SQRT_2,
    0.594_603_557_501_360_5,
    0.5,
    0.0,
];

/// Derive FFmpeg-compatible downmix info from one syncframe's header fields
/// (black-box verified).
pub(crate) fn downmix_info(eac3: bool, acmod: u8, c: &DownmixCodes) -> Option<DownmixInfo> {
    if acmod <= 2 {
        return None;
    }
    let g = |code: u8| MIX_GAINS[(code & 7) as usize];
    if !eac3 {
        // Tables 5.9 / 5.10 (reserved codes as the intermediate -4.5 / -6 dB values);
        // absent fields default to -4.5 dB / -6 dB. Lt/Rt levels are not carried by the
        // main-body syntax (FFmpeg ignores the Annex D xbsi1 fields) and report -3 dB.
        let clev = match c.cmixlev {
            Some(0) => g(4),
            Some(2) => g(6),
            _ => g(5),
        };
        let slev = match c.surmixlev {
            Some(0) => g(4),
            Some(2) => g(7),
            _ => g(6),
        };
        return Some(DownmixInfo {
            preferred_downmix_type: Some(DownmixType::Unknown),
            center_mix_level: Some(clev),
            center_mix_level_ltrt: Some(g(4)),
            surround_mix_level: Some(slev),
            surround_mix_level_ltrt: Some(g(4)),
            lfe_mix_level: Some(0.0),
        });
    }
    let pref = match c.dmixmod {
        Some(1) => DownmixType::LtRt,
        Some(2) => DownmixType::LoRo,
        Some(3) => DownmixType::DplII,
        _ => DownmixType::Unknown,
    };
    Some(DownmixInfo {
        preferred_downmix_type: Some(pref),
        center_mix_level: Some(g(c.lorocmixlev.unwrap_or(5))),
        center_mix_level_ltrt: Some(g(c.ltrtcmixlev.unwrap_or(0))),
        surround_mix_level: Some(g(c.lorosurmixlev.unwrap_or(6))),
        surround_mix_level_ltrt: Some(g(c.ltrtsurmixlev.unwrap_or(0))),
        // Annex E 2.3.1.11: LFE mix level (dB) = 10 - lfemixlevcod; absent = disabled.
        lfe_mix_level: Some(match c.lfemixlevcod {
            Some(code) => 10f64.powf((10.0 - code as f64) / 20.0),
            None => 0.0,
        }),
    })
}
