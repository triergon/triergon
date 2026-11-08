//! Layer II (ISO/IEC 11172-3 2.4.1.6 and 2.4.3.3, ISO/IEC 13818-3 2.4.3.1): 12
//! granules of three slots per frame, with bit allocation tables chosen by sample rate
//! and bit rate, scalefactor selection and grouped quantizers.

use triergon_bitstream::CachedBitReader;
use triergon_framing::{MpegAudioHeader, MpegVersion};

use crate::subband::{dequantize, joint_bound, scalefactor};
use crate::synthesis::Synthesis;

/// A quantizer of Table B.4.
#[derive(Clone, Copy, Debug)]
struct Quantizer {
    levels: u32,
    /// Bits of one sample, or of three samples when `grouped`.
    bits: u32,
    grouped: bool,
}

impl Quantizer {
    const fn new(levels: u32) -> Quantizer {
        match levels {
            3 => Quantizer {
                levels,
                bits: 5,
                grouped: true,
            },
            5 => Quantizer {
                levels,
                bits: 7,
                grouped: true,
            },
            9 => Quantizer {
                levels,
                bits: 10,
                grouped: true,
            },
            _ => Quantizer {
                levels,
                bits: levels.ilog2() + 1,
                grouped: false,
            },
        }
    }

    /// Three requantized samples.
    fn read(self, reader: &mut CachedBitReader<'_>) -> [f32; 3] {
        if self.grouped {
            let mut code = reader.read(self.bits);
            core::array::from_fn(|_| {
                let sample = code % self.levels;
                code /= self.levels;
                dequantize(sample, self.levels)
            })
        } else {
            core::array::from_fn(|_| dequantize(reader.read(self.bits), self.levels))
        }
    }
}

/// Quantizers selectable by allocation codes 1, 2, ... of a group of subbands.
type Quantizers = &'static [Quantizer];

const fn quantizers<const N: usize>(levels: [u32; N]) -> [Quantizer; N] {
    let mut out = [Quantizer::new(3); N];
    let mut i = 0;
    while i < N {
        out[i] = Quantizer::new(levels[i]);
        i += 1;
    }
    out
}

const WIDE_LOW: [Quantizer; 15] = quantizers([
    3, 7, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 16383, 32767, 65535,
]);
const WIDE_MID: [Quantizer; 15] = quantizers([
    3, 5, 7, 9, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 65535,
]);
const WIDE_HIGH: [Quantizer; 7] = quantizers([3, 5, 7, 9, 15, 31, 65535]);
const WIDE_TOP: [Quantizer; 3] = quantizers([3, 5, 65535]);
const NARROW_LOW: [Quantizer; 15] = quantizers([
    3, 5, 9, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 16383, 32767,
]);
const NARROW_HIGH: [Quantizer; 7] = quantizers([3, 5, 9, 15, 31, 63, 127]);
const LSF_LOW: [Quantizer; 15] = quantizers([
    3, 5, 7, 9, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 16383,
]);
const LSF_TOP: [Quantizer; 3] = quantizers([3, 5, 9]);

/// A bit allocation table: runs of subbands with their allocation field width and
/// quantizers. The subbands it lists end at `sblimit`.
#[derive(Debug)]
struct AllocationTable(&'static [(usize, u32, Quantizers)]);

/// Table B.2a: 27 subbands.
const B2A: AllocationTable = AllocationTable(&[
    (3, 4, &WIDE_LOW),
    (8, 4, &WIDE_MID),
    (12, 3, &WIDE_HIGH),
    (4, 2, &WIDE_TOP),
]);
/// Table B.2b: 30 subbands.
const B2B: AllocationTable = AllocationTable(&[
    (3, 4, &WIDE_LOW),
    (8, 4, &WIDE_MID),
    (12, 3, &WIDE_HIGH),
    (7, 2, &WIDE_TOP),
]);
/// Table B.2c: 8 subbands.
const B2C: AllocationTable = AllocationTable(&[(2, 4, &NARROW_LOW), (6, 3, &NARROW_HIGH)]);
/// Table B.2d: 12 subbands.
const B2D: AllocationTable = AllocationTable(&[(2, 4, &NARROW_LOW), (10, 3, &NARROW_HIGH)]);
/// ISO/IEC 13818-3 Table B.1: 30 subbands.
const LSF: AllocationTable =
    AllocationTable(&[(4, 4, &LSF_LOW), (7, 3, &NARROW_HIGH), (19, 2, &LSF_TOP)]);

impl AllocationTable {
    /// The table of 2.4.3.3.1 (Annex B, Table B.2) for a frame: by sample rate and
    /// bit rate per channel, and the single table of the low sampling frequencies.
    fn for_frame(header: &MpegAudioHeader) -> &'static AllocationTable {
        if header.version != MpegVersion::Mpeg1 {
            return &LSF;
        }
        match u32::from(header.bitrate_kbps) / u32::from(header.channels()) {
            0..=48 if header.sample_rate == 32_000 => &B2D,
            0..=48 => &B2C,
            49..=80 => &B2A,
            _ if header.sample_rate == 48_000 => &B2A,
            _ => &B2B,
        }
    }

    /// Allocation field width and quantizers of each subband below `sblimit`.
    fn subbands(&self) -> impl Iterator<Item = (u32, Quantizers)> {
        self.0
            .iter()
            .flat_map(|&(count, bits, quantizers)| core::iter::repeat_n((bits, quantizers), count))
    }
}

/// Per channel and subband: the quantizer, if the subband carries samples.
type Allocation = [[Option<Quantizer>; 32]; 2];

/// Per channel and subband: the scalefactors of the three parts of the frame
/// (granules 0-3, 4-7 and 8-11).
type Scales = [[[f32; 3]; 32]; 2];

/// Decode one whole frame into `planes` (1152 samples each).
pub(crate) fn decode(
    header: &MpegAudioHeader,
    frame: &[u8],
    synthesis: &mut [Synthesis; 2],
    planes: &mut [Vec<f32>],
) {
    let channels = usize::from(header.channels());
    let table = AllocationTable::for_frame(header);
    let sblimit = table.subbands().count();
    let bound = joint_bound(header, sblimit);
    let mut reader = CachedBitReader::at(frame, if header.crc { 48 } else { 32 });
    let allocation = read_allocation(&mut reader, table, channels, bound);
    let scales = read_scalefactors(&mut reader, &allocation, channels, sblimit);

    let mut subbands = [[[0f32; 32]; 3]; 2];
    for granule in 0..12 {
        let part = granule / 4;
        for sb in 0..sblimit {
            let coded = if sb < bound { channels } else { 1 };
            let mut fractions = [0.0; 3];
            for ch in 0..channels {
                // Above the bound both channels share the samples of the first.
                if ch < coded {
                    fractions = allocation[ch][sb].map_or([0.0; 3], |q| q.read(&mut reader));
                }
                for (slot, fraction) in fractions.iter().enumerate() {
                    subbands[ch][slot][sb] = fraction * scales[ch][sb][part];
                }
            }
        }
        for ((filter, slots), plane) in synthesis.iter_mut().zip(&subbands).zip(planes.iter_mut()) {
            let pcm = &mut plane.as_chunks_mut::<32>().0[granule * 3..][..3];
            for (subbands, pcm) in slots.iter().zip(pcm) {
                filter.run(subbands, pcm);
            }
        }
    }
}

/// The bit allocation: one field per channel below `bound`, one for both above.
fn read_allocation(
    reader: &mut CachedBitReader<'_>,
    table: &AllocationTable,
    channels: usize,
    bound: usize,
) -> Allocation {
    let mut allocation: Allocation = [[None; 32]; 2];
    for (sb, (bits, quantizers)) in table.subbands().enumerate() {
        let coded = if sb < bound { channels } else { 1 };
        for ch in 0..coded {
            let code = reader.read(bits) as usize;
            allocation[ch][sb] = code.checked_sub(1).and_then(|i| quantizers.get(i).copied());
        }
        if coded < channels {
            allocation[1][sb] = allocation[0][sb];
        }
    }
    allocation
}

/// The scalefactor selection information, then the scalefactors it selects: three,
/// two (shared by parts 0 and 1, or 1 and 2) or one per subband with samples.
fn read_scalefactors(
    reader: &mut CachedBitReader<'_>,
    allocation: &Allocation,
    channels: usize,
    sblimit: usize,
) -> Scales {
    let mut selection = [[0u32; 32]; 2];
    for sb in 0..sblimit {
        for ch in 0..channels {
            if allocation[ch][sb].is_some() {
                selection[ch][sb] = reader.read(2);
            }
        }
    }
    let mut scales: Scales = [[[0.0; 3]; 32]; 2];
    for sb in 0..sblimit {
        for ch in 0..channels {
            if allocation[ch][sb].is_none() {
                continue;
            }
            let mut read = || scalefactor(reader.read(6));
            scales[ch][sb] = match selection[ch][sb] {
                0 => [read(), read(), read()],
                1 => {
                    let (first, last) = (read(), read());
                    [first, first, last]
                }
                2 => [read(); 3],
                _ => {
                    let (first, last) = (read(), read());
                    [first, last, last]
                }
            };
        }
    }
    scales
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_cover_their_subband_limits() {
        let limits = [&B2A, &B2B, &B2C, &B2D, &LSF].map(|t| t.subbands().count());
        assert_eq!(limits, [27, 30, 8, 12, 30]);
        assert_eq!(Quantizer::new(65535).bits, 16);
        assert_eq!(Quantizer::new(7).bits, 3);
    }
}
