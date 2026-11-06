//! RGBA → YUV 4:2:0, ITU-R BT.601 matrix, limited ("studio") range.
//!
//! Contract: alpha is ignored;
//! chroma is the mean of each horizontal pixel pair, then decimated vertically
//! with center-sited taps 1/8, 3/8, 3/8, 1/8 over rows 2k-1..2k+2 with the edge
//! rows replicated. The conversion is linear, so chroma is formed from the
//! filtered weighted sums in one rounding step.

use crate::{Error, Geometry, Picture, Result};

/// Row order of the caller's RGBA buffer. Bottom-up buffers are read in reverse
/// without a separate flip pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    TopDown,
    BottomUp,
}

/// Fixed-point coefficients scaled by 2^16. Luma carries the 219/255 range
/// factor, chroma 224/255; each chroma row sums to zero so neutral grey maps
/// to exactly 128. `tests::coefficients_follow_bt601` derives them.
const Y_R: i32 = 16829;
const Y_G: i32 = 33039;
const Y_B: i32 = 6416;
const CB_R: i32 = -9714;
const CB_G: i32 = -19071;
const CB_B: i32 = 28785;
const CR_R: i32 = 28785;
const CR_G: i32 = -24104;
const CR_B: i32 = -4681;

const Y_OFFSET: i32 = (16 << 16) + (1 << 15);
/// Chroma sums carry 2 (pair) × 8 (vertical taps) × 2^16.
const C_SHIFT: u32 = 20;
const C_OFFSET: i32 = (128 << C_SHIFT) + (1 << (C_SHIFT - 1));

/// Reusable conversion scratch: four source rows of pair-summed chroma terms.
#[derive(Debug, Default)]
pub struct Converter {
    cb_rows: [Vec<i32>; 4],
    cr_rows: [Vec<i32>; 4],
}

impl Converter {
    pub fn new(geometry: Geometry) -> Converter {
        let half = geometry.width / 2;
        Converter {
            cb_rows: std::array::from_fn(|_| vec![0; half]),
            cr_rows: std::array::from_fn(|_| vec![0; half]),
        }
    }

    /// Convert one RGBA frame into `out` (whose geometry selects the size) and
    /// replicate the coded padding.
    pub fn convert(
        &mut self,
        rgba: &[u8],
        orientation: Orientation,
        out: &mut Picture,
    ) -> Result<()> {
        let g = out.geometry;
        if rgba.len() < g.rgba_len() {
            return Err(Error::InputLength {
                expected: g.rgba_len(),
                actual: rgba.len(),
            });
        }
        let half = g.width / 2;
        if self.cb_rows[0].len() != half {
            *self = Converter::new(g);
        }
        let row_bytes = g.width * 4;
        let source_row = |r: usize| {
            let r = match orientation {
                Orientation::TopDown => r,
                Orientation::BottomUp => g.height - 1 - r,
            };
            &rgba[r * row_bytes..(r + 1) * row_bytes]
        };
        let luma_stride = g.luma_stride();
        let chroma_stride = g.chroma_stride();
        let mut computed = 0;
        for k in 0..g.height / 2 {
            let last = (2 * k + 2).min(g.height - 1);
            while computed <= last {
                let slot = computed & 3;
                let y_row = &mut out.y[computed * luma_stride..computed * luma_stride + g.width];
                luma_and_pairs(
                    source_row(computed),
                    y_row,
                    &mut self.cb_rows[slot],
                    &mut self.cr_rows[slot],
                );
                computed += 1;
            }
            let rows = [(2 * k).saturating_sub(1), 2 * k, 2 * k + 1, last];
            let start = k * chroma_stride;
            decimate(&self.cb_rows, rows, &mut out.cb[start..start + half]);
            decimate(&self.cr_rows, rows, &mut out.cr[start..start + half]);
        }
        out.pad_edges();
        Ok(())
    }
}

/// Convenience wrapper allocating its own scratch.
pub fn rgba_to_yuv420(rgba: &[u8], orientation: Orientation, out: &mut Picture) -> Result<()> {
    Converter::new(out.geometry).convert(rgba, orientation, out)
}

/// Luma of a row and the pair-summed chroma terms, one pass over pixel pairs
/// (indexed byte loads become de-interleaving vector loads).
fn luma_and_pairs(rgba: &[u8], y: &mut [u8], cb: &mut [i32], cr: &mut [i32]) {
    let m = (y.len() / 2).min(cb.len()).min(cr.len());
    let rgba = &rgba[..8 * m];
    let (y, cb, cr) = (&mut y[..2 * m], &mut cb[..m], &mut cr[..m]);
    for i in 0..m {
        let (r0, g0, b0) = (
            rgba[8 * i] as i32,
            rgba[8 * i + 1] as i32,
            rgba[8 * i + 2] as i32,
        );
        let (r1, g1, b1) = (
            rgba[8 * i + 4] as i32,
            rgba[8 * i + 5] as i32,
            rgba[8 * i + 6] as i32,
        );
        y[2 * i] = ((Y_R * r0 + Y_G * g0 + Y_B * b0 + Y_OFFSET) >> 16) as u8;
        y[2 * i + 1] = ((Y_R * r1 + Y_G * g1 + Y_B * b1 + Y_OFFSET) >> 16) as u8;
        let (r, g, b) = (r0 + r1, g0 + g1, b0 + b1);
        cb[i] = CB_R * r + CB_G * g + CB_B * b;
        cr[i] = CR_R * r + CR_G * g + CR_B * b;
    }
}

fn decimate(ring: &[Vec<i32>; 4], rows: [usize; 4], out: &mut [u8]) {
    let (a, b, c, d) = (
        &ring[rows[0] & 3],
        &ring[rows[1] & 3],
        &ring[rows[2] & 3],
        &ring[rows[3] & 3],
    );
    for ((((out, &a), &b), &c), &d) in out.iter_mut().zip(a).zip(b).zip(c).zip(d) {
        let sum = a + 3 * (b + c) + d;
        *out = ((sum + C_OFFSET) >> C_SHIFT).clamp(0, 255) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coefficients_follow_bt601() {
        let (kr, kb) = (0.299f64, 0.114f64);
        let kg = 1.0 - kr - kb;
        let ys = 219.0 / 255.0 * 65536.0;
        let cs = 224.0 / 255.0 * 65536.0;
        let ideal = [
            (Y_R, kr * ys),
            (Y_G, kg * ys),
            (Y_B, kb * ys),
            (CB_R, -kr / (2.0 * (1.0 - kb)) * cs),
            (CB_G, -kg / (2.0 * (1.0 - kb)) * cs),
            (CB_B, 0.5 * cs),
            (CR_R, 0.5 * cs),
            (CR_G, -kg / (2.0 * (1.0 - kr)) * cs),
            (CR_B, -kb / (2.0 * (1.0 - kr)) * cs),
        ];
        for (actual, wanted) in ideal {
            assert!(
                (actual as f64 - wanted).abs() <= 1.0,
                "{actual} vs {wanted}"
            );
        }
        assert_eq!(CB_R + CB_G + CB_B, 0);
        assert_eq!(CR_R + CR_G + CR_B, 0);
    }

    fn solid(r: u8, g: u8, b: u8, a: u8) -> (u8, u8, u8) {
        let geometry = Geometry::new(16, 8).unwrap();
        let rgba: Vec<u8> = [r, g, b, a].repeat(16 * 8);
        let mut picture = Picture::new(geometry);
        rgba_to_yuv420(&rgba, Orientation::TopDown, &mut picture).unwrap();
        (picture.y[0], picture.cb[0], picture.cr[0])
    }

    #[test]
    fn primaries_and_extremes() {
        // Values agree with the reference observation recorded in contract.json.
        assert_eq!(solid(0, 0, 0, 255), (16, 128, 128));
        assert_eq!(solid(255, 255, 255, 255), (235, 128, 128));
        assert_eq!(solid(255, 0, 0, 255), (81, 90, 240));
        assert_eq!(solid(0, 255, 0, 255), (145, 54, 34));
        assert_eq!(solid(0, 0, 255, 255), (41, 240, 110));
        assert_eq!(solid(128, 128, 128, 255), (126, 128, 128));
        assert_eq!(solid(255, 0, 0, 0), solid(255, 0, 0, 255));
    }

    fn red_row_on_grey(row: usize, orientation: Orientation) -> Picture {
        let geometry = Geometry::new(16, 8).unwrap();
        let mut rgba = Vec::new();
        for y in 0..8 {
            let pixel = if y == row {
                [255, 0, 0, 255]
            } else {
                [128, 128, 128, 255]
            };
            rgba.extend(pixel.repeat(16));
        }
        let mut picture = Picture::new(geometry);
        rgba_to_yuv420(&rgba, orientation, &mut picture).unwrap();
        picture
    }

    #[test]
    fn vertical_taps_are_center_sited() {
        let column = |p: &Picture| (0..4).map(|k| p.cr[k * 8]).collect::<Vec<_>>();
        assert_eq!(
            column(&red_row_on_grey(0, Orientation::TopDown)),
            [184, 128, 128, 128]
        );
        assert_eq!(
            column(&red_row_on_grey(1, Orientation::TopDown)),
            [170, 142, 128, 128]
        );
        assert_eq!(
            column(&red_row_on_grey(2, Orientation::TopDown)),
            [142, 170, 128, 128]
        );
        assert_eq!(
            column(&red_row_on_grey(3, Orientation::TopDown)),
            [128, 170, 142, 128]
        );
        assert_eq!(
            column(&red_row_on_grey(0, Orientation::BottomUp)),
            [128, 128, 128, 184]
        );
        let luma: Vec<u8> = (0..8)
            .map(|y| red_row_on_grey(0, Orientation::BottomUp).y[y * 16])
            .collect();
        assert_eq!(luma, [126, 126, 126, 126, 126, 126, 126, 81]);
    }

    #[test]
    fn short_input_is_rejected() {
        let mut picture = Picture::new(Geometry::new(16, 8).unwrap());
        let err = rgba_to_yuv420(&[0; 16], Orientation::TopDown, &mut picture).unwrap_err();
        assert_eq!(
            err,
            Error::InputLength {
                expected: 512,
                actual: 16
            }
        );
    }

    #[test]
    fn padding_replicates_edges() {
        let geometry = Geometry::new(18, 6).unwrap();
        let mut rgba = vec![0u8; 18 * 6 * 4];
        for (i, px) in rgba.chunks_exact_mut(4).enumerate() {
            px[1] = (i * 7 % 251) as u8;
        }
        let mut picture = Picture::new(geometry);
        rgba_to_yuv420(&rgba, Orientation::TopDown, &mut picture).unwrap();
        let stride = geometry.luma_stride();
        for y in 0..16 {
            let src = y.min(5);
            for x in 18..32 {
                assert_eq!(picture.y[y * stride + x], picture.y[src * stride + 17]);
            }
        }
    }
}
