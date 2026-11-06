//! Independent bounded zlib/DEFLATE reader, specified by RFC 1950 and RFC 1951.
//! No preset dictionary is available in Matroska ContentCompression.
use crate::{Error, Result};
use std::sync::atomic::{AtomicBool, Ordering};

mod incremental;
pub use incremental::Inflater;

/// Charged blocking decoder. A single final stored stream preserves the legacy
/// exact-capacity copy and checksum path; other streams drive the resumable
/// decoder. Source and output stay in the same live-capacity pool.
pub fn decompress_in(
    source: &crate::storage::SharedBytes,
    limits: InflateLimits,
    cancel: &AtomicBool,
    pool: &triergon_core::Budget,
) -> Result<crate::storage::SharedBytes> {
    if !source.belongs_to(pool) {
        return Err(triergon_core::Error::ForeignPool);
    }
    if let Some(range) = stored_range(source, limits, cancel)? {
        // Stored length is known and validated. Speculative capacity here can
        // exhaust the existing cumulative decoded-capacity policy for later
        // laces, although all decoded bytes fit. Keep the policy, reserve only
        // this payload, and leave every allocation attached to its pool charge.
        if range.is_empty() {
            return Ok(crate::storage::SharedBytes::default());
        }
        let mut output = triergon_core::BudgetVec::new(pool);
        output.try_reserve_exact(range.len())?;
        output.try_extend_from_slice(&source[range])?;
        let bytes = output.freeze().map_err(|error| error.error)?;
        return Ok(crate::storage::SharedBytes::from_core(bytes));
    }
    let mut decoder = Inflater::new(source.clone(), limits, pool)?;
    loop {
        match decoder.step(&mut triergon_core::WorkBudget::new(8192), cancel)? {
            crate::incremental::ParseStep::Event(bytes) => return Ok(bytes),
            crate::incremental::ParseStep::Yield => (),
            crate::incremental::ParseStep::Backpressure {
                requested,
                available,
            } => {
                return Err(Error::Backpressure {
                    requested,
                    available,
                });
            }
            _ => return Err(Error::Invalid("unexpected inflate input transition")),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct InflateLimits {
    pub output_bytes: usize,
    pub expansion_ratio: usize,
    pub work: u64,
}

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
    reservoir: u64,
    available: usize,
}
impl Bits<'_> {
    #[inline]
    fn fill(&mut self, width: usize) {
        debug_assert!(width <= 16);
        if self.available >= width {
            return;
        }
        let remaining = &self.data[self.at..];
        if remaining.len() >= 8 {
            let word = u64::from_le_bytes(remaining[..8].try_into().expect("eight-byte slice"));
            // Six complete bytes fit even with fifteen buffered bits. The other
            // two loaded bytes are masked out, preserving a zero high-bit invariant.
            self.reservoir |= (word & 0x0000_ffff_ffff_ffff) << self.available;
            self.at += 6;
            self.available += 48;
        } else {
            while self.available < width && self.at < self.data.len() {
                self.reservoir |= u64::from(self.data[self.at]) << self.available;
                self.at += 1;
                self.available += 8;
            }
        }
    }
    #[inline]
    fn peek(&mut self, width: usize) -> u32 {
        self.fill(width);
        (self.reservoir & ((1_u64 << width) - 1)) as u32
    }
    #[inline]
    fn take(&mut self, width: usize) -> Result<u32> {
        self.fill(width);
        if self.available < width {
            return Err(Error::Truncated);
        }
        let value = (self.reservoir & ((1_u64 << width) - 1)) as u32;
        self.reservoir >>= width;
        self.available -= width;
        Ok(value)
    }
    fn align(&mut self) -> Result<()> {
        self.take(self.available % 8)?;
        Ok(())
    }
    fn position(&self) -> usize {
        self.at - self.available / 8
    }
    fn skip_bytes(&mut self, length: usize) -> Result<()> {
        let end = self.position().checked_add(length).ok_or(Error::Overflow)?;
        if end > self.data.len() {
            return Err(Error::Truncated);
        }
        self.at = end;
        self.reservoir = 0;
        self.available = 0;
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
            if let Some(index) = code.checked_sub(self.first[length])
                && index < self.counts[length]
            {
                return Ok(usize::from(
                    self.symbols[usize::from(self.offsets[length] + index)],
                ));
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

fn reserve(out: &mut Vec<u8>, extra: usize, limit: usize, initial: usize) -> Result<()> {
    let total = out.len().checked_add(extra).ok_or(Error::Overflow)?;
    if total > limit {
        return Err(Error::Policy("inflated output bytes or expansion ratio"));
    }
    if total > out.capacity() {
        let target = total
            .max(out.capacity().saturating_mul(2))
            .max(if out.capacity() == 0 { initial } else { 0 })
            .min(limit);
        out.try_reserve_exact(target - out.len())
            .map_err(|_| Error::Policy("inflate allocation"))?;
    }
    Ok(())
}

fn adler32(bytes: &[u8], cancel: &AtomicBool) -> Result<u32> {
    let (mut a, mut b) = (1_u32, 0_u32);
    // Combine four groups before updating the sixteen independent lanes. This
    // shortens the sum recurrence while retaining safe, compiler-vectorized math.
    // A 4096-byte chunk bounds first <= 65280; wide combination avoids overflow.
    for chunk in bytes.chunks(4096) {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let mut first = [0_u16; 16];
        let mut second = [0_u32; 16];
        let mut groups = chunk.chunks_exact(64);
        let count = groups.len();
        for group in &mut groups {
            for i in 0..16 {
                let x = u16::from(group[i]);
                let y = u16::from(group[i + 16]);
                let z = u16::from(group[i + 32]);
                let w = u16::from(group[i + 48]);
                second[i] += 4 * u32::from(first[i]) + u32::from(4 * x + 3 * y + 2 * z + w);
                first[i] += x + y + z + w;
            }
        }
        let mut sum = 0_u32;
        let mut weighted = 0_u64;
        let mut repeated = 0_u64;
        for i in 0..16 {
            sum += u32::from(first[i]);
            weighted += i as u64 * u64::from(first[i]);
            repeated += u64::from(second[i]);
        }
        b = ((u64::from(b) + count as u64 * 64 * u64::from(a) + 16 * repeated - weighted) % 65521)
            as u32;
        a = (a + sum) % 65521;
        for &byte in groups.remainder() {
            a += u32::from(byte);
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    Ok((b << 16) | a)
}

fn header(data: &[u8]) -> Result<usize> {
    if data.len() < 6 {
        return Err(Error::Truncated);
    }
    if data[0] & 15 != 8
        || data[0] >> 4 > 7
        || !u16::from_be_bytes([data[0], data[1]]).is_multiple_of(31)
    {
        return Err(Error::Invalid("zlib header"));
    }
    if data[1] & 32 != 0 {
        return Err(Error::Unsupported("preset zlib dictionary"));
    }
    Ok(1_usize << ((data[0] >> 4) + 8))
}

/// Validate a single final stored block and borrow its decoded payload in-place.
/// The same checksum and resource rules apply to this packet-buffer fast path.
pub(crate) fn stored_range(
    data: &[u8],
    limits: InflateLimits,
    cancel: &AtomicBool,
) -> Result<Option<std::ops::Range<usize>>> {
    header(data)?;
    if data[2] & 7 != 1 {
        return Ok(None);
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    if data.len() < 11 {
        return Err(Error::Truncated);
    }
    let length = usize::from(u16::from_le_bytes([data[3], data[4]]));
    if u16::from_le_bytes([data[5], data[6]]) != !(length as u16) {
        return Err(Error::Invalid("stored block complement"));
    }
    if data.len() != length + 11 {
        return Err(if data.len() < length + 11 {
            Error::Truncated
        } else {
            Error::Invalid("trailing zlib bytes")
        });
    }
    if length
        > limits
            .output_bytes
            .min(data.len().saturating_mul(limits.expansion_ratio))
    {
        return Err(Error::Policy("inflated output bytes or expansion ratio"));
    }
    if length as u64 + 1 > limits.work {
        return Err(Error::Policy("inflate work"));
    }
    let range = 7..7 + length;
    let expected = u32::from_be_bytes(data[range.end..].try_into().expect("four checksum bytes"));
    if adler32(&data[range.clone()], cancel)? != expected {
        return Err(Error::Invalid("Adler-32 mismatch"));
    }
    Ok(Some(range))
}

/// Decode one complete RFC 1950 stream, with checked output, ratio and work limits.
/// No bytes are returned until the checksum has been verified.
pub fn decompress(data: &[u8], limits: InflateLimits, cancel: &AtomicBool) -> Result<Vec<u8>> {
    if let Some(range) = stored_range(data, limits, cancel)? {
        let mut out = Vec::new();
        out.try_reserve_exact(range.len())
            .map_err(|_| Error::Policy("inflate allocation"))?;
        out.extend_from_slice(&data[range]);
        return Ok(out);
    }
    let window = header(data)?;
    let limit = limits
        .output_bytes
        .min(data.len().saturating_mul(limits.expansion_ratio));
    let mut bits = Bits {
        data: &data[2..data.len() - 4],
        at: 0,
        reservoir: 0,
        available: 0,
    };
    let mut out = Vec::new();
    let initial = data.len().saturating_mul(8).clamp(256, 8192).min(limit);
    let mut work = limits.work;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        work = work.checked_sub(1).ok_or(Error::Policy("inflate work"))?;
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
                .ok_or(Error::Policy("inflate work"))?;
            reserve(&mut out, length, limit, initial)?;
            let start = bits.position();
            let end = start.checked_add(length).ok_or(Error::Overflow)?;
            let bytes = bits.data.get(start..end).ok_or(Error::Truncated)?;
            out.extend_from_slice(bytes);
            bits.skip_bytes(length)?;
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
                work = work.checked_sub(1).ok_or(Error::Policy("inflate work"))?;
                let symbol = literals.symbol(&mut bits)?;
                if symbol < 256 {
                    reserve(&mut out, 1, limit, initial)?;
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
                        .ok_or(Error::Policy("inflate work"))?;
                    reserve(&mut out, length, limit, initial)?;
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
    if bits.position() != bits.data.len() {
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
    fn checksum_lanes_match_the_scalar_definition_at_every_boundary() {
        let cancel = AtomicBool::new(false);
        let mut data = vec![255; 16_385];
        for pattern in 0..3 {
            if pattern > 0 {
                for (i, byte) in data.iter_mut().enumerate() {
                    *byte = if pattern == 1 {
                        i as u8
                    } else {
                        ((i * 193 + 71) ^ (i >> 3)) as u8
                    };
                }
            }
            for length in (0..=130).chain([4095, 4096, 4097, 8191, 8192, 8193, 16_384, 16_385]) {
                let (mut a, mut b) = (1_u32, 0_u32);
                for byte in &data[..length] {
                    a = (a + u32::from(*byte)) % 65521;
                    b = (b + a) % 65521;
                }
                assert_eq!(adler32(&data[..length], &cancel).unwrap(), (b << 16) | a);
            }
        }
    }

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

pub(crate) mod typed_restore;
