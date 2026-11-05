use crate::{CaptureError, Result};

/// Visible size plus the macroblock-aligned coded size MPEG-1 operates on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) mb_width: usize,
    pub(crate) mb_height: usize,
}

impl Geometry {
    pub const fn width(self) -> usize {
        self.width
    }
    pub const fn height(self) -> usize {
        self.height
    }
    pub const fn macroblock_width(self) -> usize {
        self.mb_width
    }
    pub const fn macroblock_height(self) -> usize {
        self.mb_height
    }

    /// Even sizes up to the 12-bit sequence-header fields are accepted.
    pub fn new(width: u32, height: u32) -> Result<Geometry> {
        if width == 0
            || height == 0
            || width % 2 != 0
            || height % 2 != 0
            || width > 4095
            || height > 4095
        {
            return Err(CaptureError::Geometry { width, height });
        }
        let (width, height) = (width as usize, height as usize);
        Ok(Geometry {
            width,
            height,
            mb_width: width.div_ceil(16),
            mb_height: height.div_ceil(16),
        })
    }

    pub fn luma_stride(&self) -> usize {
        self.mb_width * 16
    }

    pub fn chroma_stride(&self) -> usize {
        self.mb_width * 8
    }

    pub fn rgba_len(&self) -> usize {
        self.width * self.height * 4
    }
}

/// A YUV 4:2:0 picture stored at the coded (macroblock-aligned) size. Samples
/// beyond the visible area replicate the nearest visible edge sample.
#[derive(Clone, Debug)]
pub struct Picture {
    pub(crate) geometry: Geometry,
    pub(crate) y: Vec<u8>,
    pub(crate) cb: Vec<u8>,
    pub(crate) cr: Vec<u8>,
}

impl Picture {
    pub const fn geometry(&self) -> Geometry {
        self.geometry
    }
    /// Complete coded planes. Mutable slices may change samples, never their
    /// extent, stride or the validated geometry.
    pub fn y(&self) -> &[u8] {
        &self.y
    }
    pub fn cb(&self) -> &[u8] {
        &self.cb
    }
    pub fn cr(&self) -> &[u8] {
        &self.cr
    }
    pub fn y_mut(&mut self) -> &mut [u8] {
        &mut self.y
    }
    pub fn cb_mut(&mut self) -> &mut [u8] {
        &mut self.cb
    }
    pub fn cr_mut(&mut self) -> &mut [u8] {
        &mut self.cr
    }
    pub fn y_row(&self, row: usize) -> Option<&[u8]> {
        if row >= self.geometry.height {
            return None;
        }
        let start = row.checked_mul(self.geometry.luma_stride())?;
        self.y.get(start..start.checked_add(self.geometry.width)?)
    }
    pub fn cb_row(&self, row: usize) -> Option<&[u8]> {
        if row >= self.geometry.height / 2 {
            return None;
        }
        let start = row.checked_mul(self.geometry.chroma_stride())?;
        self.cb
            .get(start..start.checked_add(self.geometry.width / 2)?)
    }
    pub fn cr_row(&self, row: usize) -> Option<&[u8]> {
        if row >= self.geometry.height / 2 {
            return None;
        }
        let start = row.checked_mul(self.geometry.chroma_stride())?;
        self.cr
            .get(start..start.checked_add(self.geometry.width / 2)?)
    }

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

    /// Replicate the visible right/bottom edges into the coded padding.
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

    /// The visible planes, tightly packed (Y, then Cb, then Cr): the layout the
    /// reference worker writes for conversion comparisons.
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
