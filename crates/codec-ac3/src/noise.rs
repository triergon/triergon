//! Session noise for the A/52 path and the pinned decoder compatibility path.

const LEGACY_SEED: u32 = 0x2545_f491;
const SIGNED_UNIT: f32 = 1.0 / 2_147_483_648.0;

/// FFmpeg 9.0.2 seed-zero state.
/// The first eight slots are overwritten before they can be read.
const SEED_ZERO: [u32; 64] = [
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
    0x3b376b5e, 0xf56be763, 0xcba2a7fc, 0x9751b2c7, 0x5b1fe69a, 0x860b05af, 0x2b2650ca, 0x92a97fb0,
    0x7315e8eb, 0x8b6287a3, 0x2ddbd8b3, 0x1d098ca2, 0x6a65b478, 0xc9b22db4, 0x9637fbcf, 0xd4427934,
    0xb390e2b5, 0x92a84d32, 0x2110b086, 0x25a9182f, 0x239295f1, 0xc6f846f7, 0x5c5b3a51, 0xe6a7ba34,
    0xd6597325, 0xa537e3df, 0xcb80e534, 0xeaecf2dc, 0x9bbb6bd1, 0x2ea8bd11, 0x8c06ce60, 0x6859af1d,
    0xf159f64e, 0xc213bd06, 0x2a5d7785, 0xe621dc54, 0xb8cb9a8e, 0xe6a491f8, 0x85d7df25, 0x917d9aee,
    0xb156c513, 0x5bd929dd, 0xf6a9dab1, 0xc3ec5f2a, 0x5215230d, 0xe2b677d7, 0x701e36e7, 0x5405e2c7,
    0xddcb3ae2, 0x95b866ea, 0x1eb13c9a, 0x346646c2, 0x8ba48526, 0xb325e9bf, 0x29f07d49, 0xc986cf38,
];

struct LaggedNoise {
    words: [u32; 64],
    head: usize,
}

impl LaggedNoise {
    fn new() -> Self {
        Self {
            words: SEED_ZERO,
            head: 0,
        }
    }

    #[inline]
    fn draw(&mut self) -> u32 {
        let recent = self.words[(self.head + 40) % 64];
        let older = self.words[(self.head + 9) % 64];
        let word = recent.wrapping_add(older);
        self.words[self.head] = word;
        self.head = (self.head + 1) % 64;
        word
    }
}

pub(crate) struct Rng {
    legacy: u32,
    lagged: Option<LaggedNoise>,
}

impl Rng {
    pub(crate) fn new(compatible: bool) -> Self {
        Self {
            legacy: LEGACY_SEED,
            lagged: compatible.then(LaggedNoise::new),
        }
    }

    pub(crate) fn compatible(&self) -> bool {
        self.lagged.is_some()
    }

    /// The reference restarts its noise stream when it flushes decoder history.
    pub(crate) fn reset_compatible(&mut self) {
        if let Some(lagged) = &mut self.lagged {
            *lagged = LaggedNoise::new();
        }
    }

    #[inline]
    fn draw(&mut self) -> u32 {
        if let Some(lagged) = &mut self.lagged {
            return lagged.draw();
        }
        let mut x = self.legacy;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.legacy = x;
        x
    }

    #[inline]
    pub(crate) fn uniform(&mut self) -> f32 {
        (self.draw() as i32) as f32 * SIGNED_UNIT
    }

    /// BAP-zero noise in Q23, before the bin's exponent shift.
    #[inline]
    pub(crate) fn dither_q23(&mut self) -> i32 {
        let compatible = self.compatible();
        let word = self.draw();
        if compatible {
            let upper24 = word >> 8;
            ((upper24 * 181) >> 8) as i32 - 5_931_008
        } else {
            ((((word as i32) >> 8) as i64 * 23_170) >> 15) as i32
        }
    }

    #[inline]
    pub(crate) fn aht_coefficient(&mut self) -> f32 {
        if self.compatible() {
            let centered = (self.draw() & 0x7f_ffff) as i32 - 0x40_0000;
            centered as f32 * (1.0 / 8_388_608.0)
        } else {
            self.uniform() * 0.5
        }
    }

    #[inline]
    pub(crate) fn compatible_spx_noise(&mut self, amplitude: f32) -> f32 {
        (self.draw() as i32) as f32 * (-amplitude * SIGNED_UNIT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatible_stream_crosses_the_history_wrap_and_restarts() {
        let mut rng = Rng::new(true);
        let mut recorded = [0; 160];
        recorded.fill_with(|| rng.draw());
        assert_eq!(
            recorded[..6],
            [
                0xe6c5ddb1, 0x8db66502, 0xc1af2a4c, 0x4141c2ee, 0x3ed6a03d, 0x11cae2c2,
            ]
        );
        assert_eq!(
            recorded[60..68],
            [
                0x1436c57b, 0x1747b1a0, 0xda71cee2, 0xf5a6d745, 0x5601b481, 0xc1265974, 0x3698c795,
                0xc5d55081,
            ]
        );
        assert_eq!(
            recorded[152..160],
            [
                0xde6faae9, 0x1f64a66d, 0x2c862d31, 0x940523f9, 0x8f1d8b1c, 0x34b45909, 0xa02afc2b,
                0xc4554877,
            ]
        );
        rng.reset_compatible();
        assert_eq!(rng.draw(), recorded[0]);
    }

    #[test]
    fn different_noise_users_advance_one_session_stream() {
        let mut rng = Rng::new(true);
        assert_eq!(rng.dither_q23(), 4762085);
        assert_eq!(rng.aht_coefficient().to_bits(), 0xbd99afe0);
        assert_eq!(rng.compatible_spx_noise(1.0).to_bits(), 0x3ef94357);
        assert_eq!(rng.compatible_spx_noise(0.0).abs(), 0.0);
        assert_eq!(rng.dither_q23(), -3019329);
    }

    #[test]
    fn noncompatible_stream_and_reset_preserve_xorshift() {
        let mut rng = Rng::new(false);
        assert_eq!(rng.draw(), 0xe124b63a);
        rng.reset_compatible();
        assert_eq!(rng.draw(), 0x8b9a74ab);
        assert_eq!(rng.draw(), 0x64e1b3ac);
    }
}
