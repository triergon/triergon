//! The polyphase synthesis filterbank all three layers share (ISO/IEC 11172-3
//! 2.4.3.3.2 and Figure A.2).
//!
//! The specification's matrixing `V[i] = sum_k N[i][k] S[k]` with
//! `N[i][k] = cos((16 + i)(2k + 1)π / 64)` is a 32-point DCT-II `C` of the subband
//! samples, rearranged: `V[i] = C[16 + i]` for `i < 16`, `V[16] = 0`,
//! `V[i] = -C[48 - i]` for `16 < i < 48` and `V[i] = -C[i - 48]` above. The DCT runs
//! as Lee's recursive factorisation. Windowing keeps the last 16 vectors `V`; output
//! sample `j` is `sum_a D[32a + j] V_a[32 (a mod 2) + j]` over their ages `a`.

use core::f32::consts::FRAC_1_SQRT_2;
use std::sync::LazyLock;

/// The synthesis window `D[i]` of Table B.3 for `i <= 256`, in units of `2^-16`; the
/// rest follows from `D[512 - i] = -D[i]`, except `D[512 - i] = D[i]` at multiples of 64.
#[rustfmt::skip]
const WINDOW_HALF: [i32; 257] = [
    0, -1, -1, -1, -1, -1, -1, -2, -2, -2, -2, -3, -3, -4, -4, -5,
    -5, -6, -7, -7, -8, -9, -10, -11, -13, -14, -16, -17, -19, -21, -24, -26,
    -29, -31, -35, -38, -41, -45, -49, -53, -58, -63, -68, -73, -79, -85, -91, -97,
    -104, -111, -117, -125, -132, -139, -147, -154, -161, -169, -176, -183, -190, -196, -202, -208,
    213, 218, 222, 225, 227, 228, 228, 227, 224, 221, 215, 208, 200, 189, 177, 163,
    146, 127, 106, 83, 57, 29, -2, -36, -72, -111, -153, -197, -244, -294, -347, -401,
    -459, -519, -581, -645, -711, -779, -848, -919, -991, -1064, -1137, -1210, -1283, -1356, -1428, -1498,
    -1567, -1634, -1698, -1759, -1817, -1870, -1919, -1962, -2001, -2032, -2057, -2075, -2085, -2087, -2080, -2063,
    2037, 2000, 1952, 1893, 1822, 1739, 1644, 1535, 1414, 1280, 1131, 970, 794, 605, 402, 185,
    -45, -288, -545, -814, -1095, -1388, -1692, -2006, -2330, -2663, -3004, -3351, -3705, -4063, -4425, -4788,
    -5153, -5517, -5879, -6237, -6589, -6935, -7271, -7597, -7910, -8209, -8491, -8755, -8998, -9219, -9416, -9585,
    -9727, -9838, -9916, -9959, -9966, -9935, -9863, -9750, -9592, -9389, -9139, -8840, -8492, -8092, -7640, -7134,
    6574, 5959, 5288, 4561, 3776, 2935, 2037, 1082, 70, -998, -2122, -3300, -4533, -5818, -7154, -8540,
    -9975, -11455, -12980, -14548, -16155, -17799, -19478, -21189, -22929, -24694, -26482, -28289, -30112, -31947, -33791, -35640,
    -37489, -39336, -41176, -43006, -44821, -46617, -48390, -50137, -51853, -53534, -55178, -56778, -58333, -59838, -61289, -62684,
    -64019, -65290, -66494, -67629, -68692, -69679, -70590, -71420, -72169, -72835, -73415, -73908, -74313, -74630, -74856, -74992,
    75038,
];

/// The full window `D[0..512]`.
const WINDOW: [f32; 512] = {
    let mut window = [0.0; 512];
    let mut i = 0;
    while i < 512 {
        let value = if i <= 256 {
            WINDOW_HALF[i]
        } else if i % 64 == 0 {
            WINDOW_HALF[512 - i]
        } else {
            -WINDOW_HALF[512 - i]
        };
        window[i] = value as f32 / 65536.0;
        i += 1;
    }
    window
};

/// `1 / (2 cos(π (2k + 1) / 2N))`: the odd-half factors of Lee's DCT for each size.
struct LeeFactors {
    n32: [f32; 16],
    n16: [f32; 8],
    n8: [f32; 4],
    n4: [f32; 2],
}

static LEE: LazyLock<LeeFactors> = LazyLock::new(|| {
    fn factors<const H: usize>() -> [f32; H] {
        core::array::from_fn(|k| {
            let angle = core::f64::consts::PI * (2 * k + 1) as f64 / (4 * H) as f64;
            (0.5 / angle.cos()) as f32
        })
    }
    LeeFactors {
        n32: factors(),
        n16: factors(),
        n8: factors(),
        n4: factors(),
    }
});

/// A DCT-II `X[m] = sum_k x[k] cos(π m (2k + 1) / 2N)` of size `$n`, from two of size
/// `$n / 2`: the even outputs transform `x[k] + x[N-1-k]`, the odd ones the differences
/// scaled by `$factors`, summed pairwise.
macro_rules! lee_dct {
    ($name:ident, $half:ident, $n:literal, $factors:ident) => {
        #[inline(always)]
        fn $name(x: &[f32; $n], lee: &LeeFactors) -> [f32; $n] {
            const H: usize = $n / 2;
            let sums: [f32; H] = core::array::from_fn(|k| x[k] + x[$n - 1 - k]);
            let differences: [f32; H] =
                core::array::from_fn(|k| (x[k] - x[$n - 1 - k]) * lee.$factors[k]);
            let even = $half(&sums, lee);
            let odd = $half(&differences, lee);
            core::array::from_fn(|m| {
                let h = m / 2;
                if m % 2 == 0 {
                    even[h]
                } else if h + 1 < H {
                    odd[h] + odd[h + 1]
                } else {
                    odd[h]
                }
            })
        }
    };
}

lee_dct!(dct32, dct16, 32, n32);
lee_dct!(dct16, dct8, 16, n16);
lee_dct!(dct8, dct4, 8, n8);
lee_dct!(dct4, dct2, 4, n4);

#[inline(always)]
fn dct2(x: &[f32; 2], _: &LeeFactors) -> [f32; 2] {
    [x[0] + x[1], (x[0] - x[1]) * FRAC_1_SQRT_2]
}

/// The filterbank state of one channel: the last 16 vectors `V`.
#[derive(Clone, Debug)]
pub(crate) struct Synthesis {
    history: [[f32; 64]; 16],
    /// Index of the newest vector in `history`.
    newest: usize,
}

impl Default for Synthesis {
    fn default() -> Self {
        Synthesis {
            history: [[0.0; 64]; 16],
            newest: 0,
        }
    }
}

impl Synthesis {
    pub(crate) fn reset(&mut self) {
        *self = Synthesis::default();
    }

    /// Filter one time slot: 32 subband samples in, 32 output samples out.
    pub(crate) fn run(&mut self, subbands: &[f32; 32], pcm: &mut [f32; 32]) {
        let c = dct32(subbands, &LEE);
        self.newest = (self.newest + 15) % 16;
        let v = &mut self.history[self.newest];
        v[..16].copy_from_slice(&c[16..]);
        v[16] = 0.0;
        for i in 17..48 {
            v[i] = -c[48 - i];
        }
        for i in 48..64 {
            v[i] = -c[i - 48];
        }
        *pcm = [0.0; 32];
        let (window, []) = WINDOW.as_chunks::<32>() else {
            unreachable!("the window is 16 rows of 32");
        };
        for (age, d) in window.iter().enumerate() {
            let vector = &self.history[(self.newest + age) % 16];
            let half = if age % 2 == 0 {
                &vector[..32]
            } else {
                &vector[32..]
            };
            for ((out, &d), &v) in pcm.iter_mut().zip(d).zip(half) {
                *out += d * v;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The filterbank exactly as Figure A.2 draws it: shift `V`, matrix, build `U`,
    /// window, sum.
    struct Reference {
        v: Vec<f64>,
    }

    impl Reference {
        fn run(&mut self, s: &[f32; 32]) -> [f64; 32] {
            self.v.rotate_right(64);
            for i in 0..64 {
                self.v[i] = (0..32)
                    .map(|k| {
                        let n = ((16 + i) * (2 * k + 1)) as f64 * core::f64::consts::PI / 64.0;
                        n.cos() * f64::from(s[k])
                    })
                    .sum();
            }
            let mut u = [0.0; 512];
            for i in 0..8 {
                for j in 0..32 {
                    u[i * 64 + j] = self.v[i * 128 + j];
                    u[i * 64 + 32 + j] = self.v[i * 128 + 96 + j];
                }
            }
            core::array::from_fn(|j| {
                (0..16)
                    .map(|i| u[j + 32 * i] * f64::from(WINDOW[j + 32 * i]))
                    .sum()
            })
        }
    }

    #[test]
    fn matches_the_direct_formula() {
        let mut fast = Synthesis::default();
        let mut reference = Reference { v: vec![0.0; 1024] };
        let mut seed = 0x1234_5678u32;
        let mut worst = 0.0f64;
        for _ in 0..40 {
            let s: [f32; 32] = core::array::from_fn(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / (1 << 24) as f32 - 0.5
            });
            let mut pcm = [0.0; 32];
            fast.run(&s, &mut pcm);
            let expected = reference.run(&s);
            for (got, want) in pcm.iter().zip(expected) {
                worst = worst.max((f64::from(*got) - want).abs());
            }
        }
        assert!(worst < 2e-6, "worst difference {worst}");
    }

    #[test]
    fn dct32_matches_its_definition() {
        let x: [f32; 32] = core::array::from_fn(|k| ((k * 13 % 17) as f32 - 8.0) / 8.0);
        let fast = dct32(&x, &LEE);
        for (m, &got) in fast.iter().enumerate() {
            let want: f64 = (0..32)
                .map(|k| {
                    f64::from(x[k])
                        * (core::f64::consts::PI * (m * (2 * k + 1)) as f64 / 64.0).cos()
                })
                .sum();
            assert!((f64::from(got) - want).abs() < 1e-5, "{m}");
        }
    }
}
