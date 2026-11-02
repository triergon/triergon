//! Independent bounded zlib/DEFLATE reader, specified by RFC 1950 and RFC 1951.
//! No preset dictionary is available in Matroska ContentCompression.
use crate::{Error, Result};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug)]
pub struct InflateLimits {
    pub output_bytes: usize,
    pub expansion_ratio: usize,
    pub work: u64,
}

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}
impl Bits<'_> {
    fn peek(&self, width: usize) -> u32 {
        let start = self.at / 8;
        let remaining = &self.data[start..];
        let word = if remaining.len() >= 4 {
            u32::from_le_bytes(remaining[..4].try_into().expect("four-byte slice"))
        } else {
            remaining
                .iter()
                .enumerate()
                .fold(0, |v, (i, b)| v | (u32::from(*b) << (i * 8)))
        };
        (word >> (self.at % 8)) & ((1 << width) - 1)
    }
    fn take(&mut self, width: usize) -> Result<u32> {
        if self.data.len().saturating_mul(8).saturating_sub(self.at) < width {
            return Err(Error::Truncated);
        }
        let end = self.at.checked_add(width).ok_or(Error::Overflow)?;
        let value = self.peek(width);
        self.at = end;
        Ok(value)
    }
    fn align(&mut self) -> Result<()> {
        self.at = self.at.checked_add(7).ok_or(Error::Overflow)? / 8 * 8;
        Ok(())
    }
}

struct Huffman {
    quick: [u16; 512],
    counts: [u16; 16],
    first: [u16; 16],
    offsets: [u16; 16],
    symbols: [u16; 288],
    max: usize,
}
impl Huffman {
    fn build(lengths: &[u8], code_lengths: bool) -> Result<Self> {
        let mut tree = Self {
            quick: [0; 512],
            counts: [0; 16],
            first: [0; 16],
            offsets: [0; 16],
            symbols: [0; 288],
            max: 0,
        };
        for &length in lengths {
            if length > 15 {
                return Err(Error::Invalid("Huffman length"));
            }
            if length != 0 {
                tree.counts[usize::from(length)] += 1;
                tree.max = tree.max.max(usize::from(length));
            }
        }
        let mut available = 1_i32;
        for count in tree.counts.iter().skip(1) {
            available = available * 2 - i32::from(*count);
            if available < 0 {
                return Err(Error::Invalid("oversubscribed Huffman tree"));
            }
        }
        if available != 0 && (code_lengths || tree.max != 1) && tree.max != 0 {
            return Err(Error::Invalid("incomplete Huffman tree"));
        }
        if code_lengths && tree.max == 0 {
            return Err(Error::Invalid("empty code-length tree"));
        }
        let mut offset = 0;
        let mut code = 0;
        for length in 1..=15 {
            code = (code + tree.counts[length - 1]) << 1;
            tree.first[length] = code;
            tree.offsets[length] = offset;
            offset += tree.counts[length];
        }
        let mut next = tree.first;
        let mut positions = tree.offsets;
        for (symbol, &length) in lengths.iter().enumerate() {
            if length == 0 {
                continue;
            }
            let length = usize::from(length);
            tree.symbols[usize::from(positions[length])] = symbol as u16;
            positions[length] += 1;
            if length <= 9 {
                let reversed = (next[length].reverse_bits() >> (16 - length)) as usize;
                for key in (reversed..512).step_by(1 << length) {
                    tree.quick[key] = ((symbol as u16) << 4) | length as u16;
                }
            }
            next[length] += 1;
        }
        Ok(tree)
    }
    fn symbol(&self, bits: &mut Bits<'_>) -> Result<usize> {
        let entry = self.quick[bits.peek(9) as usize];
        if entry != 0 {
            bits.take(usize::from(entry & 15))?;
            return Ok(usize::from(entry >> 4));
        }
        let mut code = 0_u16;
        for length in 1..=self.max {
            code = (code << 1) | bits.take(1)? as u16;
            if let Some(index) = code.checked_sub(self.first[length]) {
                if index < self.counts[length] {
                    return Ok(usize::from(
                        self.symbols[usize::from(self.offsets[length] + index)],
                    ));
                }
            }
        }
        Err(Error::Invalid("invalid Huffman code"))
    }
}

fn dynamic(bits: &mut Bits<'_>) -> Result<(Huffman, Huffman)> {
    let literals = bits.take(5)? as usize + 257;
    let distances = bits.take(5)? as usize + 1;
    let codes = bits.take(4)? as usize + 4;
    if literals > 286 {
        return Err(Error::Invalid("literal alphabet size"));
    }
    let order = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let mut lengths = [0_u8; 19];
    for &index in &order[..codes] {
        lengths[index] = bits.take(3)? as u8;
    }
    let tree = Huffman::build(&lengths, true)?;
    let mut lengths = [0_u8; 318];
    let total = literals + distances;
    let mut at = 0;
    while at < total {
        let code = tree.symbol(bits)?;
        let (value, count) = match code {
            0..=15 => (code as u8, 1),
            16 if at != 0 => (lengths[at - 1], bits.take(2)? as usize + 3),
            17 => (0, bits.take(3)? as usize + 3),
            18 => (0, bits.take(7)? as usize + 11),
            _ => return Err(Error::Invalid("code-length repeat without predecessor")),
        };
        if at + count > total {
            return Err(Error::Invalid("code-length repeat overrun"));
        }
        lengths[at..at + count].fill(value);
        at += count;
    }
    if lengths[256] == 0 {
        return Err(Error::Invalid("missing end-of-block code"));
    }
    Ok((
        Huffman::build(&lengths[..literals], false)?,
        Huffman::build(&lengths[literals..total], false)?,
    ))
}

fn fixed() -> &'static (Huffman, Huffman) {
    static TREES: std::sync::OnceLock<(Huffman, Huffman)> = std::sync::OnceLock::new();
    TREES.get_or_init(|| {
        let mut lengths = [8; 288];
        lengths[144..256].fill(9);
        lengths[256..280].fill(7);
        (
            Huffman::build(&lengths, false).expect("complete fixed literal tree"),
            Huffman::build(&[5; 32], false).expect("complete fixed distance tree"),
        )
    })
}

fn reserve(out: &mut Vec<u8>, extra: usize, limit: usize) -> Result<()> {
    let total = out.len().checked_add(extra).ok_or(Error::Overflow)?;
    if total > limit {
        return Err(Error::Limit("inflated output bytes or expansion ratio"));
    }
    if total > out.capacity() {
        let target = total.max(out.capacity().saturating_mul(2)).min(limit);
        out.try_reserve_exact(target - out.len())
            .map_err(|_| Error::Limit("inflate allocation"))?;
    }
    Ok(())
}

fn adler32(bytes: &[u8], cancel: &AtomicBool) -> Result<u32> {
    let (mut a, mut b) = (1_u32, 0_u32);
    for chunk in bytes.chunks(5552) {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    Ok((b << 16) | a)
}

/// Decode one complete RFC 1950 stream, with checked output, ratio and work limits.
/// No bytes are returned until the checksum has been verified.
pub fn decompress(data: &[u8], limits: InflateLimits, cancel: &AtomicBool) -> Result<Vec<u8>> {
    if data.len() < 6 {
        return Err(Error::Truncated);
    }
    if data[0] & 15 != 8 || data[0] >> 4 > 7 || u16::from_be_bytes([data[0], data[1]]) % 31 != 0 {
        return Err(Error::Invalid("zlib header"));
    }
    if data[1] & 32 != 0 {
        return Err(Error::Unsupported("preset zlib dictionary"));
    }
    let window = 1_usize << ((data[0] >> 4) + 8);
    let limit = limits
        .output_bytes
        .min(data.len().saturating_mul(limits.expansion_ratio));
    let mut bits = Bits {
        data: &data[2..data.len() - 4],
        at: 0,
    };
    let mut out = Vec::new();
    let mut work = limits.work;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        work = work.checked_sub(1).ok_or(Error::Limit("inflate work"))?;
        let last = bits.take(1)? != 0;
        let kind = bits.take(2)?;
        if kind == 0 {
            bits.align()?;
            let length = bits.take(16)? as usize;
            if bits.take(16)? as u16 != !(length as u16) {
                return Err(Error::Invalid("stored block complement"));
            }
            work = work
                .checked_sub(length as u64)
                .ok_or(Error::Limit("inflate work"))?;
            reserve(&mut out, length, limit)?;
            let start = bits.at / 8;
            let end = start.checked_add(length).ok_or(Error::Overflow)?;
            let bytes = bits.data.get(start..end).ok_or(Error::Truncated)?;
            out.extend_from_slice(bytes);
            bits.at = bits
                .at
                .checked_add(length.checked_mul(8).ok_or(Error::Overflow)?)
                .ok_or(Error::Overflow)?;
        } else if kind <= 2 {
            let trees;
            let (literals, distances) = if kind == 1 {
                fixed()
            } else {
                trees = dynamic(&mut bits)?;
                &trees
            };
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                work = work.checked_sub(1).ok_or(Error::Limit("inflate work"))?;
                let symbol = literals.symbol(&mut bits)?;
                if symbol < 256 {
                    reserve(&mut out, 1, limit)?;
                    out.push(symbol as u8);
                } else if symbol == 256 {
                    break;
                } else {
                    const LENGTH: [usize; 29] = [
                        3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59,
                        67, 83, 99, 115, 131, 163, 195, 227, 258,
                    ];
                    const EXTRA: [usize; 29] = [
                        0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5,
                        5, 5, 5, 0,
                    ];
                    const DIST: [usize; 30] = [
                        1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513,
                        769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
                    ];
                    if symbol > 285 {
                        return Err(Error::Invalid("reserved length symbol"));
                    }
                    let length = LENGTH[symbol - 257] + bits.take(EXTRA[symbol - 257])? as usize;
                    let code = distances.symbol(&mut bits)?;
                    if code >= 30 {
                        return Err(Error::Invalid("reserved distance symbol"));
                    }
                    let extra = if code < 4 { 0 } else { code / 2 - 1 };
                    let distance = DIST[code] + bits.take(extra)? as usize;
                    if distance > out.len() || distance > window {
                        return Err(Error::Invalid("DEFLATE distance outside window"));
                    }
                    work = work
                        .checked_sub(length as u64)
                        .ok_or(Error::Limit("inflate work"))?;
                    reserve(&mut out, length, limit)?;
                    let end = out.len() + length;
                    if distance == 1 {
                        let value = out[out.len() - 1];
                        out.resize(end, value);
                    } else {
                        let start = out.len() - distance;
                        while out.len() < end {
                            let count = (end - out.len()).min(out.len() - start);
                            out.extend_from_within(start..start + count);
                        }
                    }
                }
            }
        } else {
            return Err(Error::Invalid("reserved DEFLATE block type"));
        }
        if last {
            break;
        }
    }
    bits.align()?;
    if bits.at / 8 != bits.data.len() {
        return Err(Error::Invalid("trailing zlib bytes"));
    }
    let expected = u32::from_be_bytes(
        data[data.len() - 4..]
            .try_into()
            .expect("four checksum bytes"),
    );
    if adler32(&out, cancel)? != expected {
        return Err(Error::Invalid("Adler-32 mismatch"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_trees_and_cancel_are_explicit_errors() {
        assert!(Huffman::build(&[1, 1, 1], false).is_err());
        assert!(Huffman::build(&[2], false).is_err());
        assert!(Huffman::build(&[0; 19], true).is_err());
        let input = [120, 1, 1, 0, 0, 255, 255, 0, 0, 0, 1];
        let limits = InflateLimits {
            output_bytes: 32,
            expansion_ratio: 1024,
            work: 100,
        };
        assert!(matches!(
            decompress(&input, limits, &AtomicBool::new(true)),
            Err(Error::Cancelled)
        ));
        assert!(
            decompress(&input, limits, &AtomicBool::new(false))
                .unwrap()
                .is_empty()
        );
    }
}
