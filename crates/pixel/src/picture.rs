use crate::{Error, Result};

/// A visible picture size and the coded size around it: each dimension
/// rounded up to a whole number of 16×16 macroblocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) mb_width: usize,
    pub(crate) mb_height: usize,
}

impl Geometry {
    /// Visible width in pixels.
    pub const fn width(self) -> usize {
        self.width
    }
    /// Visible height in pixels.
    pub const fn height(self) -> usize {
        self.height
    }
    /// Coded width in macroblocks.
    pub const fn macroblock_width(self) -> usize {
        self.mb_width
    }
    /// Coded height in macroblocks.
    pub const fn macroblock_height(self) -> usize {
        self.mb_height
    }

    /// A geometry of `width` × `height` visible pixels. Both must be even and
    /// from 2 to 4094, the range of the 12-bit MPEG-1 size fields;
    /// [`Error::Geometry`] otherwise.
    pub fn new(width: u32, height: u32) -> Result<Geometry> {
        if width == 0
            || height == 0
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
            || width > 4095
            || height > 4095
        {
            return Err(Error::Geometry { width, height });
        }
        let (width, height) = (width as usize, height as usize);
        Ok(Geometry {
            width,
            height,
            mb_width: width.div_ceil(16),
            mb_height: height.div_ceil(16),
        })
    }

    /// Samples per coded luma row.
    pub fn luma_stride(&self) -> usize {
        self.mb_width * 16
    }

    /// Samples per coded chroma row.
    pub fn chroma_stride(&self) -> usize {
        self.mb_width * 8
    }

    /// Bytes of an RGBA frame of the visible size.
    pub fn rgba_len(&self) -> usize {
        self.width * self.height * 4
    }
}

/// An 8-bit YUV 4:2:0 picture stored at the coded size of its [`Geometry`]:
/// rows of [`luma_stride`](Geometry::luma_stride) and
/// [`chroma_stride`](Geometry::chroma_stride) samples. After
/// [`pad_edges`](Self::pad_edges) the samples beyond the visible area repeat
/// the nearest visible one.
#[derive(Clone, Debug)]
pub struct Picture {
    pub(crate) geometry: Geometry,
    pub(crate) y: Vec<u8>,
    pub(crate) cb: Vec<u8>,
    pub(crate) cr: Vec<u8>,
}

impl Picture {
    /// The picture's sizes.
    pub const fn geometry(&self) -> Geometry {
        self.geometry
    }
    /// The coded luma plane, padding included.
    pub fn y(&self) -> &[u8] {
        &self.y
    }
    /// The coded Cb plane, padding included.
    pub fn cb(&self) -> &[u8] {
        &self.cb
    }
    /// The coded Cr plane, padding included.
    pub fn cr(&self) -> &[u8] {
        &self.cr
    }
    /// The coded luma plane, to write samples into.
    pub fn y_mut(&mut self) -> &mut [u8] {
        &mut self.y
    }
    /// The coded Cb plane, to write samples into.
    pub fn cb_mut(&mut self) -> &mut [u8] {
        &mut self.cb
    }
    /// The coded Cr plane, to write samples into.
    pub fn cr_mut(&mut self) -> &mut [u8] {
        &mut self.cr
    }
    /// The visible samples of luma row `row`; `None` when there is no such row.
    pub fn y_row(&self, row: usize) -> Option<&[u8]> {
        if row >= self.geometry.height {
            return None;
        }
        let start = row.checked_mul(self.geometry.luma_stride())?;
        self.y.get(start..start.checked_add(self.geometry.width)?)
    }
    /// The visible samples of Cb row `row`; `None` when there is no such row.
    pub fn cb_row(&self, row: usize) -> Option<&[u8]> {
        if row >= self.geometry.height / 2 {
            return None;
        }
        let start = row.checked_mul(self.geometry.chroma_stride())?;
        self.cb
            .get(start..start.checked_add(self.geometry.width / 2)?)
    }
    /// The visible samples of Cr row `row`; `None` when there is no such row.
    pub fn cr_row(&self, row: usize) -> Option<&[u8]> {
        if row >= self.geometry.height / 2 {
            return None;
        }
        let start = row.checked_mul(self.geometry.chroma_stride())?;
        self.cr
            .get(start..start.checked_add(self.geometry.width / 2)?)
    }

    /// A picture of luma 0 and chroma 128 throughout.
    pub fn new(geometry: Geometry) -> Picture {
        let luma = geometry.luma_stride() * geometry.mb_height * 16;
        let chroma = geometry.chroma_stride() * geometry.mb_height * 8;
        Picture {
            geometry,
            y: vec![0; luma],
            cb: vec![128; chroma],
            cr: vec![128; chroma],
        }
    }

    /// Repeats the last visible column and row through the coded padding to
    /// their right and below.
    pub fn pad_edges(&mut self) {
        let g = self.geometry;
        pad_plane(
            &mut self.y,
            g.luma_stride(),
            g.width,
            g.height,
            g.mb_height * 16,
        );
        pad_plane(
            &mut self.cb,
            g.chroma_stride(),
            g.width / 2,
            g.height / 2,
            g.mb_height * 8,
        );
        pad_plane(
            &mut self.cr,
            g.chroma_stride(),
            g.width / 2,
            g.height / 2,
            g.mb_height * 8,
        );
    }

    /// The visible samples without padding: every Y row, then every Cb row,
    /// then every Cr row.
    pub fn visible_yuv420(&self) -> Vec<u8> {
        let g = self.geometry;
        let mut out = Vec::with_capacity(g.width * g.height * 3 / 2);
        for row in self.y.chunks_exact(g.luma_stride()).take(g.height) {
            out.extend_from_slice(&row[..g.width]);
        }
        for plane in [&self.cb, &self.cr] {
            for row in plane.chunks_exact(g.chroma_stride()).take(g.height / 2) {
                out.extend_from_slice(&row[..g.width / 2]);
            }
        }
        out
    }
}

fn pad_plane(plane: &mut [u8], stride: usize, width: usize, height: usize, coded_height: usize) {
    if width < stride {
        for row in plane.chunks_exact_mut(stride).take(height) {
            let edge = row[width - 1];
            row[width..].fill(edge);
        }
    }
    let (visible, padding) = plane.split_at_mut(height * stride);
    let last = &visible[(height - 1) * stride..];
    for row in padding.chunks_exact_mut(stride).take(coded_height - height) {
        row.copy_from_slice(last);
    }
}
