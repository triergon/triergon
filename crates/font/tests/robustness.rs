//! Mutated fonts must produce errors, never panics or unbounded work. The real
//! corpus is mutated through the font worker by `cargo xtask fuzz --contract`.

mod support;

use support::{Rng, cff_tables, collection, exercise, font, mutate, truetype_tables};
use triergon_font::Limits;

#[test]
fn mutated_fonts_never_panic() {
    let seeds = [
        font(
            b"\0\x01\0\0",
            &truetype_tables(400, 0, "Test Sans", "Regular"),
        ),
        font(b"OTTO", &cff_tables()),
        collection(&[
            (
                b"\0\x01\0\0",
                truetype_tables(700, 0x20, "Test Sans", "Bold"),
            ),
            (b"OTTO", cff_tables()),
        ]),
    ];
    let limits = Limits {
        work: 1 << 16,
        ..Limits::default()
    };
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for seed in &seeds {
        exercise(seed, limits);
        for round in 0..3000 {
            let mut data = seed.clone();
            mutate(&mut data, &mut rng, round);
            if round % 3 == 0 {
                mutate(&mut data, &mut rng, round + 1);
            }
            exercise(&data, limits);
        }
    }
}
