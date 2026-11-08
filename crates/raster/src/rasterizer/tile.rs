//! 16×16 tile kernels: solid fill, one half-plane, the generic sum of
//! trapezoids, and the union of two tiles.
//!
//! Coverage inside a tile is 16-bit fixed point with full coverage at
//! [`FULL`]; the arithmetic wraps at 16 bits exactly where libass's engines
//! truncate to `int16_t`.

use super::{Line, LineFlags};

/// Tile edge in pixels and its log2.
pub(super) const TILE: usize = 16;
pub(super) const TILE_ORDER: u32 = 4;
/// Full coverage of one pixel.
const FULL: i16 = 1 << (14 - TILE_ORDER);

/// A tile's top-left byte and the row stride of the bitmap it lives in.
pub(super) struct TileMut<'a> {
    pub(super) data: &'a mut [u8],
    pub(super) stride: usize,
}

/// `(ab · scale + 2^49) >> 50`: a line coefficient in tile units.
#[inline(always)]
fn rescale_ab(ab: i32, scale: i32) -> i16 {
    ((i64::from(ab) * i64::from(scale) + (1 << (45 + TILE_ORDER))) >> (46 + TILE_ORDER)) as i16
}

/// The line constant in tile units.
#[inline(always)]
fn rescale_c(c: i64, scale: i32) -> i64 {
    (i64::from((c >> (7 + TILE_ORDER)) as i32) * i64::from(scale) + (1 << 44)) >> 45
}

pub(super) fn fill_solid(tile: &mut TileMut<'_>, set: bool) {
    let value = if set { 255 } else { 0 };
    let stride = tile.stride;
    for y in 0..TILE {
        tile.data[y * stride..][..TILE].fill(value);
    }
}

/// Fills `a·x + b·y < c` with antialiasing: the mean of two clamped distances
/// offset by a quarter of the smaller coefficient.
pub(super) fn fill_halfplane(tile: &mut TileMut<'_>, a: i32, b: i32, c: i64, scale: i32) {
    let aa = rescale_ab(a, scale);
    let bb = rescale_ab(b, scale);
    let mut cc = (rescale_c(c, scale) + i64::from(FULL / 2)
        - i64::from((i32::from(aa) + i32::from(bb)) >> 1)) as i16;
    let abs_a = aa.wrapping_abs();
    let abs_b = bb.wrapping_abs();
    let delta = ((i32::from(abs_a.min(abs_b)) + 2) >> 2) as i16;
    let mut va1 = [0i16; TILE];
    let mut va2 = [0i16; TILE];
    for x in 0..TILE {
        let ax = aa.wrapping_mul(x as i16);
        va1[x] = ax.wrapping_sub(delta);
        va2[x] = ax.wrapping_add(delta);
    }
    let stride = tile.stride;
    for y in 0..TILE {
        let row = &mut tile.data[y * stride..][..TILE];
        for x in 0..TILE {
            let c1 = cc.wrapping_sub(va1[x]).clamp(0, FULL);
            let c2 = cc.wrapping_sub(va2[x]).clamp(0, FULL);
            let res = (c1 + c2) >> (7 - TILE_ORDER);
            row[x] = res.min(255) as u8;
        }
        cc = cc.wrapping_sub(bb);
    }
}

/// Adds the antialiased top or bottom edge of a trapezoid, covering the
/// sub-row span `up..dn` (in 1/64 pixel), to `res`.
#[allow(clippy::too_many_arguments)] // the edge's precomputed terms
#[inline(always)]
fn update_border_line(
    res: &mut [i16; TILE],
    abs_a: i16,
    va: &[i16; TILE],
    b: i16,
    abs_b: i16,
    c: i16,
    up: i16,
    dn: i16,
) {
    let size = dn - up;
    let w = (i32::from(FULL) + (i32::from(size) << (8 - TILE_ORDER)) - i32::from(abs_a)) as i16;
    let w = i32::from(w.min(FULL) << (2 * TILE_ORDER - 5));
    let dc_b = ((i32::from(abs_b) * i32::from(size)) >> 6) as i16;
    let dc = ((i32::from(abs_a.min(dc_b)) + 2) >> 2) as i16;
    let base = ((i32::from(b) * i32::from(up + dn)) >> 7) as i16;
    let (base, dc) = (i32::from(base), i32::from(dc));
    let offs1 = (i32::from(size) - (((base + dc) * w) >> 16)) as i16;
    let offs2 = (i32::from(size) - (((base - dc) * w) >> 16)) as i16;
    let size = size << 1;
    for x in 0..TILE {
        let cw = (((i32::from(c) - i32::from(va[x])) * w) >> 16) as i16;
        let c1 = cw.wrapping_add(offs1).clamp(0, size);
        let c2 = cw.wrapping_add(offs2).clamp(0, size);
        res[x] = res[x].wrapping_add(c1 + c2);
    }
}

/// Renders each line as the trapezoid between it and the tile's left edge,
/// summing signed coverage, then stores the clamped magnitude.
pub(super) fn fill_generic(tile: &mut TileMut<'_>, lines: &[Line], winding: i32) {
    let mut res = [[0i16; TILE]; TILE];
    let mut delta = [0i16; TILE + 2];
    for line in lines {
        let mut up_delta: i16 = if line.flags.contains(LineFlags::DN) {
            4
        } else {
            0
        };
        let mut dn_delta = up_delta;
        if line.x_min == 0 && line.flags.contains(LineFlags::EXACT_LEFT) {
            dn_delta ^= 4;
        }
        if line.flags.contains(LineFlags::UL_DR) {
            std::mem::swap(&mut up_delta, &mut dn_delta);
        }
        let mut up = (line.y_min >> 6) as usize;
        let dn = (line.y_max >> 6) as usize;
        let up_pos = (line.y_min & 63) as i16;
        let up_delta1 = up_delta * up_pos;
        let dn_pos = (line.y_max & 63) as i16;
        let dn_delta1 = dn_delta * dn_pos;
        delta[up + 1] = delta[up + 1].wrapping_sub(up_delta1);
        delta[up] = delta[up].wrapping_sub((up_delta << 6) - up_delta1);
        delta[dn + 1] = delta[dn + 1].wrapping_add(dn_delta1);
        delta[dn] = delta[dn].wrapping_add((dn_delta << 6) - dn_delta1);
        if line.y_min == line.y_max {
            continue;
        }

        let a = rescale_ab(line.a, line.scale);
        let b = rescale_ab(line.b, line.scale);
        let mut c =
            (rescale_c(line.c, line.scale) - i64::from(a >> 1) - i64::from(b) * up as i64) as i16;
        let mut va = [0i16; TILE];
        for (x, v) in va.iter_mut().enumerate() {
            *v = a.wrapping_mul(x as i16);
        }
        let abs_a = a.wrapping_abs();
        let abs_b = b.wrapping_abs();
        let dc = ((i32::from(abs_a.min(abs_b)) + 2) >> 2) as i16;
        let base = FULL / 2 - (b >> 1);
        let dc1 = base.wrapping_add(dc);
        let dc2 = base.wrapping_sub(dc);

        if up_pos != 0 {
            if dn == up {
                update_border_line(&mut res[up], abs_a, &va, b, abs_b, c, up_pos, dn_pos);
                continue;
            }
            update_border_line(&mut res[up], abs_a, &va, b, abs_b, c, up_pos, 64);
            up += 1;
            c = c.wrapping_sub(b);
        }
        for row in &mut res[up..dn] {
            for x in 0..TILE {
                let base = c.wrapping_sub(va[x]);
                let c1 = base.wrapping_add(dc1).clamp(0, FULL);
                let c2 = base.wrapping_add(dc2).clamp(0, FULL);
                row[x] = row[x].wrapping_add((c1 + c2) >> (7 - TILE_ORDER));
            }
            c = c.wrapping_sub(b);
        }
        if dn_pos != 0 {
            update_border_line(&mut res[dn], abs_a, &va, b, abs_b, c, 0, dn_pos);
        }
    }

    let mut cur = (256 * i32::from(winding as i8)) as i16;
    let stride = tile.stride;
    for y in 0..TILE {
        cur = cur.wrapping_add(delta[y]);
        let out = &mut tile.data[y * stride..][..TILE];
        for x in 0..TILE {
            let val = res[y][x].wrapping_add(cur);
            let val = val.max(val.wrapping_neg());
            out[x] = val.min(255) as u8;
        }
    }
}

/// The union of a tile with `other`: the larger coverage of each pixel.
pub(super) fn merge(tile: &mut TileMut<'_>, other: &[u8; TILE * TILE]) {
    let stride = tile.stride;
    for (y, src) in other.chunks_exact(TILE).enumerate() {
        let row = &mut tile.data[y * stride..][..TILE];
        for (d, &s) in row.iter_mut().zip(src) {
            *d = (*d).max(s);
        }
    }
}
