//! A shape: one or two outlines drawn together as the union of their nonzero
//! fills, with the control box of both.

use triergon_core::{Budget, Result};

use crate::geometry::{Rect, Vector};
use crate::outline::Outline;

/// One or two outlines with the bounding box of all their control points: a
/// glyph or drawing (one outline), its border (the outer and inner offsets), or
/// an opaque box. They are rasterised together as the union of their nonzero
/// fills.
#[derive(Debug)]
pub struct Shape {
    outlines: [Outline; 2],
    cbox: Rect,
}

impl Shape {
    /// An empty shape.
    pub fn new(budget: &Budget) -> Self {
        Self::from_outlines(Outline::new(budget), Outline::new(budget))
    }

    /// A shape of `first` and `second`; leave `second` empty for a glyph.
    pub fn from_outlines(first: Outline, second: Outline) -> Self {
        let mut shape = Self {
            outlines: [first, second],
            cbox: Rect::EMPTY,
        };
        shape.update_cbox();
        shape
    }

    /// The unit square (0, 0)–(64, 64), which the transform of
    /// [`PlacedBorder::Box`](crate::PlacedBorder::Box) scales into the opaque
    /// box of `\bord` style 3.
    ///
    /// # Errors
    ///
    /// The budget's errors.
    pub fn unit_box(budget: &Budget) -> Result<Self> {
        let mut outline = Outline::new(budget);
        outline.push_rect(Vector::new(0, 0), Vector::new(64, 64))?;
        Ok(Self::from_outlines(outline, Outline::new(budget)))
    }

    /// The two outlines: the glyph, drawing, box or outer border first, the
    /// inner border or nothing second.
    pub fn outlines(&self) -> &[Outline; 2] {
        &self.outlines
    }

    /// The control box of both outlines, as libass keeps it with an outline:
    /// the bounds of all their points, on- and off-curve, and all zero when
    /// both are empty. A renderer lays glyphs out by it.
    pub fn cbox(&self) -> &Rect {
        &self.cbox
    }

    /// Whether both outlines are empty.
    pub fn is_empty(&self) -> bool {
        self.outlines.iter().all(Outline::is_empty)
    }

    /// Empties both outlines, keeping their storage.
    pub fn clear(&mut self) {
        for outline in &mut self.outlines {
            outline.clear();
        }
        self.update_cbox();
    }

    /// Clears both outlines, lets `build` fill them, then updates the box.
    pub(crate) fn rebuild(
        &mut self,
        build: impl FnOnce([&mut Outline; 2]) -> Result<()>,
    ) -> Result<()> {
        let [first, second] = &mut self.outlines;
        first.clear();
        second.clear();
        let result = build([first, second]);
        self.update_cbox();
        result
    }

    fn update_cbox(&mut self) {
        let mut cbox = self.outlines[0].control_box();
        cbox.include(self.outlines[1].control_box());
        self.cbox = if cbox.is_empty() {
            Rect::new(0, 0, 0, 0)
        } else {
            cbox
        };
    }

    /// Gives back the storage beyond what its outlines hold, for a shape
    /// kept once built.
    ///
    /// # Errors
    ///
    /// The budget's errors.
    pub fn shrink_to_fit(&mut self) -> Result<()> {
        self.outlines
            .iter_mut()
            .try_for_each(Outline::shrink_to_fit)
    }

    /// Bytes of outline storage currently charged.
    pub fn retained_bytes(&self) -> usize {
        self.outlines.iter().map(Outline::retained_bytes).sum()
    }
}
