//! Packing a per-code-point property into a three-stage table.
//!
//! The code points below `limit` split into blocks of `2^(middle_bits + leaf_bits)`;
//! `top` names each block's run of `2^middle_bits` entries in `middle`, which name
//! runs of `2^leaf_bits` values in `leaves`. Identical runs are stored once. The
//! block sizes are the ones that make the table smallest.

use std::collections::HashMap;

/// A packed table; `crates/shape/src/unicode/trie.rs` looks values up in it.
pub struct Trie {
    pub limit: u32,
    pub default: u16,
    pub leaf_bits: u32,
    pub middle_bits: u32,
    pub top: Vec<u16>,
    pub middle: Vec<u16>,
    pub leaves: Vec<u16>,
}

impl Trie {
    /// The bytes of each stage when stored in the narrowest unsigned type.
    pub fn bytes(&self) -> usize {
        [&self.top, &self.middle, &self.leaves]
            .iter()
            .map(|stage| stage.len() * width(stage))
            .sum()
    }
}

/// The byte width of the narrowest unsigned type holding every value.
pub fn width(values: &[u16]) -> usize {
    if values.iter().all(|&v| u8::try_from(v).is_ok()) {
        1
    } else {
        2
    }
}

/// Runs of `2^bits` values, each stored once in first-occurrence order: the run
/// numbers and the distinct runs.
fn dedup(values: &[u16], bits: u32) -> (Vec<u16>, Vec<u16>) {
    let mut ids = HashMap::new();
    let mut runs = Vec::new();
    let numbers = values
        .chunks(1 << bits)
        .map(|run| {
            *ids.entry(run).or_insert_with(|| {
                runs.extend_from_slice(run);
                u16::try_from((runs.len() >> bits) - 1).expect("at most 65536 runs")
            })
        })
        .collect();
    (numbers, runs)
}

/// The smallest three-stage table for `values`, one per code point, of which those
/// at and after the last value other than `default` are not stored.
pub fn pack(values: &[u16], default: u16) -> Trie {
    const MAX_BITS: u32 = 10;
    let used = values
        .iter()
        .rposition(|&v| v != default)
        .map_or(0, |i| i + 1);
    let mut best: Option<Trie> = None;
    for leaf_bits in 1..=MAX_BITS {
        // Padding to the largest block once keeps the runs a prefix for every
        // smaller one, as runs are numbered in order of first occurrence.
        let mut padded = values[..used].to_vec();
        padded.resize(used.next_multiple_of(1 << (leaf_bits + MAX_BITS)), default);
        let (leaf_numbers, leaves) = dedup(&padded, leaf_bits);
        for middle_bits in 1..=MAX_BITS {
            let blocks = used.next_multiple_of(1 << (leaf_bits + middle_bits)) >> leaf_bits;
            let numbers = &leaf_numbers[..blocks];
            let leaf_runs = numbers.iter().max().map_or(0, |&n| usize::from(n) + 1);
            let (top, middle) = dedup(numbers, middle_bits);
            let trie = Trie {
                limit: u32::try_from(blocks << leaf_bits).expect("code points fit u32"),
                default,
                leaf_bits,
                middle_bits,
                top,
                middle,
                leaves: leaves[..leaf_runs << leaf_bits].to_vec(),
            };
            if best.as_ref().is_none_or(|b| trie.bytes() < b.bytes()) {
                best = Some(trie);
            }
        }
    }
    best.expect("at least one configuration")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(trie: &Trie, cp: usize) -> u16 {
        if cp >= trie.limit as usize {
            return trie.default;
        }
        let middle = usize::from(trie.top[cp >> (trie.leaf_bits + trie.middle_bits)]);
        let middle =
            (middle << trie.middle_bits) | ((cp >> trie.leaf_bits) & ((1 << trie.middle_bits) - 1));
        let leaf = (usize::from(trie.middle[middle]) << trie.leaf_bits)
            | (cp & ((1 << trie.leaf_bits) - 1));
        trie.leaves[leaf]
    }

    #[test]
    fn packs_losslessly_and_compactly() {
        let mut values = vec![7u16; 0x11_0000];
        for (i, value) in values.iter_mut().enumerate().take(0x3_0000) {
            *value = match i {
                0x0..=0x7F => (i % 3) as u16,
                0x4E00..=0x9FFF => 1,
                0x2_0000.. => 2,
                _ if i % 4096 == 17 => 300,
                _ => 7,
            };
        }
        let trie = pack(&values, 7);
        assert!(trie.bytes() < 2048, "{} bytes", trie.bytes());
        assert!(trie.limit as usize >= 0x3_0000);
        for (cp, &value) in values.iter().enumerate() {
            assert_eq!(get(&trie, cp), value, "U+{cp:04X}");
        }
        assert_eq!(get(&trie, 0x20_0000), 7);
    }

    #[test]
    fn all_default_is_empty() {
        let trie = pack(&[3; 100], 3);
        assert_eq!((trie.limit, trie.bytes()), (0, 0));
    }
}
