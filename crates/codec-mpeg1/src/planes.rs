//! The encoder's own pictures (reference and reconstruction) in charged planes.

use triergon_core::{Budget, BudgetVec, Error};
use triergon_pixel::{Colour, Geometry, PictureView, Plane};

/// A YUV 4:2:0 picture at the coded size, laid out like `triergon_pixel::Picture`.
#[derive(Debug)]
pub(crate) struct Planes {
    geometry: Geometry,
    y: BudgetVec<u8>,
    cb: BudgetVec<u8>,
    cr: BudgetVec<u8>,
}

impl Planes {
    /// Black luma and neutral chroma, as `Picture::new`.
    pub(crate) fn try_new(budget: &Budget, geometry: Geometry) -> Result<Planes, Error> {
        let rows = geometry.macroblock_height() * 16;
        let chroma = geometry.chroma_stride() * rows / 2;
        Ok(Planes {
            geometry,
            y: BudgetVec::try_filled(budget, 0, geometry.luma_stride() * rows)?,
            cb: BudgetVec::try_filled(budget, 128, chroma)?,
            cr: BudgetVec::try_filled(budget, 128, chroma)?,
        })
    }

    #[inline]
    pub(crate) fn y(&self) -> &[u8] {
        &self.y
    }
    #[inline]
    pub(crate) fn cb(&self) -> &[u8] {
        &self.cb
    }
    #[inline]
    pub(crate) fn cr(&self) -> &[u8] {
        &self.cr
    }
    #[inline]
    pub(crate) fn y_mut(&mut self) -> &mut [u8] {
        &mut self.y
    }
    #[inline]
    pub(crate) fn cb_mut(&mut self) -> &mut [u8] {
        &mut self.cb
    }
    #[inline]
    pub(crate) fn cr_mut(&mut self) -> &mut [u8] {
        &mut self.cr
    }

    /// The visible picture over the coded planes.
    pub(crate) fn view(&self) -> PictureView<'_> {
        let g = self.geometry;
        let plane = |data, stride| Plane::U8 {
            data,
            stride,
            offset: 0,
        };
        PictureView {
            width: g.width(),
            height: g.height(),
            bit_depth: 8,
            planes: [
                plane(&self.y[..], g.luma_stride()),
                plane(&self.cb[..], g.chroma_stride()),
                plane(&self.cr[..], g.chroma_stride()),
            ],
            pts: None,
            sar: (1, 1),
            colour: Colour {
                matrix: 2,
                full_range: false,
            },
        }
    }
}
