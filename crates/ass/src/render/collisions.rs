//! Collision handling (libass's `fix_collisions`): events of one layer that
//! are placed by their margins move up or down out of each other's way, and
//! keep the place they were given on later frames.

use std::ops::Range;

use triergon_core::{BudgetVec, Result};

use super::images::Image;

/// Where an event was placed by collision handling, kept with the event
/// across frames (libass's `ASS_RenderPriv`). A record made by another
/// renderer counts as none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Placed {
    renderer: u64,
    top: i32,
    height: i32,
    left: i32,
    width: i32,
}

impl Placed {
    /// The record for `renderer`, reset when another renderer made it.
    fn for_renderer(&mut self, renderer: u64) -> &mut Self {
        if self.renderer != renderer {
            *self = Self {
                renderer,
                ..Self::default()
            };
        }
        self
    }

    fn clear(&mut self) {
        *self = Self {
            renderer: self.renderer,
            ..Self::default()
        };
    }

    fn rect(&self) -> Rect {
        Rect {
            y0: self.top,
            y1: self.top.wrapping_add(self.height),
            x0: self.left,
            x1: self.left.wrapping_add(self.width),
        }
    }
}

/// One rendered event's box and images (libass's `EventImages`).
#[derive(Debug)]
pub(super) struct EventBox {
    pub event: usize,
    pub layer: i32,
    pub read_order: i32,
    pub top: i32,
    pub height: i32,
    pub left: i32,
    pub width: i32,
    pub detect_collisions: bool,
    /// Bottom-aligned events move up, others down.
    pub moves_up: bool,
    pub images: Range<usize>,
}

impl EventBox {
    /// Whether collision handling moves the event: it detects collisions and
    /// has an area.
    pub(super) fn takes_part(&self) -> bool {
        self.detect_collisions && self.height != 0 && self.width != 0
    }

    fn rect(&self) -> Rect {
        Rect {
            y0: self.top,
            y1: self.top.wrapping_add(self.height),
            x0: self.left,
            x1: self.left.wrapping_add(self.width),
        }
    }
}

/// A box in frame pixels.
#[derive(Clone, Copy, Debug)]
pub(super) struct Rect {
    y0: i32,
    y1: i32,
    x0: i32,
    x1: i32,
}

impl Rect {
    fn overlaps(&self, other: &Self) -> bool {
        !(self.y0 >= other.y1 || other.y0 >= self.y1 || self.x0 >= other.x1 || other.x0 >= self.x1)
    }
}

/// `shift_event`.
fn shift(event: &mut EventBox, images: &mut [Image], shift: i32, frame_height: i32) {
    for image in &mut images[event.images.clone()] {
        image.shift(shift, frame_height);
    }
    event.top = event.top.wrapping_add(shift);
}

/// Inserts `rect` after the boxes that start above it or at the same row,
/// keeping them ordered by their top edge (libass sorts them so).
fn insert(fixed: &mut BudgetVec<Rect>, rect: Rect) -> Result<()> {
    fixed.try_push(rect).map_err(|e| e.error)?;
    let slice = fixed.as_mut_slice();
    let mut i = slice.len() - 1;
    while i > 0 && slice[i - 1].y0 > slice[i].y0 {
        slice.swap(i - 1, i);
        i -= 1;
    }
    Ok(())
}

/// `fit_rect`: the shift that moves `s` clear of every fixed box it meets,
/// down or up; the moved box joins them.
fn fit(s: Rect, fixed: &mut BudgetVec<Rect>, up: bool) -> Result<i32> {
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
    let moved = Rect {
        y0: s.y0.wrapping_add(shift),
        y1: s.y1.wrapping_add(shift),
        ..s
    };
    insert(fixed, moved)?;
    Ok(shift)
}

/// `fix_collisions` over the events of one layer and their records: those
/// placed before keep their place unless it changed height or now overlaps,
/// the others move clear of them. `used` is scratch storage, reserved for
/// every event before any record changes, so a refused reservation leaves
/// the records as they were.
pub(super) fn fix(
    events: &mut [EventBox],
    placed: &mut [Placed],
    images: &mut [Image],
    used: &mut BudgetVec<Rect>,
    renderer: u64,
    frame_height: i32,
) -> Result<()> {
    used.clear();
    // Each event joins the boxes at most once.
    used.try_reserve_exact(events.len())?;
    for (event, record) in events.iter_mut().zip(placed.iter_mut()) {
        if !event.takes_part() {
            continue;
        }
        let record = record.for_renderer(renderer);
        if record.height <= 0 {
            continue;
        }
        let s = record.rect();
        if record.height != event.height {
            record.clear();
        }
        if used.as_slice().iter().any(|u| s.overlaps(u)) {
            record.clear();
        }
        if record.height > 0 {
            insert(used, record.rect())?;
            let by = record.top.wrapping_sub(event.top);
            shift(event, images, by, frame_height);
        }
    }
    for (event, record) in events.iter_mut().zip(placed.iter_mut()) {
        if !event.takes_part() {
            continue;
        }
        let record = record.for_renderer(renderer);
        if record.height != 0 {
            continue;
        }
        let by = fit(event.rect(), used, event.moves_up)?;
        if by != 0 {
            shift(event, images, by, frame_height);
        }
        *record = Placed {
            renderer,
            top: event.top,
            height: event.height,
            left: event.left,
            width: event.width,
        };
    }
    Ok(())
}
