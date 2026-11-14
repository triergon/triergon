use triergon_core::{Budget, BudgetVec, Error};

/// MSB-first bit writer over a charged byte buffer of a fixed size: the bound of
/// what it is used for, charged before any of it is written. It never grows, so a
/// picture cannot be refused storage halfway.
#[derive(Debug)]
pub(crate) struct BitWriter {
    bytes: BudgetVec<u8>,
    /// Bytes written.
    len: usize,
    /// The `used` (< 32) bits not yet written, in its low bits; higher bits
    /// are left over from written ones and never read.
    acc: u32,
    used: u32,
    /// Output went past the bound and was dropped (the bound rules this out).
    overflow: bool,
}

/// What `BitWriter` reports when its output exceeded the bound it was sized for.
const OVERFLOW: Error = Error::Policy("picture exceeds its size bound");

impl BitWriter {
    pub(crate) fn try_new(budget: &Budget, capacity: usize) -> Result<BitWriter, Error> {
        Ok(BitWriter {
            bytes: BudgetVec::try_filled(budget, 0, capacity)?,
            len: 0,
            acc: 0,
            used: 0,
            overflow: false,
        })
    }

    /// Append the low `len` bits of `value` (len <= 32).
    #[inline]
    pub(crate) fn put(&mut self, value: u32, len: u32) {
        debug_assert!(len <= 32 && (len == 32 || value >> len == 0));
        let free = 32 - self.used;
        if len < free {
            self.acc = (self.acc << len) | value;
            self.used += len;
        } else {
            // `value`'s top `free` bits complete a word; `spill` bits remain.
            let spill = len - free;
            let word = self.acc.checked_shl(free).unwrap_or(0) | value >> spill;
            self.emit(word.to_be_bytes());
            self.acc = value & ((1u32 << spill) - 1);
            self.used = spill;
        }
    }

    #[inline]
    fn emit<const N: usize>(&mut self, bytes: [u8; N]) {
        if let Some(dst) = self.bytes.get_mut(self.len..self.len + N) {
            dst.copy_from_slice(&bytes);
            self.len += N;
        } else {
            self.overflow = true;
        }
    }

    /// Discard the written bytes, keeping the unflushed tail; returns how many
    /// were discarded. For a writer that only counts bits.
    pub(crate) fn discard_flushed(&mut self) -> usize {
        std::mem::take(&mut self.len)
    }

    /// Bits written so far, including the unflushed tail.
    pub(crate) fn bit_len(&self) -> usize {
        self.len * 8 + self.used as usize
    }

    /// Pad with zero bits to the next byte boundary.
    pub(crate) fn align(&mut self) {
        let pad = (8 - self.used % 8) % 8;
        self.put(0, pad);
        while self.used >= 8 {
            self.used -= 8;
            self.emit([(self.acc >> self.used) as u8]);
        }
    }

    /// Byte-aligned start code `00 00 01 xx`.
    pub(crate) fn start_code(&mut self, code: u8) {
        self.align();
        self.put(0x0000_0100 | code as u32, 32);
    }

    /// Hand out the aligned bytes and keep the allocation for the next picture.
    /// `out` grows to exactly the picture's size when it is smaller.
    pub(crate) fn take_into(&mut self, out: &mut Vec<u8>) -> Result<(), Error> {
        self.align();
        let written = std::mem::take(&mut self.len);
        self.check_bound()?;
        out.clear();
        out.reserve_exact(written);
        out.extend_from_slice(&self.bytes[..written]);
        Ok(())
    }

    /// Whether everything written fitted the bound; clears the report.
    pub(crate) fn check_bound(&mut self) -> Result<(), Error> {
        if std::mem::take(&mut self.overflow) {
            Err(OVERFLOW)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
impl Default for BitWriter {
    /// An empty writer on a pool of its own, for unit tests.
    fn default() -> Self {
        BitWriter::try_new(&Budget::new(1 << 20).unwrap(), 4096).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_msb_first_and_aligns() {
        let mut w = BitWriter::default();
        w.put(0b101, 3);
        w.put(0xABCDE, 20);
        w.put(1, 1);
        w.start_code(0xB3);
        w.put(0b1, 1);
        let mut out = Vec::new();
        w.take_into(&mut out).unwrap();
        // 101 1010 1011 1100 1101 1110 1 0000000 | 00 00 01 B3 | 1 0000000
        assert_eq!(
            out,
            [
                0b1011_0101,
                0b0111_1001,
                0b1011_1101,
                0x00,
                0x00,
                0x01,
                0xB3,
                0x80
            ]
        );
    }
}
