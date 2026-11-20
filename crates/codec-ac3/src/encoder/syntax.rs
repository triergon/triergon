//! Syncframe syntax (A/52:2018 5.3), mantissas (7.3) and CRCs (7.10.1).

use super::{BLOCK, CHANNELS, ChannelMode, LFE_BINS, MIN_FULL_BINS, PARAMS, Pair, fast_gain};
use crate::crc16;
use crate::tables::BAP_BITS;

pub(super) struct Bits<'a> {
    data: Option<&'a mut [u8]>,
    position: usize,
}

impl<'a> Bits<'a> {
    pub(super) fn count() -> Self {
        Self {
            data: None,
            position: 0,
        }
    }

    pub(super) fn write_to(data: &'a mut [u8]) -> Self {
        Self {
            data: Some(data),
            position: 0,
        }
    }

    pub(super) fn position(&self) -> usize {
        self.position
    }

    fn put(&mut self, value: u32, width: usize) {
        self.patch(self.position, value, width);
        self.position += width;
    }

    fn patch(&mut self, position: usize, value: u32, width: usize) {
        let Some(data) = &mut self.data else { return };
        let byte = position / 8;
        let offset = position % 8;
        let count = (offset + width).div_ceil(8);
        let value = (value << (32 - offset - width)).to_be_bytes();
        if let Some(dst) = data.get_mut(byte..byte + count) {
            for (out, byte) in dst.iter_mut().zip(value) {
                *out |= byte;
            }
        }
    }
}

pub(super) fn header(bits: &mut Bits<'_>, fscod: u8, size_code: u8, mode: ChannelMode) {
    bits.put(0x0b77, 16);
    bits.put(0, 16); // CRC1 is written after the frame.
    bits.put(u32::from(fscod), 2);
    bits.put(u32::from(size_code), 6);
    bits.put(8, 5); // bsid
    bits.put(0, 3); // complete main program
    bits.put(mode.acmod(), 3);
    match mode {
        ChannelMode::Mono => {}
        ChannelMode::Stereo => bits.put(0, 2), // surround matrix unspecified
        ChannelMode::Surround51 => {
            bits.put(0, 2); // centre at -3 dB when downmixed
            bits.put(0, 2); // surround at -3 dB when downmixed
        }
    }
    bits.put(u32::from(mode == ChannelMode::Surround51), 1);
    bits.put(31, 5); // dialnorm leaves the decoded level unchanged
    bits.put(0b0000_1000, 8); // origbs, no optional metadata
}

pub(super) fn block(bits: &mut Bits<'_>, mode: ChannelMode, block: usize, snr: u16, pair: &Pair) {
    let channels = mode.coded_order().len();
    let lfe = mode == ChannelMode::Surround51;
    let full = channels - usize::from(lfe);
    let first = block == 0;
    let update = block.is_multiple_of(2);
    bits.put(0, 2 * full + 1); // long blocks, no dither, no dynamic range word
    bits.put(u32::from(first), 1); // coupling strategy
    if first {
        bits.put(0, 1); // no coupling
    }
    if mode == ChannelMode::Stereo {
        bits.put(u32::from(first), 1);
        if first {
            bits.put(0, 4); // no stereo rematrixing
        }
    }
    for &repeat in &pair.repeats[..full] {
        bits.put(
            if update {
                repeat.trailing_zeros() + 1
            } else {
                0
            },
            2,
        );
    }
    if lfe {
        bits.put(u32::from(update), 1);
    }
    if update {
        for &end in &pair.ends[..full] {
            bits.put(((end - MIN_FULL_BINS) / 3) as u32, 6);
        }
        for ch in 0..channels {
            let exp = &pair.exponents[ch];
            let end = pair.ends[ch];
            let is_lfe = end == LFE_BINS;
            let step = pair.repeats[ch];
            let mut previous = i32::from(exp[0]);
            bits.put(u32::from(exp[0]), 4);
            for group in exp[1..end].chunks_exact(3 * step) {
                let mut code = 0;
                for &exponent in group.iter().step_by(step) {
                    let exponent = i32::from(exponent);
                    code = 5 * code + (exponent - previous + 2) as u32;
                    previous = exponent;
                }
                bits.put(code, 7);
            }
            if !is_lfe {
                bits.put(0, 2); // gainrng is unused
            }
        }
    }
    bits.put(u32::from(first), 1); // bit allocation parameters
    if first {
        bits.put(u32::from(PARAMS.sdcycod), 2);
        bits.put(u32::from(PARAMS.fdcycod), 2);
        bits.put(u32::from(PARAMS.sgaincod), 2);
        bits.put(u32::from(PARAMS.dbpbcod), 2);
        bits.put(u32::from(PARAMS.floorcod), 3);
    }
    bits.put(u32::from(first), 1); // SNR offsets
    if first {
        bits.put(u32::from(snr >> 4), 6);
        for ch in 0..channels {
            bits.put(u32::from(snr & 15), 4);
            bits.put(u32::from(fast_gain(mode.is_lfe(ch))), 3);
        }
    }
    bits.put(0, 2); // no delta allocation or skipped data
}

pub(super) fn mantissa_bits(ends: &[usize; CHANNELS], baps: &[[u8; BLOCK]; CHANNELS]) -> usize {
    let mut counts = [0usize; 16];
    for (baps, &end) in baps.iter().zip(ends) {
        for &bap in &baps[..end] {
            counts[usize::from(bap)] += 1;
        }
    }
    5 * counts[1].div_ceil(3)
        + 7 * counts[2].div_ceil(3)
        + 3 * counts[3]
        + 7 * counts[4].div_ceil(2)
        + 4 * counts[5]
        + counts[6..]
            .iter()
            .zip(&BAP_BITS[6..])
            .map(|(&count, &bits)| count * usize::from(bits))
            .sum::<usize>()
}

#[derive(Default)]
struct Group {
    position: usize,
    value: u32,
    digits: usize,
}

const GROUPS: [(u32, usize, usize); 3] = [(3, 3, 5), (5, 3, 7), (11, 2, 7)];

#[derive(Default)]
pub(super) struct Mantissas {
    groups: [Group; GROUPS.len()],
}

impl Mantissas {
    pub(super) fn push(&mut self, bits: &mut Bits<'_>, bap: u8, sample: f32) {
        let index = match bap {
            1 => 0,
            2 => 1,
            4 => 2,
            3 => return bits.put(symmetric(sample, 7), 3),
            5 => return bits.put(symmetric(sample, 15), 4),
            _ => {
                let width = usize::from(BAP_BITS[usize::from(bap)]);
                let half = 1_i32 << (width - 1);
                let value = ((sample * half as f32).round() as i32).clamp(-half, half - 1);
                return bits.put((value as u32) & ((1 << width) - 1), width);
            }
        };
        let (levels, size, width) = GROUPS[index];
        let group = &mut self.groups[index];
        if group.digits == 0 {
            group.position = bits.position();
            bits.put(0, width);
        }
        group.value = levels * group.value + symmetric(sample, levels);
        group.digits += 1;
        if group.digits == size {
            bits.patch(group.position, group.value, width);
            *group = Group::default();
        }
    }

    pub(super) fn finish(self, bits: &mut Bits<'_>) {
        for (mut group, (levels, size, width)) in self.groups.into_iter().zip(GROUPS) {
            if group.digits != 0 {
                let zero = levels / 2;
                for _ in group.digits..size {
                    group.value = levels * group.value + zero;
                }
                bits.patch(group.position, group.value, width);
            }
        }
    }
}

fn symmetric(sample: f32, levels: u32) -> u32 {
    ((sample * levels as f32 + (levels - 1) as f32) * 0.5)
        .round()
        .clamp(0.0, (levels - 1) as f32) as u32
}

const REVERSE_CRC_BYTE: [u16; 256] = {
    let mut table = [0; 256];
    let mut i = 0;
    while i < table.len() {
        let mut crc = i as u16;
        let mut bit = 0;
        while bit < 8 {
            let feedback = crc & 1;
            crc = ((crc ^ (feedback * 0x8005)) >> 1) | (feedback << 15);
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

pub(super) fn checksum(frame: &mut [u8]) {
    let words = frame.len() / 2;
    let split = 2 * ((words >> 1) + (words >> 3));
    let mut leading = crc16(0, &frame[4..split]);
    // CRC1 precedes the covered data. Unshift the remainder through the region,
    // including the two bytes that will hold CRC1.
    for _ in 2..split {
        leading = (leading >> 8) ^ REVERSE_CRC_BYTE[usize::from(leading as u8)];
    }
    frame[2..4].copy_from_slice(&leading.to_be_bytes());
    let crc_at = frame.len() - 2;
    let mut trailing = crc16(0, &frame[split..crc_at]);
    if trailing == 0x0b77 {
        frame[crc_at - 1] ^= 1; // crcrsv avoids a false syncword.
        trailing = crc16(0, &frame[split..crc_at]);
    }
    frame[crc_at..].copy_from_slice(&trailing.to_be_bytes());
}
