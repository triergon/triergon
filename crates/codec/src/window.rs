//! Transform windows shared by the audio decoders.

/// Modified Bessel function of the first kind, order 0, by its power series.
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..200 {
        term *= q / (f64::from(k) * f64::from(k));
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// Fill `out` with the rising half of the Kaiser-Bessel-derived window of length
/// `2 * out.len()` and parameter `alpha` (A/52 Table 7.33 with alpha 5; ISO/IEC
/// 14496-3 4.6.11.3.2 with alpha 4 and 6).
pub fn kbd_window(alpha: f64, out: &mut [f32]) {
    let half = out.len();
    let kernel: Vec<f64> = (0..=half)
        .map(|j| {
            let r = 2.0 * j as f64 / half as f64 - 1.0;
            bessel_i0(core::f64::consts::PI * alpha * (1.0 - r * r).max(0.0).sqrt())
        })
        .collect();
    let total: f64 = kernel.iter().sum();
    let mut acc = 0.0;
    for (w, k) in out.iter_mut().zip(&kernel) {
        acc += k;
        *w = (acc / total).sqrt() as f32;
    }
}

/// Fill `out` with the rising slope of the power-complementary window both Vorbis I
/// (section 4.3.1) and CELT (RFC 6716 section 4.3.7) overlap by:
/// `sin(π/2 · sin²((i + 1/2) / m · π/2))` for `m = out.len()`, computed in double
/// precision. The falling slope is the same read backwards.
pub fn vorbis_window(out: &mut [f32]) {
    let m = out.len() as f64;
    for (i, w) in out.iter_mut().enumerate() {
        let s = ((i as f64 + 0.5) / m * core::f64::consts::FRAC_PI_2).sin();
        *w = (core::f64::consts::FRAC_PI_2 * s * s).sin() as f32;
    }
}

#[cfg(test)]
mod tests {
    use super::kbd_window;

    /// The windows the AAC and AC-3 decoders each derived before they shared this one,
    /// as an FNV-1a digest of their `f32` bits.
    #[test]
    fn windows_keep_the_bits_both_decoders_used() {
        for (half, alpha, digest) in [
            (256, 5.0, 0x8c61_d5ab_3247_54c8_u64),
            (1024, 4.0, 0x4c1b_5dd8_a7b1_0737),
            (128, 6.0, 0x0230_ab0f_52dd_532c),
        ] {
            let mut w = vec![0.0; half];
            kbd_window(alpha, &mut w);
            let got = w.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, x| {
                (h ^ u64::from(x.to_bits())).wrapping_mul(0x0100_0000_01b3)
            });
            assert_eq!(got, digest, "{half} {alpha}");
        }
    }
}
