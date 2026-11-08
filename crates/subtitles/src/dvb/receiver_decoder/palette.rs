//! Receiver colours: CLUT entries as ARGB words, the default CLUTs and the
//! automatic palette the receiver builds for a region from its pixels.
use crate::dvb::Depth;
use triergon_core::{Budget, BudgetVec, Result};

const fn argb(r: u8, g: u8, b: u8, a: u8) -> u32 {
    (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// A Y/Cr/Cb/T entry as the receiver's ARGB word: the BT.601 studio inverse
/// in 10-bit fixed point, and fully transparent when Y is zero.
pub(super) fn ycrcbt(y: u8, cr: u8, cb: u8, t: u8) -> u32 {
    let yy = 1192 * (i32::from(y) - 16);
    let cr = i32::from(cr) - 128;
    let cb = i32::from(cb) - 128;
    let channel = |value: i32| ((value + 512) >> 10).clamp(0, 255) as u8;
    let alpha = if y == 0 { 0 } else { 255 - t };
    argb(
        channel(yy + 1634 * cr),
        channel(yy - 832 * cr - 401 * cb),
        channel(yy + 2066 * cb),
        alpha,
    )
}

const fn bit(index: u8, n: u8) -> bool {
    (index >> n) & 1 == 1
}

/// `on` where bit `n` of `index` is set.
const fn level(index: u8, n: u8, on: u8) -> u8 {
    if bit(index, n) { on } else { 0 }
}

/// Bits `low` and `low + 4` of `index` as a two-bit level.
const fn pair(index: u8, low: u8) -> usize {
    bit(index, low) as usize | (bit(index, low + 4) as usize) << 1
}

/// The receiver's default CLUT entry `index` at `depth` (EN 300 743 §10).
const fn default_word(depth: Depth, index: u8) -> u32 {
    if index == 0 {
        return 0;
    }
    match depth {
        Depth::Two => match index {
            1 => argb(255, 255, 255, 255),
            2 => argb(0, 0, 0, 255),
            _ => argb(127, 127, 127, 255),
        },
        Depth::Four => {
            let on = if index < 8 { 255 } else { 127 };
            argb(
                level(index, 0, on),
                level(index, 1, on),
                level(index, 2, on),
                255,
            )
        }
        Depth::Eight if index < 8 => argb(
            level(index, 0, 255),
            level(index, 1, 255),
            level(index, 2, 255),
            63,
        ),
        Depth::Eight => {
            let (levels, alpha) = match index & 0x88 {
                0x00 => ([0, 85, 170, 255], 255),
                0x08 => ([0, 85, 170, 255], 127),
                0x80 => ([127, 170, 212, 255], 255),
                _ => ([0, 43, 85, 128], 255),
            };
            argb(
                levels[pair(index, 0)],
                levels[pair(index, 1)],
                levels[pair(index, 2)],
                alpha,
            )
        }
    }
}

const fn table<const N: usize>(depth: Depth) -> [u32; N] {
    let mut words = [0; N];
    let mut i = 0;
    while i < N {
        words[i] = default_word(depth, i as u8);
        i += 1;
    }
    words
}

/// The CLUT tables a region of each depth reads.
#[derive(Clone, Debug)]
pub(super) struct Tables {
    two: [u32; 4],
    four: [u32; 16],
    eight: [u32; 256],
}

/// The default CLUT, used for a region whose CLUT was never defined.
pub(super) static DEFAULT: Tables = Tables {
    two: table(Depth::Two),
    four: table(Depth::Four),
    eight: table(Depth::Eight),
};

impl Tables {
    pub(super) fn get(&self, depth: Depth) -> &[u32] {
        match depth {
            Depth::Two => &self.two,
            Depth::Four => &self.four,
            Depth::Eight => &self.eight,
        }
    }

    /// Set `entry` in the first table its CDS flags select (bit 2: 2-bit,
    /// 1: 4-bit, 0: 8-bit) that has room for it; the receiver writes one table
    /// per entry, the shallowest.
    pub(super) fn set(&mut self, entry: u8, flags: u8, word: u32) {
        let entry = usize::from(entry);
        let slot = [
            (4, &mut self.two[..]),
            (2, &mut self.four[..]),
            (1, &mut self.eight[..]),
        ]
        .into_iter()
        .filter(|(bit, _)| flags & bit != 0)
        .find_map(|(_, words)| words.get_mut(entry));
        if let Some(slot) = slot {
            *slot = word;
        }
    }
}

/// The neighbour counts [`automatic`] ranks colours by, kept across regions
/// so a stream pays for them once: `touch[class * colours + colour]` is how
/// often a pixel of `colour` has a neighbour of `class`, the region edge (0)
/// or another colour (`c + 1`).
#[derive(Debug)]
pub(super) struct Census {
    touch: BudgetVec<u32>,
}

impl Census {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            touch: BudgetVec::new(budget),
        }
    }

    fn reset(&mut self, colours: usize) -> Result<&mut [u32]> {
        self.touch.clear();
        self.touch.try_resize((colours + 1) * colours, 0)?;
        Ok(self.touch.as_mut_slice())
    }
}

/// The automatic palette: grey-green levels ranked from the region edge
/// inwards. Starting from the edge, the colour that most of its outline
/// shares with the edge and the colours ranked so far comes next; ranked
/// colours take evenly spaced levels from transparent to opaque, and the rest
/// keep their values. `pixels` holds rows of `width` indices; every value is
/// a colour of its own, whatever the region's depth.
pub(super) fn automatic(
    pixels: &[u8],
    width: usize,
    census: &mut Census,
    out: &mut [u32; 256],
) -> Result<()> {
    let colours = pixels.iter().max().map_or(0, |&p| usize::from(p) + 1);
    let touch = census.reset(colours)?;
    // Pixels of each colour with a 4-neighbour of another colour or the edge.
    let mut edge = [0_u32; 256];
    let edge = &mut edge[..colours];
    let class = usize::from;
    let height = pixels.len().checked_div(width).unwrap_or(0);
    for y in 0..height {
        for x in 0..width {
            let at = y * width + x;
            let c = class(pixels[at]);
            let neighbour = |near: bool, at: usize| if near { class(pixels[at]) + 1 } else { 0 };
            let mut outline = false;
            for other in [
                neighbour(x > 0, at.wrapping_sub(1)),
                neighbour(x + 1 < width, at + 1),
                neighbour(y > 0, at.wrapping_sub(width)),
                neighbour(y + 1 < height, at + width),
            ] {
                if other != c + 1 {
                    outline = true;
                    touch[other * colours + c] += 1;
                }
            }
            edge[c] += u32::from(outline);
        }
    }
    // Neighbour count of each colour with the edge and the ranked colours.
    let mut reach = [0_u32; 256];
    let reach = &mut reach[..colours];
    reach.copy_from_slice(&touch[..colours]);
    let mut taken = [false; 256];
    let mut order = [0_u8; 256];
    let mut ranked = 0;
    loop {
        let mut best = None;
        let mut top = 0;
        for c in (0..colours).filter(|&c| !taken[c] && reach[c] != 0) {
            let score = u64::from(reach[c]) * 1024 / u64::from(edge[c]);
            if score > top {
                (top, best) = (score, Some(c));
            }
        }
        let Some(next) = best else { break };
        taken[next] = true;
        order[ranked] = next as u8;
        ranked += 1;
        for (r, &t) in reach
            .iter_mut()
            .zip(&touch[(next + 1) * colours..][..colours])
        {
            *r += t;
        }
    }
    let steps = ranked.saturating_sub(1).max(1);
    for (rank, &c) in order[..ranked].iter().enumerate() {
        let v = (rank * 255 / steps) as u8;
        out[usize::from(c)] = argb(v / 2, v, v / 2, v);
    }
    Ok(())
}
