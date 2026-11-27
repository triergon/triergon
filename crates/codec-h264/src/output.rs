//! Cropped output pictures that share their decoded frame buffers.

use std::sync::Arc;

use crate::picture::{Coding, FrameBuf, FrameStore};

/// Colour description from the VUI (Annex E); defaults are "unspecified" (2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColourDescription {
    /// colour_primaries (Table E-3).
    pub primaries: u8,
    /// transfer_characteristics (Table E-4).
    pub transfer: u8,
    /// matrix_coefficients (Table E-5).
    pub matrix: u8,
    /// video_full_range_flag.
    pub full_range: bool,
}

impl Default for ColourDescription {
    fn default() -> Self {
        ColourDescription {
            primaries: 2,
            transfer: 2,
            matrix: 2,
            full_range: false,
        }
    }
}

/// One 8-bit sample plane of an output picture. The samples are shared with the decoder's
/// frame buffer (no copy); `row(y)` returns the `width` visible samples of row `y`.
#[derive(Clone)]
pub struct Plane {
    frame: Arc<FrameBuf>,
    component: Component,
    offset: usize,
    /// Visible samples per row.
    pub width: usize,
    /// Visible rows.
    pub height: usize,
    /// Distance in samples between the starts of two rows.
    pub stride: usize,
}

/// The plane of a frame buffer a [`Plane`] shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Component {
    Y,
    Cb,
    Cr,
}

impl Plane {
    fn samples(&self) -> &[u8] {
        match self.component {
            Component::Y => &self.frame.y,
            Component::Cb => &self.frame.cb,
            Component::Cr => &self.frame.cr,
        }
    }
    /// Visible samples of row `y` (`width` bytes).
    pub fn row(&self, y: usize) -> &[u8] {
        let o = self.offset + y * self.stride;
        &self.samples()[o..o + self.width]
    }
    /// The plane from its first visible sample (rows `stride` apart).
    pub fn data(&self) -> &[u8] {
        &self.samples()[self.offset..]
    }
    /// The whole decoded plane, its stride and the index of the first visible
    /// (cropped) sample: visible sample (x, y) is `raw[offset + y * stride + x]`.
    pub fn raw(&self) -> (&[u8], usize, usize) {
        (self.samples(), self.stride, self.offset)
    }
}

impl std::fmt::Debug for Plane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Plane {{ {}x{} stride {} }}",
            self.width, self.height, self.stride
        )
    }
}

/// A decoded picture in output order (cropped, 8-bit 4:2:0; monochrome streams carry
/// chroma planes filled with 128 like FFmpeg's yuv420p output).
#[derive(Clone, Debug)]
pub struct Picture {
    /// Cropped width in luma samples.
    pub width: usize,
    /// Cropped height in luma samples.
    pub height: usize,
    /// Bits per sample: always 8.
    pub bit_depth: u32,
    /// Y, Cb and Cr; the chroma planes have half the width and height.
    pub planes: [Plane; 3],
    /// The timestamp passed with the access unit that carried the picture.
    pub pts: Option<i64>,
    /// PicOrderCnt of the frame (8.2.1): the smaller of its fields' counts.
    pub poc: i32,
    /// Sample aspect ratio from the VUI as (width, height); (0, 0) when unspecified.
    pub sar: (u32, u32),
    /// Colour description from the VUI.
    pub colour: ColourDescription,
    /// The picture was coded as fields (both fields of the output frame).
    pub interlaced: bool,
}

/// The cropped output picture of a frame store with order count `poc`, sharing its
/// samples; none for a non-existing frame.
pub(crate) fn output_picture(poc: i32, fs: &FrameStore) -> Option<Picture> {
    if fs.non_existing {
        return None;
    }
    let b = fs.buf.as_ref()?;
    let (cx, cy, cw, ch) = fs.info.crop;
    let (cx, cy, cw, ch) = (cx as usize, cy as usize, cw as usize, ch as usize);
    let (ccw, cch) = (cw / 2, ch / 2);
    let mk = |component: Component| -> Plane {
        if component == Component::Y {
            Plane {
                frame: b.clone(),
                component,
                offset: cy * b.width + cx,
                width: cw,
                height: ch,
                stride: b.width,
            }
        } else {
            Plane {
                frame: b.clone(),
                component,
                offset: (cy / 2) * b.cwidth + cx / 2,
                width: ccw,
                height: cch,
                stride: b.cwidth,
            }
        }
    };
    Some(Picture {
        width: cw,
        height: ch,
        bit_depth: 8,
        planes: [mk(Component::Y), mk(Component::Cb), mk(Component::Cr)],
        pts: fs.pts,
        poc,
        sar: fs.info.sar,
        colour: fs.info.colour,
        interlaced: fs.coding == Coding::Fields,
    })
}
