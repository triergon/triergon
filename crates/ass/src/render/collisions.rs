//! Collision handling (libass's `fix_collisions`): events of one layer that
//! are placed by their margins move up or down out of each other's way, and
//! keep the place they were given on later frames.

use std::ops::Range;

use triergon_core::{Budget, BudgetVec, Result, WorkBudget};

use super::images::Image;
use crate::{Event, EventState, VAlign, charge};

/// Where an event was placed by collision handling, kept with the event
/// across frames (libass's `ASS_RenderPriv`). A record made by another
/// renderer counts as none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Placed {
    renderer: u32,
    rect: Rect,
}

impl Placed {
    /// The record for `renderer`, reset when another renderer made it.
    fn for_renderer(&mut self, renderer: u32) -> &mut Self {
        if self.renderer != renderer {
            *self = Self {
                renderer,
                ..Self::default()
            };
        }
        self
    }

    fn clear(&mut self) {
        self.rect = Rect::default();
    }
}

/// One rendered event's box and images (libass's `EventImages`).
#[derive(Debug)]
pub(super) struct EventBox {
    pub event: usize,
    pub layer: i32,
    pub read_order: i32,
    pub rect: Rect,
    pub detect_collisions: bool,
    /// Bottom-aligned events move up, others down.
    pub moves_up: bool,
    pub images: Range<usize>,
}

impl EventBox {
    /// The box of event `index`, `event` of its script, in its state after
    /// its text, at `rect`, with its `images`.
    pub(super) fn new(
        index: usize,
        event: &Event,
        state: &EventState<'_>,
        rect: Rect,
        images: Range<usize>,
    ) -> Self {
        Self {
            event: index,
            layer: event.layer,
            read_order: event.read_order,
            rect,
            detect_collisions: state.detect_collisions,
            moves_up: state.alignment.vertical == VAlign::Bottom,
            images,
        }
    }

    /// Whether collision handling moves the event: it detects collisions and
    /// has an area.
    pub(super) fn takes_part(&self) -> bool {
        self.detect_collisions && self.rect.height() != 0 && self.rect.width() != 0
    }
}

/// An event's box in frame pixels, as libass keeps it: a top and left edge
/// with a height and width, whose far sides it adds in `int` arithmetic.
/// The far sides are exclusive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Rect {
    pub y0: i32,
    pub y1: i32,
    pub x0: i32,
    pub x1: i32,
}

impl Rect {
    /// The box from `top` and `left` over `height` and `width`, its far
    /// sides added in wrapping `int` arithmetic as libass adds them.
    pub(super) const fn new(top: i32, height: i32, left: i32, width: i32) -> Self {
        Self {
            y0: top,
            y1: top.wrapping_add(height),
            x0: left,
            x1: left.wrapping_add(width),
        }
    }

    /// The height libass keeps, which the sides give back exactly.
    pub(super) const fn height(&self) -> i32 {
        self.y1.wrapping_sub(self.y0)
    }

    /// The width libass keeps.
    pub(super) const fn width(&self) -> i32 {
        self.x1.wrapping_sub(self.x0)
    }

    /// The box moved down by `rows`.
    const fn shifted(self, rows: i32) -> Self {
        Self {
            y0: self.y0.wrapping_add(rows),
            y1: self.y1.wrapping_add(rows),
            ..self
        }
    }

    fn overlaps(&self, other: &Self) -> bool {
        !(self.y0 >= other.y1 || other.y0 >= self.y1 || self.x0 >= other.x1 || other.x0 >= self.x1)
    }

    /// `cmp_rect_y0`: the difference of the tops in `int`, which wraps for
    /// tops 2³¹ or more apart.
    const fn cmp_y0(&self, other: &Self) -> i32 {
        self.y0.wrapping_sub(other.y0)
    }
}

/// The boxes of the events placed so far in a layer, in libass's order, and
/// the scratch its sort merges through: storage kept between frames.
#[derive(Debug)]
pub(super) struct Fixed {
    boxes: BudgetVec<Rect>,
    merge: BudgetVec<Rect>,
}

impl Fixed {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            boxes: BudgetVec::new(budget),
            merge: BudgetVec::new(budget),
        }
    }

    /// Empties the boxes and reserves room for `events` of them and for the
    /// sort's scratch.
    fn reset(&mut self, events: usize) -> Result<()> {
        self.boxes.clear();
        self.merge.clear();
        self.boxes.try_reserve_exact(events)?;
        self.merge.try_resize(events, Rect::default())
    }

    /// Adds `rect` after the boxes, unsorted.
    fn push(&mut self, rect: Rect) -> Result<()> {
        self.boxes.try_push(rect).map_err(|e| e.error)
    }

    /// Sorts the boxes as libass does on glibc: `qsort` with `cmp_rect_y0`,
    /// which glibc runs as a stable merge sort. While no two tops lie 2³¹ or
    /// more apart that orders them by value, boxes with equal tops in the
    /// order they were added; otherwise the order follows the sort's splits
    /// and merges. Charged as the merges' comparisons.
    fn sort(&mut self, work: &mut WorkBudget) -> Result<()> {
        let boxes = self.boxes.as_mut_slice();
        let n = boxes.len();
        if n > 1 {
            charge(work, n.saturating_mul(n.ilog2() as usize + 1))?;
            merge_sort(boxes, &mut self.merge.as_mut_slice()[..n]);
        }
        Ok(())
    }

    /// Adds `rect` and sorts the boxes (`fit_rect`).
    fn insert(&mut self, rect: Rect, work: &mut WorkBudget) -> Result<()> {
        self.push(rect)?;
        self.sort(work)
    }

    fn as_slice(&self) -> &[Rect] {
        self.boxes.as_slice()
    }
}

/// glibc's `msort_with_tmp` with `cmp_rect_y0`: the first half and then the
/// second sorted, then merged through `tmp`, a box of the first half first
/// while it compares at most equal.
fn merge_sort(boxes: &mut [Rect], tmp: &mut [Rect]) {
    let n = boxes.len();
    if n <= 1 {
        return;
    }
    let n1 = n / 2;
    merge_sort(&mut boxes[..n1], tmp);
    merge_sort(&mut boxes[n1..], tmp);
    let (mut i, mut j, mut k) = (0, n1, 0);
    while i < n1 && j < n {
        if boxes[i].cmp_y0(&boxes[j]) <= 0 {
            tmp[k] = boxes[i];
            i += 1;
        } else {
            tmp[k] = boxes[j];
            j += 1;
        }
        k += 1;
    }
    // What is left of the first half follows; the second half's rest is
    // already in place.
    let rest = n1 - i;
    tmp[k..k + rest].copy_from_slice(&boxes[i..n1]);
    boxes[..k + rest].copy_from_slice(&tmp[..k + rest]);
}

/// `shift_event`.
fn shift(event: &mut EventBox, images: &mut [Image], shift: i32, frame_height: i32) {
    for image in &mut images[event.images.clone()] {
        image.shift(shift, frame_height);
    }
    event.rect = event.rect.shifted(shift);
}

/// `fit_rect`: the shift that moves `s` clear of every fixed box it meets,
/// down or up; the moved box joins them.
fn fit(s: Rect, fixed: &mut Fixed, up: bool, work: &mut WorkBudget) -> Result<i32> {
    let mut shift: i32 = 0;
    let meets = |f: &Rect, shift: i32| {
        !(s.y1.wrapping_add(shift) <= f.y0
            || s.y0.wrapping_add(shift) >= f.y1
            || s.x1 <= f.x0
            || s.x0 >= f.x1)
    };
    let boxes = fixed.as_slice();
    if up {
        for f in boxes.iter().rev() {
            if meets(f, shift) {
                shift = f.y0.wrapping_sub(s.y1);
            }
        }
    } else {
        for f in boxes {
            if meets(f, shift) {
                shift = f.y1.wrapping_sub(s.y0);
            }
        }
    }
    fixed.insert(s.shifted(shift), work)?;
    Ok(shift)
}

/// `fix_collisions` over the events of one layer and their records: those
/// placed before keep their place unless it changed height or now overlaps,
/// the others move clear of them. Charged each event against those placed
/// before it, and the sorts. `used` is reserved for every event before any
/// record changes, so a refused reservation leaves the records as they were.
pub(super) fn fix(
    events: &mut [EventBox],
    placed: &mut [Placed],
    images: &mut [Image],
    used: &mut Fixed,
    renderer: u32,
    frame_height: i32,
    work: &mut WorkBudget,
) -> Result<()> {
    let taking_part = events.iter().filter(|e| e.takes_part()).count();
    charge(work, taking_part.saturating_mul(taking_part))?;
    // Each event joins the boxes at most once.
    used.reset(events.len())?;
    for (event, record) in events.iter_mut().zip(placed.iter_mut()) {
        if !event.takes_part() {
            continue;
        }
        let record = record.for_renderer(renderer);
        if record.rect.height() <= 0 {
            continue;
        }
        let s = record.rect;
        if record.rect.height() != event.rect.height() {
            record.clear();
        }
        if used.as_slice().iter().any(|u| s.overlaps(u)) {
            record.clear();
        }
        if record.rect.height() > 0 {
            used.push(record.rect)?;
            let by = record.rect.y0.wrapping_sub(event.rect.y0);
            shift(event, images, by, frame_height);
        }
    }
    used.sort(work)?;
    for (event, record) in events.iter_mut().zip(placed.iter_mut()) {
        if !event.takes_part() {
            continue;
        }
        let record = record.for_renderer(renderer);
        if record.rect.height() != 0 {
            continue;
        }
        let by = fit(event.rect, used, event.moves_up, work)?;
        if by != 0 {
            shift(event, images, by, frame_height);
        }
        *record = Placed {
            renderer,
            rect: event.rect,
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Rect, merge_sort};

    fn tops(tops: [i32; 3]) -> [Rect; 3] {
        tops.map(|y0| Rect {
            y0,
            ..Rect::default()
        })
    }

    #[test]
    fn tops_far_apart_sort_as_glibc_merges_them() {
        // `cmp_rect_y0` wraps between the last box and the second, which
        // then compares below it: glibc's merge sort leaves the order as it
        // is, where sorting by value would move the last box first.
        let mut boxes = tops([-2_147_482_658, 989, -2_147_482_736]);
        merge_sort(&mut boxes, &mut [Rect::default(); 3]);
        assert_eq!(boxes.map(|r| r.y0), [-2_147_482_658, 989, -2_147_482_736]);
        let mut boxes = tops([30, 10, 20]);
        merge_sort(&mut boxes, &mut [Rect::default(); 3]);
        assert_eq!(boxes.map(|r| r.y0), [10, 20, 30]);
    }
}
