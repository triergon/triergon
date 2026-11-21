//! Output pictures: cropped views of the decoder's frames with their VUI description.

use std::sync::Arc;

use crate::picture::{AnyFrame, SharedFrame};
use triergon_codec::{Colour, PictureView};

/// Colour description from the VUI (Annex E.2.1). Code points follow Rec. ITU-T H.273;
/// 2 means "unspecified" and is the value when the stream does not signal one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColourDescription {
    /// `colour_primaries`.
    pub primaries: u8,
    /// `transfer_characteristics`.
    pub transfer: u8,
    /// `matrix_coeffs`.
    pub matrix: u8,
    /// `video_full_range_flag`: samples use the full range rather than the narrow one.
    pub full_range: bool,
    /// `video_signal_type_present_flag`: `full_range` was signalled.
    pub video_signal_type_present: bool,
    /// `colour_description_present_flag`: the three code points were signalled.
    pub colour_description_present: bool,
}

/// Chroma sampling of an output picture. The decoder outputs 4:2:0 only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChromaFormat {
    /// Chroma planes at half the luma width and height.
    Yuv420,
}

/// Borrowed samples of a plane: `U8` for 8-bit streams, `U16` for 9- and 10-bit ones.
#[derive(Debug)]
pub enum Row<'a> {
    /// 8-bit samples.
    U8(&'a [u8]),
    /// Samples of more than 8 bits, one per `u16`.
    U16(&'a [u16]),
}

/// One cropped sample plane of an output picture. It shares the decoder's frame buffer,
/// which returns to the decoder's frame pool when the last plane and reference drop.
#[derive(Debug, Clone)]
pub struct Plane {
    /// Visible width in samples.
    pub width: usize,
    /// Visible height in samples.
    pub height: usize,
    frame: Arc<SharedFrame>,
    index: usize,
    x0: usize,
    y0: usize,
}

impl Plane {
    /// The `width` visible samples of row `y`, which must be below `height`.
    pub fn row(&self, y: usize) -> Row<'_> {
        match &**self.frame {
            AnyFrame::U8(f) => {
                let p = &f.planes[self.index];
                let o = (self.y0 + y) * p.stride + self.x0;
                Row::U8(&p.data[o..o + self.width])
            }
            AnyFrame::U16(f) => {
                let p = &f.planes[self.index];
                let o = (self.y0 + y) * p.stride + self.x0;
                Row::U16(&p.data[o..o + self.width])
            }
        }
    }

    /// Zero-copy view of the whole backing plane: (samples, stride in samples, index of the
    /// first visible sample). Sample (x, y) of this plane is
    /// `slice[offset + y * stride + x]` for x < width, y < height.
    pub fn raw(&self) -> (Row<'_>, usize, usize) {
        match &**self.frame {
            AnyFrame::U8(f) => {
                let p = &f.planes[self.index];
                (Row::U8(&p.data), p.stride, self.y0 * p.stride + self.x0)
            }
            AnyFrame::U16(f) => {
                let p = &f.planes[self.index];
                (Row::U16(&p.data), p.stride, self.y0 * p.stride + self.x0)
            }
        }
    }

    /// Append the visible samples row by row (u8, or little-endian u16).
    pub fn write_raw(&self, out: &mut Vec<u8>) {
        for y in 0..self.height {
            match self.row(y) {
                Row::U8(r) => out.extend_from_slice(r),
                Row::U16(r) => {
                    for &v in r {
                        out.extend_from_slice(&v.to_le_bytes());
                    }
                }
            }
        }
    }

    /// The plane as the codec layer's borrowed plane.
    fn view(&self) -> triergon_codec::Plane<'_> {
        match self.raw() {
            (Row::U8(data), stride, offset) => triergon_codec::Plane::U8 {
                data,
                stride,
                offset,
            },
            (Row::U16(data), stride, offset) => triergon_codec::Plane::U16 {
                data,
                stride,
                offset,
            },
        }
    }
}

/// A decoded picture in output order, cropped to the SPS conformance window.
#[derive(Debug, Clone)]
pub struct Picture {
    /// Cropped luma width in samples.
    pub width: u32,
    /// Cropped luma height in samples.
    pub height: u32,
    /// Bits per sample, 8 to 10, the same for luma and chroma.
    pub bit_depth: u32,
    /// Chroma sampling of `planes[1]` and `planes[2]`.
    pub chroma: ChromaFormat,
    /// Y, Cb and Cr.
    pub planes: [Plane; 3],
    /// The timestamp of the access unit (or NAL unit) that carried the picture.
    pub pts: Option<i64>,
    /// Picture order count (8.3.1).
    pub poc: i32,
    /// Sample aspect ratio from the VUI (Table E.1), (0, 1) when unspecified.
    pub sar: (u32, u32),
    /// Colour description from the VUI.
    pub colour: ColourDescription,
}

/// What the SPS of a picture says about its output.
#[derive(Debug)]
pub(crate) struct OutInfo {
    /// Conformance window in luma samples (left, right, top, bottom).
    pub(crate) crop: [u32; 4],
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) bit_depth: u32,
    pub(crate) sar: (u32, u32),
    pub(crate) colour: ColourDescription,
}

impl Picture {
    /// The picture `frame` shows, cropped as `info` says.
    pub(crate) fn new(
        poc: i32,
        frame: &Arc<SharedFrame>,
        pts: Option<i64>,
        info: &OutInfo,
    ) -> Self {
        let [l, r, t, b] = info.crop;
        let w = (info.width - l - r) as usize;
        let h = (info.height - t - b) as usize;
        let mk = |index: usize, x0: usize, y0: usize, pw: usize, ph: usize| Plane {
            width: pw,
            height: ph,
            frame: frame.clone(),
            index,
            x0,
            y0,
        };
        Picture {
            width: w as u32,
            height: h as u32,
            bit_depth: info.bit_depth,
            chroma: ChromaFormat::Yuv420,
            planes: [
                mk(0, l as usize, t as usize, w, h),
                mk(1, l as usize / 2, t as usize / 2, w / 2, h / 2),
                mk(2, l as usize / 2, t as usize / 2, w / 2, h / 2),
            ],
            pts,
            poc,
            sar: info.sar,
            colour: info.colour,
        }
    }

    /// The picture as the codec layer's borrowed view.
    pub(crate) fn view(&self) -> PictureView<'_> {
        PictureView {
            width: self.width as usize,
            height: self.height as usize,
            bit_depth: self.bit_depth as u8,
            planes: [
                self.planes[0].view(),
                self.planes[1].view(),
                self.planes[2].view(),
            ],
            pts: self.pts,
            sar: self.sar,
            colour: Colour {
                matrix: self.colour.matrix,
                full_range: self.colour.full_range,
            },
        }
    }
}
