//! RGBA → YUV 4:2:0, ITU-R BT.601 matrix, limited ("studio") range, alpha ignored.
//!
//! The conversion follows FFmpeg 9.0.2's `swscale` (`SWS_BILINEAR`, RGBA to NV12)
//! as built for 32-bit ARMv7, whose result depends on the width:
//!
//! - **Block path**, widths of at least 16 that are a multiple of 4: luma
//!   `(66 R + 129 G + 25 B + 4224) >> 8`; chroma from the 2×2 block each sample
//!   covers, every channel averaged with rounding (`(sum + 2) >> 2`), then
//!   `Cb = (−19 R − 37 G + 56 B + 16448) >> 7` and
//!   `Cr = (56 R − 47 G − 9 B + 16448) >> 7`.
//! - **Filtered path**, every other width: chroma is averaged over each pair
//!   of pixels before applying the centre-sited vertical taps 1/8, 3/8, 3/8,
//!   1/8. Two-, four- and six-row frames have their own edge taps. Widths
//!   below 16 share this result on both targets. At widths of 2 modulo 4
//!   from 18 up, the 32-bit reference faults and conversion uses the filter.
//!
//! Converter::filtered uses the filtered conversion at every width, as the
//! AArch64 reference does.

use crate::{Error, Geometry, Picture, Result};

/// Row order of the caller's RGBA buffer. Bottom-up buffers are read in reverse
/// without a separate flip pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    TopDown,
    BottomUp,
}

/// Whether `geometry` takes the block path (see the module documentation).
pub(crate) const fn block_path(geometry: Geometry) -> bool {
    geometry.width >= 16 && geometry.width.is_multiple_of(4)
}

/// Reusable conversion scratch, sized for one geometry.
#[derive(Debug)]
pub struct Converter {
    scratch: Scratch,
    filtered_only: bool,
}

#[derive(Debug)]
enum Scratch {
    /// Per-column channel sums (R, G, B, A) of the current row pair.
    Block { width: usize, sums: [Vec<u16>; 4] },
    /// Four source rows of pair-summed chroma terms.
    Filtered {
        half: usize,
        cb: [Vec<i16>; 4],
        cr: [Vec<i16>; 4],
    },
}

impl Scratch {
    fn new(geometry: Geometry, use_blocks: bool) -> Scratch {
        let width = geometry.width;
        if use_blocks {
            Scratch::Block {
                width,
                sums: std::array::from_fn(|_| vec![0; width]),
            }
        } else {
            let half = width / 2;
            Scratch::Filtered {
                half,
                cb: std::array::from_fn(|_| vec![0; half]),
                cr: std::array::from_fn(|_| vec![0; half]),
            }
        }
    }

    fn fits(&self, geometry: Geometry, use_blocks: bool) -> bool {
        match self {
            Scratch::Block { width, .. } => use_blocks && *width == geometry.width,
            Scratch::Filtered { half, .. } => !use_blocks && *half == geometry.width / 2,
        }
    }
}

impl Converter {
    pub fn new(geometry: Geometry) -> Converter {
        Converter {
            scratch: Scratch::new(geometry, block_path(geometry)),
            filtered_only: false,
        }
    }

    /// Use pair averaging and centre-sited vertical filtering at every width.
    pub fn filtered(geometry: Geometry) -> Converter {
        Converter {
            scratch: Scratch::new(geometry, false),
            filtered_only: true,
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
        let use_blocks = !self.filtered_only && block_path(g);
        if !self.scratch.fits(g, use_blocks) {
            self.scratch = Scratch::new(g, use_blocks);
        }
        let row_bytes = g.width * 4;
        let source_row = |r: usize| {
            let r = match orientation {
                Orientation::TopDown => r,
                Orientation::BottomUp => g.height - 1 - r,
            };
            &rgba[r * row_bytes..(r + 1) * row_bytes]
        };
        match &mut self.scratch {
            Scratch::Block { sums, .. } => block::convert(source_row, sums, out),
            Scratch::Filtered { cb, cr, .. } => filtered::convert(source_row, cb, cr, out),
        }
        out.pad_edges();
        Ok(())
    }
}

/// Convenience wrapper allocating its own scratch.
pub fn rgba_to_yuv420(rgba: &[u8], orientation: Orientation, out: &mut Picture) -> Result<()> {
    Converter::new(out.geometry).convert(rgba, orientation, out)
}

/// The block path in 16-bit arithmetic: every intermediate below lies in
/// 0..2^16, so the loops vectorise into 16-bit lanes.
mod block {
    use crate::Picture;

    const Y: [u16; 3] = [66, 129, 25];
    const Y_OFFSET: u16 = (16 << 8) + (1 << 7);
    const C_OFFSET: u16 = (128 << 7) + (1 << 6);

    pub(super) fn convert<'a>(
        source_row: impl Fn(usize) -> &'a [u8],
        sums: &mut [Vec<u16>; 4],
        out: &mut Picture,
    ) {
        let g = out.geometry;
        let (luma_stride, chroma_stride) = (g.luma_stride(), g.chroma_stride());
        let half = g.width / 2;
        for k in 0..g.height / 2 {
            let (top, bottom) = out.y[2 * k * luma_stride..].split_at_mut(luma_stride);
            luma_and_sums(
                source_row(2 * k),
                source_row(2 * k + 1),
                &mut top[..g.width],
                &mut bottom[..g.width],
                sums,
            );
            let start = k * chroma_stride;
            chroma(
                sums,
                &mut out.cb[start..start + half],
                &mut out.cr[start..start + half],
            );
        }
    }

    /// Luma of two rows and their per-column channel sums. Alpha is summed
    /// although nothing reads it: with every byte of a pixel used, the loop
    /// reads pixels with de-interleaving vector loads (`vld4`/`ld4`) instead of
    /// assembling vectors lane by lane.
    fn luma_and_sums(
        top: &[u8],
        bottom: &[u8],
        y_top: &mut [u8],
        y_bottom: &mut [u8],
        [r, g, b, a]: &mut [Vec<u16>; 4],
    ) {
        let n = y_top.len();
        let (top, bottom) = (&top[..4 * n], &bottom[..4 * n]);
        let (y_bottom, r, g, b, a) = (
            &mut y_bottom[..n],
            &mut r[..n],
            &mut g[..n],
            &mut b[..n],
            &mut a[..n],
        );
        for i in 0..n {
            let t = [top[4 * i], top[4 * i + 1], top[4 * i + 2], top[4 * i + 3]].map(u16::from);
            let u = [
                bottom[4 * i],
                bottom[4 * i + 1],
                bottom[4 * i + 2],
                bottom[4 * i + 3],
            ]
            .map(u16::from);
            y_top[i] = ((Y[0] * t[0] + Y[1] * t[1] + Y[2] * t[2] + Y_OFFSET) >> 8) as u8;
            y_bottom[i] = ((Y[0] * u[0] + Y[1] * u[1] + Y[2] * u[2] + Y_OFFSET) >> 8) as u8;
            r[i] = t[0] + u[0];
            g[i] = t[1] + u[1];
            b[i] = t[2] + u[2];
            a[i] = t[3] + u[3];
        }
    }

    /// Chroma of each 2×2 block from the column sums: Cb and Cr stay within
    /// 2168..=30728 before the shift, so no clamp is needed.
    fn chroma([r, g, b, _]: &[Vec<u16>; 4], cb: &mut [u8], cr: &mut [u8]) {
        let n = cb.len();
        let (r, g, b, cr) = (&r[..2 * n], &g[..2 * n], &b[..2 * n], &mut cr[..n]);
        for i in 0..n {
            let mr = (r[2 * i] + r[2 * i + 1] + 2) >> 2;
            let mg = (g[2 * i] + g[2 * i + 1] + 2) >> 2;
            let mb = (b[2 * i] + b[2 * i + 1] + 2) >> 2;
            cb[i] = ((C_OFFSET + 56 * mb - (19 * mr + 37 * mg)) >> 7) as u8;
            cr[i] = ((C_OFFSET + 56 * mr - (47 * mg + 9 * mb)) >> 7) as u8;
        }
    }
}

/// The filtered conversion quantizes the limited-range BT.601 components to
/// six fractional bits before vertical filtering. The chroma coefficients
/// were recovered from the reference's observed output. The capture worker's
/// short_filtered_frames_against_reference test checks the coefficients,
/// rounding and edge taps against that output.
mod filtered {
    use crate::Picture;

    pub(super) const Y_R: i32 = 8414;
    pub(super) const Y_G: i32 = 16519;
    pub(super) const Y_B: i32 = 3208;
    // The extra 1 << 8 was recovered by fitting the fixed-point formula to the
    // reference's observed luma bytes in the short_filtered_frames_against_reference probe.
    // RGB (0, 85, 170) gives 76 in the probe and 75 with only the half-unit bias.
    const Y_OFFSET: i32 = (16 << 15) + (1 << 14) + (1 << 8);

    pub(super) fn convert<'a>(
        source_row: impl Fn(usize) -> &'a [u8],
        cb_rows: &mut [Vec<i16>; 4],
        cr_rows: &mut [Vec<i16>; 4],
        out: &mut Picture,
    ) {
        let g = out.geometry;
        let half = g.width / 2;
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
                    &mut cb_rows[slot],
                    &mut cr_rows[slot],
                );
                computed += 1;
            }
            let rows = [(2 * k).saturating_sub(1), 2 * k, 2 * k + 1, last];
            let start = k * chroma_stride;
            let decimate_plane = |ring: &[Vec<i16>; 4], plane: &mut [u8]| match (g.height, k) {
                (2, _) => decimate_short::<1, 0, 0, 6>(ring, 1, plane),
                (4, _) => decimate_short::<1, 1, 0, 7>(ring, 2 * k, plane),
                // The short-frame probe gives weights 3/7, 3/7 and 1/7.
                // Their 12-bit fixed-point coefficients sum to 4096.
                (6, 0) => decimate_short::<1755, 1756, 585, 18>(ring, 0, plane),
                _ => decimate(ring, rows, plane),
            };
            decimate_plane(cb_rows, &mut out.cb[start..start + half]);
            decimate_plane(cr_rows, &mut out.cr[start..start + half]);
        }
    }

    /// Luma of a row and the pair-summed chroma terms, one pass over pixel pairs
    /// (indexed byte loads become de-interleaving vector loads).
    fn luma_and_pairs(rgba: &[u8], y: &mut [u8], cb: &mut [i16], cr: &mut [i16]) {
        let m = (y.len() / 2).min(cb.len()).min(cr.len());
        let rgba = &rgba[..8 * m];
        let (y, cb, cr) = (&mut y[..2 * m], &mut cb[..m], &mut cr[..m]);
        for i in 0..m {
            let (r0, g0, b0) = (
                u16::from(rgba[8 * i]),
                u16::from(rgba[8 * i + 1]),
                u16::from(rgba[8 * i + 2]),
            );
            let (r1, g1, b1) = (
                u16::from(rgba[8 * i + 4]),
                u16::from(rgba[8 * i + 5]),
                u16::from(rgba[8 * i + 6]),
            );
            y[2 * i] = luma(r0, g0, b0);
            y[2 * i + 1] = luma(r1, g1, b1);
            (cb[i], cr[i]) = chroma((r0 + r1) as i16, (g0 + g1) as i16, (b0 + b1) as i16);
        }
    }

    // Split at seven bits, rounding the coarse coefficients to keep the
    // residual small.
    const Y_COARSE: [i32; 3] = [(Y_R + 64) >> 7, (Y_G + 64) >> 7, (Y_B + 64) >> 7];
    const Y_RESIDUAL: [i32; 3] = [
        Y_R - 128 * Y_COARSE[0],
        Y_G - 128 * Y_COARSE[1],
        Y_B - 128 * Y_COARSE[2],
    ];

    const _: () = {
        let channel_max = u8::MAX as i32;
        let offset = Y_OFFSET >> 7;
        assert!(Y_OFFSET % 128 == 0 && offset >= 0);
        assert!(Y_COARSE[0] >= 0 && Y_COARSE[1] >= 0 && Y_COARSE[2] >= 0);
        let coarse_max = (Y_COARSE[0] + Y_COARSE[1] + Y_COARSE[2]) * channel_max + offset;
        assert!(coarse_max <= u16::MAX as i32);

        assert!(Y_RESIDUAL[0] <= 0 && Y_RESIDUAL[1] >= 0 && Y_RESIDUAL[2] >= 0);
        let residual_min = Y_RESIDUAL[0] * channel_max;
        let residual_max = (Y_RESIDUAL[1] + Y_RESIDUAL[2]) * channel_max;
        assert!(residual_min >= i16::MIN as i32 && residual_max <= i16::MAX as i32);

        // The original positive coefficients give the corrected sum's
        // extrema at black and white, before the final shift.
        assert!(Y_R >= 0 && Y_G >= 0 && Y_B >= 0);
        let corrected_max = ((Y_R + Y_G + Y_B) * channel_max + Y_OFFSET) >> 7;
        assert!(corrected_max <= u16::MAX as i32);
    };

    #[inline]
    fn luma(r: u16, g: u16, b: u16) -> u8 {
        let coarse = Y_COARSE[0] as u16 * r
            + Y_COARSE[1] as u16 * g
            + Y_COARSE[2] as u16 * b
            + (Y_OFFSET >> 7) as u16;
        let residual = Y_RESIDUAL[0] as i16 * r as i16
            + Y_RESIDUAL[1] as i16 * g as i16
            + Y_RESIDUAL[2] as i16 * b as i16;
        // Shifting the residual first preserves the floor. The signed
        // correction leaves the sum within u16.
        let sum = (i32::from(coarse) + i32::from(residual >> 7)) as u16;
        (sum >> 8) as u8
    }

    const CB_R: i32 = -4865;
    const CB_G: i32 = -9528;
    const CB_B: i32 = 14392;
    const CR_R: i32 = 14392;
    const CR_G: i32 = -12061;
    const CR_B: i32 = -2332;

    const C_SCALE: i32 = CB_B / 256;
    const CB_RED: i32 = -CB_R / 256;
    const CR_BLUE: i32 = -CR_B / 256;
    const CR_BLUE_RESIDUAL: i32 = -CR_B % 256;
    const C_ROUND: i16 = 512 >> 8;

    const _: () = {
        // Divide the coefficients at eight bits. Expanding the colour
        // differences recovers all six original coefficients.
        let shared = C_SCALE * 257;
        assert!(CB_B == shared && CR_R == shared);
        assert!(CB_R == -256 * CB_RED - 1);
        assert!(CB_G == 256 * CB_RED - shared);
        assert!(CR_B == -256 * CR_BLUE - CR_BLUE_RESIDUAL);
        assert!(CR_G == 256 * CR_BLUE + CR_BLUE_RESIDUAL - shared - 1);

        // The shared green component bounds each difference of scaled
        // differences by the dominant product. The extra -r or -g extends
        // only the negative end of the residual range.
        assert!(0 <= CB_RED && CB_RED <= C_SCALE);
        assert!(0 <= CR_BLUE && CR_BLUE <= C_SCALE);
        assert!(0 <= CR_BLUE_RESIDUAL && CR_BLUE_RESIDUAL <= C_SCALE);
        let pair_max = 2 * u8::MAX as i32;
        let scaled_max = C_SCALE * pair_max;
        let residual_min = -scaled_max - pair_max;
        assert!(residual_min >= i16::MIN as i32 && scaled_max <= i16::MAX as i32);

        let sum_min = -scaled_max + C_ROUND as i32 + (residual_min >> 8);
        let sum_max = scaled_max + C_ROUND as i32 + (scaled_max >> 8);
        assert!(sum_min >= i16::MIN as i32 && sum_max <= i16::MAX as i32);
    };

    #[inline]
    fn chroma(r: i16, g: i16, b: i16) -> (i16, i16) {
        let red = r - g;
        let blue = b - g;
        let red_scaled = C_SCALE as i16 * red;
        let blue_scaled = C_SCALE as i16 * blue;
        let cb = (blue_scaled - CB_RED as i16 * red + C_ROUND + ((blue_scaled - r) >> 8)) >> 2;
        let cr = (red_scaled - CR_BLUE as i16 * blue
            + C_ROUND
            + ((red_scaled - CR_BLUE_RESIDUAL as i16 * blue - g) >> 8))
            >> 2;
        (cb, cr)
    }

    fn decimate(ring: &[Vec<i16>; 4], rows: [usize; 4], out: &mut [u8]) {
        let [a, b, c, d] = rows.map(|row| ring[row & 3].as_slice());
        for ((((out, &a), &b), &c), &d) in out.iter_mut().zip(a).zip(b).zip(c).zip(d) {
            let sum = i32::from(a) + 3 * (i32::from(b) + i32::from(c)) + i32::from(d);
            *out = quantize::<9>(sum);
        }
    }

    fn decimate_short<const A: i32, const B: i32, const C: i32, const SHIFT: u32>(
        ring: &[Vec<i16>; 4],
        first: usize,
        out: &mut [u8],
    ) {
        let [a, b, c] = std::array::from_fn(|i| &ring[(first + i) & 3]);
        for (((out, &a), &b), &c) in out.iter_mut().zip(a).zip(b).zip(c) {
            let sum = A * i32::from(a) + B * i32::from(b) + C * i32::from(c);
            *out = quantize::<SHIFT>(sum);
        }
    }

    fn quantize<const SHIFT: u32>(value: i32) -> u8 {
        let offset = (128 << SHIFT) + (1 << (SHIFT - 1));
        ((value + offset) >> SHIFT).clamp(0, 255) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::filtered::{Y_B, Y_G, Y_R};
    use super::*;

    #[test]
    fn luma_coefficients_follow_bt601() {
        let (kr, kb) = (0.299f64, 0.114f64);
        let kg = 1.0 - kr - kb;
        let scale = 219.0 / 255.0 * 32768.0;
        for (actual, coefficient) in [(Y_R, kr), (Y_G, kg), (Y_B, kb)] {
            assert_eq!(actual, (scale * coefficient).round() as i32);
        }
    }

    #[test]
    fn path_follows_width() {
        let path = |w| block_path(Geometry::new(w, 8).unwrap());
        assert!(!path(2) && !path(14) && !path(18) && !path(962) && !path(4094));
        assert!(path(16) && path(20) && path(476) && path(480) && path(960) && path(4092));
    }

    fn solid(width: u32, (r, g, b, a): (u8, u8, u8, u8)) -> (u8, u8, u8) {
        let geometry = Geometry::new(width, 8).unwrap();
        let rgba: Vec<u8> = [r, g, b, a].repeat(width as usize * 8);
        let mut picture = Picture::new(geometry);
        rgba_to_yuv420(&rgba, Orientation::TopDown, &mut picture).unwrap();
        (picture.y[0], picture.cb[0], picture.cr[0])
    }

    #[test]
    fn filtered_luma_matches_the_observed_rounding_boundary() {
        let [r, g, b, a] = [0, 85, 170, 255];
        let geometry = Geometry::new(16, 8).unwrap();
        let rgba = [r, g, b, a].repeat(16 * 8);
        let mut picture = Picture::new(geometry);
        Converter::filtered(geometry)
            .convert(&rgba, Orientation::TopDown, &mut picture)
            .unwrap();

        assert_eq!(solid(16, (r, g, b, a)).0, 75);
        assert_eq!((solid(14, (r, g, b, a)).0, picture.y[0]), (76, 76));
    }

    #[test]
    fn primaries_and_extremes() {
        // (width 16: block path, width 14: filtered path), each as FFmpeg 9.0.2
        // built for ARMv7 converts it.
        let cases = [
            ((0, 0, 0, 255), (16, 128, 128), (16, 128, 128)),
            ((255, 255, 255, 255), (235, 128, 128), (235, 128, 128)),
            ((255, 0, 0, 255), (82, 90, 240), (81, 90, 240)),
            ((0, 255, 0, 255), (144, 54, 34), (145, 54, 34)),
            ((0, 0, 255, 255), (41, 240, 110), (41, 240, 110)),
            ((128, 128, 128, 255), (126, 128, 128), (126, 128, 128)),
        ];
        for (rgba, block, filtered) in cases {
            assert_eq!(solid(16, rgba), block, "{rgba:?}");
            assert_eq!(solid(14, rgba), filtered, "{rgba:?}");
        }
        for width in [14, 16] {
            assert_eq!(solid(width, (255, 0, 0, 0)), solid(width, (255, 0, 0, 255)));
        }
    }

    #[test]
    fn block_chroma_rounds_each_channel_mean() {
        // 2x2 blocks on grey whose results tell the rounded channel mean from the
        // floored one and from one rounding of the summed products; luma and
        // chroma as observed from FFmpeg's ARMv7 build.
        // (RGB of the block's four pixels in row order, their luma, Cb, Cr)
        type Case = ([[u8; 3]; 4], [u8; 4], u8, u8);
        let cases: [Case; 3] = [
            (
                [[171, 206, 5], [206, 120, 131], [161, 73, 250], [13, 71, 98]],
                [164, 142, 119, 65],
                126,
                137,
            ),
            (
                [
                    [216, 97, 7],
                    [182, 27, 156],
                    [43, 240, 173],
                    [253, 157, 185],
                ],
                [121, 92, 165, 178],
                121,
                147,
            ),
            (
                [[212, 210, 2], [40, 177, 103], [87, 18, 48], [219, 213, 212]],
                [177, 126, 52, 200],
                102,
                126,
            ),
        ];
        let geometry = Geometry::new(16, 2).unwrap();
        for (block, luma, cb, cr) in cases {
            let mut rgba = [128u8, 128, 128, 255].repeat(32);
            for (i, rgb) in block.iter().enumerate() {
                let at = (i / 2) * 64 + (i % 2) * 4;
                rgba[at..at + 3].copy_from_slice(rgb);
            }
            let mut picture = Picture::new(geometry);
            rgba_to_yuv420(&rgba, Orientation::TopDown, &mut picture).unwrap();
            let stride = geometry.luma_stride();
            let y = [
                picture.y[0],
                picture.y[1],
                picture.y[stride],
                picture.y[stride + 1],
            ];
            assert_eq!((y, picture.cb[0], picture.cr[0]), (luma, cb, cr));
        }
    }

    fn red_row_on_grey(width: u32, height: u32, row: usize, orientation: Orientation) -> Picture {
        let geometry = Geometry::new(width, height).unwrap();
        let mut rgba = Vec::new();
        for y in 0..height as usize {
            let pixel = if y == row {
                [255, 0, 0, 255]
            } else {
                [128, 128, 128, 255]
            };
            rgba.extend(pixel.repeat(width as usize));
        }
        let mut picture = Picture::new(geometry);
        rgba_to_yuv420(&rgba, orientation, &mut picture).unwrap();
        picture
    }

    #[test]
    fn filtered_vertical_taps_are_center_sited() {
        let column = |p: &Picture| (0..4).map(|k| p.cr[k * 8]).collect::<Vec<_>>();
        let red = |row, orientation| red_row_on_grey(14, 8, row, orientation);
        assert_eq!(column(&red(0, Orientation::TopDown)), [184, 128, 128, 128]);
        assert_eq!(column(&red(1, Orientation::TopDown)), [170, 142, 128, 128]);
        assert_eq!(column(&red(2, Orientation::TopDown)), [142, 170, 128, 128]);
        assert_eq!(column(&red(3, Orientation::TopDown)), [128, 170, 142, 128]);
        assert_eq!(column(&red(0, Orientation::BottomUp)), [128, 128, 128, 184]);
        let luma: Vec<u8> = (0..8)
            .map(|y| red(0, Orientation::BottomUp).y[y * 16])
            .collect();
        assert_eq!(luma, [126, 126, 126, 126, 126, 126, 126, 81]);
    }

    #[test]
    fn short_filtered_frames_use_the_observed_chroma_taps() {
        let observed: [&[&[u8]]; 3] = [
            &[&[128], &[240]],
            &[&[184, 128], &[184, 128], &[128, 184], &[128, 184]],
            &[
                &[176, 128, 128],
                &[176, 142, 128],
                &[144, 170, 128],
                &[128, 170, 142],
                &[128, 142, 170],
                &[128, 128, 184],
            ],
        ];
        for (height, impulses) in (2..=6).step_by(2).zip(observed) {
            for (row, expected) in impulses.iter().enumerate() {
                for orientation in [Orientation::TopDown, Orientation::BottomUp] {
                    let source_row = match orientation {
                        Orientation::TopDown => row,
                        Orientation::BottomUp => height as usize - 1 - row,
                    };
                    let picture = red_row_on_grey(14, height, source_row, orientation);
                    let stride = picture.geometry.chroma_stride();
                    let actual: Vec<_> = picture
                        .cr
                        .chunks_exact(stride)
                        .take(height as usize / 2)
                        .map(|chroma_row| chroma_row[0])
                        .collect();
                    assert_eq!(
                        actual, *expected,
                        "height {height}, row {row}, {orientation:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn block_chroma_covers_its_row_pair() {
        let column = |p: &Picture| (0..4).map(|k| p.cr[k * 8]).collect::<Vec<_>>();
        let red = |row, orientation| red_row_on_grey(16, 8, row, orientation);
        assert_eq!(column(&red(0, Orientation::TopDown)), [184, 128, 128, 128]);
        assert_eq!(column(&red(1, Orientation::TopDown)), [184, 128, 128, 128]);
        assert_eq!(column(&red(2, Orientation::TopDown)), [128, 184, 128, 128]);
        assert_eq!(column(&red(7, Orientation::TopDown)), [128, 128, 128, 184]);
        assert_eq!(column(&red(0, Orientation::BottomUp)), [128, 128, 128, 184]);
        let luma: Vec<u8> = (0..8)
            .map(|y| red(0, Orientation::BottomUp).y[y * 16])
            .collect();
        assert_eq!(luma, [126, 126, 126, 126, 126, 126, 126, 82]);
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
        for width in [18, 20] {
            let geometry = Geometry::new(width, 6).unwrap();
            let w = width as usize;
            let mut rgba = vec![0u8; w * 6 * 4];
            for (i, px) in rgba.chunks_exact_mut(4).enumerate() {
                px[1] = (i * 7 % 251) as u8;
            }
            let mut picture = Picture::new(geometry);
            rgba_to_yuv420(&rgba, Orientation::TopDown, &mut picture).unwrap();
            let stride = geometry.luma_stride();
            for y in 0..16 {
                let src = y.min(5);
                for x in w..32 {
                    assert_eq!(picture.y[y * stride + x], picture.y[src * stride + w - 1]);
                }
            }
        }
    }
}
