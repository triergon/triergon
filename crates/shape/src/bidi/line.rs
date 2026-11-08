//! Rules L1 (for the end of a line) and L2, as `fribidi_reorder_line` with no
//! flags applies them.

use super::class::{BidiClass, Level};
use super::{Limits, charge};
use crate::Direction;
use triergon_core::{Error, Result};

/// Reorders one line of a paragraph within the default [`Limits`]:
/// `classes` and `levels` are the line's part of the paragraph's classes and
/// resolved levels, and `items` holds one value per character (character
/// indices, for a visual-to-logical map).
///
/// First the whitespace, BN and embedding characters at the end of the line are
/// set to the paragraph level of `direction` (FriBidi leaves isolate formatting
/// characters there as they are). Then `items` is reordered as L2 reverses
/// characters: from the highest level to the lowest odd one, every maximal
/// stretch at that level or above. `levels` keeps logical order. Combining marks
/// are not moved (L3 is not applied).
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
/// charged, at most one per level from the highest to the lowest odd one.
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
    let mut work = limits.budget(levels.len());
    charge(&mut work, levels.len())?;
    for (class, level) in classes.iter().zip(levels.iter_mut()).rev() {
        if !(class.is_removed() || *class == BidiClass::Ws) {
            break;
        }
        *level = Level::of(direction);
    }

    let (Some(&lowest), Some(&highest)) = (levels.iter().min(), levels.iter().max()) else {
        return Ok(());
    };
    for level in ((lowest.get() | 1)..=highest.get()).rev() {
        charge(&mut work, levels.len())?;
        let mut start = 0;
        while let Some(offset) = levels[start..].iter().position(|l| l.get() >= level) {
            let from = start + offset;
            let len = levels[from..]
                .iter()
                .position(|l| l.get() < level)
                .unwrap_or(levels.len() - from);
            items[from..from + len].reverse();
            start = from + len;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use BidiClass::*;

    fn reorder(
        classes: &[BidiClass],
        direction: Direction,
        levels: &[u8],
    ) -> (Vec<u8>, Vec<usize>) {
        let mut levels: Vec<Level> = levels.iter().map(|&l| Level(l)).collect();
        let mut map: Vec<usize> = (0..levels.len()).collect();
        reorder_line(classes, direction, &mut levels, &mut map).expect("reorder");
        (levels.iter().map(|l| l.0).collect(), map)
    }

    #[test]
    fn reverses_from_the_highest_level() {
        let (_, map) = reorder(&[L, R, R, L, L], Direction::LeftToRight, &[0, 1, 1, 2, 2]);
        assert_eq!(map, [0, 3, 4, 2, 1]);
        let (_, map) = reorder(&[L; 4], Direction::LeftToRight, &[2, 2, 2, 2]);
        assert_eq!(map, [0, 1, 2, 3]);
    }

    #[test]
    fn trailing_whitespace_takes_the_paragraph_level() {
        let (levels, map) = reorder(&[R, Ws, Bn, Rli], Direction::LeftToRight, &[1, 1, 1, 1]);
        assert_eq!(levels, [1, 1, 1, 1], "an isolate initiator stops the reset");
        assert_eq!(map, [3, 2, 1, 0]);
        let (levels, map) = reorder(&[R, Ws, Bn, Pdf], Direction::LeftToRight, &[1, 1, 1, 1]);
        assert_eq!(levels, [1, 0, 0, 0]);
        assert_eq!(map, [0, 1, 2, 3]);
    }

    #[test]
    fn deep_levels_need_work() {
        let classes = [L; 4];
        let mut levels = [Level(125), Level(0), Level(125), Level(0)];
        let mut map = [0, 1, 2, 3];
        let tight = Limits {
            work_per_char: 1,
            minimum_work: 0,
        };
        assert_eq!(
            reorder_line_with_limits(
                tight,
                &classes,
                Direction::LeftToRight,
                &mut levels,
                &mut map
            ),
            Err(Error::Policy("bidi work"))
        );
        assert_eq!(
            reorder_line(&classes, Direction::LeftToRight, &mut levels, &mut map),
            Ok(())
        );
    }

    #[test]
    fn lengths_must_agree() {
        let mut levels = [Level(0)];
        let mut map = [0_usize; 2];
        assert!(reorder_line(&[L], Direction::LeftToRight, &mut levels, &mut map).is_err());
    }
}
