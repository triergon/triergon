//! Stereo downmix and 16-bit PCM for the device path (`-ac 2` to Pulse/ALSA).
//!
//! The coefficients are the conventional matrix (front pair unity, centre and
//! surrounds at -3 dB, LFE omitted) normalised so no output row can exceed
//! full scale, which is what a fixed-point device format needs. The exact
//! reference behaviour (matrix, normalisation and float-to-s16 rounding) is a
//! native observation frozen in `research/d4/DOWNMIX-CONTRACT.json`; this
//! module implements that contract and its tests pin it.
use triergon_codec::{AudioFrame, ChannelLayout, DownmixLevels, Speaker, SurroundEncoding};

const SQRT_HALF: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Left/right gains for every input plane.
#[derive(Clone, Debug, PartialEq)]
pub struct StereoMatrix {
    pub gains: Vec<[f32; 2]>,
}

impl StereoMatrix {
    pub fn for_layout(speakers: &[Speaker]) -> Self {
        Self::with_levels(speakers, DownmixLevels::default())
    }
    /// Unity front pair; centre at `center`; back centre at `surround`/sqrt(2)
    /// to each side; back/side pairs at `surround` to their side (Pro Logic II:
    /// -sqrt(3/2)/-sqrt(1/2) and +sqrt(1/2)/+sqrt(3/2) times `surround`, Dolby:
    /// -/+ sqrt(1/2) times `surround`); front-centre pair at unity to its side;
    /// LFE (and LFE2) at `lfe`/sqrt(2); top front pair at sqrt(1/2) to its side, every
    /// other height/wide channel dropped (`research/d4/DOWNMIX-ADDENDUM-heights.json`).
    /// Rows are normalised to an absolute sum of at most one.
    pub fn with_levels(speakers: &[Speaker], levels: DownmixLevels) -> Self {
        let DownmixLevels {
            center,
            surround: s,
            lfe,
            encoding,
        } = levels;
        if speakers == [Speaker::FrontCenter] || speakers.len() == 1 {
            return Self {
                gains: vec![[SQRT_HALF, SQRT_HALF]],
            };
        }
        let surround_pair = |left: bool| -> [f32; 2] {
            match (encoding, left) {
                (SurroundEncoding::None, true) => [s, 0.0],
                (SurroundEncoding::None, false) => [0.0, s],
                (SurroundEncoding::Dolby, true) => [-s * SQRT_HALF, s * SQRT_HALF],
                (SurroundEncoding::Dolby, false) => [-s * SQRT_HALF, s * SQRT_HALF],
                (SurroundEncoding::ProLogicII, true) => [-s * 1.5_f32.sqrt(), s * SQRT_HALF],
                (SurroundEncoding::ProLogicII, false) => [-s * SQRT_HALF, s * 1.5_f32.sqrt()],
            }
        };
        let mut gains: Vec<[f32; 2]> = speakers
            .iter()
            .map(|speaker| match speaker {
                Speaker::FrontLeft | Speaker::FrontLeftOfCenter => [1.0, 0.0],
                Speaker::FrontRight | Speaker::FrontRightOfCenter => [0.0, 1.0],
                Speaker::TopFrontLeft => [SQRT_HALF, 0.0],
                Speaker::TopFrontRight => [0.0, SQRT_HALF],
                Speaker::FrontCenter => [center, center],
                Speaker::LowFrequency | Speaker::LowFrequency2 => {
                    [lfe * SQRT_HALF, lfe * SQRT_HALF]
                }
                Speaker::TopCenter
                | Speaker::TopFrontCenter
                | Speaker::TopBackLeft
                | Speaker::TopBackCenter
                | Speaker::TopBackRight
                | Speaker::WideLeft
                | Speaker::WideRight
                | Speaker::SurroundDirectLeft
                | Speaker::SurroundDirectRight
                | Speaker::TopSideLeft
                | Speaker::TopSideRight
                | Speaker::BottomFrontCenter
                | Speaker::BottomFrontLeft
                | Speaker::BottomFrontRight => [0.0, 0.0],
                Speaker::BackCenter => [s * SQRT_HALF, s * SQRT_HALF],
                Speaker::BackLeft | Speaker::SideLeft => surround_pair(true),
                Speaker::BackRight | Speaker::SideRight => surround_pair(false),
            })
            .collect();
        let peak = (0..2)
            .map(|side| gains.iter().map(|g| g[side].abs()).sum::<f32>())
            .fold(0.0_f32, f32::max);
        if peak > 1.0 {
            for gain in &mut gains {
                gain[0] /= peak;
                gain[1] /= peak;
            }
        }
        Self { gains }
    }
    pub fn is_identity_stereo(&self) -> bool {
        self.gains == [[1.0, 0.0], [0.0, 1.0]]
    }
}

/// Stereo downmix of decoded frames; the matrix is rebuilt only when the layout or
/// the stream's levels change.
#[derive(Default)]
pub struct Downmix {
    cached: Option<(ChannelLayout, DownmixLevels, StereoMatrix)>,
}

impl Downmix {
    /// Append `frame` to `out` as interleaved stereo s16.
    pub fn mix(&mut self, frame: &AudioFrame, levels: DownmixLevels, out: &mut Vec<i16>) {
        if !self
            .cached
            .as_ref()
            .is_some_and(|(layout, cached, _)| *layout == frame.layout && *cached == levels)
        {
            let matrix = StereoMatrix::with_levels(frame.layout.speakers(), levels);
            self.cached = Some((frame.layout, levels, matrix));
        }
        let (_, _, matrix) = self.cached.as_ref().expect("matrix");
        mix_into(matrix, &frame.planes, frame.samples_per_channel, out);
    }
}

/// Mix `samples` frames of planar float into interleaved stereo s16, appending.
pub fn mix_into(matrix: &StereoMatrix, planes: &[Vec<f32>], samples: usize, out: &mut Vec<i16>) {
    let start = out.len();
    out.resize(start + 2 * samples, 0);
    let frames = &mut out[start..];
    if matrix.is_identity_stereo() {
        for ((frame, &l), &r) in frames
            .chunks_exact_mut(2)
            .zip(&planes[0][..samples])
            .zip(&planes[1][..samples])
        {
            frame[0] = to_s16(l);
            frame[1] = to_s16(r);
        }
        return;
    }
    for (i, frame) in frames.chunks_exact_mut(2).enumerate() {
        let (mut l, mut r) = (0.0_f32, 0.0_f32);
        for (plane, gain) in planes.iter().zip(&matrix.gains) {
            let v = plane[i];
            l += v * gain[0];
            r += v * gain[1];
        }
        frame[0] = to_s16(l);
        frame[1] = to_s16(r);
    }
}

/// Full scale 1.0 maps to 32768, rounded toward negative infinity and
/// saturated: the reference device path's observed conversion
/// (`research/d4/DOWNMIX-CONTRACT.json`).
#[inline]
pub fn to_s16(sample: f32) -> i16 {
    (sample * 32768.0).floor().clamp(-32768.0, 32767.0) as i16
}

#[cfg(test)]
mod tests {
    use super::*;
    use Speaker::*;

    fn close(m: &StereoMatrix, left: &[f32]) {
        for (g, want) in m.gains.iter().zip(left) {
            assert!((g[0] - want).abs() < 2e-6, "{:?} vs {:?}", m.gains, left);
        }
    }

    // Native observations (DOWNMIX-ADDENDUM-heights.json): al22_chCfg0PCE_44's
    // 5.1.2(back) layout, and a lavfi superset with every id the decoders emit.
    #[test]
    fn matches_observed_height_matrices() {
        let m = StereoMatrix::with_levels(
            &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                BackLeft,
                BackRight,
                TopFrontLeft,
                TopFrontRight,
            ],
            DownmixLevels::default(),
        );
        close(
            &m,
            &[0.320377, 0.0, 0.226541, 0.0, 0.226541, 0.0, 0.226541, 0.0],
        );
        let m = StereoMatrix::with_levels(
            &[
                FrontLeft,
                FrontRight,
                TopCenter,
                TopFrontCenter,
                TopBackCenter,
                WideLeft,
                TopSideLeft,
                LowFrequency2,
            ],
            DownmixLevels::default(),
        );
        close(&m, &[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    }

    // Reference matrices as FFmpeg 9.0.2 reports them for actual vectors
    // (development observation; the native freeze is DOWNMIX-CONTRACT.json).
    #[test]
    fn matches_observed_reference_matrices() {
        let ac3_51 = DownmixLevels {
            center: SQRT_HALF,
            surround: 0.5,
            lfe: 0.0,
            encoding: SurroundEncoding::None,
        };
        close(
            &StereoMatrix::with_levels(
                &[
                    FrontLeft,
                    FrontRight,
                    FrontCenter,
                    LowFrequency,
                    SideLeft,
                    SideRight,
                ],
                ac3_51,
            ),
            &[0.453082, 0.0, 0.320377, 0.0, 0.226541, 0.0],
        );
        close(
            &StereoMatrix::for_layout(&[FrontLeft, FrontRight, FrontCenter, BackCenter]),
            &[0.453082, 0.0, 0.320377, 0.226541],
        );
        let seven = DownmixLevels {
            center: 0.594604,
            surround: 0.5,
            lfe: 0.0,
            encoding: SurroundEncoding::None,
        };
        close(
            &StereoMatrix::with_levels(
                &[
                    FrontLeft,
                    FrontRight,
                    FrontCenter,
                    LowFrequency,
                    BackLeft,
                    BackRight,
                    SideLeft,
                    SideRight,
                ],
                seven,
            ),
            &[0.385415, 0.0, 0.229169, 0.0, 0.192708, 0.0, 0.192708, 0.0],
        );
        let dpl2 = DownmixLevels {
            center: SQRT_HALF,
            surround: SQRT_HALF,
            lfe: 1.0,
            encoding: SurroundEncoding::ProLogicII,
        };
        let m = StereoMatrix::with_levels(
            &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                SideLeft,
                SideRight,
            ],
            dpl2,
        );
        close(
            &m,
            &[0.264534, 0.0, 0.187053, 0.187053, -0.229093, -0.132267],
        );
        assert!((m.gains[4][1] - 0.132267).abs() < 2e-6 && (m.gains[5][1] - 0.229093).abs() < 2e-6);
    }

    #[test]
    fn stereo_and_mono_pass_at_reference_gains_and_saturate() {
        let mut out = Vec::new();
        mix_into(
            &StereoMatrix::for_layout(&[FrontLeft, FrontRight]),
            &[vec![0.5, 1.5], vec![-0.5, -1.5]],
            2,
            &mut out,
        );
        assert_eq!(out, [16384, -16384, 32767, -32768]);
        out.clear();
        mix_into(
            &StereoMatrix::for_layout(&[FrontCenter]),
            &[vec![0.25]],
            1,
            &mut out,
        );
        assert_eq!(out, [5792, 5792], "mono reaches each side at -3 dB");
    }
}
