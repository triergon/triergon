//! Huffman decoding of the quantized spectrum (ISO/IEC 11172-3 2.4.2.7 and 2.4.3.4.4):
//! the big-value regions with their pair tables and linbits, then the count1 region of
//! quadruples.

use std::sync::LazyLock;

use triergon_bitstream::CachedBitReader;

use super::bands::Bands;
use super::huffman_tables::*;
use super::requantize::{MAX_MAGNITUDE, pow_4_3};
use super::side_info::GranuleChannel;

/// Codeword bits a lookup resolves in its first step.
const ROOT_BITS: u8 = 8;

/// One lookup entry in a single word, so the common case is one load: a codeword's
/// symbol and its whole length, or a link to the subtable that resolves longer codewords.
#[derive(Clone, Copy, Debug, Default)]
struct Entry(u32);

impl Entry {
    const LINK: u32 = 1 << 31;

    /// A codeword of `length` bits standing for `symbol`.
    fn leaf(symbol: u8, length: u8) -> Entry {
        Entry(u32::from(symbol) | u32::from(length) << 8)
    }

    /// Codewords continuing in the subtable at `offset`, indexed by their next `bits`
    /// bits.
    fn link(offset: usize, bits: u8) -> Entry {
        Entry(Self::LINK | (offset as u32) << 8 | u32::from(bits))
    }

    fn is_link(self) -> bool {
        self.0 & Self::LINK != 0
    }

    /// For a link: the subtable's offset and index width.
    fn subtable(self) -> (usize, u32) {
        (((self.0 & !Self::LINK) >> 8) as usize, self.0 & 31)
    }

    /// For a leaf: the symbol and the codeword length.
    fn decoded(self) -> (u8, u32) {
        (self.0 as u8, (self.0 >> 8) & 31)
    }
}

/// A two-step lookup decoding one code table.
#[derive(Debug)]
struct Lookup {
    root_bits: u32,
    /// The root table of `1 << root_bits` entries, then the subtables.
    entries: Box<[Entry]>,
}

impl Lookup {
    fn new(table: &CodeTable) -> Lookup {
        let codes: Vec<(u32, u8, u8)> = table
            .codes
            .iter()
            .zip(table.lengths)
            .enumerate()
            .map(|(index, (&code, &length))| (u32::from(code), length, table.symbols.symbol(index)))
            .collect();
        let root_bits = table
            .lengths
            .iter()
            .copied()
            .max()
            .unwrap_or(1)
            .min(ROOT_BITS);
        // The subtable width of each root prefix that starts longer codewords.
        let mut widths = vec![0u8; 1 << root_bits];
        for &(code, length, _) in codes.iter().filter(|c| c.1 > root_bits) {
            let rest = length - root_bits;
            let width = &mut widths[(code >> rest) as usize];
            *width = (*width).max(rest);
        }
        let mut entries = vec![Entry::default(); 1 << root_bits];
        for (prefix, &bits) in widths.iter().enumerate() {
            if bits > 0 {
                entries[prefix] = Entry::link(entries.len(), bits);
                entries.resize(entries.len() + (1 << bits), Entry::default());
            }
        }
        for &(code, length, symbol) in &codes {
            let leaf = Entry::leaf(symbol, length);
            if length <= root_bits {
                let first = (code << (root_bits - length)) as usize;
                entries[first..first + (1 << (root_bits - length))].fill(leaf);
            } else {
                let rest = length - root_bits;
                let (offset, bits) = entries[(code >> rest) as usize].subtable();
                let first =
                    offset + (((code & ((1 << rest) - 1)) << (bits - u32::from(rest))) as usize);
                entries[first..first + (1 << (bits - u32::from(rest)))].fill(leaf);
            }
        }
        Lookup {
            root_bits: u32::from(root_bits),
            entries: entries.into_boxed_slice(),
        }
    }

    /// The symbol of the codeword at the top of `window`, and the codeword's length:
    /// `x << 4 | y` for a pair table, `vwxy` for a quadruple table.
    #[inline(always)]
    fn find(&self, window: u32) -> (u8, u32) {
        let root = self.root_bits;
        let mut entry = self.entries[(window >> (32 - root)) as usize];
        if entry.is_link() {
            let (offset, bits) = entry.subtable();
            entry = self.entries[offset + ((window << root) >> (32 - bits)) as usize];
        }
        entry.decoded()
    }

    /// Read the next symbol.
    #[inline(always)]
    fn decode(&self, reader: &mut CachedBitReader<'_>) -> u8 {
        let (symbol, length) = self.find(reader.peek32());
        reader.skip(length as usize);
        symbol
    }
}

/// The distinct pair code tables of Table B.7.
const PAIR_TABLES: [&CodeTable; 15] = [
    &TABLE_1, &TABLE_2, &TABLE_3, &TABLE_5, &TABLE_6, &TABLE_7, &TABLE_8, &TABLE_9, &TABLE_10,
    &TABLE_11, &TABLE_12, &TABLE_13, &TABLE_15, &TABLE_16, &TABLE_24,
];

/// The code table (an index into [`PAIR_TABLES`]) and `linbits` of each `table_select`.
/// Selection 0 codes nothing; 4 and 14 are not used by the specification and are
/// decoded as 0 too.
const SELECTIONS: [Option<(usize, u32)>; 32] = [
    None,
    Some((0, 0)),
    Some((1, 0)),
    Some((2, 0)),
    None,
    Some((3, 0)),
    Some((4, 0)),
    Some((5, 0)),
    Some((6, 0)),
    Some((7, 0)),
    Some((8, 0)),
    Some((9, 0)),
    Some((10, 0)),
    Some((11, 0)),
    None,
    Some((12, 0)),
    Some((13, 1)),
    Some((13, 2)),
    Some((13, 3)),
    Some((13, 4)),
    Some((13, 6)),
    Some((13, 8)),
    Some((13, 10)),
    Some((13, 13)),
    Some((14, 4)),
    Some((14, 5)),
    Some((14, 6)),
    Some((14, 7)),
    Some((14, 8)),
    Some((14, 9)),
    Some((14, 11)),
    Some((14, 13)),
];

#[derive(Debug)]
struct Codebooks {
    pairs: [Lookup; 15],
    /// Count1 tables A and B.
    quads: [Lookup; 2],
}

static CODEBOOKS: LazyLock<Codebooks> = LazyLock::new(|| Codebooks {
    pairs: PAIR_TABLES.map(Lookup::new),
    quads: [Lookup::new(&TABLE_A), Lookup::new(&TABLE_B)],
});

/// Decode the Huffman-coded spectrum of one granule and channel, which ends at bit
/// `end`, into `out` as `sign(is) |is|^(4/3)`, the first half of requantization.
/// Returns the number of lines that may be nonzero; the rest are 0.
///
/// A count1 quadruple that runs past `end` is dropped (ISO/IEC 11172-3 2.4.3.4.4 ends
/// the region at `part2_3_length`).
pub(crate) fn read_spectrum(
    bits: &mut CachedBitReader<'_>,
    end: usize,
    g: &GranuleChannel,
    bands: &Bands,
    out: &mut [f32; 576],
) -> usize {
    // A local copy keeps the reader's state in registers through the loops.
    let mut reader = *bits;
    let lines = read_lines(&mut reader, end, g, bands, out);
    *bits = reader;
    lines
}

#[inline(always)]
fn read_lines(
    reader: &mut CachedBitReader<'_>,
    end: usize,
    g: &GranuleChannel,
    bands: &Bands,
    out: &mut [f32; 576],
) -> usize {
    let books = &*CODEBOOKS;
    let pow = pow_4_3();
    let mut line = 0;
    for (region_end, &select) in g.region_ends(bands).into_iter().zip(&g.table_select) {
        let region = &mut out[line..region_end.max(line)];
        line += region.len();
        let Some((table, linbits)) = SELECTIONS[usize::from(select)] else {
            region.fill(0.0);
            continue;
        };
        read_pairs(reader, &books.pairs[table], linbits, pow, region);
    }
    let quads = &books.quads[usize::from(g.count1_table_b)];
    while line + 4 <= out.len() && reader.position() < end {
        let symbol = quads.decode(reader);
        let mut values = [0.0; 4];
        for (i, value) in values.iter_mut().enumerate() {
            if symbol & (8 >> i) != 0 {
                *value = if reader.bit() { -1.0 } else { 1.0 };
            }
        }
        if reader.position() > end {
            break;
        }
        out[line..line + 4].copy_from_slice(&values);
        line += 4;
    }
    out[line..].fill(0.0);
    line
}

/// The pairs of one big-value region, coded with `lookup` and `linbits`. Kept out of
/// line: as a loop of its own it holds the reader and the tables in registers.
#[inline(never)]
fn read_pairs(
    reader: &mut CachedBitReader<'_>,
    lookup: &Lookup,
    linbits: u32,
    pow: &[f32; MAX_MAGNITUDE + 1],
    region: &mut [f32],
) {
    let mut bits = *reader;
    for pair in region.as_chunks_mut::<2>().0 {
        let window = bits.peek32();
        let (symbol, length) = lookup.find(window);
        let (x, y) = (symbol >> 4, symbol & 15);
        if linbits > 0 && (x == 15 || y == 15) {
            bits.skip(length as usize);
            *pair = [x, y].map(|value| escaped(&mut bits, value, linbits, pow));
            continue;
        }
        // The codeword and at most two sign bits lie within the 32-bit window.
        let mut signs = window << length;
        let mut used = length;
        *pair = [x, y].map(|value| {
            let magnitude = pow[usize::from(value)];
            if value == 0 {
                return magnitude;
            }
            let sign = signs & 0x8000_0000;
            signs <<= 1;
            used += 1;
            f32::from_bits(magnitude.to_bits() | sign)
        });
        bits.skip(used as usize);
    }
    *reader = bits;
}

/// One value of a pair of a table with linbits: they extend 15, and a sign bit follows
/// a nonzero value.
#[inline(always)]
fn escaped(reader: &mut CachedBitReader<'_>, value: u8, linbits: u32, pow: &[f32]) -> f32 {
    if value == 0 {
        return 0.0;
    }
    let mut magnitude = usize::from(value);
    if magnitude == 15 {
        magnitude += reader.read(linbits) as usize;
    }
    let magnitude = pow[magnitude];
    if reader.bit() { -magnitude } else { magnitude }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every table is a complete prefix code: the Kraft sum is exactly 1 and no codeword
    /// starts another one.
    #[test]
    fn code_tables_are_complete_prefix_codes() {
        for table in PAIR_TABLES.iter().copied().chain([&TABLE_A, &TABLE_B]) {
            let longest = u32::from(*table.lengths.iter().max().unwrap());
            let kraft: u64 = table
                .lengths
                .iter()
                .map(|&l| 1u64 << (longest - u32::from(l)))
                .sum();
            assert_eq!(kraft, 1 << longest);
            for (i, (&a, &la)) in table.codes.iter().zip(table.lengths).enumerate() {
                assert!(u32::from(a) < 1 << la);
                for (j, (&b, &lb)) in table.codes.iter().zip(table.lengths).enumerate() {
                    if i != j && lb >= la {
                        assert_ne!(u32::from(b) >> (lb - la), u32::from(a), "{i} prefixes {j}");
                    }
                }
            }
        }
    }

    /// Writes codewords MSB first.
    struct Writer {
        bytes: Vec<u8>,
        bits: usize,
    }

    impl Writer {
        fn put(&mut self, value: u32, length: u8) {
            for i in (0..length).rev() {
                if self.bits.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                if value >> i & 1 != 0 {
                    *self.bytes.last_mut().unwrap() |= 0x80 >> (self.bits % 8);
                }
                self.bits += 1;
            }
        }
    }

    #[test]
    fn every_codeword_of_every_table_decodes_to_its_symbol() {
        for table in PAIR_TABLES.iter().copied().chain([&TABLE_A, &TABLE_B]) {
            let lookup = Lookup::new(table);
            let mut writer = Writer {
                bytes: Vec::new(),
                bits: 0,
            };
            for (&code, &length) in table.codes.iter().zip(table.lengths) {
                writer.put(u32::from(code), length);
            }
            let mut reader = CachedBitReader::new(&writer.bytes);
            for index in 0..table.codes.len() {
                assert_eq!(lookup.decode(&mut reader), table.symbols.symbol(index));
            }
            assert_eq!(reader.position(), writer.bits);
        }
    }

    #[test]
    fn linbits_and_signs_follow_each_value() {
        // Table 16 with one linbit, x = 1 and y = 15 (symbol index 31).
        let g = GranuleChannel {
            big_values: 1,
            table_select: [16, 0, 0],
            ..GranuleChannel::default()
        };
        let mut writer = Writer {
            bytes: Vec::new(),
            bits: 0,
        };
        writer.put(u32::from(TABLE_16.codes[31]), TABLE_16.lengths[31]);
        writer.put(1, 1); // x sign: negative
        writer.put(1, 1); // y linbits: 15 + 1
        writer.put(0, 1); // y sign: positive
        let end = writer.bits;
        let mut reader = CachedBitReader::new(&writer.bytes);
        let mut out = [7.0; 576];
        let bands = Bands::for_rate(44_100).unwrap();
        let lines = read_spectrum(&mut reader, end, &g, bands, &mut out);
        assert_eq!(&out[..3], &[-1.0, (16f64.powf(4.0 / 3.0)) as f32, 0.0]);
        assert_eq!(lines, 2);
        assert!(out[2..].iter().all(|&v| v == 0.0));
    }
}
