//! Explicit pinned-receiver colours, separate from unknown source RGB.

/// Per-slot provenance. Automatic receiver colours never become source facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteOrigin {
    Supplied { selector: u8, raw_rgb: u32 },
    ReceiverGray { class: u8, classes: u8, level: u8 },
    ReceiverTransparent,
}

/// A constant-size native view retains every supplied RGB word and both raw
/// command words. No heap allocation, palette configuration parsing or bitmap
/// decoding occurs here. Unknown source colour remains `None`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Palette {
    source: Option<[u32; 16]>,
    palette_word: u16,
    alpha_word: u16,
    argb: [u32; 4],
    origins: [PaletteOrigin; 4],
}

impl Palette {
    pub fn source_rgb(&self) -> Option<&[u32; 16]> {
        self.source.as_ref()
    }
    pub fn palette_word(&self) -> u16 {
        self.palette_word
    }
    pub fn alpha_word(&self) -> u16 {
        self.alpha_word
    }
    pub fn argb(&self) -> &[u32; 4] {
        &self.argb
    }
    pub fn origins(&self) -> &[PaletteOrigin; 4] {
        &self.origins
    }
}

/// The receiver's default-white palette operation. The caller explicitly supplies a
/// parsed palette or its absence; this function does not guess which raw text
/// the receiver would accept as configuration. Its automatic greys are receiver
/// defaults, not a source colour-space or normative DVD colour declaration.
///
/// For `m` opaque selector classes, levels are nearest integers to
/// `255 * rank / (m - 1)`; the single-class level is255. White scaling uses256,
/// preserving the observed 254 rather than replacing it with full white.
pub fn resolve_palette(source: Option<&[u32; 16]>, palette_word: u16, alpha_word: u16) -> Palette {
    let selector: [u8; 4] = std::array::from_fn(|i| ((palette_word >> (i * 4)) & 15) as u8);
    let alpha: [u8; 4] = std::array::from_fn(|i| ((alpha_word >> (i * 4)) & 15) as u8);
    let mut argb = [0; 4];
    let mut origins = [PaletteOrigin::ReceiverTransparent; 4];
    if let Some(rgb) = source {
        for i in 0..4 {
            let raw_rgb = rgb[usize::from(selector[i])];
            argb[i] = (raw_rgb & 0x00ff_ffff) | ((u32::from(alpha[i]) * 17) << 24);
            origins[i] = PaletteOrigin::Supplied {
                selector: selector[i],
                raw_rgb,
            };
        }
    } else {
        let mut class = [u8::MAX; 16];
        let mut classes = 0_u8;
        for i in 0..4 {
            if alpha[i] != 0 && class[usize::from(selector[i])] == u8::MAX {
                class[usize::from(selector[i])] = classes;
                classes += 1;
            }
        }
        for i in 0..4 {
            if alpha[i] == 0 {
                continue;
            }
            let rank = class[usize::from(selector[i])];
            let level = if classes == 1 {
                255
            } else {
                let denominator = u16::from(classes - 1);
                ((255 * u16::from(rank) + denominator / 2) / denominator) as u8
            };
            let gray = 255 * u32::from(level) / 256;
            argb[i] = ((u32::from(alpha[i]) * 17) << 24) | (gray * 0x0001_0101);
            origins[i] = PaletteOrigin::ReceiverGray {
                class: rank,
                classes,
                level,
            };
        }
    }
    Palette {
        source: source.copied(),
        palette_word,
        alpha_word,
        argb,
        origins,
    }
}
