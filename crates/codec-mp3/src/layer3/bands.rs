//! Scalefactor bands: ISO/IEC 11172-3 Table B.8, ISO/IEC 13818-3 Table B.2 and the
//! MPEG-2.5 extension to 11.025, 12 and 8 kHz.

/// Scalefactor band boundaries of one sample rate, in spectral lines.
#[derive(Debug)]
pub(crate) struct Bands {
    /// Start of long-block band `b` (22 bands and the end, 576).
    pub(crate) long: [u16; 23],
    /// Start of short-block band `b` within one window (13 bands and the end, 192).
    pub(crate) short: [u16; 14],
}

impl Bands {
    /// The bands of `sample_rate`; `None` for a rate MPEG audio does not define.
    pub(crate) fn for_rate(sample_rate: u32) -> Option<&'static Bands> {
        Some(match sample_rate {
            44_100 => &B8_44100,
            48_000 => &B8_48000,
            32_000 => &B8_32000,
            22_050 => &B2_22050,
            24_000 => &B2_24000,
            16_000 | 11_025 | 12_000 => &B2_16000,
            8_000 => &BANDS_8000,
            _ => return None,
        })
    }

    /// Lines of long band `b`.
    pub(crate) fn long_band(&self, b: usize) -> core::ops::Range<usize> {
        usize::from(self.long[b])..usize::from(self.long[b + 1])
    }

    /// Lines of short band `b` within one window.
    pub(crate) fn short_band(&self, b: usize) -> core::ops::Range<usize> {
        usize::from(self.short[b])..usize::from(self.short[b + 1])
    }
}

/// Table B.8, 44.1 kHz.
#[rustfmt::skip]
const B8_44100: Bands = Bands {
    long: [0, 4, 8, 12, 16, 20, 24, 30, 36, 44, 52, 62, 74, 90, 110, 134, 162, 196, 238, 288, 342, 418, 576],
    short: [0, 4, 8, 12, 16, 22, 30, 40, 52, 66, 84, 106, 136, 192],
};

/// Table B.8, 48 kHz.
#[rustfmt::skip]
const B8_48000: Bands = Bands {
    long: [0, 4, 8, 12, 16, 20, 24, 30, 36, 42, 50, 60, 72, 88, 106, 128, 156, 190, 230, 276, 330, 384, 576],
    short: [0, 4, 8, 12, 16, 22, 28, 38, 50, 64, 80, 100, 126, 192],
};

/// Table B.8, 32 kHz.
#[rustfmt::skip]
const B8_32000: Bands = Bands {
    long: [0, 4, 8, 12, 16, 20, 24, 30, 36, 44, 54, 66, 82, 102, 126, 156, 194, 240, 296, 364, 448, 550, 576],
    short: [0, 4, 8, 12, 16, 22, 30, 42, 58, 78, 104, 138, 180, 192],
};

/// ISO/IEC 13818-3 Table B.2, 22.05 kHz.
#[rustfmt::skip]
const B2_22050: Bands = Bands {
    long: [0, 6, 12, 18, 24, 30, 36, 44, 54, 66, 80, 96, 116, 140, 168, 200, 238, 284, 336, 396, 464, 522, 576],
    short: [0, 4, 8, 12, 18, 24, 32, 42, 56, 74, 100, 132, 174, 192],
};

/// ISO/IEC 13818-3 Table B.2, 24 kHz.
#[rustfmt::skip]
const B2_24000: Bands = Bands {
    long: [0, 6, 12, 18, 24, 30, 36, 44, 54, 66, 80, 96, 114, 136, 162, 194, 232, 278, 332, 394, 464, 540, 576],
    short: [0, 4, 8, 12, 18, 26, 36, 48, 62, 80, 104, 136, 180, 192],
};

/// ISO/IEC 13818-3 Table B.2, 16 kHz; MPEG-2.5 uses it for 11.025 and 12 kHz too.
#[rustfmt::skip]
const B2_16000: Bands = Bands {
    long: [0, 6, 12, 18, 24, 30, 36, 44, 54, 66, 80, 96, 116, 140, 168, 200, 238, 284, 336, 396, 464, 522, 576],
    short: [0, 4, 8, 12, 18, 26, 36, 48, 62, 80, 104, 134, 174, 192],
};

/// MPEG-2.5, 8 kHz.
#[rustfmt::skip]
const BANDS_8000: Bands = Bands {
    long: [0, 12, 24, 36, 48, 60, 72, 88, 108, 132, 160, 192, 232, 280, 336, 400, 476, 566, 568, 570, 572, 574, 576],
    short: [0, 8, 16, 24, 36, 52, 72, 96, 124, 160, 162, 164, 166, 192],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_table_is_increasing_and_ends_at_the_granule() {
        for rate in [
            44_100, 48_000, 32_000, 22_050, 24_000, 16_000, 11_025, 12_000, 8_000,
        ] {
            let bands = Bands::for_rate(rate).unwrap();
            assert!(bands.long.windows(2).all(|w| w[0] < w[1]), "{rate}");
            assert!(bands.short.windows(2).all(|w| w[0] < w[1]), "{rate}");
            assert_eq!((bands.long[22], bands.short[13]), (576, 192));
        }
    }
}
