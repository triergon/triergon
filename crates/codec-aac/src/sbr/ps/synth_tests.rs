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
        let mut line = Line::default();
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
            let mut line = Line::default();
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

#[test]
fn every_band_owns_its_own_state_line() {
    // probes o04, o12: 71 and 91 lines; QMF bands above the split line up by QMF band - 20
    for (lay, n) in layouts().iter().zip([71, 91]) {
        let mut seen = [false; MAX_HYBRID];
        for band in &lay.bands {
            assert!(!seen[band.line], "line {} twice", band.line);
            seen[band.line] = true;
        }
        assert_eq!(lay.bands.len(), n);
        assert!(seen[..n].iter().all(|&s| s));
    }
    let line = |big: bool, k: usize| {
        layouts()[usize::from(big)]
            .bands
            .iter()
            .find(|b| b.low.is_none() && b.qmf == k)
            .unwrap()
            .line
    };
    assert_eq!((line(false, 3), line(true, 5)), (10, 32));
    assert_eq!(line(false, 55), line(true, 35));
    // 34 bands: QMF band 0's sub-bands 0..11 in order, QMF band 1's from line 12
    let lines: Vec<_> = layouts()[1].bands[..14].iter().map(|b| b.line).collect();
    assert_eq!(lines, [0, 11, 1, 10, 2, 9, 3, 4, 5, 6, 7, 8, 14, 15]);
    // 20 bands: QMF band 0's bands in order of frequency (o12)
    let lines: Vec<_> = layouts()[0].bands[..6].iter().map(|b| b.line).collect();
    assert_eq!(lines, [2, 1, 3, 0, 4, 5]);
}

#[test]
fn the_sections_keep_lattice_state() {
    // probe o05: s[t] = u[t] + a y[t], which is (1 - a^2) u[t] for the first input
    let band = layouts()[0].bands.iter().find(|b| b.qmf == 10).unwrap();
    let Kind::AllPass { decay, .. } = band.kind else {
        panic!("QMF band 10 runs the all-pass")
    };
    let mut line = Line::default();
    let mut x = [Cpx::default(); SLOTS];
    // after the lead delay the impulse enters the first section at slot 29 of 32
    x[27] = Cpx::new(1.0, 0.0);
    let mut y = [Cpx::default(); SLOTS];
    decorrelate(band.kind, &mut line, &x, &mut y);
    let a = decay[0];
    let s: Vec<f32> = line.sections[..3]
        .iter()
        .map(|v| v.re.hypot(v.im))
        .collect();
    assert!((s[0] - (1.0 - a * a)).abs() < 1e-6, "{s:?}");
    assert_eq!(&s[1..], &[0.0, 0.0]);
    assert!(line.sections_live && line.input_live);
}
