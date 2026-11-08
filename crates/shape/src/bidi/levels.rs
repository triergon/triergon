//! Rules I1–I2 and the paragraph's part of L1: the levels of every character.
//!
//! FriBidi's choices, reproduced here:
//! - The characters X9 removed get the level of the character before them,
//!   the paragraph level at the start.
//! - L1 resets segment and paragraph separators, and whitespace, isolate
//!   formatting characters, BN and the embedding characters before a separator
//!   or at the end of the paragraph, to the paragraph level.

use super::class::{BidiClass, Level};
use super::runs::Run;

/// I1 and I2.
fn implicit(run: &Run) -> u8 {
    if run.class.is_number() {
        (run.level + 2) & !1
    } else {
        run.level + ((run.level % 2) ^ run.class.rtl_bit())
    }
}

/// Writes the level of every character of the paragraph from its resolved runs.
pub(super) fn write(runs: &[Run], classes: &[BidiClass], base_level: u8, levels: &mut [Level]) {
    // Explicit processing stopped at the first paragraph separator.
    let explicit = classes
        .iter()
        .position(|&c| c == BidiClass::B)
        .unwrap_or(classes.len());
    let mut runs = runs.iter();
    let (mut level, mut left) = (base_level, 0);
    let mut previous = base_level;
    for (at, (class, out)) in classes.iter().zip(levels.iter_mut()).enumerate() {
        if at >= explicit || !class.is_removed() {
            if left == 0
                && let Some(run) = runs.next()
            {
                (level, left) = (implicit(run), run.len);
            }
            left = left.saturating_sub(1);
            previous = level;
        }
        *out = Level(previous);
    }

    // L1, reading backwards: within a stretch that ends at a separator or at
    // the end, until something else is found.
    let mut trailing = true;
    for (class, out) in classes.iter().zip(levels.iter_mut()).rev() {
        if class.is_separator() {
            trailing = true;
        } else if !(class.is_removed() || class.is_isolate() || *class == BidiClass::Ws) {
            trailing = false;
        }
        if trailing {
            *out = Level(base_level);
        }
    }
}
