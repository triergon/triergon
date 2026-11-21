//! Stereo downmix and 16-bit PCM for the device path (`-ac 2` to Pulse/ALSA).
//!
//! The coefficients are the conventional matrix (front pair unity, centre and
//! surrounds at -3 dB, LFE omitted) normalised so no output row can exceed
//! full scale, which is what a fixed-point device format needs. The exact
//! reference behaviour (matrix, normalisation and float-to-s16 rounding) is a
//! native observation; this module implements it and its tests pin it.
//!
//! Float and 32-bit input mixes in `f32`. 16-bit input mixes in fixed point, as
//! the reference's resampler does (`harness/codecs/downmix-integer.md`): Q15
//! gains, one rounding per output sample and two's-complement wraparound.
use triergon_codec::{
    AudioFrame, ChannelLayout, DownmixLevels, SampleFormat, Speaker, SurroundEncoding,
};

const SQRT_HALF: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Left/right gains for every input plane.
#[derive(Clone, Debug, PartialEq)]
struct StereoMatrix {
    gains: Vec<[f32; 2]>,
}

impl StereoMatrix {
    /// Unity front pair; centre at `center`; back centre at `surround`/sqrt(2)
    /// to each side; back/side pairs at `surround` to their side (Pro Logic II:
    /// -sqrt(3/2)/-sqrt(1/2) and +sqrt(1/2)/+sqrt(3/2) times `surround`, Dolby:
    /// -/+ sqrt(1/2) times `surround`); front-centre pair at unity to its side;
    /// LFE (and LFE2) at `lfe`/sqrt(2); top front pair at sqrt(1/2) to its side, every
    /// other height/wide channel dropped.
    /// Rows are normalised to an absolute sum of at most one.
    fn with_levels(speakers: &[Speaker], levels: DownmixLevels) -> Self {
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
    fn is_identity_stereo(&self) -> bool {
        self.gains == [[1.0, 0.0], [0.0, 1.0]]
    }
}

/// Gains with 15 fractional bits, for mixing 16-bit samples.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Q15Matrix {
    /// Left/right gains for every input plane. Each side's absolute sum stays within
    /// a few units of `ONE`, so a mix of 16-bit samples cannot overflow `i32`.
    gains: Vec<[i32; 2]>,
}

impl Q15Matrix {
    const ONE: f32 = 32_768.0;

    /// [`StereoMatrix::with_levels`] with each gain rounded to the nearest 1/32768.
    ///
    /// For 5.1, 5.1(side) and 7.1 the reference gives the centre what remains of the
    /// row's rounded sum once the other gains are rounded (observed at the default
    /// levels: 9597 rather than 9598 for 5.1, 7424 rather than 7423 for 7.1).
    fn with_levels(speakers: &[Speaker], levels: DownmixLevels) -> Self {
        use Speaker::{
            BackLeft, BackRight, FrontCenter, FrontLeft, FrontRight, LowFrequency, SideLeft,
            SideRight,
        };
        const CENTRE: usize = 2;
        let matrix = StereoMatrix::with_levels(speakers, levels);
        let mut gains: Vec<[i32; 2]> = matrix
            .gains
            .iter()
            .map(|gain| gain.map(|g| (g * Self::ONE).round() as i32))
            .collect();
        if let [
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            surrounds @ ..,
        ] = speakers
            && matches!(
                surrounds,
                [BackLeft, BackRight]
                    | [SideLeft, SideRight]
                    | [BackLeft, BackRight, SideLeft, SideRight]
            )
        {
            for side in 0..2 {
                let row: f32 = matrix.gains.iter().map(|g| g[side]).sum();
                let others: i32 = gains.iter().map(|g| g[side]).sum::<i32>() - gains[CENTRE][side];
                gains[CENTRE][side] = (row * Self::ONE).round() as i32 - others;
            }
        }
        Self { gains }
    }
}

/// How one layout mixes: in `f32`, or in fixed point for 16-bit input.
#[derive(Clone, Debug)]
enum Mixer {
    Float(StereoMatrix),
    Q15(Q15Matrix),
}

impl Mixer {
    fn new(layout: ChannelLayout, levels: DownmixLevels, format: SampleFormat) -> Self {
        // FFmpeg's resampler mixes an unspecified order as its default layout.
        let layout = Some(layout)
            .filter(|layout| layout.mask() != 0 || layout.channels() < 2)
            .or_else(|| ChannelLayout::default_for(layout.channels()))
            .unwrap_or(layout);
        let matrix = StereoMatrix::with_levels(layout.speakers(), levels);
        // A 16-bit stereo pair passes through exactly on either path.
        if format == SampleFormat::S16 && !matrix.is_identity_stereo() {
            Mixer::Q15(Q15Matrix::with_levels(layout.speakers(), levels))
        } else {
            Mixer::Float(matrix)
        }
    }
}

/// Stereo downmix of decoded frames; the matrix is rebuilt only when the layout,
/// the stream's levels or the sample format change.
#[derive(Debug, Default)]
pub struct Downmix {
    cached: Option<(ChannelLayout, DownmixLevels, SampleFormat, Mixer)>,
}

impl Downmix {
    /// Append `frame` to `out` as interleaved stereo s16.
    pub fn mix(&mut self, frame: &AudioFrame, levels: DownmixLevels, out: &mut Vec<i16>) {
        let format = frame.sample_format;
        let mixer = match &mut self.cached {
            Some((layout, cached, f, mixer))
                if *layout == frame.layout && *cached == levels && *f == format =>
            {
                mixer
            }
            slot => {
                let mixer = Mixer::new(frame.layout, levels, format);
                &mut slot.insert((frame.layout, levels, format, mixer)).3
            }
        };
        let samples = frame.samples_per_channel;
        match mixer {
            Mixer::Float(matrix) => mix_into(matrix, &frame.planes, samples, out),
            Mixer::Q15(matrix) => mix_q15_into(matrix, &frame.planes, samples, out),
        }
    }
}

/// Mix `samples` frames of planar float into interleaved stereo s16, appending.
fn mix_into(matrix: &StereoMatrix, planes: &[Vec<f32>], samples: usize, out: &mut Vec<i16>) {
    let start = out.len();
    out.resize(start + 2 * samples, 0);
    let frames = &mut out[start..];
    if matrix.is_identity_stereo() {
        // 32-bit ARM converts from the bits, which vectorises there (see `to_s16_bits`).
        #[cfg(target_arch = "arm")]
        let to_s16 = to_s16_bits;
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
    // Eight frames at a time, the planes in order: each sum starts at zero and adds every
    // plane's product in plane order, as a per-sample loop would, while the sums stay in
    // registers (one vector per side where the target has float vectors) across the
    // planes. A plane with no gain on either side adds only zeros and is skipped.
    const LANES: usize = 8;
    let whole = samples - samples % LANES;
    let (body, tail) = frames.split_at_mut(2 * whole);
    for (block, frames) in body
        .as_chunks_mut::<{ 2 * LANES }>()
        .0
        .iter_mut()
        .enumerate()
    {
        let start = block * LANES;
        let (mut left, mut right) = ([0.0_f32; LANES], [0.0_f32; LANES]);
        for (plane, &[gl, gr]) in planes.iter().zip(&matrix.gains) {
            if silent([gl, gr]) {
                continue;
            }
            #[expect(
                clippy::expect_used,
                reason = "a slice of `LANES` elements; measured loop"
            )]
            let block: &[f32; LANES] = plane[start..start + LANES].try_into().expect("block");
            for (l, &v) in left.iter_mut().zip(block) {
                *l += v * gl;
            }
            for (r, &v) in right.iter_mut().zip(block) {
                *r += v * gr;
            }
        }
        for ((frame, l), r) in frames
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .zip(left)
            .zip(right)
        {
            *frame = [to_s16(l), to_s16(r)];
        }
    }
    for (i, frame) in (whole..samples).zip(tail.chunks_exact_mut(2)) {
        let (mut l, mut r) = (0.0_f32, 0.0_f32);
        for (plane, &[gl, gr]) in planes.iter().zip(&matrix.gains) {
            if silent([gl, gr]) {
                continue;
            }
            l += plane[i] * gl;
            r += plane[i] * gr;
        }
        frame[0] = to_s16(l);
        frame[1] = to_s16(r);
    }
}

/// Mix `samples` frames of planar 16-bit samples (each exactly `x / 2^15`) into
/// interleaved stereo s16, appending: `(sum(x * gain) + 2^14) >> 15`, wrapped to 16 bits.
fn mix_q15_into(matrix: &Q15Matrix, planes: &[Vec<f32>], samples: usize, out: &mut Vec<i16>) {
    const HALF: i32 = 1 << 14;
    let start = out.len();
    out.resize(start + 2 * samples, 0);
    for (i, frame) in out[start..].chunks_exact_mut(2).enumerate() {
        let (mut l, mut r) = (HALF, HALF);
        for (plane, gain) in planes.iter().zip(&matrix.gains) {
            let v = (plane[i] * Q15Matrix::ONE) as i32;
            l += v * gain[0];
            r += v * gain[1];
        }
        // Truncation wraps a full-scale overshoot, as the reference does.
        frame[0] = (l >> 15) as i16;
        frame[1] = (r >> 15) as i16;
    }
}

/// Whether a plane's gains are both zero, of either sign, so that it adds nothing.
/// 32-bit ARM tests the bits: there a float comparison's result has to move from the
/// floating-point status register to the core's flags, which stalls the pipeline once
/// per plane and block of the mixing loops. Elsewhere the float comparison is cheaper.
#[inline]
fn silent(gains: [f32; 2]) -> bool {
    if cfg!(target_arch = "arm") {
        (gains[0].to_bits() | gains[1].to_bits()) << 1 == 0
    } else {
        gains[0] == 0.0 && gains[1] == 0.0
    }
}

/// Full scale 1.0 maps to 32768, rounded toward negative infinity and
/// saturated: the reference device path's observed conversion. NaN maps to 0.
///
/// The floor is the conversion toward zero, less one where that rounded up; both
/// steps saturate, and so does the narrowing to 16 bits. `f32::floor` would be a
/// library call on 32-bit ARM, which has no rounding instruction before ARMv8; this
/// is a conversion, a comparison and integer saturation on every target.
#[inline]
pub fn to_s16(sample: f32) -> i16 {
    let x = sample * 32768.0;
    let toward_zero = x as i32;
    let floor = toward_zero.saturating_sub(i32::from(toward_zero as f32 > x));
    floor.clamp(i16::MIN.into(), i16::MAX.into()) as i16
}

/// [`to_s16`] with integer operations only, which LLVM vectorises on 32-bit ARM, where
/// it emits no vector float code.
///
/// A magnitude is `significand * 2^(exponent - 150)`, so its multiple of 2^15 is the
/// significand shifted right by `135 - exponent`; an arithmetic shift of the signed
/// significand rounds toward negative infinity. A magnitude of 1.0 or more leaves at
/// least 2^15 and saturates in the narrowing. The sign bit is shifted out rather than
/// masked: LLVM turns a mask of it into a float absolute value, and the loop then no
/// longer vectorises.
#[inline]
#[cfg_attr(
    not(target_arch = "arm"),
    allow(dead_code, reason = "used on 32-bit ARM, tested on every target")
)]
fn to_s16_bits(sample: f32) -> i16 {
    let bits = sample.to_bits();
    // The magnitude's bits, doubled.
    let doubled = bits << 1;
    let exponent = doubled >> 24;
    // The implicit leading bit, except for zero and subnormal values.
    let significand = (bits & 0x7f_ffff) | (exponent.min(1) << 23);
    let sign = (bits as i32) >> 31;
    let signed = (significand as i32 ^ sign) - sign;
    let floor = signed >> 135_u32.saturating_sub(exponent).min(31);
    // NaN: a doubled magnitude above infinity's.
    let value = if doubled > f32::INFINITY.to_bits() << 1 {
        0
    } else {
        floor
    };
    value.clamp(i16::MIN.into(), i16::MAX.into()) as i16
}

#[cfg(test)]
mod tests {
    use super::*;
    use Speaker::*;

    fn for_layout(speakers: &[Speaker]) -> StereoMatrix {
        StereoMatrix::with_levels(speakers, DownmixLevels::default())
    }

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
            &for_layout(&[FrontLeft, FrontRight, FrontCenter, BackCenter]),
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

    fn q15(speakers: &[Speaker]) -> Q15Matrix {
        Q15Matrix::with_levels(speakers, DownmixLevels::default())
    }

    fn left(m: &Q15Matrix) -> Vec<i32> {
        m.gains.iter().map(|g| g[0]).collect()
    }

    // FFmpeg 9.0.2's s16 rematrix, fitted on single-channel ramps
    // (harness/codecs/downmix-integer.md).
    #[test]
    fn matches_observed_q15_gains() {
        assert_eq!(left(&q15(&[FrontCenter])), [23170]);
        assert_eq!(
            left(&q15(&[FrontLeft, FrontRight, FrontCenter])),
            [19195, 0, 13573]
        );
        let five = [FrontLeft, FrontRight, FrontCenter, SideLeft, SideRight];
        assert_eq!(left(&q15(&five)), [13573, 0, 9598, 9598, 0]);
        let five_one = [
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            SideLeft,
            SideRight,
        ];
        assert_eq!(left(&q15(&five_one)), [13573, 0, 9597, 0, 9598, 0]);
        let m = q15(&[
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight,
        ]);
        assert_eq!(m.gains[2], [9597, 9597]);
        let seven_one = [
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight,
            SideLeft,
            SideRight,
        ];
        assert_eq!(
            left(&q15(&seven_one)),
            [10498, 0, 7424, 0, 7423, 0, 7423, 0]
        );
        let five_one_two = [
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            SideLeft,
            SideRight,
            TopFrontLeft,
            TopFrontRight,
        ];
        let m = q15(&five_one_two);
        assert_eq!(
            m.gains[2],
            [7423, 7423],
            "only the three listed layouts adjust"
        );
    }

    #[test]
    fn q15_mix_rounds_once_and_wraps() {
        let full = 32767.0 / 32768.0;
        let mut out = Vec::new();
        mix_q15_into(
            &q15(&[FrontLeft, FrontRight, FrontCenter, SideLeft, SideRight]),
            &[
                vec![full, -1.0, full],
                vec![full, -1.0, 0.0],
                vec![full, -1.0, full],
                vec![full, -1.0, full],
                vec![full, -1.0, 0.0],
            ],
            3,
            &mut out,
        );
        // Gains summing to 32769 overshoot full scale and wrap.
        assert_eq!(out, [-32768, -32768, 32767, 32767, -32768, 9598]);
        out.clear();
        let five_one = [
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            SideLeft,
            SideRight,
        ];
        let mut planes = vec![vec![0.0]; 6];
        planes[2][0] = 14225.0 / 32768.0;
        mix_q15_into(&q15(&five_one), &planes, 1, &mut out);
        assert_eq!(out, [4166, 4166]);
    }

    #[test]
    fn downmix_follows_the_sample_format() {
        let layout = ChannelLayout::new([FrontLeft, FrontRight, FrontCenter]).expect("layout");
        let mut frame = AudioFrame {
            sample_rate: 48_000,
            layout,
            samples_per_channel: 1,
            sample_format: SampleFormat::S16,
            planes: vec![vec![3.0 / 32768.0], vec![0.0], vec![0.0]],
        };
        let mut downmix = Downmix::default();
        let mut out = Vec::new();
        downmix.mix(&frame, DownmixLevels::default(), &mut out);
        assert_eq!(out, [2, 0], "(3 * 19195 + 2^14) >> 15");
        frame.sample_format = SampleFormat::Float;
        downmix.mix(&frame, DownmixLevels::default(), &mut out);
        assert_eq!(out[2..], [1, 0], "floor(3 * 0.585786)");

        let stereo = ChannelLayout::new([FrontLeft, FrontRight]).expect("layout");
        let samples: Vec<f32> = (-32768..32768)
            .step_by(7)
            .map(|x| x as f32 / 32768.0)
            .collect();
        let frame = AudioFrame {
            sample_rate: 48_000,
            layout: stereo,
            samples_per_channel: samples.len(),
            sample_format: SampleFormat::S16,
            planes: vec![samples.clone(), samples.clone()],
        };
        out.clear();
        downmix.mix(&frame, DownmixLevels::default(), &mut out);
        assert!(
            out.chunks_exact(2)
                .zip(&samples)
                .all(|(pair, &x)| pair == [(x * 32768.0) as i16; 2]),
            "a 16-bit stereo pair passes through exactly"
        );
    }

    #[test]
    fn float_mix_adds_the_planes_in_order_for_every_frame() {
        let five_one = [
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight,
        ];
        let matrix = for_layout(&five_one);
        let mut seed = 7_u32;
        for samples in [0, 1, 3, 4, 5, 1023, 1024] {
            let planes: Vec<Vec<f32>> = (0..6)
                .map(|_| {
                    (0..samples)
                        .map(|_| {
                            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                            (seed as i32) as f32 / 2_147_483_648.0
                        })
                        .collect()
                })
                .collect();
            let mut out = vec![9];
            mix_into(&matrix, &planes, samples, &mut out);
            let mut want = vec![9];
            for i in 0..samples {
                for side in 0..2 {
                    let mut sum = 0.0_f32;
                    for (plane, gain) in planes.iter().zip(&matrix.gains) {
                        if gain[0] != 0.0 || gain[1] != 0.0 {
                            sum += plane[i] * gain[side];
                        }
                    }
                    want.push(to_s16(sum));
                }
            }
            assert_eq!(out, want, "{samples} frames");
        }
    }

    #[test]
    fn silent_planes_are_those_with_zero_gains() {
        let values = [
            0.0,
            -0.0,
            f32::from_bits(1),
            -f32::from_bits(1),
            f32::MIN_POSITIVE,
            0.5,
            -1.0,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
            -f32::NAN,
        ];
        for left in values {
            for right in values {
                let zero = left == 0.0 && right == 0.0;
                assert_eq!(silent([left, right]), zero, "{left:e} {right:e}");
                let bits = (left.to_bits() | right.to_bits()) << 1 == 0;
                assert_eq!(bits, zero, "{left:e} {right:e} from bits");
            }
        }
    }

    #[test]
    fn to_s16_floors_and_saturates() {
        let floored = |x: f32| (x * 32768.0).floor().clamp(-32768.0, 32767.0) as i16;
        let specials = [
            0.0,
            -0.0,
            f32::MIN_POSITIVE,
            -f32::MIN_POSITIVE,
            0.5 / 32768.0,
            -0.5 / 32768.0,
            32767.5 / 32768.0,
            -32768.5 / 32768.0,
            1.0,
            -1.0,
            f32::MAX,
            f32::MIN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
            -f32::NAN,
            f32::from_bits(1),
            -f32::from_bits(1),
            1.0 - f32::EPSILON / 2.0,
            -1.0 + f32::EPSILON / 2.0,
            1.0 / 32768.0,
            -1.0 / 32768.0,
            -3.0 / 32768.0,
            -3.25 / 32768.0,
        ];
        let sweep = (0..=u32::MAX).step_by(997).map(f32::from_bits);
        for x in specials.into_iter().chain(sweep) {
            let want = floored(x);
            assert_eq!(to_s16(x), want, "{x:e}");
            assert_eq!(to_s16_bits(x), want, "{x:e} from bits");
        }
    }

    #[test]
    fn stereo_and_mono_pass_at_reference_gains_and_saturate() {
        let mut out = Vec::new();
        mix_into(
            &for_layout(&[FrontLeft, FrontRight]),
            &[vec![0.5, 1.5], vec![-0.5, -1.5]],
            2,
            &mut out,
        );
        assert_eq!(out, [16384, -16384, 32767, -32768]);
        out.clear();
        mix_into(&for_layout(&[FrontCenter]), &[vec![0.25]], 1, &mut out);
        assert_eq!(out, [5792, 5792], "mono reaches each side at -3 dB");
    }
}
