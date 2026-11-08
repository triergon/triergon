//! Mutated fonts must produce errors, never panics or unbounded work. `cargo xtask
//! fonts fuzz` mutates the real corpus through the font worker.

mod support;

use support::{cff_tables, collection, exercise, font, truetype_tables};
use triergon_font::Limits;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// One of four edits: bit flips, a truncation, an overwritten burst, a removed range.
fn mutate(data: &mut Vec<u8>, rng: &mut Rng, round: usize) {
    match round % 4 {
        0 => {
            for _ in 0..1 + rng.below(8) {
                let at = rng.below(data.len());
                data[at] ^= 1 << rng.below(8);
            }
        }
        1 => {
            let len = rng.below(data.len());
            data.truncate(len);
        }
        2 => {
            let at = rng.below(data.len());
            for k in 0..rng.below(16) {
                if let Some(byte) = data.get_mut(at + k) {
                    *byte = rng.next() as u8;
                }
            }
        }
        _ => {
            let a = rng.below(data.len());
            let b = (a + rng.below(64)).min(data.len());
            data.drain(a..b);
        }
    }
}

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
