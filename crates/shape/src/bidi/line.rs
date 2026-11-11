//! Rules L1 (for the end of a line) and L2.

use super::class::{BidiClass, Level};
use super::{Limits, charge};
use crate::Direction;
use triergon_core::{Error, Result};

/// Reorders one line of a paragraph within the default [`Limits`]:
/// `classes` and `levels` are the line's part of the paragraph's classes and
/// resolved levels, and `items` holds one value per character (character
/// indices, for a visual-to-logical map).
///
/// First the whitespace, BN and embedding, override and PDF characters at the
/// end of the line take the paragraph level of `direction`. Unlike UAX #9's
/// L1, and as FriBidi applies it, an isolate formatting character ends that
/// stretch and keeps its level, like the characters before it. Then `items` is
/// reordered as L2
/// reverses characters: from the highest level to the lowest odd one, every
/// maximal stretch at that level or above. `levels` keeps logical order.
/// Combining marks are not moved (L3 is not applied).
///
/// # Errors
///
/// `Error::Invalid` if the three slices differ in length; `Error::Policy` when
/// the work exceeds the limits.
pub fn reorder_line<T>(
    classes: &[BidiClass],
    direction: Direction,
    levels: &mut [Level],
    items: &mut [T],
) -> Result<()> {
    reorder_line_with_limits(Limits::default(), classes, direction, levels, items)
}

/// [`reorder_line`] within `limits`: each level's pass over the line is
/// charged, one per level from the highest to the lowest odd one.
///
/// # Errors
///
/// As [`reorder_line`].
pub fn reorder_line_with_limits<T>(
    limits: Limits,
    classes: &[BidiClass],
    direction: Direction,
    levels: &mut [Level],
    items: &mut [T],
) -> Result<()> {
    if classes.len() != levels.len() || items.len() != levels.len() {
        return Err(Error::Invalid("bidi line lengths differ"));
    }
    let paragraph = Level::of(direction);
    for (&class, level) in classes.iter().zip(levels.iter_mut()).rev() {
        if class != BidiClass::Ws && !class.is_removed_by_x9() {
            break;
        }
        *level = paragraph;
    }
    let Some(highest) = levels.iter().map(|l| l.0).max() else {
        return Ok(());
    };
    let lowest_odd = levels
        .iter()
        .map(|l| l.0)
        .filter(|l| l & 1 == 1)
        .min()
        .unwrap_or(highest + 1);
    let mut work = limits.work(levels.len());
    for level in (lowest_odd..=highest).rev() {
        charge(&mut work, levels.len())?;
        let mut at = 0;
        while at < levels.len() {
            if levels[at].0 < level {
                at += 1;
                continue;
            }
            let end = levels[at..]
                .iter()
                .position(|l| l.0 < level)
                .map_or(levels.len(), |n| at + n);
            items[at..end].reverse();
            at = end;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverses_from_the_highest_level() {
        let classes = [BidiClass::L, BidiClass::R, BidiClass::En, BidiClass::Ws];
        let mut levels = [0, 1, 2, 1].map(Level);
        let mut items = [0, 1, 2, 3];
        reorder_line(&classes, Direction::LeftToRight, &mut levels, &mut items).unwrap();
        assert_eq!(levels.map(|l| l.0), [0, 1, 2, 0]);
        assert_eq!(items, [0, 2, 1, 3]);
    }

    #[test]
    fn limits_bound_the_passes() {
        let classes = [BidiClass::R; 4];
        let mut levels = [2, 3, 4, 5].map(Level);
        let mut items = [0; 4];
        let tight = Limits {
            work_per_char: 1,
            minimum_work: 0,
        };
        let result = reorder_line_with_limits(
            tight,
            &classes,
            Direction::LeftToRight,
            &mut levels,
            &mut items,
        );
        assert_eq!(result, Err(Error::Policy("bidi work")));
    }
}
