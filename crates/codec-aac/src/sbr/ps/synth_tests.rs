//! Properties of the hybrid filters and the decorrelator the probes established.

use super::*;

#[test]
fn hybrid_bands_of_a_qmf_band_add_up_to_a_unit_impulse() {
    // neutral PS output equals its input (probes e12, p02)
    for lay in layouts() {
        for k in 0..lay.split {
            let mut sum = [(0f32, 0f32); 13];
            for band in lay.bands.iter().filter(|b| b.qmf == k) {
                for (s, c) in sum.iter_mut().zip(&lay.taps[band.low.unwrap()]) {
                    s.0 += c.re;
                    s.1 += c.im;
                }
            }
            for (d, s) in sum.iter().enumerate() {
                let want = if d == 6 { 1.0 } else { 0.0 };
                assert!(
                    (s.0 - want).abs() < 1e-6 && s.1.abs() < 1e-6,
                    "{k} {d} {s:?}"
                );
            }
        }
    }
}

#[test]
fn upper_bands_are_plain_delays() {
    // probes e16, d10, d11: 14 slots from QMF band 23, one slot from band 35
    let mut x = [Cpx::default(); SLOTS];
    x[3] = Cpx::new(1.0, -2.0);
    for (n, start) in [(t::LONG_DELAY, t::LONG_START), (1, t::SHORT_START)] {
        let band = layouts()[0].bands.iter().find(|b| b.qmf == start).unwrap();
        let mut line = [Cpx::default(); 14];
        let mut y = [Cpx::default(); SLOTS];
        decorrelate(band.kind, &mut line, &x, &mut y);
        assert_eq!(y[3 + n], x[3]);
        assert_eq!(y.iter().filter(|v| **v != Cpx::default()).count(), 1);
    }
}

#[test]
fn all_pass_bands_keep_the_energy_of_an_impulse() {
    for lay in layouts() {
        for band in lay
            .bands
            .iter()
            .filter(|b| matches!(b.kind, Kind::AllPass { .. }))
        {
            let mut line = [Cpx::default(); 14];
            let mut x = [Cpx::default(); SLOTS];
            x[0] = Cpx::new(1.0, 0.0);
            let mut y = [Cpx::default(); SLOTS];
            let mut energy = 0f64;
            for _ in 0..40 {
                decorrelate(band.kind, &mut line, &x, &mut y);
                energy += y
                    .iter()
                    .map(|v| f64::from(v.re * v.re + v.im * v.im))
                    .sum::<f64>();
                x[0] = Cpx::default();
            }
            assert!((energy - 1.0).abs() < 1e-4, "{} {energy}", band.qmf);
        }
    }
}
