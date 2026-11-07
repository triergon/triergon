//! The zero-extending reader of the decoders' inner loops.

/// How a [`Reader`] gets the bits at its position. [`Direct`] and [`Cached`] are the two
/// implementations.
pub trait Window: Copy {
    /// The window of a reader starting at bit `position` of `data`.
    fn new(data: &[u8], position: usize) -> Self;

    /// The bits at bit `position` of `data`, left aligned. At least the top 32 are stream
    /// bits, inside the part of `data` this window can reach.
    fn at(&mut self, data: &[u8], position: usize) -> u64;
}

/// One unaligned eight-byte load per read. It has no state, so its cost does not depend
/// on the widths of the fields; this suits fixed-width fields and short tables.
///
/// A load that does not fit in the slice reads as zero, so the slice carries eight bytes
/// of padding after its last field. Assembling a short tail here instead costs AC-3 about
/// 1% more instructions.
#[derive(Clone, Copy, Debug)]
pub struct Direct;

impl Window for Direct {
    fn new(_: &[u8], _: usize) -> Self {
        Direct
    }

    #[inline(always)]
    fn at(&mut self, data: &[u8], position: usize) -> u64 {
        let byte = position >> 3;
        // Written out: going through `load` costs AC-3 about 2% more instructions.
        let bytes = match data.get(byte..byte + 8) {
            Some(slice) => {
                let mut bytes = [0; 8];
                bytes.copy_from_slice(slice);
                bytes
            }
            None => [0; 8],
        };
        u64::from_be_bytes(bytes) << (position & 7)
    }
}

/// The eight bytes at `byte`, when they lie inside `data`.
#[inline(always)]
fn load(data: &[u8], byte: usize) -> Option<[u8; 8]> {
    let slice = data.get(byte..byte + 8)?;
    let mut bytes = [0; 8];
    bytes.copy_from_slice(slice);
    Some(bytes)
}

/// The last bytes of a slice, zero extended.
#[cold]
fn tail(data: &[u8], byte: usize) -> [u8; 8] {
    let mut bytes = [0; 8];
    if let Some(tail) = data.get(byte..) {
        bytes[..tail.len()].copy_from_slice(tail);
    }
    bytes
}

/// Eight bytes held in a register: a read is a shift of that word, and consecutive
/// codewords do not wait for a memory load. This suits variable-length codes, at the
/// price of a load branch that a stream of short fixed-width fields predicts badly.
/// Bits past the end of the slice read as zero.
#[derive(Clone, Copy, Debug)]
pub struct Cached {
    word: u64,
    start: usize,
}

impl Window for Cached {
    fn new(data: &[u8], position: usize) -> Self {
        let start = position & !7;
        let byte = start >> 3;
        let bytes = load(data, byte).unwrap_or_else(|| tail(data, byte));
        Cached {
            word: u64::from_be_bytes(bytes),
            start,
        }
    }

    #[inline(always)]
    fn at(&mut self, data: &[u8], position: usize) -> u64 {
        let offset = position.wrapping_sub(self.start);
        if offset > 32 {
            *self = Self::new(data, position);
            return self.word << (position & 7);
        }
        self.word << offset
    }
}

/// MSB-first bits of a slice. Reads past what the window can reach yield zero bits and
/// never fail, so a decode loop tests [`Reader::overrun`] at syntax boundaries instead of
/// branching on every field.
#[derive(Clone, Copy, Debug)]
pub struct Reader<'a, W> {
    data: &'a [u8],
    position: usize,
    end: usize,
    window: W,
}

/// The reader for fixed-width fields and short tables, over a slice padded as [`Direct`]
/// describes.
pub type BitReader<'a> = Reader<'a, Direct>;

/// The reader for variable-length codes.
pub type CachedBitReader<'a> = Reader<'a, Cached>;

impl<'a, W: Window> Reader<'a, W> {
    /// A reader at the first bit of `data`, ending at its last bit.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self::at(data, 0)
    }

    /// A reader `position` bits into `data`.
    #[must_use]
    pub fn at(data: &'a [u8], position: usize) -> Self {
        Self {
            data,
            position,
            end: data.len() * 8,
            window: W::new(data, position),
        }
    }

    #[inline(always)]
    fn window(&mut self) -> u64 {
        self.window.at(self.data, self.position)
    }

    /// The next 32 bits without consuming them.
    #[inline(always)]
    pub fn peek32(&mut self) -> u32 {
        (self.window() >> 32) as u32
    }

    /// The next `width` bits (at most 32) as an unsigned value.
    #[inline(always)]
    pub fn read(&mut self, width: u32) -> u32 {
        debug_assert!(width <= 32);
        let value = ((self.window() >> 1) >> (63 - width)) as u32;
        self.position += width as usize;
        value
    }

    /// The next `width` bits (1 to 32) as a two's complement value.
    #[inline(always)]
    pub fn read_signed(&mut self, width: u32) -> i32 {
        debug_assert!((1..=32).contains(&width));
        let value = ((self.window() as i64) >> (64 - width)) as i32;
        self.position += width as usize;
        value
    }

    /// The next bit as a flag.
    #[inline(always)]
    pub fn bit(&mut self) -> bool {
        self.read(1) != 0
    }

    /// Passes over `count` bits.
    #[inline(always)]
    pub fn skip(&mut self, count: usize) {
        self.position += count;
    }

    /// Moves to bit `position`.
    #[inline]
    pub fn seek(&mut self, position: usize) {
        self.position = position;
    }

    /// Moves to the next multiple of `bits` from the start of the slice.
    #[inline]
    pub fn align(&mut self, bits: usize) {
        self.position = self.position.next_multiple_of(bits);
    }

    /// Bits consumed since the start of the slice.
    #[inline(always)]
    #[must_use]
    pub fn position(&self) -> usize {
        self.position
    }

    /// Bits left before the end, negative after an overrun.
    #[inline(always)]
    #[must_use]
    pub fn left(&self) -> isize {
        self.end as isize - self.position as isize
    }

    /// True when a read went past the end.
    #[inline(always)]
    #[must_use]
    pub fn overrun(&self) -> bool {
        self.position > self.end
    }

    /// The end position in bits.
    #[must_use]
    pub fn end(&self) -> usize {
        self.end
    }

    /// Sets the position at which [`Reader::overrun`] and [`Reader::left`] measure
    /// the end. The bytes the reader returns do not change.
    pub fn set_end(&mut self, end: usize) {
        self.end = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_msb_first_and_zero_extends() {
        let data = [0b1010_1100, 0xff, 0x01];
        let mut reader = CachedBitReader::new(&data);
        assert_eq!(reader.read(1), 1);
        assert_eq!(reader.read(3), 0b010);
        assert_eq!(reader.read(8), 0b1100_1111);
        assert_eq!(reader.read(12), 0b1111_0000_0001);
        assert!(!reader.overrun());
        assert_eq!(reader.left(), 0);
        assert_eq!(reader.read(5), 0);
        assert!(reader.overrun());
        assert_eq!(reader.left(), -5);
    }

    #[test]
    fn direct_reads_a_load_that_does_not_fit_as_zero() {
        let data = [0xff; 9];
        let mut reader = BitReader::new(&data);
        reader.seek(8);
        assert_eq!(reader.read(32), u32::MAX);
        reader.seek(9 * 8 - 64 + 8);
        assert_eq!(reader.read(32), 0);
        assert_eq!(BitReader::new(&[0xff; 7]).peek32(), 0);
    }

    #[test]
    fn signed_fields() {
        let data = [0b1011_0000, 0xff, 0x00, 0x80];
        let mut reader = CachedBitReader::new(&data);
        reader.skip(8);
        assert_eq!(reader.read_signed(4), -1);
        assert_eq!(reader.read(12), 0xf00);
        assert_eq!(reader.read_signed(8), -128);
        reader.seek(8);
        assert_eq!(reader.read_signed(24), -65408);
        reader.skip(10_000);
        assert_eq!(reader.read(32), 0);
    }

    #[test]
    fn every_width_at_every_alignment_matches_a_scalar_read() {
        let data: Vec<u8> = (0..256u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        widths::<Direct>(&data, (data.len() - 8) * 8);
        widths::<Cached>(&data, data.len() * 8 + 16);
    }

    fn widths<W: Window>(data: &[u8], positions: usize) {
        let scalar = |position: usize, width: usize| {
            (position..position + width).fold(0u32, |value, at| {
                let bit = data
                    .get(at / 8)
                    .map_or(0, |byte| (byte >> (7 - at % 8)) & 1);
                (value << 1) | u32::from(bit)
            })
        };
        for position in 0..positions {
            for width in 0..=32 {
                let mut reader = Reader::<W>::at(data, position);
                assert_eq!(reader.read(width as u32), scalar(position, width));
            }
        }
        // A sequence that keeps the cached word across reads, and moves backwards.
        let mut reader = Reader::<W>::new(data);
        let mut position = 0;
        for width in (0..=32).chain((0..=32).rev()) {
            assert_eq!(reader.peek32(), scalar(position, 32));
            assert_eq!(reader.read(width as u32), scalar(position, width));
            position += width;
        }
        reader.seek(3);
        assert_eq!(reader.read(32), scalar(3, 32));
    }

    #[test]
    fn end_can_move_without_changing_the_bytes() {
        let mut reader = CachedBitReader::new(&[0xff, 0xff]);
        reader.set_end(4);
        assert_eq!(reader.end(), 4);
        assert_eq!(reader.read(6), 0x3f);
        assert!(reader.overrun());
    }

    #[test]
    fn align_counts_from_the_slice_start() {
        let mut reader = BitReader::new(&[0; 8]);
        reader.skip(17);
        reader.align(32);
        assert_eq!(reader.position(), 32);
        reader.align(32);
        assert_eq!(reader.position(), 32);
    }
}
