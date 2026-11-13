//! Palettes as the reference presents them: the default CLUTs (EN 300 743
//! clause 10), CLUT entries converted to ARGB words, and the automatic palette
//! (`compute_clut`).
//!
//! The automatic palette gives each value a region holds one level, from
//! transparent black up to `0xff7fff7f`, ranking the values from the region's
//! edge inwards ([`automatic`]).
use crate::dvb::Depth;
use triergon_core::{Budget, BudgetVec, Result};
use triergon_pixel::{ChannelRounding, YcbcrMatrix};

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
    /// every CLUT it flags; the reference applies it to one table only: flags
    /// 3 with id 1 change only the 4-bit table, and with id 20 only the 8-bit
    /// one.
    pub(super) fn set(&mut self, id: u8, depths: u8, word: u32) {
        let id = usize::from(id);
        if depths & 4 != 0 && id < 4 {
            self.two[id] = word;
        } else if depths & 2 != 0 && id < 16 {
            self.four[id] = word;
        } else if depths & 1 != 0 {
            self.eight[id] = word;
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
/// | 0 | 0 | 0 | 0x55 | 0xaa | 0xff (bits 4–6 clear: low 0xff, alpha 0x3f) |
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

/// One CLUT entry (8-bit Y, Cr, Cb, T) as the reference presents it: the
/// colour by ITU-R BT.601 in 1/1024 steps rounded to nearest, as other
/// subtitle palettes ([`YcbcrMatrix`]), and alpha `255 − T`, or 0 for Y = 0,
/// which EN 300 743 makes fully transparent. The colour of such an entry
/// still follows Y = 0.
pub(super) fn entry_word(y: u8, cr: u8, cb: u8, t: u8) -> u32 {
    // As a product rather than a branch, the conversion stays one path.
    let alpha = (255 - t) * u8::from(y != 0);
    YcbcrMatrix::BT601.argb(y, cr, cb, alpha, ChannelRounding::Nearest)
}

/// Values a pixel can hold.
const VALUES: usize = 256;

/// The words of a census beyond its sides: each value's counted pixels,
/// exposure and score.
const TALLIES: usize = 3 * VALUES;

/// A score not worked out since its value's exposure last changed.
const STALE: u32 = u32::MAX;

/// The automatic palette's working storage, charged to the decoder's budget:
/// one table of words holding each value's counted pixels, exposure and score
/// (256 words each), then the sides between unlike neighbours. Those are a
/// table of every ordered pair of values below the region's stride (4, 16 or
/// 256), `stride²` words, or for 8-bit values, when they are smaller, lists of
/// the neighbours each value touches: 257 list starts and two words per pair of
/// neighbouring pixels, or per unlike pair where lists for every pair would not
/// be smaller. A 16 × 16 region of all 256 values holds 3,072 + 4,868 bytes
/// rather than 3,072 + 262,144. Between regions every word is zero:
/// [`automatic`] clears the pair table as it reads it and the lists once
/// ranked, so a region costs no clearing beyond the words its own values touch.
#[derive(Debug)]
pub(super) struct Census {
    table: BudgetVec<u32>,
}

impl Census {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            table: BudgetVec::new(budget),
        }
    }

    /// Bytes the census holds now.
    pub(super) fn bytes(&self) -> usize {
        self.table.capacity() * size_of::<u32>()
    }

    /// The most the census allocates for a region whose pixel values are all
    /// below `values` (4, 16 or 256): the pair table's size, as the lists are
    /// used only when smaller.
    pub(super) fn bytes_for(values: usize) -> usize {
        let stride = stride(values);
        (TALLIES + stride * stride) * size_of::<u32>()
    }
}

/// The pair table's stride for pixel values below `values`.
fn stride(values: usize) -> usize {
    match values {
        0..=4 => 4,
        5..=16 => 16,
        _ => VALUES,
    }
}

/// The automatic palette (`compute_clut`) of a region: `pixels` row by row,
/// `width` per row, into `out`. Entries of values no pixel holds keep their
/// words.
///
/// The values the region holds are ranked, and the `n`-th of `m` (from 0) gets
/// the level `255 × n / (m − 1)` rounded down, presented as the word with alpha
/// and green at the level and red and blue at half of it: with three values 0,
/// `0x7f3f7f3f` and `0xff7fff7f`; with eight, levels 0, 36, 72, 109, 145, 182,
/// 218 and 255. The first value ranked, the background, is transparent black; a
/// region of one value is all background.
///
/// A value's *counted* pixels are those on the region's edge or with a
/// 4-neighbour of another value; pixels enclosed by their own value do not
/// count. Its *exposure* starts as its pixels' sides on the region's edge (a
/// corner pixel has two; in a region one pixel high every pixel has two and the
/// ends three). Each step ranks the value with the highest score, exposure ×
/// 1024 / counted pixels rounded down, the smaller value on a tie; then every
/// value still unranked adds to its exposure its sides against the pixels of
/// the value just ranked, as if those pixels were the edge. So the background
/// is the value most exposed on the edge, and the ranks peel inwards from it:
/// - the row 1 2 3 ranks 1 2 3: 1 and 3 tie at 3072 (three sides on the edge
///   each) and 1 ranks first; then 2, with two sides on the edge and one
///   against 1, ties with 3 and ranks before it;
/// - the row 1 1 2 3 ranks 3 2 1: 3 scores 3072 against 1's 2560;
/// - the row 2 1 3 1 ranks 2 1 3: once 2 is ranked, 1 has six sides on two
///   pixels;
/// - in a 5 × 5 region of 1, a 2 at a corner ranks first (2048 against 1's 18
///   sides on 15 counted pixels, 1228), and a 2 at the middle of an edge ranks
///   after 1 (1024 against 19 sides on 16 pixels, 1216), though counting every
///   pixel of 1 would put 2 first;
/// - two rows, 600 columns of 5 then 680 of 3, rank 3 first: both score 1025,
///   though 5's exact ratio is the greater; with 505 columns of 5 and 520 of 3,
///   5 scores 1026 and ranks first.
///
/// Every depth follows the same rule, with no limit from the width or height up
/// to 1280 × 1024: a 1/2 checkerboard there, framed in 1, gives 2 an exposure
/// above 2³¹ / 1024 once 1 is ranked, and 2 still ranks before a 3 × 3 block of
/// 3. Entries no pixel uses keep the words of the region's last palette across
/// redrawing, resizing, a new depth or a display set without the region; a new
/// epoch (a mode change or an acquisition point) starts its regions from
/// transparent black.
///
/// The work is linear in the pixels, plus at most 256² steps for the ranking.
pub(super) fn automatic(
    pixels: &[u8],
    width: usize,
    census: &mut Census,
    out: &mut [u32; 256],
) -> Result<()> {
    let Some(max) = pixels.iter().copied().max() else {
        return Ok(());
    };
    if width == 0 {
        return Ok(());
    }
    let stride = stride(usize::from(max) + 1);
    let table = stride * stride;
    // Lists for 8-bit values, when smaller than the table.
    let lists = if stride == VALUES {
        list_words(pixels, width, table)
    } else {
        usize::MAX
    };
    let sides = table.min(lists);
    if census.table.len() < TALLIES + sides {
        census.table.try_resize(TALLIES + sides, 0)?;
    }
    let (tallies, rest) = census.table.as_mut_slice().split_at_mut(TALLIES);
    let rest = &mut rest[..sides];
    let (counted, rest_tallies) = tallies.split_at_mut(VALUES);
    let (exposure, score) = rest_tallies.split_at_mut(VALUES);
    if lists < table {
        let (starts, neighbours) = rest.split_at_mut(VALUES + 1);
        let lists = Lists::new(pixels, width, starts, neighbours);
        let mut lists = count(pixels, width, counted, exposure, lists);
        let filled = VALUES + 1 + lists.starts[VALUES] as usize;
        rank(stride, counted, exposure, score, &mut lists, out);
        rest[..filled].fill(0);
    } else {
        let pairs = Pairs {
            stride,
            pairs: rest,
        };
        let mut pairs = count(pixels, width, counted, exposure, pairs);
        rank(stride, counted, exposure, score, &mut pairs, out);
    }
    Ok(())
}

/// What a walk over a region's pixels visits.
trait Walker {
    /// Each row before its pixels, with whether rows lie above and below it;
    /// `false` ends the walk.
    fn row(&mut self, _row: &[u8], _up: bool, _down: bool) -> bool {
        true
    }

    /// Each pixel, with whether it is counted: on the region's edge or with a
    /// 4-neighbour of another value.
    fn pixel(&mut self, _value: u8, _counted: bool) {}

    /// Each pair of 4-neighbours with unlike values, left then right or upper
    /// then lower.
    fn pair(&mut self, a: u8, b: u8);
}

/// Walk the pixels of a region, `width` per row: the one walk over its rows,
/// pixels and unlike pairs that sizing the lists, their degrees and the count
/// share.
fn walk(pixels: &[u8], width: usize, walker: &mut impl Walker) {
    fn pair_if_unlike(walker: &mut impl Walker, a: u8, b: u8) {
        if a != b {
            walker.pair(a, b);
        }
    }
    // Each row with the rows above and below it, if the region has them.
    let mut rows = pixels.chunks_exact(width);
    let mut up: Option<&[u8]> = None;
    let mut next = rows.next();
    while let Some(row) = next {
        let down = rows.next();
        next = down;
        if !walker.row(row, up.is_some(), down.is_some()) {
            return;
        }
        let last = row.len() - 1;
        match (up, down) {
            (Some(up), Some(down)) if last > 1 => {
                walker.pixel(row[0], true);
                walker.pixel(row[last], true);
                pair_if_unlike(walker, row[0], row[1]);
                pair_if_unlike(walker, row[0], down[0]);
                pair_if_unlike(walker, row[last], down[last]);
                for ((window, &u), &d) in row.windows(3).zip(&up[1..]).zip(&down[1..]) {
                    let (l, v, r) = (window[0], window[1], window[2]);
                    pair_if_unlike(walker, v, r);
                    pair_if_unlike(walker, v, d);
                    walker.pixel(v, l != v || r != v || u != v || d != v);
                }
            }
            // The first or last row, or a region at most two wide: every pixel is
            // on the edge.
            _ => {
                for &v in row {
                    walker.pixel(v, true);
                }
                for window in row.windows(2) {
                    pair_if_unlike(walker, window[0], window[1]);
                }
                for (&a, &b) in down.into_iter().flat_map(|down| row.iter().zip(down)) {
                    pair_if_unlike(walker, a, b);
                }
            }
        }
        up = Some(row);
    }
}

/// The words of neighbour lists for `pairs` pairs: 257 starts and two per pair.
fn list_words_for(pairs: usize) -> usize {
    VALUES + 1 + 2 * pairs
}

/// The words of neighbour lists for `pixels`, or `usize::MAX` when they would
/// not be smaller than `table` words. Pairs are counted only when lists for
/// every pair being unlike would not be smaller; then the lists are sized for
/// that.
fn list_words(pixels: &[u8], width: usize, table: usize) -> usize {
    let height = pixels.len() / width;
    let every = (width - 1) * height + width * height.saturating_sub(1);
    if list_words_for(every) < table {
        return list_words_for(every);
    }
    unlike(pixels, width, table).map_or(usize::MAX, list_words_for)
}

/// The number of unlike pairs in `pixels`, or `None` once lists for them would
/// not be smaller than `table` words.
fn unlike(pixels: &[u8], width: usize, table: usize) -> Option<usize> {
    struct Unlike {
        pairs: usize,
        table: usize,
    }
    impl Walker for Unlike {
        fn row(&mut self, _row: &[u8], _up: bool, _down: bool) -> bool {
            list_words_for(self.pairs) < self.table
        }

        fn pair(&mut self, _a: u8, _b: u8) {
            self.pairs += 1;
        }
    }
    let mut unlike = Unlike { pairs: 0, table };
    walk(pixels, width, &mut unlike);
    (list_words_for(unlike.pairs) < table).then_some(unlike.pairs)
}

/// Where the sides between unlike neighbours are counted, and how a ranked
/// value's sides reach the values still unranked.
trait Sides {
    /// A side between unlike neighbours `a` and `b`.
    fn side(&mut self, a: u8, b: u8);

    /// Add to the exposure of each value in `left` its sides against `ranked`,
    /// marking its score stale.
    fn expose(&mut self, ranked: usize, left: &[u8], tally: &mut Ranking<'_>);
}

/// Sides counted per ordered pair of values, at `left or upper × stride + right
/// or lower`, each cleared as it is read.
struct Pairs<'a> {
    stride: usize,
    pairs: &'a mut [u32],
}

impl Sides for Pairs<'_> {
    fn side(&mut self, a: u8, b: u8) {
        self.pairs[usize::from(a) * self.stride + usize::from(b)] += 1;
    }

    fn expose(&mut self, ranked: usize, left: &[u8], tally: &mut Ranking<'_>) {
        for &v in left {
            let v = usize::from(v);
            let sides = std::mem::take(&mut self.pairs[ranked * self.stride + v])
                + std::mem::take(&mut self.pairs[v * self.stride + ranked]);
            if sides > 0 {
                tally.exposure[v] += sides;
                tally.score[v] = STALE;
            }
        }
    }
}

/// Each value's neighbours, one entry per side: value `v`'s are
/// `neighbours[starts[v]..starts[v + 1]]`.
struct Lists<'a> {
    starts: &'a mut [u32],
    neighbours: &'a mut [u32],
}

impl<'a> Lists<'a> {
    /// Lists sized for the sides of `pixels`: `starts` holds where each value's
    /// list ends, and [`Sides::side`] fills it from there down to its start.
    fn new(pixels: &[u8], width: usize, starts: &'a mut [u32], neighbours: &'a mut [u32]) -> Self {
        struct Degrees<'s>(&'s mut [u32]);
        impl Walker for Degrees<'_> {
            fn pair(&mut self, a: u8, b: u8) {
                self.0[usize::from(a)] += 1;
                self.0[usize::from(b)] += 1;
            }
        }
        walk(pixels, width, &mut Degrees(starts));
        let mut end = 0;
        for start in starts.iter_mut() {
            end += *start;
            *start = end;
        }
        Self { starts, neighbours }
    }
}

impl Sides for Lists<'_> {
    fn side(&mut self, a: u8, b: u8) {
        for (v, neighbour) in [(a, b), (b, a)] {
            let start = &mut self.starts[usize::from(v)];
            *start -= 1;
            self.neighbours[*start as usize] = u32::from(neighbour);
        }
    }

    fn expose(&mut self, ranked: usize, _left: &[u8], tally: &mut Ranking<'_>) {
        let list = self.starts[ranked] as usize..self.starts[ranked + 1] as usize;
        for &v in &self.neighbours[list] {
            let v = v as usize;
            // Ranked values have no counted pixels left.
            if tally.counted[v] > 0 {
                tally.exposure[v] += 1;
                tally.score[v] = STALE;
            }
        }
    }
}

/// Count each value's counted pixels, its sides on the region's edge into
/// `exposure`, and the sides between unlike neighbours into `sides`, which
/// comes back for the ranking.
fn count<S: Sides>(
    pixels: &[u8],
    width: usize,
    counted: &mut [u32],
    exposure: &mut [u32],
    sides: S,
) -> S {
    // A length of 256 lets pixel values index the tallies unchecked.
    let mut tally = Tally {
        counted: &mut counted[..VALUES],
        exposure: &mut exposure[..VALUES],
        sides,
    };
    walk(pixels, width, &mut tally);
    tally.sides
}

/// The tallies of one region as its pixels are walked.
struct Tally<'a, S> {
    counted: &'a mut [u32],
    exposure: &'a mut [u32],
    sides: S,
}

impl<S: Sides> Walker for Tally<'_, S> {
    /// Sides on the edge: the row's ends, and all of the first and last rows.
    fn row(&mut self, row: &[u8], up: bool, down: bool) -> bool {
        self.exposure[usize::from(row[0])] += 1;
        self.exposure[usize::from(row[row.len() - 1])] += 1;
        let edges = u32::from(!up) + u32::from(!down);
        if edges > 0 {
            for &v in row {
                self.exposure[usize::from(v)] += edges;
            }
        }
        true
    }

    fn pixel(&mut self, value: u8, counted: bool) {
        self.counted[usize::from(value)] += u32::from(counted);
    }

    fn pair(&mut self, a: u8, b: u8) {
        self.sides.side(a, b);
    }
}

/// The tallies the ranking reads and clears.
struct Ranking<'a> {
    counted: &'a mut [u32],
    exposure: &'a mut [u32],
    score: &'a mut [u32],
}

/// Rank the counted values, all below `values`, into `out`, clearing the
/// tallies behind them.
fn rank(
    values: usize,
    counted: &mut [u32],
    exposure: &mut [u32],
    score: &mut [u32],
    sides: &mut impl Sides,
    out: &mut [u32; 256],
) {
    let mut tally = Ranking {
        counted: &mut counted[..VALUES],
        exposure: &mut exposure[..VALUES],
        score: &mut score[..VALUES],
    };
    // The values still to rank, in ascending order.
    let mut left = [0u8; VALUES];
    let mut len = 0;
    for v in (0..=u8::MAX).take(values) {
        if tally.counted[usize::from(v)] > 0 {
            left[len] = v;
            len += 1;
            tally.score[usize::from(v)] = STALE;
        }
    }
    // At least 1, so that a region of one value ranks it at level 0.
    let top = len.saturating_sub(1).max(1);
    for rank in 0..len {
        let remaining = len - rank;
        let mut at = 0;
        let mut high = 0;
        for (i, &v) in left[..remaining].iter().enumerate() {
            let v = usize::from(v);
            if tally.score[v] == STALE {
                tally.score[v] = ratio(tally.exposure[v], tally.counted[v]);
            }
            if i == 0 || tally.score[v] > high {
                at = i;
                high = tally.score[v];
            }
        }
        let best = usize::from(left[at]);
        left.copy_within(at + 1..remaining, at);
        let level = (255 * rank / top) as u32;
        out[best] = argb(level, level >> 1, level, level >> 1);
        tally.counted[best] = 0;
        tally.exposure[best] = 0;
        tally.score[best] = 0;
        sides.expose(best, &left[..remaining - 1], &mut tally);
    }
}

/// A score: `exposure` sides per counted pixel in 1024ths, rounded down. A
/// value's exposed sides belong to its counted pixels, at most four each, so
/// the score is at most 4096.
fn ratio(exposure: u32, counted: u32) -> u32 {
    match exposure.checked_mul(1024) {
        Some(scaled) => scaled / counted,
        None => (u64::from(exposure) * 1024 / u64::from(counted)) as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The automatic palette of `rows` over `out`.
    fn palette_over(rows: &[Vec<u8>], out: &mut [u32; 256]) {
        let budget = Budget::new(1 << 20).unwrap();
        let mut census = Census::new(&budget);
        let pixels: Vec<u8> = rows.concat();
        automatic(&pixels, rows[0].len(), &mut census, out).unwrap();
    }

    fn palette(rows: &[Vec<u8>]) -> [u32; 256] {
        let mut out = [0; 256];
        palette_over(rows, &mut out);
        out
    }

    /// The entries `out` holds other than 0.
    fn entries(out: &[u32; 256]) -> Vec<(usize, u32)> {
        out.iter()
            .copied()
            .enumerate()
            .filter(|&(_, word)| word != 0)
            .collect()
    }

    /// The values of `rows` from the background up, as their levels rank them.
    fn order(rows: &[Vec<u8>]) -> Vec<u8> {
        let out = palette(rows);
        let mut values: Vec<u8> = rows.concat();
        values.sort_unstable();
        values.dedup();
        values.sort_by_key(|&v| out[usize::from(v)] >> 24);
        values
    }

    fn row(values: &[u8]) -> Vec<Vec<u8>> {
        vec![values.to_vec()]
    }

    /// `count` rows of `values`.
    fn stripes(values: &[u8], count: usize) -> Vec<Vec<u8>> {
        vec![values.to_vec(); count]
    }

    /// A 5 × 5 region of 1 with a 2 at (`x`, `y`).
    fn dot(x: usize, y: usize) -> Vec<Vec<u8>> {
        let mut rows = vec![vec![1; 5]; 5];
        rows[y][x] = 2;
        rows
    }

    #[test]
    fn levels_rise_evenly_from_the_background() {
        // Columns 0 to 7, four rows: 0 and 7 tie at the ends, the rest tie
        // between them, so the values rank in order.
        let out = palette(&stripes(&[0, 1, 2, 3, 4, 5, 6, 7], 4));
        assert_eq!(
            out[..8],
            [
                0, 0x24122412, 0x48244824, 0x6d366d36, 0x91489148, 0xb65bb65b, 0xda6dda6d,
                0xff7fff7f
            ]
        );
        assert_eq!(entries(&out).len(), 7);
        // Columns 1 to 15 then 0: 0 and 1 tie at the ends, 0 first; levels step
        // by 17.
        let mut values: Vec<u8> = (1..16).collect();
        values.push(0);
        let out = palette(&stripes(&values, 4));
        for v in 0..16 {
            let level = 17 * v as u32;
            assert_eq!(out[v], argb(level, level >> 1, level, level >> 1), "{v}");
        }
    }

    #[test]
    fn a_region_of_one_value_is_transparent() {
        for (rows, v) in [
            (stripes(&[5; 4], 4), 5),
            (row(&[3]), 3),
            (stripes(&[255; 16], 16), 255),
        ] {
            let mut out = [7; 256];
            palette_over(&rows, &mut out);
            assert_eq!(out[v], 0);
            assert_eq!(out.iter().filter(|&&word| word == 7).count(), 255);
        }
    }

    #[test]
    fn ties_go_to_the_smaller_value() {
        assert_eq!(order(&row(&[1, 2])), [1, 2]);
        assert_eq!(order(&row(&[2, 1])), [1, 2]);
        assert_eq!(order(&row(&[1, 2, 3])), [1, 2, 3]);
        assert_eq!(order(&row(&[3, 2, 1])), [1, 2, 3]);
        assert_eq!(order(&row(&[1, 2, 1, 2])), [1, 2]);
        assert_eq!(order(&row(&[2, 1, 2, 1])), [1, 2]);
    }

    #[test]
    fn values_rank_by_exposure_per_counted_pixel() {
        // One row: every pixel is counted and on the edge above and below.
        assert_eq!(order(&row(&[1, 1, 2, 3])), [3, 2, 1]);
        assert_eq!(order(&row(&[1, 2, 2, 3])), [1, 3, 2]);
        assert_eq!(order(&row(&[1, 2, 1])), [1, 2]);
        assert_eq!(order(&row(&[2, 1, 2])), [2, 1]);
        assert_eq!(order(&row(&[5, 9, 5, 9, 5])), [5, 9]);
        assert_eq!(order(&row(&[1, 1, 1, 2, 2, 3])), [3, 2, 1]);
        // A column ranks as the row does.
        let column: Vec<Vec<u8>> = [1, 1, 2, 3].iter().map(|&v| vec![v]).collect();
        assert_eq!(order(&column), [3, 2, 1]);
        // Four rows: the middle of 1's three columns is enclosed in rows 1 and 2.
        assert_eq!(order(&stripes(&[1, 1, 1, 2, 2, 3], 4)), [3, 1, 2]);
        assert_eq!(order(&stripes(&[1, 1, 2, 3], 4)), [3, 2, 1]);
        assert_eq!(order(&stripes(&[1, 2, 2, 3], 4)), [1, 3, 2]);
    }

    #[test]
    fn ranked_values_expose_their_neighbours() {
        // 2 ranks first; then each 1 has a side against it or the edge.
        assert_eq!(order(&row(&[2, 1, 3, 1])), [2, 1, 3]);
        assert_eq!(order(&row(&[1, 2, 1, 3])), [3, 1, 2]);
        assert_eq!(order(&row(&[3, 1, 2, 1])), [3, 1, 2]);
        // A frame of 1 around 2 around a 3 ranks from the outside in.
        let mut rows = vec![vec![1; 7]; 7];
        for line in &mut rows[1..6] {
            line[1..6].fill(2);
        }
        rows[3][3] = 3;
        assert_eq!(order(&rows), [1, 2, 3]);
    }

    #[test]
    fn enclosed_pixels_do_not_count() {
        // A corner 2 has two sides on the edge (2048); 1 has 18 on its 15 counted
        // pixels (1228).
        assert_eq!(order(&dot(0, 0)), [2, 1]);
        // At the middle of the top edge 2 has one (1024); 1 has 19 on 16 counted
        // pixels (1216), though 19 sides on all 24 of its pixels would be 810.
        assert_eq!(order(&dot(2, 0)), [1, 2]);
        assert_eq!(order(&dot(2, 2)), [1, 2]);
    }

    #[test]
    fn scores_are_1024ths_rounded_down() {
        // Two rows, `a` columns of `left` then `b` of `right`: scores
        // 1024 + 1024 / a and 1024 + 1024 / b, rounded down.
        let first = |a: usize, left: u8, b: usize, right: u8| {
            let mut line = vec![left; a];
            line.resize(a + b, right);
            order(&stripes(&line, 2))[0]
        };
        // 1025 each, though 5's exposure per pixel is the greater.
        assert_eq!(first(600, 5, 680, 3), 3);
        assert_eq!(first(731, 3, 549, 5), 3);
        assert_eq!(first(700, 3, 560, 5), 3);
        assert_eq!(first(100, 5, 101, 3), 3);
        // 1026 against 1025.
        assert_eq!(first(505, 5, 520, 3), 5);
        assert_eq!(first(512, 5, 513, 3), 5);
        assert_eq!(first(513, 3, 512, 5), 5);
    }

    #[test]
    fn every_depth_ranks_alike() {
        let two = palette(&[
            vec![1, 1, 2, 2],
            vec![1, 1, 2, 2],
            vec![1, 1, 1, 1],
            vec![2, 2, 2, 2],
        ]);
        assert_eq!(entries(&two), [(1, 0xff7fff7f)]);
        let eight = palette(&[
            vec![10, 10, 20, 20],
            vec![10, 30, 30, 20],
            vec![10, 10, 20, 20],
            vec![30, 30, 30, 30],
        ]);
        assert_eq!(entries(&eight), [(20, 0xff7fff7f), (30, 0x7f3f7f3f)]);
    }

    #[test]
    fn unused_entries_keep_their_words() {
        let mut out = [0; 256];
        palette_over(
            &[
                vec![1, 1, 2, 2],
                vec![1, 3, 3, 2],
                vec![1, 1, 2, 2],
                vec![3, 3, 3, 3],
            ],
            &mut out,
        );
        palette_over(
            &[
                vec![4, 4, 5, 5],
                vec![4, 4, 5, 5],
                vec![4, 4, 4, 4],
                vec![5, 5, 5, 5],
            ],
            &mut out,
        );
        assert_eq!(
            entries(&out),
            [(2, 0xff7fff7f), (3, 0x7f3f7f3f), (4, 0xff7fff7f)]
        );
    }

    #[test]
    fn a_large_exposure_still_ranks() {
        // 1280 × 1024: a frame of 1 around a 1/2 checkerboard with a 3 × 3 block
        // of 3 in the middle. Once 1 is ranked, 2 has about 2.6 million exposed
        // sides, beyond 2^31 / 1024, and still ranks before 3.
        let (width, height) = (1280, 1024);
        let rows: Vec<Vec<u8>> = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| {
                        if x == 0 || y == 0 || x == width - 1 || y == height - 1 {
                            1
                        } else if (511..514).contains(&y) && (639..642).contains(&x) {
                            3
                        } else {
                            1 + ((x + y) % 2) as u8
                        }
                    })
                    .collect()
            })
            .collect();
        assert_eq!(palette(&rows)[..4], [0, 0, 0x7f3f7f3f, 0xff7fff7f]);
    }

    #[test]
    fn the_census_stays_within_its_plan() {
        let budget = Budget::new(1 << 20).unwrap();
        let mut census = Census::new(&budget);
        assert_eq!(census.bytes(), 0);
        let mut out = [0; 256];
        // Words held after each 2 × 2 region 0 max / 1 max: the tallies, then the
        // pair table, or for 8-bit values lists for its four neighbouring pairs,
        // 257 + 8 words, which are smaller. The census keeps its largest, and
        // leaves it clear; it never holds more than the plan for the largest
        // values so far.
        let mut planned = 0;
        for (max, words) in [(3, 16), (15, 256), (2, 256), (255, 265), (9, 265)] {
            automatic(&[0, max, 1, max], 2, &mut census, &mut out).unwrap();
            assert_eq!(census.bytes(), (768 + words) * 4, "{max}");
            planned = planned.max(Census::bytes_for(usize::from(max) + 1));
            assert!(census.bytes() <= planned);
            assert_eq!(
                budget.used(),
                Budget::control_storage_bytes() + census.bytes()
            );
            assert!(census.table.iter().all(|&word| word == 0));
        }
        assert_eq!(Census::bytes_for(4), (768 + 16) * 4);
        assert_eq!(Census::bytes_for(16), (768 + 256) * 4);
        assert_eq!(Census::bytes_for(256), (768 + 65536) * 4);
        // A 16 × 16 region of all 256 values: lists for its 480 pairs.
        let mut fresh = Census::new(&budget);
        let all: Vec<u8> = (0..=255).collect();
        automatic(&all, 16, &mut fresh, &mut out).unwrap();
        assert_eq!(fresh.bytes(), (768 + 257 + 960) * 4);
        // 8-bit noise with more unlike pairs than the table has words uses the
        // table, as much as the plan allows.
        let noise = noise(200, 200, 16, 240);
        automatic(&noise.concat(), 200, &mut census, &mut out).unwrap();
        assert_eq!(census.bytes(), Census::bytes_for(256));
        assert!(census.table.iter().all(|&word| word == 0));
    }

    /// `width × height` pixels of `count` values from `base` up, from a fixed
    /// sequence.
    fn noise(width: usize, height: usize, count: u32, base: u8) -> Vec<Vec<u8>> {
        let mut state = 12345u32;
        (0..height)
            .map(|_| {
                (0..width)
                    .map(|_| {
                        state = state.wrapping_mul(1103515245).wrapping_add(12345);
                        base + ((state >> 16) % count) as u8
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn lists_are_kept_while_smaller_than_the_table() {
        // One row of 40,000 pixels alternating 16 and 17 for its first `changes`
        // steps: lists for 32,639 unlike pairs take 257 + 65,278 = 65,535 words,
        // one under the 65,536-word table; for 32,640 they would take 65,537.
        let row = |changes: usize| -> Vec<u8> {
            (0..40_000usize)
                .map(|x| 16 + (x.min(changes) % 2) as u8)
                .collect()
        };
        assert_eq!(unlike(&row(32_639), 40_000, VALUES * VALUES), Some(32_639));
        assert_eq!(unlike(&row(32_640), 40_000, VALUES * VALUES), None);
    }

    #[test]
    fn the_pair_table_and_the_lists_rank_alike() {
        // The same pattern in values 0 to 15 (the 16-value table) and 240 to 255
        // (the lists when small, the 256-value table when not) ranks the same.
        for (width, height) in [(16, 16), (5, 40), (64, 10), (200, 200)] {
            let low = noise(width, height, 16, 0);
            let high = noise(width, height, 16, 240);
            let shifted: Vec<u8> = order(&low).iter().map(|v| v + 240).collect();
            assert_eq!(order(&high), shifted, "{width} × {height}");
            assert_eq!(palette(&low)[..16], palette(&high)[240..]);
        }
    }

    #[test]
    fn a_refused_census_is_an_error() {
        let budget = Budget::new(Budget::control_storage_bytes() + Census::bytes_for(16)).unwrap();
        let mut census = Census::new(&budget);
        let mut out = [9; 256];
        assert!(automatic(&[200, 1, 1, 1], 2, &mut census, &mut out).is_err());
        assert_eq!(out, [9; 256]);
        automatic(&[15, 1, 1, 1], 2, &mut census, &mut out).unwrap();
        assert_eq!(out[1], 0);
        assert_eq!(out[15], 0xff7fff7f);
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
            assert_eq!(entry_word(y, cr, cb, t), word, "{y} {cr} {cb} {t}");
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
}
