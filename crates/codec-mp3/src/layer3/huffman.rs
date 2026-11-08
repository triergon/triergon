//! Huffman decoding of the quantized spectrum (ISO/IEC 11172-3 2.4.2.7 and 2.4.3.4.4):
//! the big-value regions with their pair tables and linbits, then the count1 region of
//! quadruples.

use std::sync::LazyLock;

use triergon_bitstream::CachedBitReader;

use super::bands::Bands;
use super::huffman_tables::*;
use super::side_info::GranuleChannel;

/// Codeword bits a lookup resolves in its first step.
const ROOT_BITS: u8 = 8;

/// A decoded codeword: its symbol and its length beyond the start of its step.
#[derive(Clone, Copy, Debug, Default)]
struct Leaf {
    symbol: u8,
    length: u8,
}

/// An entry of the root step.
#[derive(Clone, Copy, Debug)]
enum Entry {
    Leaf(Leaf),
    /// Codewords longer than the root step continue in the subtable of `leaves` at
    /// `offset`, indexed by their next `bits` bits.
    Link {
        offset: u16,
        bits: u8,
    },
}

/// A two-step lookup decoding one code table.
#[derive(Debug)]
struct Lookup {
    root_bits: u8,
    root: Box<[Entry]>,
    /// The subtables of the codewords longer than `root_bits`.
    leaves: Box<[Leaf]>,
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
        let mut leaves = Vec::new();
        let mut root: Vec<Entry> = widths
            .iter()
            .map(|&bits| {
                if bits == 0 {
                    return Entry::Leaf(Leaf::default());
                }
                let offset = leaves.len() as u16;
                leaves.resize(leaves.len() + (1 << bits), Leaf::default());
                Entry::Link { offset, bits }
            })
            .collect();
        for &(code, length, symbol) in &codes {
            if length <= root_bits {
                let first = (code << (root_bits - length)) as usize;
                root[first..first + (1 << (root_bits - length))]
                    .fill(Entry::Leaf(Leaf { symbol, length }));
            } else if let Entry::Link { offset, bits } =
                root[(code >> (length - root_bits)) as usize]
            {
                let rest = length - root_bits;
                let first =
                    usize::from(offset) + (((code & ((1 << rest) - 1)) << (bits - rest)) as usize);
                leaves[first..first + (1 << (bits - rest))].fill(Leaf {
                    symbol,
                    length: rest,
                });
            }
        }
        Lookup {
            root_bits,
            root: root.into_boxed_slice(),
            leaves: leaves.into_boxed_slice(),
        }
    }

    /// The next symbol: `x << 4 | y` for a pair table, `vwxy` for a quadruple table.
    #[inline(always)]
    fn decode(&self, reader: &mut CachedBitReader<'_>) -> u8 {
        let window = reader.peek32();
        let root = u32::from(self.root_bits);
        let (leaf, step) = match self.root[(window >> (32 - root)) as usize] {
            Entry::Leaf(leaf) => (leaf, 0),
            Entry::Link { offset, bits } => {
                let index =
                    usize::from(offset) + ((window << root) >> (32 - u32::from(bits))) as usize;
                (self.leaves[index], self.root_bits)
            }
        };
        reader.skip(usize::from(step + leaf.length));
        leaf.symbol
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
/// `end`, into `out`. Returns the number of lines that may be nonzero; the rest are 0.
///
/// A count1 quadruple that runs past `end` is dropped (ISO/IEC 11172-3 2.4.3.4.4 ends
/// the region at `part2_3_length`).
pub(crate) fn read_spectrum(
    reader: &mut CachedBitReader<'_>,
    end: usize,
    g: &GranuleChannel,
    bands: &Bands,
    out: &mut [i32; 576],
) -> usize {
    let books = &*CODEBOOKS;
    let mut line = 0;
    for (region_end, &select) in g.region_ends(bands).into_iter().zip(&g.table_select) {
        let region = &mut out[line..region_end.max(line)];
        line += region.len();
        match SELECTIONS[usize::from(select)] {
            None => region.fill(0),
            Some((table, linbits)) => {
                let lookup = &books.pairs[table];
                for pair in region.chunks_exact_mut(2) {
                    let symbol = lookup.decode(reader);
                    pair[0] = magnitude(reader, symbol >> 4, linbits);
                    pair[1] = magnitude(reader, symbol & 15, linbits);
                }
            }
        }
    }
    let quads = &books.quads[usize::from(g.count1_table_b)];
    while line + 4 <= out.len() && reader.position() < end {
        let symbol = quads.decode(reader);
        let mut values = [0; 4];
        for (i, value) in values.iter_mut().enumerate() {
            if symbol & (8 >> i) != 0 {
                *value = if reader.bit() { -1 } else { 1 };
            }
        }
        if reader.position() > end {
            break;
        }
        out[line..line + 4].copy_from_slice(&values);
        line += 4;
    }
    out[line..].fill(0);
    line
}

/// One value of a pair: `linbits` extend 15, and a sign bit follows a nonzero value.
#[inline(always)]
fn magnitude(reader: &mut CachedBitReader<'_>, value: u8, linbits: u32) -> i32 {
    if value == 0 {
        return 0;
    }
    let mut magnitude = i32::from(value);
    if magnitude == 15 && linbits > 0 {
        magnitude += reader.read(linbits) as i32;
    }
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
        let mut out = [7; 576];
        let bands = Bands::for_rate(44_100).unwrap();
        let lines = read_spectrum(&mut reader, end, &g, bands, &mut out);
        assert_eq!(&out[..3], &[-1, 16, 0]);
        assert_eq!(lines, 2);
        assert!(out[2..].iter().all(|&v| v == 0));
    }
}
