//! Palettes as the reference presents them: the default CLUTs (EN 300 743
//! clause 10), CLUT entries converted to ARGB words, and the automatic palette
//! (`compute_clut`).
//!
//! The automatic palette is a grey ramp ranked from the region's index plane.
//! Each pixel value is a node; the region's edge counts as already ranked.
//! The value most exposed to what is ranked takes the next level up, so the
//! values touching the edge rank first and those enclosed by others last.
use crate::dvb::Depth;
use triergon_core::{Budget, BudgetVec, Result};

/// A CLUT's tables for 2-, 4- and 8-bit regions, as ARGB words
/// (`0xAARRGGBB`).
#[derive(Clone, Debug)]
pub(super) struct Tables {
    two: [u32; 4],
    four: [u32; 16],
    eight: [u32; 256],
}

impl Tables {
    /// Store `word` at entry `id` of one table: the first, in the order 2-,
    /// 4-, 8-bit, whose flag is set in `depths` (bit 2: 2-bit, bit 1: 4-bit,
    /// bit 0: 8-bit) and that has entry `id`. EN 300 743 applies an entry to
    /// every flagged CLUT; the reference stores it once, so flags 3 with id 1
    /// change only the 4-bit table, and with id 20 only the 8-bit one.
    pub(super) fn set(&mut self, id: u8, depths: u8, word: u32) {
        let id = usize::from(id);
        let entry = [
            (4, &mut self.two[..]),
            (2, &mut self.four[..]),
            (1, &mut self.eight[..]),
        ]
        .into_iter()
        .filter(|(flag, _)| depths & flag != 0)
        .find_map(|(_, table)| table.get_mut(id));
        if let Some(entry) = entry {
            *entry = word;
        }
    }

    /// The table a region of `depth` uses.
    pub(super) fn get(&self, depth: Depth) -> &[u32] {
        match depth {
            Depth::Two => &self.two,
            Depth::Four => &self.four,
            Depth::Eight => &self.eight,
        }
    }
}

/// The default CLUTs of EN 300 743 clause 10, which a CLUT definition starts
/// from and a region without a defined CLUT uses. The reference writes the
/// clause's intensities as 255 for 100 %, 0xaa for 66.7 %, 0x7f for 50 %, 0x55
/// for 33.3 % and 0x2b for 16.7 %, and its transparencies as alpha 0x3f for
/// 75 % and 0x7f for 50 %. Entry 0 of each table is 0 (transparent black).
pub(super) static DEFAULT: Tables = Tables {
    two: [0, 0xffffffff, 0xff000000, 0xff7f7f7f],
    four: default_four(),
    eight: default_eight(),
};

const fn argb(alpha: u32, red: u32, green: u32, blue: u32) -> u32 {
    alpha << 24 | red << 16 | green << 8 | blue
}

/// Entries 1–7 are the full-intensity primaries and their mixes, 8–15 the
/// same at 0x7f; all are opaque.
const fn default_four() -> [u32; 16] {
    let mut table = [0; 16];
    let mut i = 1;
    while i < 16 {
        let level = if i < 8 { 0xff } else { 0x7f };
        table[i] = argb(
            0xff,
            bit(i, 0) * level,
            bit(i, 1) * level,
            bit(i, 2) * level,
        );
        i += 1;
    }
    table
}

/// Bit `n` of `i`, 0 or 1.
const fn bit(i: usize, n: usize) -> u32 {
    ((i >> n) & 1) as u32
}

/// Channel `n` (0 red, 1 green, 2 blue) of 8-bit default entry `i`: `base`,
/// plus `low` for bit `n` and `high` for bit `n + 4`.
const fn channel(i: usize, n: usize, [base, low, high]: [u32; 3]) -> u32 {
    base + bit(i, n) * low + bit(i, n + 4) * high
}

/// Bits 0–2 add red, green and blue at the low step and bits 4–6 at the high
/// step, onto a base; bits 7 and 3 select the group:
///
/// | bit 7 | bit 3 | base | low | high | alpha |
/// |---|---|---|---|---|---|
/// | 0 | 0 | 0 | 0x55 | 0xaa | 0xff, or 0x3f with low 0xff when bits 4–6 are clear |
/// | 0 | 1 | 0 | 0x55 | 0xaa | 0x7f |
/// | 1 | 0 | 0x7f | 0x2b | 0x55 | 0xff |
/// | 1 | 1 | 0 | 0x2b | 0x55 | 0xff |
const fn default_eight() -> [u32; 256] {
    let mut table = [0; 256];
    let mut i = 1;
    while i < 256 {
        let (base, low, high, alpha) = match (i & 0x80 != 0, i & 0x08 != 0) {
            (false, false) if i & 0x70 == 0 => (0, 0xff, 0, 0x3f),
            (false, false) => (0, 0x55, 0xaa, 0xff),
            (false, true) => (0, 0x55, 0xaa, 0x7f),
            (true, false) => (0x7f, 0x2b, 0x55, 0xff),
            (true, true) => (0, 0x2b, 0x55, 0xff),
        };
        let steps = [base, low, high];
        table[i] = argb(
            alpha,
            channel(i, 0, steps),
            channel(i, 1, steps),
            channel(i, 2, steps),
        );
        i += 1;
    }
    table
}

/// One CLUT entry (8-bit Y, Cr, Cb, T) as the reference presents it. Colour
/// follows ITU-R BT.601 in 1/1024 steps: with `l = 1192 × (Y − 16)`, red is
/// `l + 1634 × (Cr − 128)`, green `l − 832 × (Cr − 128) − 401 × (Cb − 128)`
/// and blue `l + 2066 × (Cb − 128)`, each plus 512, shifted right by 10 and
/// clamped to 0–255. Alpha is `255 − T`, and 0 for Y = 0, which EN 300 743
/// makes fully transparent; the colour of such an entry still follows Y = 0.
pub(super) fn ycrcbt(y: u8, cr: u8, cb: u8, t: u8) -> u32 {
    let luma = 1192 * (i32::from(y) - 16);
    let (cr, cb) = (i32::from(cr) - 128, i32::from(cb) - 128);
    let channel = |value: i32| ((value + 512) >> 10).clamp(0, 255) as u32;
    let alpha = if y == 0 { 0 } else { 255 - u32::from(t) };
    argb(
        alpha,
        channel(luma + 1634 * cr),
        channel(luma - 832 * cr - 401 * cb),
        channel(luma + 2066 * cb),
    )
}

/// The automatic palette's working storage: for the `n` values a region
/// uses, the `n × n` matrix of sides where a pixel of one meets a pixel of
/// the other, then each value's exposed sides and counted pixels. It is
/// allocated through the budget and kept for the next region.
#[derive(Debug)]
pub(super) struct Census {
    words: BudgetVec<u32>,
}

impl Census {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            words: BudgetVec::new(budget),
        }
    }

    /// The bytes the census holds now.
    pub(super) fn bytes(&self) -> usize {
        self.words.capacity() * size_of::<u32>()
    }

    /// The most [`automatic`] allocates for a region whose pixel values are
    /// all below `values`.
    pub(super) fn bytes_for(values: usize) -> usize {
        Self::words_for(values) * size_of::<u32>()
    }

    fn words_for(values: usize) -> usize {
        values * values + 2 * values
    }

    /// `len` zeroed words. Growing frees the old storage before the new one
    /// is allocated, so the census never holds more than its largest size.
    fn zeroed(&mut self, len: usize) -> Result<&mut [u32]> {
        if len > self.words.capacity() {
            let budget = self.words.budget().clone();
            self.words = BudgetVec::new(&budget);
            self.words = BudgetVec::try_filled(&budget, 0, len)?;
        } else {
            self.words.clear();
            self.words.try_resize(len, 0)?;
        }
        Ok(self.words.as_mut_slice())
    }
}

/// The automatic palette of a region's index plane, `pixels` row by row and
/// `width` per row, written to the entries of the values it uses; the other
/// entries of `out` keep their values, as in the reference.
///
/// - A pixel counts unless all four of its neighbours lie inside the region
///   and have its value. Each counted pixel has four sides; a side is exposed
///   when it lies on the region's edge or faces a pixel of a ranked value.
/// - A value's exposure is its counted pixels' exposed sides per counted
///   pixel, in 1/1024 steps rounded down.
/// - Values are ranked one at a time from the bottom: the most exposed value
///   next, the lower value first among equals.
/// - The `k`-th of `n` ranked values gets level `255 × k / (n − 1)` rounded
///   down (0 when the region has one value), and the word with alpha and green
///   at that level and red and blue at half of it, rounded down.
///
/// The work is one pass over the pixels and up to `n²` steps for the ranking.
/// A refused allocation is an error and leaves `out` as it was.
pub(super) fn automatic(
    pixels: &[u8],
    width: usize,
    census: &mut Census,
    out: &mut [u32; 256],
) -> Result<()> {
    if width == 0 {
        return Ok(());
    }
    let pixels = &pixels[..pixels.len() / width * width];
    // Dense indices in ascending value order, so that the lower index is the
    // lower value.
    let mut index = [0u8; 256];
    for &value in pixels {
        index[usize::from(value)] = 1;
    }
    let mut values = [0u8; 256];
    let mut count = 0;
    for value in 0..256 {
        if index[value] != 0 {
            index[value] = count as u8;
            values[count] = value as u8;
            count += 1;
        }
    }
    if count == 0 {
        return Ok(());
    }
    let words = census.zeroed(Census::words_for(count))?;
    let (sides, rest) = words.split_at_mut(count * count);
    let (exposed, counted) = rest.split_at_mut(count);
    tally(pixels, width, &index, count, sides, exposed, counted);
    rank(count, sides, exposed, counted, |rank, id| {
        let level = (255 * rank).checked_div(count - 1).unwrap_or(0) as u32;
        out[usize::from(values[id])] = argb(level, level >> 1, level, level >> 1);
    });
    Ok(())
}

/// One pass over the pixels: the sides where values meet (`sides[a × n + b]`
/// for dense indices `a` and `b`), and each value's counted pixels and their
/// sides on the region's edge.
fn tally(
    pixels: &[u8],
    width: usize,
    index: &[u8; 256],
    count: usize,
    sides: &mut [u32],
    exposed: &mut [u32],
    counted: &mut [u32],
) {
    let mut lines = pixels.chunks_exact(width).peekable();
    let mut above: Option<&[u8]> = None;
    while let Some(line) = lines.next() {
        let below = lines.peek().copied();
        for (x, &value) in line.iter().enumerate() {
            let a = usize::from(index[usize::from(value)]);
            let neighbours = [
                x.checked_sub(1).map(|x| line[x]),
                line.get(x + 1).copied(),
                above.map(|above| above[x]),
                below.map(|below| below[x]),
            ];
            let mut edge = 0;
            let mut differs = false;
            for neighbour in neighbours {
                match neighbour {
                    None => edge += 1,
                    Some(other) => differs |= other != value,
                }
            }
            if edge != 0 || differs {
                counted[a] += 1;
                exposed[a] += edge;
            }
            // Each meeting of two values once, from its left or upper pixel.
            for other in [neighbours[1], neighbours[3]].into_iter().flatten() {
                if other != value {
                    let b = usize::from(index[usize::from(other)]);
                    sides[a * count + b] += 1;
                    sides[b * count + a] += 1;
                }
            }
        }
        above = Some(line);
    }
}

/// Rank the `count` values bottom up, calling `place(rank, id)` for each. A
/// ranked value's count is cleared; every value has a counted pixel before,
/// as its topmost pixel lies on the edge or below another value.
fn rank(
    count: usize,
    sides: &[u32],
    exposed: &mut [u32],
    counted: &mut [u32],
    mut place: impl FnMut(usize, usize),
) {
    for rank in 0..count {
        let mut best: Option<(usize, u64)> = None;
        for (id, (&edges, &pixels)) in exposed.iter().zip(counted.iter()).enumerate() {
            if pixels == 0 {
                continue;
            }
            let share = (u64::from(edges) << 10) / u64::from(pixels);
            if best.is_none_or(|(_, top)| share > top) {
                best = Some((id, share));
            }
        }
        let Some((next, _)) = best else {
            return;
        };
        counted[next] = 0;
        place(rank, next);
        let meets = &sides[next * count..(next + 1) * count];
        for (exposed, &meet) in exposed.iter_mut().zip(meets) {
            *exposed += meet;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The levels of `values` in the automatic palette of `rows`.
    fn levels(rows: &[&[u8]], values: &[u8]) -> Vec<u32> {
        let budget = Budget::new(1 << 20).unwrap();
        let mut census = Census::new(&budget);
        let mut out = [0; 256];
        let pixels: Vec<u8> = rows.concat();
        automatic(&pixels, rows[0].len(), &mut census, &mut out).unwrap();
        values.iter().map(|&v| out[usize::from(v)] >> 24).collect()
    }

    /// A sea of value 1 with the given cells set.
    fn sea(width: usize, height: usize, cells: &[(usize, usize, u8)]) -> Vec<Vec<u8>> {
        let mut rows = vec![vec![1; width]; height];
        for &(y, x, value) in cells {
            rows[y][x] = value;
        }
        rows
    }

    fn sea_levels(rows: &[Vec<u8>], values: &[u8]) -> Vec<u32> {
        let rows: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
        levels(&rows, values)
    }

    #[test]
    fn conversion_matches_the_reference() {
        // (Y, Cr, Cb, T) and the word the reference presents.
        for (y, cr, cb, t, word) in [
            (16, 128, 128, 0, 0xff000000),
            (17, 128, 128, 0, 0xff010101),
            (1, 128, 128, 0, 0xff000000),
            (235, 128, 128, 0, 0xffffffff),
            (255, 255, 255, 0, 0xffff7dff),
            (255, 0, 0, 255, 0x004aff14),
            (128, 0, 128, 0, 0xff00ea82),
            (128, 128, 0, 0, 0xff82b500),
            (128, 255, 128, 0, 0xffff1b82),
            (128, 128, 255, 0, 0xff8251ff),
            (128, 128, 128, 128, 0x7f828282),
            (128, 128, 128, 255, 0x00828282),
            (16, 240, 16, 0, 0xffb30000),
            (235, 16, 240, 0, 0xff4cffff),
            // Y = 0: transparent, the colour still converted.
            (0, 129, 166, 101, 0x0000003a),
            (0, 224, 39, 51, 0x00870000),
        ] {
            assert_eq!(ycrcbt(y, cr, cb, t), word, "{y} {cr} {cb} {t}");
        }
    }

    #[test]
    fn default_tables_match_the_reference() {
        assert_eq!(
            DEFAULT.get(Depth::Two),
            [0, 0xffffffff, 0xff000000, 0xff7f7f7f]
        );
        assert_eq!(
            DEFAULT.get(Depth::Four),
            [
                0, 0xffff0000, 0xff00ff00, 0xffffff00, 0xff0000ff, 0xffff00ff, 0xff00ffff,
                0xffffffff, 0xff000000, 0xff7f0000, 0xff007f00, 0xff7f7f00, 0xff00007f, 0xff7f007f,
                0xff007f7f, 0xff7f7f7f
            ]
        );
        let eight = DEFAULT.get(Depth::Eight);
        for (i, word) in [
            (0, 0),
            (1, 0x3fff0000),
            (7, 0x3fffffff),
            (8, 0x7f000000),
            (9, 0x7f550000),
            (15, 0x7f555555),
            (16, 0xffaa0000),
            (17, 0xffff0000),
            (24, 0x7faa0000),
            (25, 0x7fff0000),
            (127, 0x7fffffff),
            (128, 0xff7f7f7f),
            (129, 0xffaa7f7f),
            (136, 0xff000000),
            (137, 0xff2b0000),
            (144, 0xffd47f7f),
            (152, 0xff550000),
            (153, 0xff800000),
            (255, 0xff808080),
        ] {
            assert_eq!(eight[i], word, "entry {i}");
        }
        // The whole observed 8-bit table, folded.
        let fold = eight.iter().fold(0u64, |fold, &w| {
            fold.wrapping_mul(31).wrapping_add(u64::from(w))
        });
        assert_eq!(fold, 0x52ed2feec355d280);
    }

    #[test]
    fn an_entry_goes_to_the_first_flagged_table_that_has_it() {
        let mut tables = DEFAULT.clone();
        // 2- and 8-bit flags: the 2-bit table has entry 3.
        tables.set(3, 4 | 1, 7);
        // 4- and 8-bit flags: entry 20 only exists in the 8-bit table.
        tables.set(20, 2 | 1, 9);
        // 2- and 4-bit flags: no table of those has entry 30.
        tables.set(30, 4 | 2, 11);
        assert_eq!(tables.get(Depth::Two)[3], 7);
        assert_eq!(tables.get(Depth::Eight)[3], DEFAULT.get(Depth::Eight)[3]);
        assert_eq!(tables.get(Depth::Eight)[20], 9);
        assert_eq!(tables.get(Depth::Eight)[30], DEFAULT.get(Depth::Eight)[30]);
        assert_eq!(tables.get(Depth::Four), DEFAULT.get(Depth::Four));
    }

    #[test]
    fn a_value_at_both_ends_ranks_first() {
        assert_eq!(levels(&[&[1, 2, 1]], &[1, 2]), [0, 255]);
    }

    #[test]
    fn distinct_values_peel_from_the_edge() {
        // Each value is one pixel: the fewest unranked neighbours goes next.
        assert_eq!(
            levels(&[&[0, 2, 4, 1, 3]], &[0, 1, 2, 3, 4]),
            [0, 191, 63, 127, 255]
        );
        assert_eq!(
            levels(
                &[&[0, 1, 2], &[3, 4, 5], &[6, 7, 8]],
                &[0, 1, 2, 3, 4, 5, 6, 7, 8]
            ),
            [0, 31, 63, 95, 159, 191, 127, 223, 255]
        );
        assert_eq!(levels(&[&[1], &[2], &[2], &[3]], &[1, 2, 3]), [0, 255, 127]);
        assert_eq!(levels(&[&[1, 1], &[1, 2]], &[1, 2]), [0, 255]);
    }

    #[test]
    fn runs_rank_by_their_exposed_sides() {
        let row = [[2; 6].as_slice(), &[1; 4], &[4; 10], &[3; 8]].concat();
        assert_eq!(levels(&[&row], &[1, 2, 3, 4]), [85, 0, 170, 255]);
    }

    #[test]
    fn enclosed_pixels_do_not_count() {
        // A plus of 2 against a pair of 3: with its centre left out, the plus
        // is as exposed as the pair, and the lower value ranks first.
        let rows = sea(
            9,
            7,
            &[
                (1, 2, 2),
                (2, 1, 2),
                (2, 2, 2),
                (2, 3, 2),
                (3, 2, 2),
                (5, 5, 3),
                (5, 6, 3),
            ],
        );
        assert_eq!(sea_levels(&rows, &[1, 2, 3]), [0, 127, 255]);
    }

    #[test]
    fn exposure_is_compared_in_1024ths() {
        // 2: 64 exposed sides over 51 pixels; 3: 54 over 43, slightly more
        // but the same in 1/1024 steps, so the lower value ranks first.
        let mut cells = Vec::new();
        cells.extend((2..9).map(|x| (1, x, 2)));
        cells.extend((2..24).flat_map(|x| [(3, x, 2), (4, x, 2)]));
        cells.extend((2..7).map(|x| (6, x, 3)));
        cells.extend((2..21).flat_map(|x| [(8, x, 3), (9, x, 3)]));
        assert_eq!(sea_levels(&sea(26, 11, &cells), &[1, 2, 3]), [0, 127, 255]);
    }

    #[test]
    fn a_value_only_on_the_edge_can_rank_below_the_background() {
        let rows = sea(8, 5, &[(0, 3, 2), (2, 3, 3), (2, 4, 3)]);
        assert_eq!(sea_levels(&rows, &[1, 2, 3]), [127, 0, 255]);
    }

    #[test]
    fn unused_entries_keep_their_words() {
        let budget = Budget::new(1 << 20).unwrap();
        let mut census = Census::new(&budget);
        let mut out = [0x1234; 256];
        automatic(&[5, 5, 5, 5], 2, &mut census, &mut out).unwrap();
        assert_eq!(out[5], 0);
        assert!(out.iter().enumerate().all(|(i, &w)| i == 5 || w == 0x1234));
        automatic(&[1, 2], 2, &mut census, &mut out).unwrap();
        assert_eq!((out[1], out[2], out[3]), (0, 0xff7fff7f, 0x1234));
    }

    #[test]
    fn the_census_stays_within_its_plan() {
        let budget = Budget::new(1 << 20).unwrap();
        let base = budget.used();
        let mut census = Census::new(&budget);
        let mut out = [0; 256];
        let small: Vec<u8> = (0..64).map(|i| (i * 7 % 16) as u8).collect();
        automatic(&small, 8, &mut census, &mut out).unwrap();
        assert!(census.bytes() <= Census::bytes_for(16));
        let all: Vec<u8> = (0..=255).collect();
        automatic(&all, 16, &mut census, &mut out).unwrap();
        assert!(census.bytes() <= Census::bytes_for(256));
        assert_eq!(budget.used() - base, census.bytes());
    }

    #[test]
    fn a_refused_census_is_an_error() {
        let budget = Budget::new(64).unwrap();
        let mut census = Census::new(&budget);
        let mut out = [7; 256];
        let all: Vec<u8> = (0..=255).collect();
        assert!(automatic(&all, 16, &mut census, &mut out).is_err());
        assert_eq!(out, [7; 256]);
    }
}
