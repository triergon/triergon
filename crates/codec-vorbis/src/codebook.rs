//! Codebooks (Vorbis I section 3): entropy-coded entry numbers and their VQ vectors.
//!
//! Entry decode peeks the next bits LSB first and looks them up in a table indexed by
//! the bit-reversed codewords, so a codeword of up to [`FAST_BITS`] bits costs one
//! lookup. Longer codewords, rare in practice, are found by binary search over the
//! remaining codewords sorted by their left-aligned value.

use triergon_bitstream::LsbReader;

use crate::bits::BitWindow;
use crate::storage::Meter;
use crate::{DecodeError, ilog};

/// Codeword lengths resolved by a single table lookup.
const FAST_BITS: u32 = 11;

/// The longest codeword length a codebook can declare.
const MAX_LENGTH: u32 = 32;

/// How a codebook's VQ vectors are stored in the setup header (section 3.2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LookupType {
    /// Scalar use only.
    None,
    /// Lattice VQ: each vector permutes a short list of values.
    Lattice,
    /// Every scalar of every vector is coded.
    Explicit,
}

/// One decoded codebook.
#[derive(Debug)]
pub(crate) struct Codebook {
    dimensions: usize,
    entries: u32,
    tree: Tree,
    /// `dimensions` values per entry; empty without a lookup table.
    vectors: Vec<f32>,
}

impl Codebook {
    /// Reads one codebook (section 3.2.1), charging its tables to `meter`.
    pub(crate) fn read(r: &mut LsbReader<'_>, meter: &mut Meter) -> Result<Codebook, DecodeError> {
        if r.read(24) != 0x56_4342 {
            return Err(DecodeError::Invalid("codebook sync pattern"));
        }
        let dimensions = r.read(16) as usize;
        let entries = r.read(24);
        let lengths = read_lengths(r, entries, meter)?;
        let lookup = match r.read(4) {
            0 => LookupType::None,
            1 => LookupType::Lattice,
            2 => LookupType::Explicit,
            _ => return Err(DecodeError::Invalid("codebook lookup type")),
        };
        let vectors = read_vectors(r, lookup, entries, dimensions, meter)?;
        if r.overrun() {
            return Err(DecodeError::Invalid("truncated codebook"));
        }
        let tree = Tree::build(&lengths, meter);
        meter.release::<u8>(lengths.len());
        Ok(Codebook {
            dimensions,
            entries,
            tree: tree?,
            vectors,
        })
    }

    pub(crate) fn dimensions(&self) -> usize {
        self.dimensions
    }

    pub(crate) fn entries(&self) -> u32 {
        self.entries
    }

    /// True if the codebook can be read in VQ context.
    pub(crate) fn has_vectors(&self) -> bool {
        !self.vectors.is_empty()
    }

    /// The next entry number; `None` if no codeword matches, which only a codebook
    /// without used entries allows.
    #[inline(always)]
    pub(crate) fn scalar(&self, r: &mut LsbReader<'_>) -> Option<u32> {
        self.tree.decode(r)
    }

    /// The VQ vector of the next entry; `None` if no codeword matches or without a
    /// lookup table.
    #[inline(always)]
    pub(crate) fn vector(&self, r: &mut LsbReader<'_>) -> Option<&[f32]> {
        let entry = self.tree.decode(r)?;
        self.vector_of(entry)
    }

    #[inline(always)]
    fn vector_of(&self, entry: u32) -> Option<&[f32]> {
        let start = entry as usize * self.dimensions;
        self.vectors.get(start..start + self.dimensions)
    }

    /// Calls `f` with the VQ vectors of the next `count` entries, in order; false if a
    /// codeword does not decode (after the vectors before it).
    ///
    /// Equivalent to `count` calls of [`Codebook::vector`], but codewords are cut from
    /// one 32-bit window of the reader at a time, so the reader is advanced once per
    /// window instead of once per codeword.
    #[inline(always)]
    pub(crate) fn vectors(
        &self,
        r: &mut LsbReader<'_>,
        count: usize,
        mut f: impl FnMut(&[f32]),
    ) -> bool {
        let mut window = BitWindow::new(r);
        for _ in 0..count {
            let Some(vector) = self.entry(&mut window).and_then(|e| self.vector_of(e)) else {
                return false;
            };
            f(vector);
        }
        true
    }

    /// The next entry number, cut from `window`; `None` as for [`Codebook::scalar`].
    #[inline(always)]
    pub(crate) fn entry(&self, window: &mut BitWindow<'_, '_>) -> Option<u32> {
        let fast = &self.tree.fast;
        let mask = fast.len() - 1;
        let mut packed = fast[window.ahead() as usize & mask];
        if packed == 0 || !window.holds(packed & 0xff) {
            window.refill();
            packed = fast[window.ahead() as usize & mask];
            if packed == 0 {
                packed = self.tree.long(window.ahead())?;
            }
        }
        window.consume(packed & 0xff);
        Some(packed >> 8)
    }
}

/// Codeword lengths in entry order, 0 for an unused entry.
fn read_lengths(
    r: &mut LsbReader<'_>,
    entries: u32,
    meter: &mut Meter,
) -> Result<Vec<u8>, DecodeError> {
    let count = entries as usize;
    // Every entry costs at least one bit unless the lengths are ordered.
    let ordered = r.bit();
    if !ordered && count > r.left() {
        return Err(DecodeError::Invalid("truncated codebook"));
    }
    let mut lengths = meter.vec(count, 0u8)?;
    if ordered {
        let mut length = r.read(5) + 1;
        let mut entry = 0;
        while entry < count {
            if length > MAX_LENGTH {
                return Err(DecodeError::Invalid("codeword length"));
            }
            let number = r.read(ilog((count - entry) as u32)) as usize;
            let run = lengths
                .get_mut(entry..entry + number)
                .ok_or(DecodeError::Invalid("codeword length count"))?;
            run.fill(length as u8);
            entry += number;
            length += 1;
            if r.overrun() {
                return Err(DecodeError::Invalid("truncated codebook"));
            }
        }
    } else {
        let sparse = r.bit();
        for length in &mut lengths {
            if !sparse || r.bit() {
                *length = r.read(5) as u8 + 1;
            }
        }
    }
    Ok(lengths)
}

/// float32_unpack (section 9.2.2).
fn float32_unpack(x: u32) -> f32 {
    let mantissa = f64::from(x & 0x1f_ffff);
    let exponent = ((x & 0x7fe0_0000) >> 21) as i32;
    let value = if x & 0x8000_0000 != 0 {
        -mantissa
    } else {
        mantissa
    };
    (value * 2f64.powi(exponent - 788)) as f32
}

/// lookup1_values (section 9.2.3): the greatest `r` with `r^dimensions <= entries`.
fn lookup1_values(entries: u32, dimensions: usize) -> u32 {
    let fits = |r: u64| {
        let mut power = 1u64;
        for _ in 0..dimensions {
            power = power.saturating_mul(r);
            if power > u64::from(entries) {
                return false;
            }
        }
        true
    };
    let mut r = f64::from(entries).powf(1.0 / dimensions as f64).floor() as u64;
    while r > 0 && !fits(r) {
        r -= 1;
    }
    while fits(r + 1) {
        r += 1;
    }
    r as u32
}

/// The VQ vectors of every entry, unpacked once (section 3.2.1, "VQ lookup table vector
/// representation").
fn read_vectors(
    r: &mut LsbReader<'_>,
    lookup: LookupType,
    entries: u32,
    dimensions: usize,
    meter: &mut Meter,
) -> Result<Vec<f32>, DecodeError> {
    if lookup == LookupType::None {
        return Ok(Vec::new());
    }
    if dimensions == 0 {
        return Err(DecodeError::Invalid("codebook dimensions"));
    }
    let minimum = float32_unpack(r.read(32));
    let delta = float32_unpack(r.read(32));
    let value_bits = r.read(4) + 1;
    let sequence = r.bit();
    let lookup_values = match lookup {
        LookupType::Lattice => u64::from(lookup1_values(entries, dimensions)),
        _ => u64::from(entries) * dimensions as u64,
    };
    if lookup_values * u64::from(value_bits) > r.left() as u64 {
        return Err(DecodeError::Invalid("truncated codebook"));
    }
    let size = (entries as usize)
        .checked_mul(dimensions)
        .ok_or(DecodeError::Invalid("codebook size"))?;
    let mut vectors = meter.vec(size, 0f32)?;
    let unpack = |multiplicand: u32| multiplicand as f32 * delta + minimum;
    match lookup {
        LookupType::Lattice => {
            // The unpacked lattice values, and entry `e`'s digits in base `count`
            // (least significant first) stepped like an odometer.
            let count = lookup_values as usize;
            let mut values = meter.with_capacity(count)?;
            values.extend((0..count).map(|_| unpack(r.read(value_bits))));
            let mut digits = meter.vec(dimensions, 0usize)?;
            for vector in vectors.chunks_exact_mut(dimensions) {
                let mut last = 0.0;
                for (value, &digit) in vector.iter_mut().zip(&digits) {
                    *value = values[digit] + last;
                    if sequence {
                        last = *value;
                    }
                }
                for digit in &mut digits {
                    *digit += 1;
                    if *digit < count {
                        break;
                    }
                    *digit = 0;
                }
            }
            meter.release::<f32>(values.len());
            meter.release::<usize>(digits.len());
        }
        _ => {
            for vector in vectors.chunks_exact_mut(dimensions) {
                let mut last = 0.0;
                for value in vector {
                    *value = unpack(r.read(value_bits)) + last;
                    if sequence {
                        last = *value;
                    }
                }
            }
        }
    }
    Ok(vectors)
}

/// The Huffman decision tree of one codebook as lookup tables.
#[derive(Debug)]
struct Tree {
    /// Index width of `fast`.
    fast_bits: u32,
    /// `entry << 8 | length` for every `fast_bits`-bit prefix that completes a codeword
    /// of at most `fast_bits` bits; 0 where a longer codeword starts.
    fast: Vec<u32>,
    /// Codewords longer than `fast_bits`: (left-aligned codeword, `entry << 8 | length`),
    /// ascending.
    long: Vec<(u32, u32)>,
}

impl Tree {
    /// Assigns each used entry, in entry order, the lowest available codeword of its
    /// length (section 3.2.1). Over- and underspecified trees are errors, except a tree
    /// of a single entry, which must have length 1 and reads one bit of either value,
    /// and a tree without entries, from which nothing decodes (errata 20150226).
    fn build(lengths: &[u8], meter: &mut Meter) -> Result<Tree, DecodeError> {
        let mut used = lengths
            .iter()
            .enumerate()
            .filter(|&(_, &length)| length != 0);
        let max_length = lengths.iter().copied().max().unwrap_or(0);
        let fast_bits = u32::from(max_length).clamp(1, FAST_BITS);
        let mut fast = meter.vec(1usize << fast_bits, 0u32)?;
        let long_count = lengths
            .iter()
            .filter(|&&length| u32::from(length) > fast_bits)
            .count();
        let mut long = meter.with_capacity(long_count)?;
        let single = used.clone().nth(1).is_none();
        if single {
            if let Some((entry, &length)) = used.next() {
                if length != 1 {
                    return Err(DecodeError::Invalid("single-entry codebook length"));
                }
                fast.fill((entry as u32) << 8 | 1);
            }
            return Ok(Tree {
                fast_bits,
                fast,
                long,
            });
        }
        // The free subtrees, at most one per depth (each assignment takes the deepest
        // free one that is not deeper than the codeword and frees its right siblings
        // on the way down), as MSB-first codeword prefixes.
        let mut free: [Option<u64>; MAX_LENGTH as usize + 1] = [None; MAX_LENGTH as usize + 1];
        free[0] = Some(0);
        for (entry, &length) in used {
            let length = usize::from(length);
            let depth = (0..=length)
                .rev()
                .find(|&depth| free[depth].is_some())
                .ok_or(DecodeError::Invalid("overspecified codebook"))?;
            let prefix = free[depth].take().unwrap_or(0);
            for (below, slot) in free.iter_mut().enumerate().take(length + 1).skip(depth + 1) {
                *slot = Some(prefix << (below - depth) | 1);
            }
            let codeword = (prefix << (length - depth)) as u32;
            let packed = (entry as u32) << 8 | length as u32;
            if length as u32 <= fast_bits {
                let reversed = codeword.reverse_bits() >> (32 - length);
                for slot in fast.iter_mut().skip(reversed as usize).step_by(1 << length) {
                    *slot = packed;
                }
            } else {
                long.push((codeword << (32 - length), packed));
            }
        }
        if free.iter().any(Option::is_some) {
            return Err(DecodeError::Invalid("underspecified codebook"));
        }
        long.sort_unstable_by_key(|&(codeword, _)| codeword);
        Ok(Tree {
            fast_bits,
            fast,
            long,
        })
    }

    #[inline(always)]
    fn decode(&self, r: &mut LsbReader<'_>) -> Option<u32> {
        let packed = self.fast[r.peek(self.fast_bits) as usize & (self.fast.len() - 1)];
        if packed == 0 {
            return self.decode_long(r);
        }
        r.skip(packed & 0xff);
        Some(packed >> 8)
    }

    #[cold]
    fn decode_long(&self, r: &mut LsbReader<'_>) -> Option<u32> {
        let packed = self.long(r.peek(32))?;
        r.skip(packed & 0xff);
        Some(packed >> 8)
    }

    /// The `entry << 8 | length` of the codeword longer than the fast table that starts
    /// `ahead`, the next 32 bits.
    #[cold]
    fn long(&self, ahead: u32) -> Option<u32> {
        let bits = ahead.reverse_bits();
        let at = self.long.partition_point(|&(codeword, _)| codeword <= bits);
        let &(codeword, packed) = self.long.get(at.checked_sub(1)?)?;
        let length = packed & 0xff;
        ((codeword ^ bits) >> (32 - length) == 0).then_some(packed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use triergon_codec::Budget;

    fn tree(lengths: &[u8]) -> Result<Tree, DecodeError> {
        let budget = Budget::new(1 << 20).unwrap();
        let mut meter = Meter(triergon_codec::Reservation::empty(&budget));
        Tree::build(lengths, &mut meter)
    }

    /// Packs MSB-first codewords into an LSB-first Vorbis bitstream.
    fn pack(codewords: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut at = 0usize;
        for codeword in codewords {
            for bit in codeword.bytes() {
                if at.is_multiple_of(8) {
                    out.push(0);
                }
                if bit == b'1' {
                    *out.last_mut().unwrap() |= 1 << (at % 8);
                }
                at += 1;
            }
        }
        out
    }

    #[test]
    fn codewords_follow_the_specification_example() {
        // Section 3.2.1: lengths 2 4 4 4 4 2 3 3 give 00 0100 0101 0110 0111 10 110 111.
        let tree = tree(&[2, 4, 4, 4, 4, 2, 3, 3]).unwrap();
        let words = ["111", "00", "0110", "10", "0100", "110", "0111", "0101"];
        let data = pack(&words);
        let mut r = LsbReader::new(&data);
        let entries: Vec<u32> = (0..words.len())
            .map(|_| tree.decode(&mut r).unwrap())
            .collect();
        assert_eq!(entries, [7, 0, 3, 5, 1, 6, 4, 2]);
    }

    #[test]
    fn long_codewords_and_sparse_entries_decode() {
        // Entry 1 unused; a complete tree with codewords of 1 to 14 bits.
        let mut lengths = vec![1, 0];
        lengths.extend(2..=14u8);
        lengths.push(14);
        let tree = tree(&lengths).unwrap();
        assert!(!tree.long.is_empty());
        let mut words = vec!["0".to_string()];
        for length in 2..=14 {
            words.push(format!("{}0", "1".repeat(length - 1)));
        }
        words.push("1".repeat(14));
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        let data = pack(&refs);
        let mut r = LsbReader::new(&data);
        let want: Vec<u32> = std::iter::once(0).chain(2..=15).collect();
        let got: Vec<u32> = (0..want.len())
            .map(|_| tree.decode(&mut r).unwrap())
            .collect();
        assert_eq!(got, want);
        // Past the end of the packet the reader yields zero bits: the all-zero codeword.
        assert_eq!(tree.decode(&mut r), Some(0));
    }

    #[test]
    fn incomplete_and_overfull_trees_are_rejected() {
        assert_eq!(
            tree(&[2, 2, 2]).unwrap_err(),
            DecodeError::Invalid("underspecified codebook")
        );
        assert_eq!(
            tree(&[1, 2, 2, 2]).unwrap_err(),
            DecodeError::Invalid("overspecified codebook")
        );
        // Order does not matter for completeness.
        assert!(tree(&[3, 1, 2, 3]).is_ok());
    }

    #[test]
    fn single_entry_codebooks_read_one_bit_of_either_value() {
        let tree = tree(&[0, 0, 1, 0]).unwrap();
        let data = [0b10];
        let mut r = LsbReader::new(&data);
        assert_eq!(tree.decode(&mut r), Some(2));
        assert_eq!(tree.decode(&mut r), Some(2));
        assert_eq!(r.position(), 2);
        assert_eq!(
            self::tree(&[0, 3]).unwrap_err(),
            DecodeError::Invalid("single-entry codebook length")
        );
        let empty = self::tree(&[0, 0]).unwrap();
        assert_eq!(empty.decode(&mut LsbReader::new(&[0xff])), None);
    }

    #[test]
    fn float32_unpack_and_lookup1_values() {
        // 1.0 = 2^20 * 2^(768 - 788): mantissa 0x100000, exponent 768.
        assert_eq!(float32_unpack(768 << 21 | 0x10_0000), 1.0);
        assert_eq!(float32_unpack(0x8000_0000 | 770 << 21 | 0x18_0000), -6.0);
        assert_eq!(float32_unpack(0), 0.0);
        assert_eq!(lookup1_values(81, 4), 3);
        assert_eq!(lookup1_values(80, 4), 2);
        assert_eq!(lookup1_values(1, 8), 1);
        assert_eq!(lookup1_values(0, 2), 0);
        assert_eq!(lookup1_values(16_777_215, 1), 16_777_215);
        assert_eq!(lookup1_values(1000, 3), 10);
    }
}
