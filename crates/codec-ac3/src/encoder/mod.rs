//! A/52 AC-3 encoding from planar PCM. Each call emits one complete syncframe.

mod syntax;

use crate::bitalloc::{
    Ac3BandExponents, BaCache, BaParams, Dba, MaskParams, ac3_bap_from_exponents, stage_mask,
    stage_psd,
};
use crate::header::{BITRATE_KBPS, SAMPLE_RATES, ac3_frame_words};
use crate::imdct::analysis::Analysis;
use crate::tables::FASTGAIN;
use syntax::{BAP_COSTS, Bits, Mantissas, mantissa_bits};

const BLOCK: usize = 256;
const BLOCKS: usize = 6;
const CHANNELS: usize = 6;
const MIN_FULL_BINS: usize = 73;
const MAX_FULL_BINS: usize = 253;
const LFE_BINS: usize = 7;
const PARAMS: BaParams = BaParams {
    sdcycod: 2,
    fdcycod: 1,
    sgaincod: 1,
    dbpbcod: 2,
    floorcod: 4,
};

/// PCM samples consumed per plane and syncframe.
pub const SAMPLES_PER_FRAME: usize = BLOCKS * BLOCK;
/// Silent history at the start of a stream, in samples per plane.
pub const DELAY_SAMPLES: usize = BLOCK;
/// Space sufficient for any syncframe this encoder emits.
pub const MAX_FRAME_BYTES: usize = 3_840;

/// A complete audio program and the order of its input planes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelMode {
    /// Centre. 192 kbit/s.
    Mono,
    /// Left, right. 256 kbit/s.
    Stereo,
    /// Left, right, centre, LFE, surround left, surround right. 640 kbit/s.
    Surround51,
}

impl ChannelMode {
    fn coded_order(self) -> &'static [usize] {
        match self {
            Self::Mono => &[0],
            Self::Stereo => &[0, 1],
            Self::Surround51 => &[0, 2, 1, 4, 5, 3],
        }
    }

    fn acmod(self) -> u32 {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::Surround51 => 7,
        }
    }

    fn bitrate(self) -> u16 {
        match self {
            Self::Mono => 192,
            Self::Stereo => 256,
            Self::Surround51 => 640,
        }
    }

    fn is_lfe(self, channel: usize) -> bool {
        self == Self::Surround51 && channel == CHANNELS - 1
    }

    fn full_channels(self) -> usize {
        self.coded_order().len() - usize::from(self == Self::Surround51)
    }
}

fn fast_gain(lfe: bool) -> u8 {
    if lfe { 7 } else { 4 }
}

/// An encoding request that could not emit a frame. Input and output sizes are checked
/// before consuming samples or advancing the stream clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// The sample rate is outside the three AC-3 rates this encoder supports.
    SampleRate(u32),
    /// PCM does not contain exactly one frame in the selected plane order.
    InputShape,
    /// PCM contains a non-finite value or a value outside the normalized range.
    InvalidSample,
    /// The next syncframe cannot fit in the caller's buffer.
    OutputLength { needed: usize },
    /// The encoded fields cannot be represented in the frame.
    BitAllocation,
}

impl core::fmt::Display for EncodeError {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SampleRate(rate) => write!(out, "unsupported AC-3 rate {rate} Hz"),
            Self::InputShape => out.write_str("incorrect AC-3 input plane count or length"),
            Self::InvalidSample => {
                out.write_str("AC-3 input samples must be finite and in [-1, 1]")
            }
            Self::OutputLength { needed } => write!(out, "AC-3 syncframe needs {needed} bytes"),
            Self::BitAllocation => out.write_str("AC-3 data exceeds the syncframe budget"),
        }
    }
}

impl std::error::Error for EncodeError {}

#[derive(Debug)]
struct ExponentSet {
    ends: [usize; CHANNELS],
    repeats: [usize; CHANNELS],
    exponents: [[u8; BLOCK]; CHANNELS],
    masks: [[i32; 50]; CHANNELS],
    band_exponents: [Ac3BandExponents; CHANNELS],
}

impl Default for ExponentSet {
    fn default() -> Self {
        Self {
            ends: [0; CHANNELS],
            repeats: [1; CHANNELS],
            exponents: [[0; BLOCK]; CHANNELS],
            masks: [[0; 50]; CHANNELS],
            band_exponents: core::array::from_fn(|_| Ac3BandExponents::default()),
        }
    }
}

#[derive(Debug)]
struct Work {
    spectra: Box<[[f32; BLOCK]]>,
    sets: [ExponentSet; BLOCKS],
    mask_scratch: BaCache,
    bap: [[u8; BLOCK]; CHANNELS],
}

/// A fixed-rate AC-3 encoder for 32, 44.1 or 48 kHz PCM. The first frame starts
/// with silent transform history. Supply zeros after the input to recover the
/// final delayed samples, and discard padding using the source sample count.
#[derive(Debug)]
pub struct Encoder {
    mode: ChannelMode,
    fscod: u8,
    size_code: u8,
    word_fraction: u32,
    remainder: u32,
    started: bool,
    previous: [[f32; BLOCK]; CHANNELS],
    previous_silent: [bool; CHANNELS],
    analysis: Analysis,
    work: Box<Work>,
}

impl Encoder {
    /// Open an encoder for the selected PCM layout and one of the three AC-3 rates.
    #[allow(clippy::large_stack_arrays)] // Fixed-size, allocated once with the encoder.
    pub fn new(sample_rate: u32, mode: ChannelMode) -> Result<Self, EncodeError> {
        let fscod = SAMPLE_RATES
            .iter()
            .position(|&rate| rate == sample_rate)
            .ok_or(EncodeError::SampleRate(sample_rate))? as u8;
        let bitrate = mode.bitrate();
        let size_code = BITRATE_KBPS
            .iter()
            .position(|&rate| rate == bitrate)
            .ok_or(EncodeError::BitAllocation)? as u8
            * 2;
        let word_fraction =
            (u32::from(bitrate) * 1_000 * (SAMPLES_PER_FRAME as u32 / 16)) % sample_rate;
        Ok(Self {
            mode,
            fscod,
            size_code,
            word_fraction,
            remainder: 0,
            started: false,
            previous: [[0.0; BLOCK]; CHANNELS],
            previous_silent: [true; CHANNELS],
            analysis: Analysis::new(),
            work: Box::new(Work {
                spectra: vec![[0.0; BLOCK]; BLOCKS * mode.coded_order().len()].into_boxed_slice(),
                sets: core::array::from_fn(|_| ExponentSet::default()),
                mask_scratch: BaCache::default(),
                bap: [[0; BLOCK]; CHANNELS],
            }),
        })
    }

    /// Clear transform history and restart the frame-size clock at a discontinuity.
    pub fn reset(&mut self) {
        self.previous.fill([0.0; BLOCK]);
        self.previous_silent.fill(true);
        self.remainder = 0;
        self.started = false;
    }

    /// Encode exactly 1,536 normalized samples per plane into `output`.
    /// Returns the byte count, which can vary by two bytes at 44.1 kHz.
    pub fn encode(&mut self, input: &[&[f32]], output: &mut [u8]) -> Result<usize, EncodeError> {
        let order = self.mode.coded_order();
        if input.len() != order.len() || input.iter().any(|plane| plane.len() != SAMPLES_PER_FRAME)
        {
            return Err(EncodeError::InputShape);
        }
        let sample_rate = SAMPLE_RATES[usize::from(self.fscod)];
        let remainder = self.remainder + self.word_fraction;
        let size_code = self.size_code + u8::from(remainder >= sample_rate);
        let bytes = 2 * ac3_frame_words(self.fscod, size_code).ok_or(EncodeError::BitAllocation)?;
        let output = output
            .get_mut(..bytes)
            .ok_or(EncodeError::OutputLength { needed: bytes })?;
        let mut silent = [[false; BLOCKS]; CHANNELS];
        for (plane, blocks) in input.iter().zip(&mut silent) {
            let (samples, _) = plane.as_chunks::<BLOCK>();
            for (samples, silent) in samples.iter().zip(blocks) {
                // Positive f32 bit patterns increase with magnitude. NaNs and
                // infinities have larger patterns than every allowed sample.
                let peak = samples
                    .iter()
                    .fold(0, |peak, sample| peak.max(sample.to_bits() & 0x7fff_ffff));
                if peak > 1.0_f32.to_bits() {
                    return Err(EncodeError::InvalidSample);
                }
                *silent = peak == 0;
            }
        }

        let Work {
            spectra,
            sets,
            mask_scratch,
            bap,
        } = self.work.as_mut();
        let mode = self.mode;
        let channels = order.len();
        let full_channels = mode.full_channels();
        let mut switches = [0_u8; BLOCKS];
        let mut boundaries = [false; BLOCKS];
        for (block, (spectra, switch_mask)) in spectra
            .chunks_exact_mut(channels)
            .zip(&mut switches)
            .enumerate()
        {
            let at = block * BLOCK;
            let pcm = |coded: usize, channel: usize| {
                let plane = input[channel];
                let previous = if block == 0 {
                    self.previous[coded].as_slice()
                } else {
                    &plane[at - BLOCK..at]
                };
                (previous, &plane[at..at + BLOCK])
            };
            for (coded, &ch) in order.iter().enumerate() {
                let previous_silent = if block == 0 {
                    self.previous_silent[coded]
                } else {
                    silent[ch][block - 1]
                };
                let next_silent = silent[ch][block];
                if previous_silent && next_silent {
                    spectra[coded].fill(0.0);
                    continue;
                }
                let (previous, next) = pcm(coded, ch);
                let lfe = mode.is_lfe(coded);
                let boundary = !lfe && previous_silent != next_silent;
                boundaries[block] |= boundary;
                let history = (self.started || block != 0).then_some(previous);
                if !lfe && (boundary || has_attack(history, next)) {
                    // One block-switch bit per full-bandwidth channel (A/52:2018 5.4.3.1).
                    *switch_mask |= 1 << (full_channels - coded - 1);
                    self.analysis.short(previous, next, &mut spectra[coded]);
                } else {
                    self.analysis.long(previous, next, &mut spectra[coded]);
                }
            }
        }
        let mut blocks_per_set = [0; BLOCKS];
        let mut set_for_block = [0; BLOCKS];
        let mut block = 0;
        let mut set_count = 0;
        while block < BLOCKS {
            let first = &spectra[block * channels..(block + 1) * channels];
            let second_block = block + 1;
            // A fresh exponent set preserves the spectral envelope at a transform
            // change or a boundary with silence (A/52:2018 8.2.8).
            let share = second_block < BLOCKS
                && switches[block] == switches[second_block]
                && !boundaries[block]
                && !boundaries[second_block];
            let second = if share {
                &spectra[second_block * channels..(second_block + 1) * channels]
            } else {
                first
            };
            sets[set_count].prepare(mode, self.fscod, first, second, mask_scratch)?;
            blocks_per_set[set_count] = 1 + usize::from(share);
            set_for_block[block..block + blocks_per_set[set_count]].fill(set_count);
            block += blocks_per_set[set_count];
            set_count += 1;
        }
        let mut fixed = Bits::count();
        syntax::header(&mut fixed, self.fscod, size_code, mode);
        for (block, &short) in switches.iter().enumerate() {
            let group = set_for_block[block];
            let fresh = block == 0 || group != set_for_block[block - 1];
            syntax::block(&mut fixed, mode, block, short, 0, &sets[group], fresh);
        }
        // Two zero flag bits (auxdatae and crcrsv) precede the final CRC word.
        let available = (bytes * 8)
            .checked_sub(fixed.position() + 18)
            .ok_or(EncodeError::BitAllocation)?;
        let snr = select_snr(&sets[..set_count], &blocks_per_set, available);

        output.fill(0);
        let mut bits = Bits::write_to(output);
        syntax::header(&mut bits, self.fscod, size_code, mode);
        for (index, (coefficients, short)) in
            spectra.chunks_exact(channels).zip(switches).enumerate()
        {
            let group = set_for_block[index];
            let fresh = index == 0 || group != set_for_block[index - 1];
            let set = &sets[group];
            syntax::block(&mut bits, mode, index, short, snr, set, fresh);
            if fresh {
                set.allocate(snr, bap);
            }
            let mut mantissas = Mantissas::default();
            for (ch, channel_coefficients) in coefficients.iter().enumerate() {
                let codes = &bap[ch][..set.ends[ch]];
                for ((&coefficient, &exponent), &code) in channel_coefficients
                    .iter()
                    .zip(&set.exponents[ch])
                    .zip(codes)
                {
                    if code != 0 {
                        let normal =
                            coefficient * f32::from_bits((127 + u32::from(exponent)) << 23);
                        mantissas.push(&mut bits, code, normal);
                    }
                }
            }
            mantissas.finish(&mut bits);
        }
        if bits.position() > bytes * 8 - 18 {
            return Err(EncodeError::BitAllocation);
        }
        syntax::checksum(output);
        for (coded, &channel) in order.iter().enumerate() {
            self.previous[coded].copy_from_slice(&input[channel][SAMPLES_PER_FRAME - BLOCK..]);
            self.previous_silent[coded] = silent[channel][BLOCKS - 1];
        }
        self.remainder = remainder % sample_rate;
        self.started = true;
        Ok(bytes)
    }
}

fn has_attack(previous: Option<&[f32]>, next: &[f32]) -> bool {
    const WINDOW: usize = 64;
    // The second difference emphasizes high frequencies. Its unnormalized
    // peak has a range of [0, 4], so 0.04 is one percent of full scale.
    let difference = |a: f32, b: f32, c: f32| ((a + c) - 2.0 * b).abs();
    let peak = |[a, b]: [f32; 2], samples: &[f32]| {
        let boundary = difference(a, b, samples[0]).max(difference(b, samples[0], samples[1]));
        samples
            .windows(3)
            .map(|s| difference(s[0], s[1], s[2]))
            .fold(boundary, f32::max)
    };
    let (mut last, mut before) = if let Some(previous) = previous {
        let at = BLOCK - WINDOW;
        (
            peak([previous[at - 2], previous[at - 1]], &previous[at..]),
            [previous[BLOCK - 2], previous[BLOCK - 1]],
        )
    } else {
        // Establish the first signal level from input samples.
        (f32::INFINITY, [0.0; 2])
    };
    for samples in next.chunks_exact(WINDOW) {
        let current = peak(before, samples);
        if current > (16.0 * last).max(0.04) && samples.iter().any(|sample| sample.abs() > 0.01) {
            return true;
        }
        last = current;
        before = [samples[WINDOW - 2], samples[WINDOW - 1]];
    }
    false
}

impl ExponentSet {
    fn prepare(
        &mut self,
        mode: ChannelMode,
        fscod: u8,
        first: &[[f32; BLOCK]],
        second: &[[f32; BLOCK]],
        mask_scratch: &mut BaCache,
    ) -> Result<(), EncodeError> {
        for (ch, (first, second)) in first.iter().zip(second).enumerate() {
            let lfe = mode.is_lfe(ch);
            let end = if lfe {
                LFE_BINS
            } else {
                full_bandwidth(first, second)
            };
            let group = if lfe || end <= MAX_FULL_BINS / 2 {
                1
            } else {
                2
            };
            self.ends[ch] = end;
            self.repeats[ch] = group;
            let exponents = &mut self.exponents[ch];
            // Fit the peak from every block that uses this set. Limit the changes
            // in both directions so each exponent differential is representable.
            let peak = |bin: usize| first[bin].abs().max(second[bin].abs());
            exponents[0] = exponent(peak(0)).min(15);
            for bin in (1..end).step_by(group) {
                let max = (bin..bin + group).map(peak).fold(0.0, f32::max);
                exponents[bin..bin + group].fill(exponent(max));
            }
            for bin in (1..end).step_by(group).rev() {
                let previous = (bin - 1).saturating_sub(group - 1);
                let limit = exponents[bin] + 2;
                for exp in &mut exponents[previous..bin] {
                    *exp = (*exp).min(limit);
                }
            }
            for bin in (1..end).step_by(group) {
                let value = exponents[bin].min(exponents[bin - 1] + 2);
                exponents[bin..bin + group].fill(value);
            }
            stage_psd(exponents, 0, end, mask_scratch).map_err(|_| EncodeError::BitAllocation)?;
            let params = MaskParams {
                start: 0,
                end,
                fscod: usize::from(fscod),
                rate_shift: 0,
                params: PARAMS,
                fgain: FASTGAIN[usize::from(fast_gain(lfe))],
                cpl_leak: None,
                dba: &Dba::default(),
            };
            stage_mask(&params, mask_scratch).map_err(|_| EncodeError::BitAllocation)?;
            self.masks[ch] = *mask_scratch.masks();
            self.band_exponents[ch].prepare(exponents, end);
        }
        Ok(())
    }

    fn allocate(&self, snr: u16, bap: &mut [[u8; BLOCK]; CHANNELS]) {
        for (ch, (output, &end)) in bap.iter_mut().zip(&self.ends).enumerate() {
            if end == 0 {
                break;
            }
            if snr == 0 {
                output[..end].fill(0);
            } else {
                ac3_bap_from_exponents(
                    &self.masks[ch],
                    &self.exponents[ch][..end],
                    snr_offset(snr),
                    PARAMS.floorcod,
                    output,
                );
            }
        }
    }

    fn mantissa_bits(&self, snr: u16) -> usize {
        let offset = snr_offset(snr);
        let mut cost = 0;
        for (ch, (&end, mask)) in self.ends.iter().zip(&self.masks).enumerate() {
            if end == 0 {
                break;
            }
            cost +=
                self.band_exponents[ch].cost(mask, &self.exponents[ch][..end], offset, &BAP_COSTS);
        }
        mantissa_bits(cost)
    }
}

fn snr_offset(snr: u16) -> i32 {
    (i32::from(snr) - (15 << 4)) << 2
}

fn full_bandwidth(first: &[f32; BLOCK], second: &[f32; BLOCK]) -> usize {
    let energy = |start, end| {
        first[start..end]
            .iter()
            .zip(&second[start..end])
            .map(|(a, b)| a * a + b * b)
            .sum::<f32>()
    };
    let floor = energy(0, MAX_FULL_BINS) * 1.0e-9;
    let mut discarded = 0.0;
    let mut end = MAX_FULL_BINS;
    // Keep complete exponent groups and lose at most 90 dB of energy
    // below the total of the two blocks.
    while end > MIN_FULL_BINS {
        discarded += energy(end - 6, end);
        if discarded > floor {
            break;
        }
        end -= 6;
    }
    end
}

fn exponent(peak: f32) -> u8 {
    let binary_exponent = ((peak.to_bits() >> 23) & 255) as i32;
    (126 - binary_exponent).clamp(0, 24) as u8
}

fn select_snr(sets: &[ExponentSet], blocks_per_set: &[usize; BLOCKS], available: usize) -> u16 {
    let mut accepted = 0;
    let mut rejected = 1024;
    while rejected - accepted > 1 {
        let trial = (accepted + rejected) / 2;
        let mut used = 0;
        for (pair, &count) in sets.iter().zip(blocks_per_set) {
            used += count * pair.mantissa_bits(trial);
            if used > available {
                break;
            }
        }
        if used <= available {
            accepted = trial;
        } else {
            rejected = trial;
        }
    }
    accepted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trial_cost_matches_allocated_mantissas_at_every_snr() {
        let coefficients = |offset: usize| {
            core::array::from_fn::<_, CHANNELS, _>(|channel| {
                core::array::from_fn(|bin| {
                    let exponent = 126 - (bin * 17 + channel * 13 + offset) % 24;
                    f32::from_bits((exponent as u32) << 23)
                })
            })
        };
        let first = coefficients(0);
        let second = coefficients(13);
        let mut pair = ExponentSet::default();
        pair.prepare(
            ChannelMode::Surround51,
            0,
            &first,
            &second,
            &mut BaCache::default(),
        )
        .expect("allocation parameters");
        let mut bap = [[0; BLOCK]; CHANNELS];
        for snr in 1..1024 {
            pair.allocate(snr, &mut bap);
            assert_eq!(
                pair.mantissa_bits(snr),
                syntax::allocated_mantissa_bits(&pair.ends, &bap)
            );
        }
    }

    #[test]
    fn silence_boundary_switches_each_channel_independently() {
        let mode = ChannelMode::Stereo;
        let steady = [0.25_f32; SAMPLES_PER_FRAME];
        let silence = [0.0; SAMPLES_PER_FRAME];
        let mut encoder = Encoder::new(48_000, mode).expect("48 kHz stereo");
        let mut syncframe = [0; MAX_FRAME_BYTES];
        encoder
            .encode(&[&steady, &steady], &mut syncframe)
            .expect("steady frame");
        encoder
            .encode(&[&silence, &steady], &mut syncframe)
            .expect("one channel becomes silent");

        let mut header = Bits::count();
        syntax::header(&mut header, encoder.fscod, encoder.size_code, mode);
        let flag = |channel| {
            let at = header.position() + channel;
            (syncframe[at / 8] >> (7 - at % 8)) & 1
        };
        assert_eq!([flag(0), flag(1)], [1, 0]);
    }

    #[test]
    fn attacks_require_input_history_and_signal_after_the_edge() {
        let steady: [f32; BLOCK] =
            core::array::from_fn(|i| if i.is_multiple_of(2) { -0.2 } else { 0.2 });
        let silence = [0.0; BLOCK];
        assert!(!has_attack(None, &steady));
        assert!(!has_attack(Some(&[0.4; BLOCK]), &silence));

        let mut attack = silence;
        attack[64..].fill(0.4);
        assert!(has_attack(Some(&silence), &attack));
        assert!(has_attack(None, &attack));
    }
}
