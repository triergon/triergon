/// MSB-first bit writer over a reusable byte vector.
#[derive(Debug, Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    acc: u64,
    used: u32,
}

impl BitWriter {
    pub fn with_capacity(bytes: usize) -> BitWriter {
        BitWriter {
            bytes: Vec::with_capacity(bytes),
            acc: 0,
            used: 0,
        }
    }

    /// Append the low `len` bits of `value` (len <= 32).
    #[inline]
    pub fn put(&mut self, value: u32, len: u32) {
        debug_assert!(len <= 32 && (len == 32 || value >> len == 0));
        self.acc = (self.acc << len) | value as u64;
        self.used += len;
        if self.used >= 32 {
            self.used -= 32;
            let word = (self.acc >> self.used) as u32;
            self.bytes.extend_from_slice(&word.to_be_bytes());
        }
    }

    /// Bits written so far, including the unflushed tail.
    pub fn bit_len(&self) -> usize {
        self.bytes.len() * 8 + self.used as usize
    }

    /// Pad with zero bits to the next byte boundary.
    pub fn align(&mut self) {
        let pad = (8 - self.used % 8) % 8;
        self.put(0, pad);
        while self.used >= 8 {
            self.used -= 8;
            self.bytes.push((self.acc >> self.used) as u8);
        }
    }

    /// Byte-aligned start code `00 00 01 xx`.
    pub fn start_code(&mut self, code: u8) {
        self.align();
        self.put(0x0000_0100 | code as u32, 32);
    }

    /// Hand out the aligned bytes and keep the allocation for the next picture.
    pub fn take_into(&mut self, out: &mut Vec<u8>) {
        self.align();
        out.clear();
        out.extend_from_slice(&self.bytes);
        self.bytes.clear();
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
        w.take_into(&mut out);
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
