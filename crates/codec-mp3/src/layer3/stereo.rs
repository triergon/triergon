//! Joint stereo of Layer III: mid/side (ISO/IEC 11172-3 2.4.3.4.9.2) and intensity
//! stereo (2.4.3.4.9.3, and ISO/IEC 13818-3 2.4.3.2 for the low sampling frequencies).

use core::f32::consts::FRAC_1_SQRT_2;
use core::ops::Range;
use std::sync::LazyLock;

use triergon_framing::{MpegAudioHeader, MpegChannelMode, MpegVersion};

use super::bands::Bands;
use super::requantize::pow2_quarters;
use super::scalefactors::Scalefactors;
use super::side_info::{BlockType, GranuleChannel};

/// The intensity-stereo scheme of a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Intensity {
    /// MPEG-1: positions 0 to 6 set `tan(is_pos π / 12)`; a band at position 7 (or
    /// above, which a 4-bit scalefactor can code) is not intensity coded.
    Mpeg1,
    /// Low sampling frequencies: positions are steps of `2^(-1/4)`, or of `2^(-1/2)` with
    /// `scale` (`intensity_scale`). Every position is intensity coded: FFmpeg does not
    /// treat the largest value of a field as an illegal position.
    Lsf { scale: bool },
}

/// Left and right gains of MPEG-1 positions 0 to 6.
static MPEG1_GAINS: LazyLock<[(f32, f32); 7]> = LazyLock::new(|| {
    core::array::from_fn(|position| {
        if position == 6 {
            return (1.0, 0.0);
        }
        let ratio = (position as f64 * core::f64::consts::PI / 12.0).tan();
        ((ratio / (1.0 + ratio)) as f32, (1.0 / (1.0 + ratio)) as f32)
    })
});

impl Intensity {
    /// Left and right gains of position `position`; `None` when it is not intensity coded.
    fn gains(self, position: u8) -> Option<(f32, f32)> {
        match self {
            Intensity::Mpeg1 => MPEG1_GAINS.get(usize::from(position)).copied(),
            Intensity::Lsf { scale } => {
                let step = if scale { 2 } else { 1 };
                // 2^(-step n / 4), n at most 8 for a 4-bit position.
                let gain = |n: u8| pow2_quarters(-step * i32::from(n));
                Some(match position {
                    0 => (1.0, 1.0),
                    p if p % 2 == 1 => (gain(p.div_ceil(2)), 1.0),
                    p => (1.0, gain(p / 2)),
                })
            }
        }
    }
}

/// The joint-stereo tools a frame's mode extension switches on.
#[derive(Clone, Copy, Debug)]
pub(crate) struct JointStereo {
    pub(crate) mid_side: bool,
    pub(crate) intensity: Option<Intensity>,
}

impl JointStereo {
    /// The tools of a joint-stereo frame; `None` for the other modes. The intensity
    /// scale of the low sampling frequencies comes with the right channel's
    /// scalefactors and is set later.
    pub(crate) fn of_frame(header: &MpegAudioHeader) -> Option<JointStereo> {
        (header.channel_mode == MpegChannelMode::JointStereo).then(|| JointStereo {
            mid_side: header.mode_extension & 2 != 0,
            intensity: (header.mode_extension & 1 != 0).then_some(
                if header.version == MpegVersion::Mpeg1 {
                    Intensity::Mpeg1
                } else {
                    Intensity::Lsf { scale: false }
                },
            ),
        })
    }

    /// Undo the joint coding of one granule. The right channel's block type, scalefactors
    /// and zero region place the intensity bands.
    pub(crate) fn apply(
        self,
        left: &mut [f32; 576],
        right: &mut [f32; 576],
        right_info: &GranuleChannel,
        right_sf: &Scalefactors,
        bands: &Bands,
    ) {
        let Some(intensity) = self.intensity else {
            if self.mid_side {
                mid_side(left, right, 0..576);
            }
            return;
        };
        let mut stereo = Stereo {
            left,
            right,
            mid_side: self.mid_side,
            intensity,
        };
        if right_info.block_type == BlockType::Short {
            stereo.short(right_sf, bands, right_info.mixed);
        } else {
            let bound = last_nonzero(stereo.right, 0..576);
            stereo.long(right_sf, bands, 0..576, bound);
        }
    }
}

struct Stereo<'a> {
    left: &'a mut [f32; 576],
    right: &'a mut [f32; 576],
    mid_side: bool,
    intensity: Intensity,
}

impl Stereo<'_> {
    /// Lines `range` of long bands, intensity coded from the first band at or above line
    /// `bound`. Bands beyond the last scalefactor take the position of band 20.
    fn long(&mut self, sf: &Scalefactors, bands: &Bands, range: Range<usize>, bound: usize) {
        let first = (0..22)
            .find(|&b| usize::from(bands.long[b]) >= bound)
            .unwrap_or(22);
        let start = usize::from(bands.long[first]).max(range.start);
        self.plain(range.start..start.min(range.end));
        for b in first..22 {
            let lines = bands.long_band(b);
            if lines.start >= range.end {
                break;
            }
            self.band(lines, sf.long[b.min(20)]);
        }
    }

    /// A short (or mixed) block: each window is intensity coded above its own last
    /// nonzero band. A mixed block's long part is intensity coded only when the whole
    /// short part of the right channel is zero.
    fn short(&mut self, sf: &Scalefactors, bands: &Bands, mixed: bool) {
        let long_lines = if mixed { 36 } else { 0 };
        let first_short = (0..13)
            .find(|&b| 3 * usize::from(bands.short[b]) >= long_lines)
            .unwrap_or(13);
        let mut short_part_zero = true;
        for w in 0..3 {
            let window_lines = |b: usize| {
                let band = bands.short_band(b);
                let start = 3 * band.start + w * band.len();
                start..start + band.len()
            };
            let first = (first_short..13)
                .rev()
                .find(|&b| last_nonzero(self.right, window_lines(b)) > 0)
                .map_or(first_short, |b| {
                    short_part_zero = false;
                    b + 1
                });
            for b in first_short..first {
                self.plain(window_lines(b));
            }
            for b in first..13 {
                self.band(window_lines(b), sf.short[b.min(11)][w]);
            }
        }
        if mixed {
            if short_part_zero {
                let bound = last_nonzero(self.right, 0..long_lines);
                self.long(sf, bands, 0..long_lines, bound);
            } else {
                self.plain(0..long_lines);
            }
        }
    }

    /// One band at intensity position `position`.
    fn band(&mut self, lines: Range<usize>, position: u8) {
        match self.intensity.gains(position) {
            Some((gain_left, gain_right)) => {
                for (l, r) in self.left[lines.clone()]
                    .iter_mut()
                    .zip(&mut self.right[lines])
                {
                    let value = *l;
                    *l = value * gain_left;
                    *r = value * gain_right;
                }
            }
            None => self.plain(lines),
        }
    }

    /// Lines that are not intensity coded.
    fn plain(&mut self, lines: Range<usize>) {
        if self.mid_side {
            mid_side(self.left, self.right, lines);
        }
    }
}

fn mid_side(left: &mut [f32; 576], right: &mut [f32; 576], lines: Range<usize>) {
    for (l, r) in left[lines.clone()].iter_mut().zip(&mut right[lines]) {
        let (mid, side) = (*l, *r);
        *l = (mid + side) * FRAC_1_SQRT_2;
        *r = (mid - side) * FRAC_1_SQRT_2;
    }
}

/// One past the last nonzero line of `lines`, as a line number; 0 when all are zero.
fn last_nonzero(values: &[f32; 576], lines: Range<usize>) -> usize {
    let start = lines.start;
    values[lines]
        .iter()
        .rposition(|&v| v != 0.0)
        .map_or(0, |p| start + p + 1)
}
