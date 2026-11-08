//! Fields and codewords cut from a window of a packet.

use triergon_bitstream::LsbReader;

/// Bits of one window.
const WINDOW: u32 = 32;

/// The next [`WINDOW`] bits of a reader, which advances past the bits consumed only when
/// the window moves on or is dropped: a run of reads costs a shift and a mask each, in
/// registers. The bits read the same as through the reader, zeros past its end included.
#[derive(Debug)]
pub(crate) struct BitWindow<'r, 'a> {
    reader: &'r mut LsbReader<'a>,
    window: u64,
    used: u32,
}

impl<'r, 'a> BitWindow<'r, 'a> {
    #[inline(always)]
    pub(crate) fn new(reader: &'r mut LsbReader<'a>) -> BitWindow<'r, 'a> {
        let window = u64::from(reader.peek(WINDOW));
        BitWindow {
            reader,
            window,
            used: 0,
        }
    }

    /// The unread bits of the window: `WINDOW - used` of them, then zeros.
    #[inline(always)]
    pub(crate) fn ahead(&self) -> u32 {
        (self.window >> self.used) as u32
    }

    /// True if the window holds `width` more bits.
    #[inline(always)]
    pub(crate) fn holds(&self, width: u32) -> bool {
        self.used + width <= WINDOW
    }

    /// Consumes `width` bits the window holds.
    #[inline(always)]
    pub(crate) fn consume(&mut self, width: u32) {
        debug_assert!(self.holds(width));
        self.used += width;
    }

    /// Moves the window to the first unread bit, so that it holds [`WINDOW`] bits.
    #[inline(always)]
    pub(crate) fn refill(&mut self) {
        self.reader.skip(self.used);
        self.used = 0;
        self.window = u64::from(self.reader.peek(WINDOW));
    }

    /// An unsigned field of `width` bits, at most 32.
    #[inline(always)]
    pub(crate) fn read(&mut self, width: u32) -> u32 {
        if !self.holds(width) {
            self.refill();
        }
        let value = u64::from(self.ahead()) & ((1 << width) - 1);
        self.consume(width);
        value as u32
    }
}

impl Drop for BitWindow<'_, '_> {
    #[inline(always)]
    fn drop(&mut self) {
        self.reader.skip(self.used);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_cross_windows_as_through_the_reader() {
        let data: Vec<u8> = (0..64u8).map(|i| i.wrapping_mul(91) ^ 0x5a).collect();
        let mut reader = LsbReader::new(&data);
        let mut through = LsbReader::new(&data);
        {
            let mut window = BitWindow::new(&mut reader);
            for width in 1..=24 {
                assert_eq!(window.read(width), through.read(width), "width {width}");
            }
        }
        assert_eq!(reader.position(), through.position());
        let mut left = reader.left() as u32;
        let mut window = BitWindow::new(&mut reader);
        while left > 0 {
            let width = left.min(WINDOW);
            assert_eq!(window.read(width), through.read(width));
            left -= width;
        }
        let past = window.read(32);
        drop(window);
        assert_eq!(past, 0, "zeros past the end");
        assert!(reader.overrun());
    }
}
