//! The quantized excitation (RFC 6716 section 4.2.7.8): rate level, pulse counts, shell
//! coded pulse positions, extra LSBs, signs, and the reconstruction with the quantization
//! offset and pseudorandom sign inversion.

use super::MAX_FRAME;
use super::frame::FrameType;
use super::tables::*;
use crate::range::RangeDecoder;

/// Samples per shell block.
const BLOCK: usize = 16;
const MAX_BLOCKS: usize = MAX_FRAME / BLOCK;
/// The pulse count symbol that announces another LSB level.
const ESCAPE: usize = 17;

/// Parse the signed pulse magnitudes of a frame of `len` samples into `pulses`, which
/// receives whole shell blocks (10 ms MB frames code 8 samples more than they use).
pub(super) fn parse_pulses(
    rc: &mut RangeDecoder<'_>,
    frame_type: FrameType,
    len: usize,
    pulses: &mut [i32; MAX_FRAME],
) {
    let blocks = len.div_ceil(BLOCK);
    let rate_level = rc.icdf(&RATE_LEVEL[frame_type.voicing()], 8);

    let mut counts = [0usize; MAX_BLOCKS];
    let mut lsbs = [0u8; MAX_BLOCKS];
    for (count, lsb) in counts[..blocks].iter_mut().zip(&mut lsbs) {
        *count = rc.icdf(&PULSE_COUNT[rate_level], 8);
        while *count == ESCAPE {
            *lsb += 1;
            // Rate level 10 is level 9 without the escape symbol.
            let table = &PULSE_COUNT[9][usize::from(*lsb == 10)..];
            *count = rc.icdf(table, 8);
        }
    }

    let pulses = &mut pulses[..blocks * BLOCK];
    for (block, &count) in pulses.chunks_exact_mut(BLOCK).zip(&counts) {
        decode_partition(rc, block, count as u32);
    }

    for (block, &lsb) in pulses.chunks_exact_mut(BLOCK).zip(&lsbs) {
        // All LSBs of one coefficient, most significant first, then the next coefficient.
        for magnitude in block.iter_mut() {
            for _ in 0..lsb {
                *magnitude = (*magnitude << 1) | rc.icdf(&EXCITATION_LSB, 8) as i32;
            }
        }
    }

    let sign_tables = &SIGN[frame_type.signal as usize][frame_type.offset as usize];
    for ((block, &count), &lsb) in pulses.chunks_exact_mut(BLOCK).zip(&counts).zip(&lsbs) {
        if count == 0 && lsb == 0 {
            continue;
        }
        let icdf = [sign_tables[count.min(6)], 0];
        for magnitude in block.iter_mut().filter(|m| **m > 0) {
            if rc.icdf(&icdf, 8) == 0 {
                *magnitude = -*magnitude;
            }
        }
    }
}

/// Distribute `count` pulses over `out` by recursive halving (section 4.2.7.8.3).
fn decode_partition(rc: &mut RangeDecoder<'_>, out: &mut [i32], count: u32) {
    if count == 0 {
        out.fill(0);
        return;
    }
    if out.len() == 1 {
        out[0] = count as i32;
        return;
    }
    let table = match out.len() {
        16 => &SHELL_SPLIT_16,
        8 => &SHELL_SPLIT_8,
        4 => &SHELL_SPLIT_4,
        _ => &SHELL_SPLIT_2,
    };
    let left = rc.icdf(table[count as usize - 1], 8) as u32;
    let (first, second) = out.split_at_mut(out.len() / 2);
    decode_partition(rc, first, left);
    decode_partition(rc, second, count - left);
}

/// Reconstruct the excitation (section 4.2.7.8.6) scaled to the nominal range of the
/// residual, `e_Q23 / 2^23`.
pub(super) fn reconstruct(pulses: &[i32], frame_type: FrameType, seed: u32, out: &mut [f32]) {
    let offset_q23 = QUANTIZATION_OFFSETS_Q23[frame_type.voicing()][frame_type.offset as usize];
    let mut seed = seed;
    for (e, &raw) in out.iter_mut().zip(pulses) {
        let mut value = (raw << 8) - raw.signum() * 20 + offset_q23;
        seed = seed.wrapping_mul(196314165).wrapping_add(907633515);
        if seed & 0x8000_0000 != 0 {
            value = -value;
        }
        seed = seed.wrapping_add(raw as u32);
        *e = value as f32 * (1.0 / 8388608.0);
    }
}

#[cfg(test)]
mod tests {
    use super::super::frame::{QuantOffset, SignalType};
    use super::*;

    /// A range decoder over bytes that make every symbol the first of its table.
    fn first_symbols() -> Vec<u8> {
        vec![0; 64]
    }

    #[test]
    fn shell_partitions_keep_their_pulse_total() {
        for seed in 0..64u8 {
            let data: Vec<u8> = (0..32u8)
                .map(|i| seed.wrapping_mul(37).wrapping_add(i.wrapping_mul(11)))
                .collect();
            let mut rc = RangeDecoder::new(&data);
            for count in [1, 5, 16] {
                let mut block = [0; BLOCK];
                decode_partition(&mut rc, &mut block, count);
                assert_eq!(block.iter().sum::<i32>(), count as i32);
                assert!(block.iter().all(|&p| p >= 0));
            }
        }
    }

    #[test]
    fn all_zero_input_parses_a_silent_frame() {
        let data = first_symbols();
        let mut rc = RangeDecoder::new(&data);
        let frame_type = FrameType {
            signal: SignalType::Unvoiced,
            offset: QuantOffset::Low,
        };
        let mut pulses = [7; MAX_FRAME];
        parse_pulses(&mut rc, frame_type, 120, &mut pulses);
        // The first pulse-count symbol of every table is zero pulses.
        assert!(pulses[..128].iter().all(|&p| p == 0));
    }

    #[test]
    fn reconstruction_adds_the_offset_and_flips_pseudorandomly() {
        let frame_type = FrameType {
            signal: SignalType::Voiced,
            offset: QuantOffset::High,
        };
        let pulses = [0, 1, -2, 0];
        let mut out = [0.0; 4];
        reconstruct(&pulses, frame_type, 0, &mut out);
        let magnitudes = [25, 256 - 20 + 25, 512 - 20 - 25];
        for (e, m) in out.iter().zip(magnitudes) {
            assert!((e.abs() * 8388608.0 - m as f32).abs() < 1e-3, "{e} {m}");
        }
        // Seed 0 gives 907633515 first (sign bit clear), then a negative state.
        assert!(out[0] > 0.0);
    }
}
