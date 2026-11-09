//! Studio-range Y'CbCr to full-range RGB in 10-bit fixed point, as FFmpeg's
//! subtitle decoders and demuxers convert palette entries: each coefficient
//! rounded to 1/1024, each channel rounded once and clamped.

/// How a channel's 1/1024 sum is rounded to an integer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelRounding {
    /// To nearest, halves up.
    Nearest,
    /// Half a step below nearest.
    HalfDown,
}

/// A matrix for studio-range input (Y' from 16 to 235, Cb and Cr from 16 to
/// 240): each component's contribution to a channel in 1/1024 units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct YcbcrMatrix {
    luma: i32,
    /// `[Cr->R, Cr->G, Cb->G, Cb->B]`.
    chroma: [i32; 4],
}

impl YcbcrMatrix {
    /// ITU-R BT.601.
    pub const BT601: Self = Self::studio([140_200, 71_414, 34_414, 177_200], 100_000);
    /// ITU-R BT.709.
    pub const BT709: Self = Self::studio([15_747, 4_682, 1_873, 18_556], 10_000);

    /// Chroma coefficients in units of `1/scale`, widened from the 219 luma
    /// and 224 chroma codes of studio range to the 255 of full range, each
    /// rounded to 1/1024.
    const fn studio(chroma: [i32; 4], scale: i32) -> Self {
        const fn fixed(n: i32, d: i32) -> i32 {
            ((n as i64 * 1024 + d as i64 / 2) / d as i64) as i32
        }
        let d = scale * 224;
        Self {
            luma: fixed(255, 219),
            chroma: [
                fixed(chroma[0] * 255, d),
                fixed(chroma[1] * 255, d),
                fixed(chroma[2] * 255, d),
                fixed(chroma[3] * 255, d),
            ],
        }
    }

    /// `0xRRGGBB` for one Y'/Cr/Cb triple, in the order subtitle palettes store
    /// them.
    #[inline]
    pub fn rgb(self, y: u8, cr: u8, cb: u8, rounding: ChannelRounding) -> u32 {
        self.argb(y, cr, cb, 0, rounding)
    }

    /// `0xAARRGGBB`: [`rgb`](Self::rgb) with `alpha` in the top byte.
    #[inline]
    pub fn argb(self, y: u8, cr: u8, cb: u8, alpha: u8, rounding: ChannelRounding) -> u32 {
        let bias = match rounding {
            ChannelRounding::Nearest => 512,
            ChannelRounding::HalfDown => -512,
        };
        let luma = self.luma * (i32::from(y) - 16) + bias;
        let (cr, cb) = (i32::from(cr) - 128, i32::from(cb) - 128);
        let [cr_red, cr_green, cb_green, cb_blue] = self.chroma;
        let channel = |sum: i32| (sum >> 10).clamp(0, 255) as u32;
        u32::from(alpha) << 24
            | channel(luma + cr_red * cr) << 16
            | channel(luma - cr_green * cr - cb_green * cb) << 8
            | channel(luma + cb_blue * cb)
    }
}

#[cfg(test)]
mod tests {
    use super::{ChannelRounding, YcbcrMatrix};

    /// FFmpeg's DVD palettes: rounded to nearest from `ifo_palette`, half a
    /// step down from the MP4 demuxer's palette text.
    #[test]
    fn bt601_matches_the_reference_palettes_in_both_roundings() {
        let rgb = |y, cr, cb, rounding| YcbcrMatrix::BT601.rgb(y, cr, cb, rounding);
        for (y, cr, cb, half_down, nearest) in [
            (200, 114, 128, 0xbfe1d5, 0xc0e2d6),
            (144, 125, 5, 0x8fc700, 0x90c800),
            (64, 128, 6, 0x376700, 0x386800),
            (190, 128, 128, 0xcacaca, 0xcbcbcb),
            (128, 128, 166, 0x8172ce, 0x8273cf),
            (113, 141, 93, 0x857329, 0x86742a),
            (218, 247, 46, 0xffaa45, 0xffab46),
            (41, 82, 172, 0x003075, 0x003176),
            (17, 128, 128, 0x000000, 0x010101),
        ] {
            assert_eq!(rgb(y, cr, cb, ChannelRounding::HalfDown), half_down);
            assert_eq!(rgb(y, cr, cb, ChannelRounding::Nearest), nearest);
        }
        for (y, grey) in [(0, 0), (16, 0), (235, 0xffffff), (255, 0xffffff)] {
            assert_eq!(rgb(y, 128, 128, ChannelRounding::Nearest), grey);
        }
    }
}
