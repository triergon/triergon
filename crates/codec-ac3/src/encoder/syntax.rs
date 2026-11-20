//! Syncframe syntax (A/52:2018 5.3), mantissas (7.3) and CRCs (7.10.1).

use super::{
    Allocation, BLOCK, CHANNELS, ChannelMode, ExponentSet, LFE_BINS, MAX_FRAME_BYTES,
    MIN_FULL_BINS, PARAMS, fast_gain,
};
use crate::bitalloc::Ac3BapCosts;
use crate::crc16;
use crate::spec_tables::BAPTAB;
use crate::tables::BAP_BITS;

const SKIP_LENGTH_BITS: usize = 9;
pub(super) const MAX_SKIP_BITS: usize = SKIP_LENGTH_BITS + ((1 << SKIP_LENGTH_BITS) - 1) * 8;

pub(super) fn skip_padding(bits: usize) -> usize {
    let partial = bits % MAX_SKIP_BITS;
    if partial == 0 {
        bits
    } else {
        bits - partial
            + SKIP_LENGTH_BITS
            + partial.saturating_sub(SKIP_LENGTH_BITS).next_multiple_of(8)
    }
}

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
        debug_assert!((1..=16).contains(&width));
        // A field of at most 16 bits fits in three bytes at any offset.
        // The two reserved flag bits and the CRC word leave that much space
        // at the last field's starting byte, including a grouped patch.
        if let Some([a, b, c]) = data.get_mut(byte..byte + 3) {
            let [first, second, third, _] = (value << (32 - offset - width)).to_be_bytes();
            *a |= first;
            *b |= second;
            *c |= third;
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

pub(super) fn block(
    bits: &mut Bits<'_>,
    mode: ChannelMode,
    block: usize,
    short_channels: u8,
    Allocation { snr, floor }: Allocation,
    set: &ExponentSet,
    update: bool,
) {
    let channels = mode.coded_order().len();
    let lfe = mode == ChannelMode::Surround51;
    let full = mode.full_channels();
    let first = block == 0;
    bits.put(u32::from(short_channels), full);
    bits.put(0, full + 1); // no dither or dynamic range word
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
    for &repeat in &set.repeats[..full] {
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
        for &end in &set.ends[..full] {
            bits.put(((end - MIN_FULL_BINS) / 3) as u32, 6);
        }
        for ch in 0..channels {
            let exp = &set.exponents[ch];
            let end = set.ends[ch];
            let is_lfe = end == LFE_BINS;
            let step = set.repeats[ch];
            bits.put(u32::from(exp[0]), 4);
            let groups = exp[1..end].chunks_exact(3 * step);
            if bits.data.is_none() {
                bits.position += groups.len() * 7;
            } else {
                let mut previous = i32::from(exp[0]);
                for group in groups {
                    let mut code = 0;
                    for &exponent in group.iter().step_by(step) {
                        let exponent = i32::from(exponent);
                        code = 5 * code + (exponent - previous + 2) as u32;
                        previous = exponent;
                    }
                    bits.put(code, 7);
                }
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
        bits.put(u32::from(floor), 3);
    }
    bits.put(u32::from(first), 1); // SNR offsets
    if first {
        bits.put(u32::from(snr >> 4), 6);
        for ch in 0..channels {
            bits.put(u32::from(snr & 15), 4);
            bits.put(u32::from(fast_gain(mode.is_lfe(ch))), 3);
        }
    }
    bits.put(0, 1); // no delta allocation
}

pub(super) fn skip_field(bits: &mut Bits<'_>, padding: usize) {
    bits.put(u32::from(padding != 0), 1);
    if padding != 0 {
        let bytes = (padding - SKIP_LENGTH_BITS) / 8;
        bits.put(bytes as u32, SKIP_LENGTH_BITS);
        bits.position += bytes * 8; // The output buffer starts with all bits clear.
    }
}

// A block has at most 1,536 coefficients. One sum holds the direct bit
// count and the three grouped-quantizer counts with room for every coefficient.
const DIRECT_BITS: u32 = 15;
const GROUP_BITS: u32 = 11;
const COSTS: [u64; 16] = {
    assert!(CHANNELS * BLOCK * 16 < 1 << DIRECT_BITS);
    assert!(CHANNELS * BLOCK < 1 << GROUP_BITS);
    let mut costs = [0; 16];
    costs[1] = 1 << DIRECT_BITS;
    costs[2] = 1 << (DIRECT_BITS + GROUP_BITS);
    costs[3] = 3;
    costs[4] = 1 << (DIRECT_BITS + 2 * GROUP_BITS);
    costs[5] = 4;
    let mut code = 6;
    while code < costs.len() {
        costs[code] = BAP_BITS[code] as u64;
        code += 1;
    }
    costs
};

pub(super) static BAP_COSTS: Ac3BapCosts = {
    let mut table = [0; 64];
    let mut address = 0;
    while address < table.len() {
        table[address] = COSTS[BAPTAB[address] as usize];
        address += 1;
    }
    Ac3BapCosts::new(&table)
};

pub(super) fn mantissa_bits(sum: u64) -> usize {
    let group = |index: u32| {
        ((sum >> (DIRECT_BITS + GROUP_BITS * index)) & ((1 << GROUP_BITS) - 1)) as usize
    };
    let direct = (sum & ((1 << DIRECT_BITS) - 1)) as usize;
    direct + 5 * group(0).div_ceil(3) + 7 * group(1).div_ceil(3) + 7 * group(2).div_ceil(2)
}

#[cfg(test)]
pub(super) fn allocated_mantissa_bits(
    ends: &[usize; CHANNELS],
    planes: &[[u8; BLOCK]; CHANNELS],
) -> usize {
    mantissa_bits(
        planes
            .iter()
            .zip(ends)
            .flat_map(|(plane, &end)| &plane[..end])
            .map(|&code| COSTS[usize::from(code)])
            .sum(),
    )
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
                let value = round_mantissa(sample * half as f32).clamp(-half, half - 1);
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
    let top = (levels - 1) as i32;
    round_mantissa((sample * levels as f32 + top as f32) * 0.5).clamp(0, top) as u32
}

#[inline]
fn round_mantissa(sample: f32) -> i32 {
    if cfg!(target_arch = "arm") {
        crate::quant::round_to_i32(sample)
    } else {
        sample.round() as i32
    }
}

const REVERSE_CRC_POWERS: usize = MAX_FRAME_BYTES.next_power_of_two().ilog2() as usize;

const fn reverse_crc_byte(mut crc: u16) -> u16 {
    let mut bit = 0;
    while bit < 8 {
        let feedback = crc & 1;
        crc = ((crc ^ (feedback * 0x8005)) >> 1) | (feedback << 15);
        bit += 1;
    }
    crc
}

#[inline]
const fn apply_crc_shift(table: &[[u16; 16]; 4], crc: u16) -> u16 {
    table[0][(crc & 15) as usize]
        ^ table[1][((crc >> 4) & 15) as usize]
        ^ table[2][((crc >> 8) & 15) as usize]
        ^ table[3][(crc >> 12) as usize]
}

// A reverse shift is linear over GF(2). Square each shift to cover twice as
// many zero bytes and store its contribution from each of the four nibbles.
const REVERSE_CRC_SHIFTS: [[[u16; 16]; 4]; REVERSE_CRC_POWERS] = {
    let mut table = [[[0; 16]; 4]; REVERSE_CRC_POWERS];
    let mut basis = [0; 16];
    let mut bit = 0;
    while bit < basis.len() {
        basis[bit] = reverse_crc_byte(1 << bit);
        bit += 1;
    }
    let mut power = 0;
    while power < table.len() {
        let mut nibble = 0;
        while nibble < 4 {
            let mut value = 1_usize;
            while value < 16 {
                let lowest = value.trailing_zeros() as usize;
                table[power][nibble][value] =
                    table[power][nibble][value & (value - 1)] ^ basis[nibble * 4 + lowest];
                value += 1;
            }
            nibble += 1;
        }
        let mut bit = 0;
        while bit < basis.len() {
            basis[bit] = apply_crc_shift(&table[power], basis[bit]);
            bit += 1;
        }
        power += 1;
    }
    table
};

fn reverse_crc(mut crc: u16, mut bytes: usize) -> u16 {
    while bytes != 0 {
        let power = bytes.trailing_zeros() as usize;
        bytes &= bytes - 1;
        crc = apply_crc_shift(&REVERSE_CRC_SHIFTS[power], crc);
    }
    crc
}

pub(super) fn first_partition_bytes(frame_bytes: usize) -> usize {
    // A/52:2018 7.10.1 truncates each term in 16-bit words (Table 7.34).
    let words = frame_bytes / 2;
    2 * ((words >> 1) + (words >> 3))
}

pub(super) fn checksum(frame: &mut [u8]) {
    let split = first_partition_bytes(frame.len());
    // CRC1 precedes the covered data. Unshift the remainder through the region,
    // including the two bytes that will hold CRC1.
    let leading = reverse_crc(crc16(0, &frame[4..split]), split - 2);
    frame[2..4].copy_from_slice(&leading.to_be_bytes());
    let crc_at = frame.len() - 2;
    let mut trailing = crc16(0, &frame[split..crc_at]);
    if trailing == 0x0b77 {
        frame[crc_at - 1] ^= 1; // crcrsv avoids a false syncword.
        trailing = crc16(0, &frame[split..crc_at]);
    }
    frame[crc_at..].copy_from_slice(&trailing.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_reverse_shifts_match_each_byte() {
        for bit in 0..16 {
            let original = 1 << bit;
            let mut expected = original;
            for bytes in 0..MAX_FRAME_BYTES {
                assert_eq!(reverse_crc(original, bytes), expected);
                expected = reverse_crc_byte(expected);
            }
        }
    }

    #[test]
    fn fields_fit_before_the_reserved_tail() {
        for width in 1..=16 {
            for byte_offset in 0..8 {
                let mut data = [0; 6];
                let latest = data.len() * 8 - 18 - width;
                let start = latest - (latest + 8 - byte_offset) % 8;
                let value = (0xaaaa_u32 >> (16 - width)) | 1;
                let mut writer = Bits::write_to(&mut data);
                writer.patch(start, value, width);

                let mut expected = [0; 6];
                for i in 0..width {
                    let at = start + i;
                    let bit = (value >> (width - 1 - i)) & 1;
                    expected[at / 8] |= (bit as u8) << (7 - at % 8);
                }
                assert_eq!(data, expected);
            }
        }
    }

    #[test]
    fn packed_mantissa_cost_matches_grouped_coding() {
        let verify = |ends, planes: &[[u8; BLOCK]; CHANNELS]| {
            let mut bits = Bits::count();
            let mut mantissas = Mantissas::default();
            for (plane, end) in planes.iter().zip(ends) {
                for &code in &plane[..end] {
                    if code != 0 {
                        mantissas.push(&mut bits, code, 0.25);
                    }
                }
            }
            mantissas.finish(&mut bits);
            assert_eq!(allocated_mantissa_bits(&ends, planes), bits.position());
        };
        for code in 0..16 {
            verify([BLOCK; CHANNELS], &[[code; BLOCK]; CHANNELS]);
        }
        let mixed = core::array::from_fn(|channel| {
            core::array::from_fn(|bin| ((bin + channel) % 16) as u8)
        });
        verify([73, 109, 151, 253, 253, 7], &mixed);
    }
}
