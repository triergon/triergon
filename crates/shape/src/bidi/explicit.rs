//! Rules P2–P3 and X1–X9: the paragraph level, explicit embedding levels and
//! the runs the later rules resolve.
//!
//! FriBidi's choices, reproduced here:
//! - The first-strong searches (P2 for the paragraph, X5c for each FSI) pair
//!   isolate initiators and PDIs by nesting alone and look past a paragraph
//!   separator.
//! - The directional status stack stops at level 125. Once an embedding or
//!   override overflows, no further embedding is pushed until as many PDFs have
//!   passed; a PDF stops at an isolate entry on top of the stack but not at an
//!   overflowed isolate; a PDI clears the embedding overflow count.
//! - An isolate initiator that fits the level limit raises the level even when
//!   the stack refuses to push it (after an embedding overflow), and its PDI
//!   then pops whatever embeddings are on the stack. An overflowed isolate's PDI
//!   leaves the isolate depth raised.
//! - A PDI takes no directional override; an unmatched PDI becomes ON.
//! - Processing stops at the first paragraph separator: it and everything after
//!   it keep level 0, isolate depth 0 and their classes, and the formatting
//!   characters after it are kept.

use super::class::{BidiClass, Bracket};
use super::runs::{MAX_ISOLATE_DEPTH, Run};
use crate::Direction;
use triergon_core::{BudgetVec, Error, Result};

/// The deepest explicit embedding level.
const MAX_LEVEL: u8 = 125;

/// Rules P2 and P3: the direction of the first letter outside isolates.
pub(super) fn first_strong(classes: &[BidiClass]) -> Option<Direction> {
    let mut isolates = 0_usize;
    for &class in classes {
        match class {
            BidiClass::Pdi => isolates = isolates.saturating_sub(1),
            BidiClass::Lri | BidiClass::Rli | BidiClass::Fsi => isolates += 1,
            BidiClass::L if isolates == 0 => return Some(Direction::LeftToRight),
            BidiClass::R | BidiClass::Al if isolates == 0 => return Some(Direction::RightToLeft),
            _ => {}
        }
    }
    None
}

/// Rule X5c for every FSI of the paragraph, in order: whether the first letter
/// of its content, outside nested isolates, is right-to-left.
pub(super) fn first_strong_isolates(
    classes: &[BidiClass],
    rtl: &mut BudgetVec<bool>,
    open: &mut BudgetVec<Option<u32>>,
) -> Result<()> {
    rtl.clear();
    open.clear();
    for &class in classes {
        match class {
            BidiClass::Pdi => {
                open.pop();
            }
            BidiClass::Lri | BidiClass::Rli | BidiClass::Fsi => {
                let pending = if class == BidiClass::Fsi {
                    let index = u32::try_from(rtl.len()).map_err(|_| Error::Overflow)?;
                    rtl.try_push(false).map_err(|e| e.error)?;
                    Some(index)
                } else {
                    None
                };
                open.try_push(pending).map_err(|e| e.error)?;
            }
            _ if class.is_letter() => {
                if let Some(top) = open.as_mut_slice().last_mut()
                    && let Some(fsi) = top.take()
                {
                    rtl.as_mut_slice()[fsi as usize] = class != BidiClass::L;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// One entry of the directional status stack.
#[derive(Clone, Copy, Debug, Default)]
struct Status {
    level: u8,
    /// L or R while an override is in effect.
    overriding: Option<BidiClass>,
    isolate_depth: u8,
    /// Whether an isolate initiator pushed the entry.
    isolate: bool,
}

/// Rules X1–X8: the embedding state while the paragraph is read in order.
struct Embeddings {
    current: Status,
    /// The pushed entries; levels only grow towards the top, so 126 suffice.
    stack: [Status; MAX_LEVEL as usize + 1],
    depth: usize,
    /// Embeddings and overrides refused since the stack last accepted one.
    overflow_embeddings: u32,
    overflow_isolates: u32,
    /// Isolate initiators that fit the level limit and whose PDI is pending.
    valid_isolates: u32,
}

impl Embeddings {
    fn new(base_level: u8) -> Self {
        Self {
            current: Status {
                level: base_level,
                ..Status::default()
            },
            stack: [Status::default(); MAX_LEVEL as usize + 1],
            depth: 0,
            overflow_embeddings: 0,
            overflow_isolates: 0,
            valid_isolates: 0,
        }
    }

    fn push(&mut self, level: u8, overriding: Option<BidiClass>, isolate: bool) {
        if self.overflow_embeddings == 0 && self.overflow_isolates == 0 && level <= MAX_LEVEL {
            // `depth` stays below the stack size: each entry's level is below the
            // next, and the last below `MAX_LEVEL`.
            if let Some(slot) = self.stack.get_mut(self.depth) {
                *slot = Status {
                    isolate,
                    ..self.current
                };
                self.depth += 1;
            }
            self.current.level = level;
            self.current.overriding = overriding;
        } else if self.overflow_isolates == 0 {
            self.overflow_embeddings += 1;
        }
    }

    fn pop(&mut self) {
        if self.depth == 0 {
            return;
        }
        if self.overflow_embeddings > 0 {
            self.overflow_embeddings -= 1;
        } else {
            self.depth -= 1;
            self.current = self.stack[self.depth];
        }
    }

    fn top_is_isolate(&self) -> bool {
        self.depth > 0 && self.stack[self.depth - 1].isolate
    }

    /// X2–X5: LRE, RLE, LRO or RLO.
    fn open_embedding(&mut self, class: BidiClass) {
        let rtl = class.rtl_bit();
        // The least greater level of the class's parity.
        let level = ((self.current.level + rtl + 2) & !1) - rtl;
        let overriding = match class {
            BidiClass::Lro => Some(BidiClass::L),
            BidiClass::Rlo => Some(BidiClass::R),
            _ => None,
        };
        self.push(level, overriding, false);
    }

    /// X7: PDF.
    fn close_embedding(&mut self) {
        if !self.top_is_isolate() {
            self.pop();
        }
    }

    /// X5a–X5c: an isolate initiator's class (overridden or not), level and depth.
    fn open_isolate(&mut self, class: BidiClass, rtl: bool) -> (BidiClass, u8, u8) {
        let Status {
            level,
            overriding,
            isolate_depth,
            ..
        } = self.current;
        let opened = (overriding.unwrap_or(class), level, isolate_depth);
        if isolate_depth < MAX_ISOLATE_DEPTH {
            self.current.isolate_depth += 1;
        }
        let inner = if rtl {
            level + 1 + level % 2
        } else {
            level + 2 - level % 2
        };
        if inner <= MAX_LEVEL {
            self.valid_isolates += 1;
            self.push(inner, None, true);
            self.current.level = inner;
        } else {
            self.overflow_isolates += 1;
        }
        opened
    }

    /// X6a: a PDI's class, level and depth.
    fn close_isolate(&mut self) -> (BidiClass, u8, u8) {
        if self.overflow_isolates > 0 {
            self.overflow_isolates -= 1;
        } else if self.valid_isolates > 0 {
            self.overflow_embeddings = 0;
            while self.depth > 0 && !self.top_is_isolate() {
                self.pop();
            }
            self.pop();
            self.current.isolate_depth = self.current.isolate_depth.saturating_sub(1);
            self.valid_isolates -= 1;
        } else {
            return (
                BidiClass::On,
                self.current.level,
                self.current.isolate_depth,
            );
        }
        (
            BidiClass::Pdi,
            self.current.level,
            self.current.isolate_depth,
        )
    }

    /// X6: the class of any other character at the current status.
    fn other(&self, class: BidiClass) -> (BidiClass, u8, u8) {
        let status = &self.current;
        (
            status.overriding.unwrap_or(class),
            status.level,
            status.isolate_depth,
        )
    }
}

/// Rules X1–X9: the runs of the paragraph's kept characters, in order, merged
/// where they share a class, level and isolate depth. `fsi_rtl` holds the
/// result of [`first_strong_isolates`].
pub(super) fn runs(
    classes: &[BidiClass],
    brackets: Option<&[Option<Bracket>]>,
    base_level: u8,
    fsi_rtl: &[bool],
    runs: &mut BudgetVec<Run>,
) -> Result<()> {
    runs.clear();
    let mut embeddings = Embeddings::new(base_level);
    let mut fsis = fsi_rtl.iter();
    // FriBidi stops at the first run of paragraph separators: the whole run
    // takes the isolate depth reached.
    let mut stopped: Option<Option<u8>> = None;
    for (at, &class) in classes.iter().enumerate() {
        let (resolved, level, isolate) = if let Some(separators) = stopped {
            let isolate = separators.filter(|_| class == BidiClass::B);
            stopped = Some(isolate);
            (class, 0, isolate.unwrap_or(0))
        } else {
            match class {
                BidiClass::Lre | BidiClass::Rle | BidiClass::Lro | BidiClass::Rlo => {
                    embeddings.open_embedding(class);
                    continue;
                }
                BidiClass::Pdf => {
                    embeddings.close_embedding();
                    continue;
                }
                BidiClass::Bn => continue,
                BidiClass::Lri => embeddings.open_isolate(class, false),
                BidiClass::Rli => embeddings.open_isolate(class, true),
                BidiClass::Fsi => {
                    let rtl = fsis.next().copied().unwrap_or(false);
                    embeddings.open_isolate(class, rtl)
                }
                BidiClass::Pdi => embeddings.close_isolate(),
                BidiClass::B => {
                    let isolate = embeddings.current.isolate_depth;
                    stopped = Some(Some(isolate));
                    (class, 0, isolate)
                }
                _ => embeddings.other(class),
            }
        };
        // Only ON characters pair as brackets (`fribidi_get_bracket_types`).
        let bracket = brackets
            .filter(|_| class == BidiClass::On)
            .and_then(|brackets| brackets.get(at).copied().flatten());
        let run = Run {
            len: 1,
            class: resolved,
            level,
            isolate,
            bracket,
        };
        match runs.as_mut_slice().last_mut() {
            Some(last) if last.continues_with(&run) => last.len += 1,
            _ => runs.try_push(run).map_err(|e| e.error)?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use BidiClass::*;
    use triergon_core::Budget;

    fn levels(classes: &[BidiClass], base_level: u8) -> Vec<(BidiClass, u8, u8, u32)> {
        let budget = Budget::new(1 << 20).expect("budget");
        let mut out = BudgetVec::new(&budget);
        runs(classes, None, base_level, &[], &mut out).expect("runs");
        out.as_slice()
            .iter()
            .map(|r| (r.class, r.level, r.isolate, r.len))
            .collect()
    }

    #[test]
    fn paragraph_direction_skips_isolates() {
        assert_eq!(first_strong(&[On, R, L]), Some(Direction::RightToLeft));
        assert_eq!(
            first_strong(&[Lri, R, Pdi, L]),
            Some(Direction::LeftToRight)
        );
        // An unmatched PDI is ignored; the search looks past a separator.
        assert_eq!(
            first_strong(&[Pdi, En, B, Al]),
            Some(Direction::RightToLeft)
        );
        assert_eq!(first_strong(&[Rli, R]), None);
    }

    #[test]
    fn first_strong_isolates_skip_nested_content() {
        let budget = Budget::new(1 << 20).expect("budget");
        let (mut rtl, mut open) = (BudgetVec::new(&budget), BudgetVec::new(&budget));
        let classes = [Fsi, Lri, R, Pdi, R, L, Pdi, Fsi, On, Pdi];
        first_strong_isolates(&classes, &mut rtl, &mut open).expect("fsi");
        assert_eq!(rtl.as_slice(), &[true, false]);
    }

    #[test]
    fn embeddings_and_overrides() {
        // a RLE b PDF c, removed characters dropped.
        assert_eq!(
            levels(&[L, Rle, L, Pdf, L], 0),
            [(L, 0, 0, 1), (L, 1, 0, 1), (L, 0, 0, 1)]
        );
        // RLO overrides the class of what it embeds.
        assert_eq!(
            levels(&[Rlo, L, En, Pdf], 0),
            [(R, 1, 0, 2)],
            "the overridden characters form one run"
        );
    }

    #[test]
    fn isolates() {
        assert_eq!(
            levels(&[L, Rli, L, Pdi, Pdi], 0),
            [
                (L, 0, 0, 1),
                (Rli, 0, 0, 1),
                (L, 1, 1, 1),
                (Pdi, 0, 0, 1),
                (On, 0, 0, 1),
            ]
        );
    }

    #[test]
    fn overflow_stops_at_level_125() {
        let mut classes = vec![Rle; 70];
        classes.push(L);
        // 70 RLEs raise the level by two each until 125 is passed.
        let runs = levels(&classes, 0);
        assert_eq!(runs, [(L, 125, 0, 1)]);
    }

    #[test]
    fn separator_stops_explicit_processing() {
        assert_eq!(
            levels(&[Rle, L, B, Lre, L], 0),
            [(L, 1, 0, 1), (B, 0, 0, 1), (Lre, 0, 0, 1), (L, 0, 0, 1)]
        );
        // The separators directly after the first share its isolate depth.
        assert_eq!(
            levels(&[Lri, B, B, L, B], 0),
            [(Lri, 0, 0, 1), (B, 0, 1, 2), (L, 0, 0, 1), (B, 0, 0, 1)]
        );
    }
}
