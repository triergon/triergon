//! NAL unit payloads: emulation prevention bytes and the RBSP behind them.

use crate::{BitError, BitSource};

/// Copies a NAL unit payload into `out` without its emulation prevention bytes. Unlike
/// [`Rbsp`] this validates nothing: every `03` after two zeros goes, whatever follows it.
pub fn unescape(ebsp: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(ebsp.len());
    let length = ebsp.len();
    let mut i = 0;
    let mut run = 0;
    while i + 2 < length {
        // A byte above 3 rules out a pattern at any of these three positions.
        if ebsp[i + 2] > 3 {
            i += 3;
        } else if ebsp[i] == 0 && ebsp[i + 1] == 0 && ebsp[i + 2] == 3 {
            out.extend_from_slice(&ebsp[run..i + 2]);
            i += 3;
            run = i;
        } else {
            i += 1;
        }
    }
    out.extend_from_slice(&ebsp[run..]);
}

/// Reads the RBSP of a NAL unit payload in place, removing emulation prevention bytes as
/// it goes, so parsing a prefix costs no copy of the suffix.
///
/// [`Rbsp::new`] rejects a payload that a conforming encoder cannot produce: `00 00`
/// followed by `00`, `01` or `02`. [`Rbsp::lenient`] accepts those bytes as data. Both
/// reject an emulation prevention byte followed by a byte above `03`.
#[derive(Clone, Copy, Debug)]
pub struct Rbsp<'a> {
    rest: &'a [u8],
    byte: u8,
    remaining: u8,
    zeroes: u8,
    strict: bool,
}

impl<'a> Rbsp<'a> {
    /// A strict reader at the first payload byte.
    #[must_use]
    pub fn new(payload: &'a [u8]) -> Self {
        Self {
            rest: payload,
            byte: 0,
            remaining: 0,
            zeroes: 0,
            strict: true,
        }
    }

    /// A lenient reader at the first payload byte.
    #[must_use]
    pub fn lenient(payload: &'a [u8]) -> Self {
        Self {
            strict: false,
            ..Self::new(payload)
        }
    }

    /// True when the payload has no set bit left: the last bit read was the
    /// rbsp_stop_one_bit or lies after it.
    #[must_use]
    pub fn stop_bit_consumed(&self) -> bool {
        self.rest.is_empty() && u32::from(self.remaining) <= self.byte.trailing_zeros()
    }

    fn next_byte(&mut self) -> Result<u8, BitError> {
        let (&first, rest) = self.rest.split_first().ok_or(BitError::End)?;
        let (byte, rest) = match first {
            3 if self.zeroes == 2 => {
                let (&next, rest) = rest.split_first().ok_or(BitError::End)?;
                if next > 3 {
                    return Err(BitError::InvalidPrevention);
                }
                self.zeroes = 0;
                (next, rest)
            }
            0..=2 if self.zeroes == 2 && self.strict => {
                return Err(BitError::MissingPrevention);
            }
            _ => (first, rest),
        };
        self.rest = rest;
        self.zeroes = if byte == 0 {
            (self.zeroes + 1).min(2)
        } else {
            0
        };
        Ok(byte)
    }
}

impl BitSource for Rbsp<'_> {
    #[inline]
    fn bit(&mut self) -> Result<u32, BitError> {
        if self.remaining == 0 {
            self.byte = self.next_byte()?;
            self.remaining = 8;
        }
        self.remaining -= 1;
        Ok(u32::from((self.byte >> self.remaining) & 1))
    }

    /// Takes the bits of one byte at a time.
    #[inline]
    fn read(&mut self, mut width: u32) -> Result<u32, BitError> {
        let mut value = 0;
        while width != 0 {
            if self.remaining == 0 {
                self.byte = self.next_byte()?;
                self.remaining = 8;
            }
            let count = width.min(u32::from(self.remaining));
            self.remaining -= count as u8;
            let bits = u32::from(self.byte >> self.remaining) & ((1 << count) - 1);
            value = (value << count) | bits;
            width -= count;
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(mut reader: Rbsp<'_>) -> Result<Vec<u32>, BitError> {
        let mut out = Vec::new();
        loop {
            match reader.read(8) {
                Ok(byte) => out.push(byte),
                Err(BitError::End) => return Ok(out),
                Err(error) => return Err(error),
            }
        }
    }

    #[test]
    fn removes_prevention_bytes() {
        let payload = [1, 0, 0, 3, 1, 0, 0, 3, 0, 0, 3, 3, 9];
        let expected = vec![1, 0, 0, 1, 0, 0, 0, 0, 3, 9];
        assert_eq!(bytes(Rbsp::new(&payload)), Ok(expected.clone()));
        assert_eq!(bytes(Rbsp::lenient(&payload)), Ok(expected));
    }

    #[test]
    fn read_matches_single_bits_at_every_alignment() {
        let payload = [0xa5, 0, 0, 3, 1, 0x5a, 0xc3, 0, 0, 3, 0, 0xff, 0x81];
        for skip in 0..16 {
            for width in 0..=32 {
                let mut bits = Rbsp::new(&payload);
                let mut fields = Rbsp::new(&payload);
                for _ in 0..skip {
                    bits.bit().unwrap();
                    fields.bit().unwrap();
                }
                let expected = (0..width).fold(0, |value, _| (value << 1) | bits.bit().unwrap());
                assert_eq!(fields.read(width), Ok(expected));
            }
        }
    }

    #[test]
    fn prevention_errors() {
        for reader in [Rbsp::new, Rbsp::lenient] {
            assert_eq!(
                bytes(reader(&[0, 0, 3, 4])),
                Err(BitError::InvalidPrevention)
            );
            // A prevention byte at the very end has nothing to protect.
            assert_eq!(bytes(reader(&[0, 0, 3])), Ok(vec![0, 0]));
        }
        assert_eq!(
            bytes(Rbsp::new(&[0, 0, 1])),
            Err(BitError::MissingPrevention)
        );
        assert_eq!(bytes(Rbsp::lenient(&[0, 0, 1])), Ok(vec![0, 0, 1]));
        assert_eq!(
            bytes(Rbsp::lenient(&[0, 0, 0, 0, 3, 1])),
            Ok(vec![0, 0, 0, 0, 1])
        );
    }

    #[test]
    fn unescape_matches_the_byte_by_byte_rule() {
        let cases: [&[u8]; 8] = [
            &[],
            &[0, 0],
            &[0, 0, 3],
            &[0, 0, 0, 3, 3],
            &[1, 0, 0, 3, 1, 0, 0, 3],
            &[0, 0, 3, 0, 0, 3, 0, 0, 3],
            &[9, 9, 9, 9, 0, 0, 3, 9],
            &[0, 0, 3, 4, 0, 0, 1],
        ];
        for case in cases {
            let mut expected = Vec::new();
            let mut zeros = 0;
            for &byte in case {
                if zeros >= 2 && byte == 3 {
                    zeros = 0;
                    continue;
                }
                zeros = if byte == 0 { zeros + 1 } else { 0 };
                expected.push(byte);
            }
            let mut out = vec![7];
            unescape(case, &mut out);
            assert_eq!(out, expected);
        }
    }

    #[test]
    fn stop_bit() {
        let mut reader = Rbsp::new(&[0b1010_0000]);
        for _ in 0..2 {
            reader.bit().unwrap();
            assert!(!reader.stop_bit_consumed());
        }
        reader.bit().unwrap();
        assert!(reader.stop_bit_consumed());
    }
}
