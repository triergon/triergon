//! Context-adaptive binary arithmetic decoding, common to H.264 clause 9.3 and H.265
//! clause 9.3. A context state byte is `(pStateIdx << 1) | valMPS`.
//!
//! [`Engine`] keeps the 9-bit offset in the top bits of a 64-bit register with look-ahead
//! stream bits below it, so a comparison uses a constant shift and renormalisation is a
//! shift plus an occasional 32-bit refill off the critical path.

/// `rangeTabLPS[pStateIdx][qCodIRangeIdx]` (H.264 Table 9-44, H.265 Table 9-52).
const RANGE_TAB_LPS: [[u8; 4]; 64] = [
    [128, 176, 208, 240],
    [128, 167, 197, 227],
    [128, 158, 187, 216],
    [123, 150, 178, 205],
    [116, 142, 169, 195],
    [111, 135, 160, 185],
    [105, 128, 152, 175],
    [100, 122, 144, 166],
    [95, 116, 137, 158],
    [90, 110, 130, 150],
    [85, 104, 123, 142],
    [81, 99, 117, 135],
    [77, 94, 111, 128],
    [73, 89, 105, 122],
    [69, 85, 100, 116],
    [66, 80, 95, 110],
    [62, 76, 90, 104],
    [59, 72, 86, 99],
    [56, 69, 81, 94],
    [53, 65, 77, 89],
    [51, 62, 73, 85],
    [48, 59, 69, 80],
    [46, 56, 66, 76],
    [43, 53, 63, 72],
    [41, 50, 59, 69],
    [39, 48, 56, 65],
    [37, 45, 54, 62],
    [35, 43, 51, 59],
    [33, 41, 48, 56],
    [32, 39, 46, 53],
    [30, 37, 43, 50],
    [29, 35, 41, 48],
    [27, 33, 39, 45],
    [26, 31, 37, 43],
    [24, 30, 35, 41],
    [23, 28, 33, 39],
    [22, 27, 32, 37],
    [21, 26, 30, 35],
    [20, 24, 29, 33],
    [19, 23, 27, 31],
    [18, 22, 26, 30],
    [17, 21, 25, 28],
    [16, 20, 23, 27],
    [15, 19, 22, 25],
    [14, 18, 21, 24],
    [14, 17, 20, 23],
    [13, 16, 19, 22],
    [12, 15, 18, 21],
    [12, 14, 17, 20],
    [11, 14, 16, 19],
    [11, 13, 15, 18],
    [10, 12, 15, 17],
    [10, 12, 14, 16],
    [9, 11, 13, 15],
    [9, 11, 12, 14],
    [8, 10, 12, 14],
    [8, 9, 11, 13],
    [7, 9, 11, 12],
    [7, 9, 10, 12],
    [7, 8, 10, 11],
    [6, 8, 9, 11],
    [6, 7, 9, 10],
    [6, 7, 8, 9],
    [2, 2, 2, 2],
];

/// transIdxLPS (H.264 Table 9-45, H.265 Table 9-53).
const TRANS_IDX_LPS: [u8; 64] = [
    0, 0, 1, 2, 2, 4, 4, 5, 6, 7, 8, 9, 9, 11, 11, 12, 13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21,
    21, 22, 22, 23, 24, 24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33, 33, 33, 34,
    34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
];

/// transIdxMPS (H.264 Table 9-45, H.265 Table 9-53).
const TRANS_IDX_MPS: [u8; 64] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50,
    51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 62, 63,
];

/// The four rangeTabLPS values of a state packed into one word (qCodIRangeIdx k in byte k), so
/// the range-dependent selection is a shift instead of a dependent table load.
static LPS_ROW: [u32; 128] = {
    let mut table = [0; 128];
    let mut state = 0;
    while state < 128 {
        let row = RANGE_TAB_LPS[state >> 1];
        table[state] =
            row[0] as u32 | (row[1] as u32) << 8 | (row[2] as u32) << 16 | (row[3] as u32) << 24;
        state += 1;
    }
    table
};
/// The next state after a most probable symbol.
static NEXT_MPS: [u8; 128] = {
    let mut table = [0; 128];
    let mut state = 0;
    while state < 128 {
        table[state] = (TRANS_IDX_MPS[state >> 1] << 1) | (state as u8 & 1);
        state += 1;
    }
    table
};
/// The next state after a least probable symbol. valMPS toggles at pStateIdx 0.
static NEXT_LPS: [u8; 128] = {
    let mut table = [0; 128];
    let mut state = 0;
    while state < 128 {
        let mps = (state as u8 & 1) ^ ((state >> 1) == 0) as u8;
        table[state] = (TRANS_IDX_LPS[state >> 1] << 1) | mps;
        state += 1;
    }
    table
};

/// The arithmetic decoding engine. Reads past the end of the slice yield zero bits; a
/// decoder bounds a corrupt stream through [`Engine::bits_read`] or [`Engine::next_byte`].
///
/// `HEADROOM` keeps one bit free above the offset and starts with seven bytes of look-ahead
/// instead of four. A conforming stream decodes the same either way, because its offset
/// stays below the range and it never asks for more bypass bins than the look-ahead holds.
/// A corrupt stream can do both, and then the register layout decides the result, so each
/// decoder keeps the layout that fixes its own results on such input.
///
/// Hot loops can work on a local copy, which keeps the registers in machine registers.
#[derive(Clone, Copy, Debug)]
pub struct Engine<'a, const HEADROOM: bool> {
    data: &'a [u8],
    next: usize,
    range: u32,
    value: u64,
    /// Look-ahead bits below the offset.
    available: i32,
}

impl<'a, const HEADROOM: bool> Engine<'a, HEADROOM> {
    /// Bit of the register that holds the lowest bit of the offset.
    const OFFSET_SHIFT: u32 = if HEADROOM { 54 } else { 55 };

    /// Initialises the engine at byte `start` of `data`: ivlCurrRange = 510 and
    /// ivlOffset = read_bits(9).
    #[must_use]
    pub fn new(data: &'a [u8], start: usize) -> Self {
        let mut engine = Self {
            data,
            next: start,
            range: 510,
            value: 0,
            available: 0,
        };
        engine.restart(start);
        engine
    }

    /// Initialises the engine again at byte `start`, after PCM samples or at a substream.
    pub fn restart(&mut self, start: usize) {
        self.next = start;
        self.range = 510;
        if HEADROOM {
            let mut bytes = 0;
            for k in 0..7 {
                bytes = (bytes << 8) | u64::from(*self.data.get(start + k).unwrap_or(&0));
            }
            self.value = bytes << 7;
            self.next = start + 7;
            self.available = 56 - 9;
        } else {
            self.value = self.load() << 32;
            self.available = 32 - 9;
        }
    }

    #[inline(always)]
    fn load(&mut self) -> u64 {
        let at = self.next;
        self.next += 4;
        if at + 4 <= self.data.len() {
            u64::from(u32::from_be_bytes([
                self.data[at],
                self.data[at + 1],
                self.data[at + 2],
                self.data[at + 3],
            ]))
        } else {
            let mut value = 0;
            for k in 0..4 {
                value = (value << 8) | u64::from(*self.data.get(at + k).unwrap_or(&0));
            }
            value
        }
    }

    /// Tops the look-ahead up to at least 16 bits.
    #[inline(always)]
    pub fn refill(&mut self) {
        if self.available < 16 {
            self.append();
        }
    }

    /// Appends 32 bits below the look-ahead. A corrupt stream's long [`Engine::bypass_bits`]
    /// can get here with too much look-ahead for 32 more bits, and the shift then wraps.
    #[inline(always)]
    fn append(&mut self) {
        let word = self.load();
        let shift = Self::OFFSET_SHIFT as i32 - self.available - 32;
        self.value |= word.wrapping_shl(shift as u32);
        self.available += 32;
    }

    /// DecodeDecision with branchless interval selection and clz renormalisation.
    #[inline(always)]
    pub fn decision(&mut self, state: &mut u8) -> u32 {
        let bin = self.decision_unfilled(state);
        self.refill();
        bin
    }

    /// [`Engine::decision`] without the refill. A decision consumes at most 6 bits, so two
    /// of them may follow one [`Engine::refill`].
    #[inline(always)]
    pub fn decision_unfilled(&mut self, state: &mut u8) -> u32 {
        let current = (*state & 127) as usize;
        let row = LPS_ROW[current];
        let lps = (row >> ((self.range >> 3) & 0x18)) & 0xff;
        let mps = self.range - lps;
        let scaled = u64::from(mps) << Self::OFFSET_SHIFT;
        let is_lps = self.value >= scaled;
        // Renormalisation shifts of both outcomes are computed in parallel.
        let shift_lps = lps.leading_zeros() - 23;
        let shift_mps = u32::from(mps < 256);
        let (value, range, shift) = if is_lps {
            (self.value - scaled, lps, shift_lps)
        } else {
            (self.value, mps, shift_mps)
        };
        *state = if is_lps {
            NEXT_LPS[current]
        } else {
            NEXT_MPS[current]
        };
        self.range = range << shift;
        self.value = value << shift;
        self.available -= shift as i32;
        (current as u32 & 1) ^ u32::from(is_lps)
    }

    /// DecodeBypass.
    #[inline(always)]
    pub fn bypass(&mut self) -> u32 {
        let range = u64::from(self.range);
        let bin;
        if HEADROOM {
            self.value <<= 1;
            let scaled = range << Self::OFFSET_SHIFT;
            bin = self.value >= scaled;
            self.value -= scaled & u64::from(bin).wrapping_neg();
        } else {
            // The top 10 bits are (ivlOffset << 1) | next bit.
            let scaled = range << (Self::OFFSET_SHIFT - 1);
            bin = self.value >= scaled;
            self.value -= scaled & u64::from(bin).wrapping_neg();
            self.value <<= 1;
        }
        self.available -= 1;
        self.refill();
        u32::from(bin)
    }

    /// The next `count` bypass bins, without consuming them. At least `count` bits of
    /// look-ahead must be available, which [`Engine::refill`] assures up to 16. Applied `count` times,
    /// DecodeBypass is a long division: the bin string is the quotient of
    /// `(ivlOffset << count) | next count bits` by ivlCurrRange.
    #[inline(always)]
    #[must_use]
    pub fn peek_bypass(&self, count: u32) -> u32 {
        ((self.value >> (Self::OFFSET_SHIFT - count)) / u64::from(self.range)) as u32
    }

    /// Consumes `count` bypass bins whose bin string is `bins`, as [`Engine::peek_bypass`]
    /// or a prefix of it gave it. The caller refills afterwards.
    #[inline(always)]
    pub fn consume_bypass(&mut self, count: u32, bins: u32) {
        let top = self.value >> (Self::OFFSET_SHIFT - count);
        let remainder = top - u64::from(bins) * u64::from(self.range);
        let rest = self.value & ((1u64 << (Self::OFFSET_SHIFT - count)) - 1);
        self.value = (remainder << Self::OFFSET_SHIFT) | (rest << count);
        self.available -= count as i32;
    }

    /// `count` bypass bins, first bin in the most significant position. A conforming
    /// stream asks for at most 16.
    #[inline(always)]
    pub fn bypass_bits(&mut self, count: u32) -> u32 {
        if count <= 1 {
            return if count == 1 { self.bypass() } else { 0 };
        }
        if self.available < count as i32 {
            self.append();
        }
        let bins = self.peek_bypass(count);
        self.consume_bypass(count, bins);
        self.refill();
        bins
    }

    /// Counts a unary prefix of bypass 1-bins ended by a 0-bin, which is consumed. Stops
    /// at `max` ones. Peeks 8 bins per division and consumes only what it uses.
    #[inline(always)]
    pub fn bypass_unary(&mut self, max: u32) -> u32 {
        let mut count = 0;
        loop {
            let bins = self.peek_bypass(8);
            let ones = (bins as u8).leading_ones();
            if ones < 8 {
                self.consume_bypass(ones + 1, bins >> (7 - ones));
                self.refill();
                return (count + ones).min(max);
            }
            self.consume_bypass(8, bins);
            self.refill();
            count += 8;
            if count >= max {
                return max;
            }
        }
    }

    /// DecodeTerminate.
    #[inline(always)]
    pub fn terminate(&mut self) -> u32 {
        self.range -= 2;
        let scaled = u64::from(self.range) << Self::OFFSET_SHIFT;
        if self.value >= scaled {
            1
        } else {
            if self.range < 256 {
                self.range <<= 1;
                self.value <<= 1;
                self.available -= 1;
                self.refill();
            }
            0
        }
    }

    /// Bits the engine has read, the look-ahead excluded.
    #[inline]
    #[must_use]
    pub fn bits_read(&self) -> usize {
        (self.next * 8).wrapping_sub(self.available as usize)
    }

    /// The byte position after the last bit read, rounded up to a boundary: where PCM samples
    /// or the next substream start after a terminate bin.
    #[inline]
    #[must_use]
    pub fn aligned_byte_position(&self) -> usize {
        self.bits_read().div_ceil(8)
    }

    /// The next byte the engine would load.
    #[inline]
    #[must_use]
    pub fn next_byte(&self) -> usize {
        self.next
    }

    /// The slice the engine reads.
    #[must_use]
    pub fn data(&self) -> &'a [u8] {
        self.data
    }
}

/// The state of a context from its (m, n) pair at `slice_qp` (H.264 9.3.1.1, H.265 9.3.2.2).
#[inline]
#[must_use]
pub fn init_state(m: i32, n: i32, slice_qp: i32) -> u8 {
    let pre = (((m * slice_qp.clamp(0, 51)) >> 4) + n).clamp(1, 126);
    if pre <= 63 {
        ((63 - pre) << 1) as u8
    } else {
        (((pre - 64) << 1) | 1) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bit-serial implementation of the specified process for cross-checking.
    struct Serial<'a> {
        data: &'a [u8],
        position: usize,
        range: u32,
        offset: u32,
    }

    impl Serial<'_> {
        fn bit(&mut self) -> u32 {
            let byte = self.data.get(self.position / 8).copied().unwrap_or(0);
            let bit = (byte >> (7 - self.position % 8)) & 1;
            self.position += 1;
            u32::from(bit)
        }

        fn new(data: &[u8]) -> Serial<'_> {
            let mut engine = Serial {
                data,
                position: 0,
                range: 510,
                offset: 0,
            };
            for _ in 0..9 {
                engine.offset = (engine.offset << 1) | engine.bit();
            }
            engine
        }

        fn decision(&mut self, state: &mut u8) -> u32 {
            let p = (*state >> 1) as usize;
            let mps = u32::from(*state & 1);
            let lps = u32::from(RANGE_TAB_LPS[p][((self.range >> 6) & 3) as usize]);
            self.range -= lps;
            let bin;
            if self.offset >= self.range {
                bin = 1 - mps;
                self.offset -= self.range;
                self.range = lps;
                let next_mps = if p == 0 { 1 - mps } else { mps };
                *state = (TRANS_IDX_LPS[p] << 1) | next_mps as u8;
            } else {
                bin = mps;
                *state = (TRANS_IDX_MPS[p] << 1) | mps as u8;
            }
            while self.range < 256 {
                self.range <<= 1;
                self.offset = (self.offset << 1) | self.bit();
            }
            bin
        }

        fn bypass(&mut self) -> u32 {
            self.offset = (self.offset << 1) | self.bit();
            if self.offset >= self.range {
                self.offset -= self.range;
                1
            } else {
                0
            }
        }
    }

    /// Both layouts, driven the same way, follow the serial process.
    fn follows_the_serial_process<const HEADROOM: bool>() {
        let mut random = noise(0x1234_5678);
        let data: Vec<u8> = (0..4096).map(|_| random() as u8).collect();
        let mut engine = Engine::<HEADROOM>::new(&data, 0);
        let mut serial = Serial::new(&data);
        let mut fast: Vec<u8> = (0..64).map(|i| init_state(i - 32, 60, 30)).collect();
        let mut slow = fast.clone();
        for i in 0..20000 {
            let k = (random() % 64) as usize;
            match random() % 8 {
                0 => assert_eq!(engine.bypass(), serial.bypass(), "bypass {i}"),
                1 => {
                    let count = 2 + random() % 21;
                    let expected = (0..count).fold(0, |bins, _| (bins << 1) | serial.bypass());
                    assert_eq!(engine.bypass_bits(count), expected, "bypass bits {i}");
                }
                2 => {
                    let max = 1 + random() % 32;
                    let mut ones = 0;
                    while ones < max && serial.bypass() == 1 {
                        ones += 1;
                    }
                    assert_eq!(engine.bypass_unary(max), ones, "unary {i}");
                    // The engine consumes whole peeks at the limit; follow it.
                    while serial.position < engine.bits_read() {
                        serial.bypass();
                    }
                }
                _ => {
                    assert_eq!(
                        engine.decision(&mut fast[k]),
                        serial.decision(&mut slow[k]),
                        "decision {i}"
                    );
                    assert_eq!(fast[k], slow[k]);
                }
            }
            assert_eq!(engine.bits_read(), serial.position, "position {i}");
        }
    }

    #[test]
    fn engine_matches_the_serial_process() {
        follows_the_serial_process::<false>();
        follows_the_serial_process::<true>();
    }

    /// A corrupt stream's reads of 24 to 31 bins, past the room for one more word.
    fn over_long_bypass_reads<const HEADROOM: bool>() -> u64 {
        let mut random = noise(0x0bad_cafe);
        let data: Vec<u8> = (0..4096).map(|_| random() as u8).collect();
        let mut engine = Engine::<HEADROOM>::new(&data, 0);
        let mut digest = 0xcbf2_9ce4_8422_2325u64;
        for _ in 0..4000 {
            let count = 2 + random() % 30;
            let bins = u64::from(engine.bypass_bits(count));
            digest = (digest ^ bins).wrapping_mul(0x0100_0000_01b3);
        }
        digest
    }

    #[test]
    fn over_long_bypass_reads_are_the_same_in_every_build() {
        assert_eq!(over_long_bypass_reads::<false>(), 15052386680786097469);
        assert_eq!(over_long_bypass_reads::<true>(), 5587722021355268864);
    }

    #[test]
    fn headroom_decides_a_corrupt_offset() {
        // Nine set bits: an offset of 511 against a range of 510, which no conforming
        // stream starts with. A bypass bin leaves 512, which needs the free bit.
        let data = [0xff, 0x80, 0, 0, 0, 0, 0, 0];
        let mut with = Engine::<true>::new(&data, 0);
        let mut without = Engine::<false>::new(&data, 0);
        assert_eq!((with.bypass(), without.bypass()), (1, 1));
        assert_eq!((with.terminate(), without.terminate()), (1, 0));
    }

    fn noise(mut seed: u32) -> impl FnMut() -> u32 {
        move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        }
    }

    #[test]
    fn terminate_and_restart() {
        let mut data = [0u8; 16];
        let mut engine = Engine::<false>::new(&data, 0);
        assert_eq!(engine.terminate(), 0);
        for _ in 0..40 {
            assert_eq!(engine.bypass(), 0);
        }
        assert_eq!(engine.terminate(), 0);
        // An offset of 510 is past any range after a terminate.
        data[3] = 0xff;
        data[4] = 0x00;
        let mut engine = Engine::<true>::new(&data, 3);
        assert_eq!(engine.bits_read(), 3 * 8 + 9);
        assert_eq!(engine.terminate(), 1);
        assert_eq!(engine.aligned_byte_position(), 5);
        engine.restart(0);
        assert_eq!(engine.bits_read(), 9);
    }

    #[test]
    fn reads_past_the_end_yield_zero_bits() {
        let mut engine = Engine::<false>::new(&[], 0);
        for _ in 0..1000 {
            assert_eq!(engine.bypass(), 0);
        }
        assert_eq!(engine.next_byte() % 4, 0);
        assert!(engine.bits_read() > 1000);
    }

    #[test]
    fn states_stay_in_range() {
        for m in -128..=127 {
            for n in [-128, -16, 0, 64, 127] {
                for qp in [-3, 0, 26, 51, 60] {
                    assert!(init_state(m, n, qp) >> 1 <= 62);
                }
            }
        }
    }

    #[test]
    fn neutral_context() {
        // m = 0, n = 64: preCtxState 64, pStateIdx 0, valMPS 1.
        assert_eq!(init_state(0, 64, 26), 1);
        // m = 0, n = 63: preCtxState 63, pStateIdx 0, valMPS 0.
        assert_eq!(init_state(0, 63, 26), 0);
    }
}
