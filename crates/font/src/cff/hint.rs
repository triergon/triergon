//! The hinter of FreeType's CFF engine (`psaux/pshints.c`, `psaux/psblues.c`)
//! as its default load runs it, without stem darkening: the stem hints and hint
//! masks a charstring declares, the blue zones of its Private DICT at a size,
//! and the hint maps that move horizontal stem edges onto pixel boundaries.
//! Vertical stems only count towards the length of masks, as in FreeType.
//!
//! The arithmetic is cf2's: 16.16 values in 32 bits that wrap where FreeType's
//! `ADD_INT32` and `SUB_INT32` wrap, with `FT_MulFix`, `FT_DivFix` and
//! `FT_MulDiv` results narrowed to 32 bits as `CF2_Fixed` narrows them.

use super::path::Fixed;
use crate::fixed::{div_fix, mul_div, mul_fix};
use crate::read::Reader;
use crate::work::charge;
use triergon_core::{Error, Result, WorkBudget};

/// The stem hints a mask can select (`CF2_MAX_HINTS`).
const MAX_HINTS: usize = 96;
/// Edges of a hint map (`CF2_MAX_HINT_EDGES`).
const MAX_EDGES: usize = 2 * MAX_HINTS;
/// Zones of BlueValues and OtherBlues (`CF2_MAX_BLUES + CF2_MAX_OTHERBLUES`).
const MAX_ZONES: usize = 12;
const ONE: Fixed = 0x1_0000;
/// The least distance kept between hint edges (`CF2_MIN_COUNTER`, 0.5).
const MIN_COUNTER: Fixed = 0x8000;
/// The em box of ideographic fonts (`CF2_ICF_Bottom`, `CF2_ICF_Top`).
const EM_BOX_BOTTOM: Fixed = -120 << 16;
const EM_BOX_TOP: Fixed = 880 << 16;

/// `FT_MulFix` narrowed to 32 bits.
pub(super) fn mul(a: Fixed, b: Fixed) -> Fixed {
    mul_fix(i64::from(a), i64::from(b)) as Fixed
}

/// `FT_DivFix` narrowed to 32 bits.
fn div(a: Fixed, b: Fixed) -> Fixed {
    div_fix(i64::from(a), i64::from(b)) as Fixed
}

/// `cf2_fixedRound`: to the nearest whole number, halves up.
const fn round(x: Fixed) -> Fixed {
    (x.cast_unsigned().wrapping_add(0x8000) & 0xffff_0000).cast_signed()
}

/// `cf2_fixedFraction`.
const fn fraction(x: Fixed) -> Fixed {
    x.wrapping_sub((x.cast_unsigned() & 0xffff_0000).cast_signed())
}

/// `cf2_fixedAbs`, which leaves the most negative value as it is.
const fn abs(x: Fixed) -> Fixed {
    if x < 0 { x.wrapping_neg() } else { x }
}

/// `cf2_intToFixed` of a value FreeType keeps in a long.
const fn int_to_fixed(v: i64) -> Fixed {
    ((v as u32) << 16).cast_signed()
}

/// What a hint edge is (`CF2_GhostBottom` and the rest).
mod flag {
    pub(super) const GHOST_BOTTOM: u8 = 0x1;
    pub(super) const GHOST_TOP: u8 = 0x2;
    pub(super) const PAIR_BOTTOM: u8 = 0x4;
    pub(super) const PAIR_TOP: u8 = 0x8;
    pub(super) const LOCKED: u8 = 0x10;
    pub(super) const SYNTHETIC: u8 = 0x20;
}

/// A hint edge (`CF2_Hint`): a character space coordinate, its device space
/// position and the scale from it to the next edge. No flags marks an edge
/// that is absent.
#[derive(Clone, Copy, Debug, Default)]
struct Edge {
    flags: u8,
    /// The stem hint the edge comes from.
    index: u8,
    cs: Fixed,
    ds: Fixed,
    scale: Fixed,
}

impl Edge {
    /// `cf2_hint_init`: the bottom or top edge of horizontal stem `index`,
    /// moved by the accent offset `origin`. A stem placed by an earlier hint
    /// map keeps its place, locked.
    fn new(stem: Stem, index: usize, origin: Fixed, scale: Fixed, bottom: bool) -> Self {
        use flag::{GHOST_BOTTOM, GHOST_TOP, PAIR_BOTTOM, PAIR_TOP};
        let width = stem.max.wrapping_sub(stem.min);
        // Widths of -21 and -20 mark ghost edges; other negative widths are
        // inverted pairs, which FreeType turns round.
        let (flags, cs) = match (width, bottom) {
            (-0x15_0000, true) => (GHOST_BOTTOM, stem.max),
            (-0x15_0000, false) | (-0x14_0000, true) => (0, 0),
            (-0x14_0000, false) => (GHOST_TOP, stem.min),
            (..0, true) => (PAIR_BOTTOM, stem.max),
            (..0, false) => (PAIR_TOP, stem.min),
            (_, true) => (PAIR_BOTTOM, stem.min),
            (_, false) => (PAIR_TOP, stem.max),
        };
        let mut edge = Self {
            flags,
            // Stems beyond 96 never reach a hint map.
            index: index as u8,
            cs: cs.wrapping_add(origin),
            ds: 0,
            scale,
        };
        if flags != 0 && stem.used {
            edge.ds = if edge.is_top() {
                stem.max_ds
            } else {
                stem.min_ds
            };
            edge.flags |= flag::LOCKED;
        } else {
            edge.ds = mul(edge.cs, scale);
        }
        edge
    }

    const fn is_valid(self) -> bool {
        self.flags != 0
    }

    const fn is_pair(self) -> bool {
        self.flags & (flag::PAIR_BOTTOM | flag::PAIR_TOP) != 0
    }

    const fn is_pair_top(self) -> bool {
        self.flags & flag::PAIR_TOP != 0
    }

    const fn is_top(self) -> bool {
        self.flags & (flag::PAIR_TOP | flag::GHOST_TOP) != 0
    }

    const fn is_bottom(self) -> bool {
        self.flags & (flag::PAIR_BOTTOM | flag::GHOST_BOTTOM) != 0
    }

    const fn is_locked(self) -> bool {
        self.flags & flag::LOCKED != 0
    }

    const fn is_synthetic(self) -> bool {
        self.flags & flag::SYNTHETIC != 0
    }

    /// A locked, synthetic ghost edge (the em box's and the baseline's).
    const fn synthetic(ghost: u8, cs: Fixed, ds: Fixed, scale: Fixed) -> Self {
        Self {
            flags: ghost | flag::LOCKED | flag::SYNTHETIC,
            index: 0,
            cs,
            ds,
            scale,
        }
    }
}

/// A stem hint (`CF2_StemHint`): its edges in character space and, once a hint
/// map has placed them, in device space.
#[derive(Clone, Copy, Debug, Default)]
struct Stem {
    min: Fixed,
    max: Fixed,
    min_ds: Fixed,
    max_ds: Fixed,
    used: bool,
}

/// A hint mask (`CF2_HintMask`): one bit per stem, horizontal stems first.
#[derive(Clone, Copy, Debug, Default)]
struct Mask {
    bits: [u8; MAX_HINTS / 8],
    count: usize,
    valid: bool,
    /// Read since the last hint map was built from it.
    new: bool,
}

impl Mask {
    /// `cf2_hintmask_setCounts`: more than 96 stems fail the glyph.
    fn set_count(&mut self, count: usize) -> Result<()> {
        if count > MAX_HINTS {
            return Err(Error::Invalid("more than 96 stem hints"));
        }
        self.count = count;
        self.valid = true;
        self.new = true;
        Ok(())
    }

    /// `cf2_hintmask_read`: the mask bytes after a `hintmask` or `cntrmask`.
    fn read(&mut self, r: &mut Reader<'_>, count: usize) -> Result<()> {
        self.set_count(count)?;
        for byte in &mut self.bits[..count.div_ceil(8)] {
            *byte = r.u8()?;
        }
        Ok(())
    }

    /// `cf2_hintmask_setAll`.
    fn set_all(&mut self, count: usize) -> Result<()> {
        self.set_count(count)?;
        let bytes = &mut self.bits[..count.div_ceil(8)];
        bytes.fill(0xff);
        if let Some(last) = bytes.last_mut() {
            *last &= !((1_u8 << (count.wrapping_neg() & 7)) - 1);
        }
        Ok(())
    }

    const fn bit(&self, i: usize) -> bool {
        self.bits[i / 8] & (0x80 >> (i % 8)) != 0
    }

    const fn clear(&mut self, i: usize) {
        self.bits[i / 8] &= !(0x80 >> (i % 8));
    }
}

/// A delta-encoded Private DICT array as FreeType stores it: 16.16 running
/// sums, written over the values of an earlier entry, and the count of the
/// last entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Deltas<const N: usize> {
    values: [i64; N],
    count: usize,
}

impl<const N: usize> Deltas<N> {
    const EMPTY: Self = Self {
        values: [0; N],
        count: 0,
    };

    /// `cff_kind_delta_fixed`: the running sums of the first `N` deltas.
    pub(super) fn set(&mut self, deltas: impl Iterator<Item = i64>) {
        let mut sum = 0_i64;
        self.count = 0;
        for (slot, delta) in self.values.iter_mut().zip(deltas) {
            sum = sum.wrapping_add(delta);
            *slot = sum;
            self.count += 1;
        }
    }

    /// The pairs FreeType's zone loops read: from each even index, with the
    /// value after an odd count's last (left from an earlier entry, or zero).
    fn pairs(&self) -> impl Iterator<Item = (Fixed, Fixed)> + '_ {
        // `N` is even, so an odd count leaves room for the value after it.
        (0..self.count)
            .step_by(2)
            .map(|i| (self.values[i] as Fixed, self.values[i + 1] as Fixed))
    }
}

/// The Private DICT values the hinter reads, as FreeType stores them
/// (`CFF_PrivateRec`): the blue arrays in 16.16, BlueScale in 16.16 times
/// 1000.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HintDict {
    pub(super) blue_values: Deltas<14>,
    pub(super) other_blues: Deltas<10>,
    pub(super) family_blues: Deltas<14>,
    pub(super) family_other_blues: Deltas<10>,
    pub(super) blue_scale: i64,
    pub(super) blue_shift: i64,
    pub(super) blue_fuzz: i64,
    pub(super) language_group: i32,
}

impl HintDict {
    /// What FreeType keeps for a Font DICT without a Private DICT: zeros.
    pub(super) const ABSENT: Self = Self {
        blue_values: Deltas::EMPTY,
        other_blues: Deltas::EMPTY,
        family_blues: Deltas::EMPTY,
        family_other_blues: Deltas::EMPTY,
        blue_scale: 0,
        blue_shift: 0,
        blue_fuzz: 0,
        language_group: 0,
    };

    /// The defaults `cff_load_private_dict` parses over: BlueScale 0.039625
    /// (`0.039625 * 0x10000 * 1000` in double), BlueShift 7, BlueFuzz 1.
    pub(super) const DEFAULTS: Self = Self {
        blue_scale: 2_596_864,
        blue_shift: 7,
        blue_fuzz: 1,
        ..Self::ABSENT
    };

    /// What `cff_load_private_dict` corrects after parsing: an even number
    /// of BlueValues, and the defaults for BlueShift and BlueFuzz outside 0 to
    /// 1000.
    pub(super) fn sanitize(&mut self) {
        self.blue_values.count &= !1;
        if !(0..=1000).contains(&self.blue_shift) {
            self.blue_shift = 7;
        }
        if !(0..=1000).contains(&self.blue_fuzz) {
            self.blue_fuzz = 1;
        }
    }
}

/// A blue zone (`CF2_BlueRec`).
#[derive(Clone, Copy, Debug, Default)]
struct Zone {
    bottom: Fixed,
    top: Fixed,
    /// The edge hints align to, from this font or its family.
    flat: Fixed,
    device_flat: Fixed,
    is_bottom: bool,
}

/// The blue zones of a Private DICT at a scale (`CF2_Blues`).
#[derive(Clone, Debug)]
pub(super) struct Blues {
    scale: Fixed,
    zones: [Zone; MAX_ZONES],
    count: usize,
    suppress_overshoot: bool,
    shift: Fixed,
    fuzz: Fixed,
    /// Synthetic ghost hints at the em box, bottom and top, which replace the
    /// zones of ideographic fonts without real ones.
    em_box: Option<[Edge; 2]>,
}

impl Blues {
    /// `cf2_blues_init` at the 16.16 vertical scale `scale`, without darkening.
    pub(super) fn new(dict: &HintDict, scale: Fixed) -> Self {
        let mut blues = Self {
            scale,
            zones: [Zone::default(); MAX_ZONES],
            count: 0,
            suppress_overshoot: false,
            shift: int_to_fixed(dict.blue_shift),
            fuzz: int_to_fixed(dict.blue_fuzz),
            em_box: None,
        };
        let blue = &dict.blue_values;
        let below = |i: usize| blue.values[i] < i64::from(EM_BOX_BOTTOM);
        let above = |i: usize| blue.values[i] > i64::from(EM_BOX_TOP);
        if dict.language_group == 1
            && (blue.count == 0
                || (blue.count == 4 && below(0) && below(1) && above(2) && above(3)))
        {
            // Outward by one so that real hints at the em box do not clash.
            let (bottom, top) = (EM_BOX_BOTTOM - 1, EM_BOX_TOP + 1);
            blues.em_box = Some([
                Edge::synthetic(
                    flag::GHOST_BOTTOM,
                    bottom,
                    round(mul(bottom, scale)).wrapping_sub(MIN_COUNTER),
                    scale,
                ),
                Edge::synthetic(
                    flag::GHOST_TOP,
                    top,
                    round(mul(top, scale)).wrapping_add(MIN_COUNTER),
                    scale,
                ),
            ]);
            return blues;
        }
        // The first BlueValues zone is a bottom zone, the others top zones;
        // all OtherBlues are bottom zones. Zones of negative height are dropped.
        let mut max_height = 0;
        let zones = blue
            .pairs()
            .enumerate()
            .map(|(i, zone)| (zone, i == 0))
            .chain(dict.other_blues.pairs().map(|zone| (zone, true)));
        for ((bottom, top), is_bottom) in zones {
            let height = top.wrapping_sub(bottom);
            if height < 0 {
                continue;
            }
            max_height = max_height.max(height);
            blues.zones[blues.count] = Zone {
                bottom,
                top,
                flat: if is_bottom { top } else { bottom },
                device_flat: 0,
                is_bottom,
            };
            blues.count += 1;
        }
        // A family edge within a pixel replaces a zone's flat edge.
        let units_per_pixel = div(ONE, scale);
        let family = &dict.family_blues;
        for zone in &mut blues.zones[..blues.count] {
            let flat = zone.flat;
            let mut closest = Fixed::MAX;
            let mut consider = |edge: Fixed, zone: &mut Zone| {
                let diff = abs(flat.wrapping_sub(edge));
                let closer = diff < closest && diff < units_per_pixel;
                if closer {
                    zone.flat = edge;
                    closest = diff;
                }
                closer && diff == 0
            };
            if zone.is_bottom {
                for (_, edge) in dict.family_other_blues.pairs() {
                    if consider(edge, zone) {
                        break;
                    }
                }
                // The first FamilyBlues zone is a bottom zone.
                if family.count >= 2 {
                    consider(family.values[1] as Fixed, zone);
                }
            } else {
                for (edge, _) in family.pairs().skip(1) {
                    if consider(edge, zone) {
                        break;
                    }
                }
            }
        }
        let mut blue_scale = div_fix(dict.blue_scale, 1000 << 16) as Fixed;
        if max_height > 0 {
            let most = div_fix(i64::from(ONE), i64::from(max_height));
            if i64::from(blue_scale) > most {
                blue_scale = most as Fixed;
            }
        }
        // Below BlueScale, overshoot is suppressed and zones are boosted, by
        // up to 0.6 of a pixel (`cf2_doubleToFixed(.6)`), kept below one half.
        let mut boost = 0;
        if scale < blue_scale {
            blues.suppress_overshoot = true;
            let six_tenths = 39322;
            boost = (six_tenths - mul_div(six_tenths, i64::from(scale), i64::from(blue_scale)))
                as Fixed;
            boost = boost.min(0x7fff);
        }
        for zone in &mut blues.zones[..blues.count] {
            let device = mul_fix(i64::from(zone.flat), i64::from(scale));
            let boosted = if zone.is_bottom {
                device - i64::from(boost)
            } else {
                device + i64::from(boost)
            };
            zone.device_flat = round(boosted as Fixed);
        }
        blues
    }

    /// `cf2_blues_capture`: whether a zone captures the bottom edge of
    /// a stem, or its top edge, searched in one pass; a captured stem moves
    /// onto the zone's flat edge (overshoot suppressed), at least a pixel past
    /// it (BlueShift), or onto the nearest pixel, and both its edges lock.
    fn capture(&self, bottom: &mut Edge, top: &mut Edge) -> bool {
        let fuzz = self.fuzz;
        let within = |zone: &Zone, cs: Fixed| {
            zone.bottom.wrapping_sub(fuzz) <= cs && cs <= zone.top.wrapping_add(fuzz)
        };
        let mut moved = None;
        for zone in &self.zones[..self.count] {
            if zone.is_bottom && bottom.is_bottom() && within(zone, bottom.cs) {
                let ds = if self.suppress_overshoot {
                    zone.device_flat
                } else if zone.top.wrapping_sub(bottom.cs) >= self.shift {
                    round(bottom.ds).min(zone.device_flat.wrapping_sub(ONE))
                } else {
                    round(bottom.ds)
                };
                moved = Some(ds.wrapping_sub(bottom.ds));
                break;
            }
            if !zone.is_bottom && top.is_top() && within(zone, top.cs) {
                let ds = if self.suppress_overshoot {
                    zone.device_flat
                } else if top.cs.wrapping_sub(zone.bottom) >= self.shift {
                    round(top.ds).max(zone.device_flat.wrapping_add(ONE))
                } else {
                    round(top.ds)
                };
                moved = Some(ds.wrapping_sub(top.ds));
                break;
            }
        }
        let Some(moved) = moved else {
            return false;
        };
        for edge in [bottom, top] {
            if edge.is_valid() {
                edge.ds = edge.ds.wrapping_add(moved);
                edge.flags |= flag::LOCKED;
            }
        }
        true
    }
}

/// A hint map (`CF2_HintMap`): edges sorted by character space coordinate,
/// between which device space coordinates are interpolated.
#[derive(Clone, Debug)]
pub(super) struct HintMap {
    edges: [Edge; MAX_EDGES],
    count: usize,
    valid: bool,
}

impl HintMap {
    const fn new() -> Self {
        Self {
            edges: [Edge {
                flags: 0,
                index: 0,
                cs: 0,
                ds: 0,
                scale: 0,
            }; MAX_EDGES],
            count: 0,
            valid: false,
        }
    }

    /// A copy of `other`, as FreeType copies a map by structure.
    fn copy_from(&mut self, other: &Self) {
        self.edges[..other.count].copy_from_slice(&other.edges[..other.count]);
        self.count = other.count;
        self.valid = other.valid;
    }

    /// `cf2_hintmap_map`: the device space coordinate of `cs`, uniformly
    /// scaled by `scale` without edges. FreeType searches from its last hit;
    /// the edges are sorted, so the edge it finds is this one, the highest at
    /// or below `cs`, or below them all the first.
    pub(super) fn map(&self, cs: Fixed, scale: Fixed) -> Fixed {
        let edges = &self.edges[..self.count];
        let Some(&lowest) = edges.first() else {
            return mul(cs, scale);
        };
        let (edge, scale) = match edges.partition_point(|e| e.cs <= cs) {
            0 => (lowest, scale),
            n => (edges[n - 1], edges[n - 1].scale),
        };
        mul(cs.wrapping_sub(edge.cs), scale).wrapping_add(edge.ds)
    }

    /// `cf2_hintmap_insertHint`: inserts a stem's edges, either of which may be
    /// absent, unless they overlap the map's in character space or, placed
    /// through the `initial` map, in device space.
    fn insert(
        &mut self,
        bottom: Edge,
        top: Edge,
        initial: Option<&Self>,
        scale: Fixed,
        work: &mut WorkBudget,
    ) -> Result<()> {
        charge(work, self.count + 1)?;
        let (mut first, mut second, pair) = match (bottom.is_valid(), top.is_valid()) {
            (false, _) => (top, top, false),
            (true, false) => (bottom, top, false),
            (true, true) => (bottom, top, true),
        };
        if pair && top.cs < bottom.cs {
            return Ok(());
        }
        let edges = &self.edges[..self.count];
        let at = edges
            .iter()
            .position(|e| e.cs >= first.cs)
            .unwrap_or(self.count);
        if let Some(next) = edges.get(at)
            && (next.cs == first.cs || (pair && next.cs <= second.cs) || next.is_pair_top())
        {
            return Ok(());
        }
        if let Some(initial) = initial
            && initial.valid
            && !first.is_locked()
        {
            if pair {
                // The initial map places the stem's middle; the stem keeps its
                // width at the nominal scale.
                let half = second.cs.wrapping_sub(first.cs) / 2;
                let middle = initial.map(first.cs.wrapping_add(half), scale);
                let half = mul(half, scale);
                first.ds = middle.wrapping_sub(half);
                second.ds = middle.wrapping_add(half);
            } else {
                first.ds = initial.map(first.cs, scale);
            }
        }
        if at > 0 && first.ds < edges[at - 1].ds {
            return Ok(());
        }
        if let Some(next) = edges.get(at) {
            let upper = if pair { second.ds } else { first.ds };
            if upper > next.ds {
                return Ok(());
            }
        }
        let added = if pair { 2 } else { 1 };
        if self.count + added > MAX_EDGES {
            return Ok(());
        }
        self.edges.copy_within(at..self.count, at + added);
        self.edges[at] = first;
        if pair {
            self.edges[at + 1] = second;
        }
        self.count += added;
        Ok(())
    }

    /// `cf2_hintmap_adjustHints`: moves each unlocked edge or pair the least
    /// distance that puts one of its edges on a pixel boundary, keeping half a
    /// pixel from its neighbours, bottom up; a second pass, top down, retries
    /// the edges that could not move up. Scales are set by the first pass.
    fn adjust(&mut self, work: &mut WorkBudget) -> Result<()> {
        let count = self.count;
        charge(work, count)?;
        let edges = &mut self.edges;
        let mut retries = [(0_usize, 0 as Fixed); MAX_EDGES];
        let mut retry_count = 0;
        let mut i = 0;
        while i < count {
            // Pairs are inserted together, so a pair's top edge follows.
            let pair = edges[i].is_pair() && i + 1 < count;
            let j = if pair { i + 1 } else { i };
            let (ds_i, ds_j) = (edges[i].ds, edges[j].ds);
            if !edges[i].is_locked() {
                let (down, up) = (fraction(ds_i), fraction(ds_j));
                let up_to_pixel = |frac: Fixed| if frac == 0 { 0 } else { ONE - frac };
                let move_up = up_to_pixel(down).min(up_to_pixel(up));
                let move_down = (-down).max(-up);
                let room_up =
                    j + 1 >= count || edges[j + 1].ds >= ds_j.wrapping_add(move_up + MIN_COUNTER);
                let room_down =
                    i == 0 || edges[i - 1].ds <= ds_i.wrapping_add(move_down - MIN_COUNTER);
                let (moved, retry) = match (room_up, room_down) {
                    (true, true) if -move_down < move_up => (move_down, false),
                    (true, _) => (move_up, false),
                    (false, true) => (move_down, move_up < -move_down),
                    (false, false) => (0, true),
                };
                if retry && j + 1 < count && !edges[j + 1].is_locked() {
                    retries[retry_count] = (j, move_up - moved);
                    retry_count += 1;
                }
                edges[i].ds = ds_i.wrapping_add(moved);
                if pair {
                    edges[j].ds = ds_j.wrapping_add(moved);
                }
            }
            for k in i..=j {
                if k > 0 && edges[k].cs != edges[k - 1].cs {
                    edges[k - 1].scale = div(
                        edges[k].ds.wrapping_sub(edges[k - 1].ds),
                        edges[k].cs.wrapping_sub(edges[k - 1].cs),
                    );
                }
            }
            i = j + 1;
        }
        for &(j, move_up) in retries[..retry_count].iter().rev() {
            if edges[j + 1].ds >= edges[j].ds.wrapping_add(move_up + MIN_COUNTER) {
                edges[j].ds = edges[j].ds.wrapping_add(move_up);
                if edges[j].is_pair() && j > 0 {
                    edges[j - 1].ds = edges[j - 1].ds.wrapping_add(move_up);
                }
            }
        }
        Ok(())
    }
}

/// The horizontal stem hints (`hStemHintArray`): the first 96, since a mask
/// over more fails.
type Stems = [Stem; MAX_HINTS];

/// The bottom and top edges of horizontal stem `i`.
fn edges(stems: &Stems, i: usize, origin: Fixed, scale: Fixed) -> (Edge, Edge) {
    let stem = stems[i];
    (
        Edge::new(stem, i, origin, scale, true),
        Edge::new(stem, i, origin, scale, false),
    )
}

/// What only hinting keeps: the horizontal stems' edges and the glyph path's
/// hint maps.
#[derive(Debug)]
struct Hinted<'b> {
    blues: &'b Blues,
    stems: Stems,
    /// The edges blue zones capture from all stems, which place the others
    /// (`initialHintMap`).
    initial: HintMap,
    /// The map of the current mask (`hintMap`).
    current: HintMap,
    /// The map at the current subpath's first point (`firstHintMap`).
    first: HintMap,
    /// The map of a counter mask, which FreeType builds and discards.
    counters: HintMap,
}

/// Which of [`Hinted`]'s maps [`Hinted::place`] builds.
#[derive(Clone, Copy)]
enum Map {
    Initial,
    Current,
    Counters,
}

impl Hinted<'_> {
    /// `cf2_hintmap_build` placing the edges of the `horizontal` stems
    /// `mask` selects into the `target` map: the em box's, then those
    /// captured or placed by an earlier map, then the rest, adjusted. The
    /// initial map holds the captured edges alone and one at zero below or
    /// above them all; the others are placed through it, and record where
    /// their stems went, so that later maps keep them there.
    fn place(
        &mut self,
        target: Map,
        horizontal: usize,
        mask: &Mask,
        origin: Fixed,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let (map, initial) = match target {
            Map::Initial => (&mut self.initial, None),
            Map::Current => (&mut self.current, Some(&self.initial)),
            Map::Counters => (&mut self.counters, Some(&self.initial)),
        };
        let (blues, stems) = (self.blues, &mut self.stems);
        map.count = 0;
        let scale = blues.scale;
        if let Some([bottom, top]) = blues.em_box {
            map.insert(bottom, Edge::default(), initial, scale, work)?;
            map.insert(Edge::default(), top, initial, scale, work)?;
        }
        // The mask covers the horizontal stems, so there are at most 96.
        let mut pending = *mask;
        for i in 0..horizontal {
            if pending.bit(i) {
                let (mut bottom, mut top) = edges(stems, i, origin, scale);
                if bottom.is_locked() || top.is_locked() || blues.capture(&mut bottom, &mut top) {
                    map.insert(bottom, top, initial, scale, work)?;
                    pending.clear(i);
                }
            }
        }
        if initial.is_some() {
            for i in (0..horizontal).filter(|&i| pending.bit(i)) {
                let (bottom, top) = edges(stems, i, origin, scale);
                map.insert(bottom, top, initial, scale, work)?;
            }
        } else if map.count == 0 || map.edges[0].cs > 0 || map.edges[map.count - 1].cs < 0 {
            // Locks the baseline of glyphs without hints there.
            let zero = Edge::synthetic(flag::GHOST_BOTTOM, 0, 0, scale);
            map.insert(zero, Edge::default(), initial, scale, work)?;
        }
        map.adjust(work)?;
        if initial.is_some() {
            for edge in map.edges[..map.count].iter().filter(|e| !e.is_synthetic()) {
                let stem = &mut stems[usize::from(edge.index)];
                if edge.is_top() {
                    stem.max_ds = edge.ds;
                } else {
                    stem.min_ds = edge.ds;
                }
                stem.used = true;
            }
        }
        map.valid = true;
        Ok(())
    }

    /// The initial map, once: from all `count` stems, of which `horizontal`
    /// are horizontal, at the origin of the map that needs it first.
    fn build_initial(
        &mut self,
        count: usize,
        horizontal: usize,
        origin: Fixed,
        work: &mut WorkBudget,
    ) -> Result<()> {
        if self.initial.valid {
            return Ok(());
        }
        let mut all = Mask::default();
        all.set_all(count)?;
        self.place(Map::Initial, horizontal, &all, origin, work)
    }
}

/// The hints of one charstring as FreeType's interpreter keeps them: the
/// number of its stem hints and its hint mask, which decide how the
/// charstring reads, and, hinting, the stems' edges and the hint maps of its
/// glyph path. Unhinted, the hint maps hold no edges, so points scale
/// uniformly; only whether the current one is built is kept.
#[derive(Debug)]
pub(super) struct Hints<'b> {
    horizontal: usize,
    vertical: usize,
    mask: Mask,
    /// Whether the current hint map is built (`hintMap.isValid`).
    built: bool,
    /// The vertical offset of a `seac` accent, which moves its stems.
    origin: Fixed,
    hinted: Option<Hinted<'b>>,
}

impl<'b> Hints<'b> {
    /// The hints of a charstring placed by `blues`, or unhinted without.
    pub(super) fn new(blues: Option<&'b Blues>, origin: Fixed) -> Self {
        Self {
            horizontal: 0,
            vertical: 0,
            mask: Mask::default(),
            built: false,
            origin,
            hinted: blues.map(|blues| Hinted {
                blues,
                stems: [Stem::default(); MAX_HINTS],
                initial: HintMap::new(),
                current: HintMap::new(),
                first: HintMap::new(),
                counters: HintMap::new(),
            }),
        }
    }

    const fn count(&self) -> usize {
        self.horizontal + self.vertical
    }

    /// Whether the stems are fixed: FreeType ignores stem operators once a
    /// hint mask is read or, at the first move, set.
    pub(super) const fn are_fixed(&self) -> bool {
        self.mask.valid
    }

    /// Whether a hint mask was read since the last hint map was built.
    pub(super) const fn mask_is_new(&self) -> bool {
        self.mask.new
    }

    /// The current hint map; `None` unhinted.
    pub(super) fn current(&self) -> Option<&HintMap> {
        self.hinted.as_ref().map(|h| &h.current)
    }

    /// The hint map of the subpath's first point; `None` unhinted.
    pub(super) fn first(&self) -> Option<&HintMap> {
        self.hinted.as_ref().map(|h| &h.first)
    }

    pub(super) const fn current_is_valid(&self) -> bool {
        self.built
    }

    /// `cf2_doStems` without the width: the stems of an operator's operands,
    /// past an odd first one, as running sums.
    pub(super) fn add_stems(&mut self, horizontal: bool, operands: &[Fixed]) {
        let mut position: Fixed = 0;
        for pair in operands[operands.len() % 2..].chunks_exact(2) {
            let min = position.wrapping_add(pair[0]);
            position = min.wrapping_add(pair[1]);
            if !horizontal {
                self.vertical += 1;
                continue;
            }
            if let Some(stem) = self
                .hinted
                .as_mut()
                .and_then(|h| h.stems.get_mut(self.horizontal))
            {
                *stem = Stem {
                    min,
                    max: position,
                    ..Stem::default()
                };
            }
            self.horizontal += 1;
        }
    }

    /// The mask after a `hintmask`.
    pub(super) fn read_mask(&mut self, r: &mut Reader<'_>) -> Result<()> {
        self.mask.read(r, self.count())
    }

    /// The mask after a `cntrmask`. Hinting, FreeType builds a hint map from
    /// it and discards it, which places and locks the stems it selects.
    pub(super) fn read_counter_mask(
        &mut self,
        r: &mut Reader<'_>,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let mut mask = Mask::default();
        mask.read(r, self.count())?;
        let (count, horizontal) = (self.count(), self.horizontal);
        if let Some(h) = &mut self.hinted {
            // FreeType places counter hints, and an initial map built for
            // them, without the accent offset.
            h.build_initial(count, horizontal, 0, work)?;
            if horizontal > mask.count {
                return Ok(());
            }
            h.place(Map::Counters, horizontal, &mask, 0, work)?;
        }
        Ok(())
    }

    /// `cf2_hintmap_build` of the current hint map from the mask, all stems
    /// when no mask was read. A mask over fewer stems than the horizontal
    /// ones leaves the map without edges, and as built as it was.
    pub(super) fn build_current(&mut self, work: &mut WorkBudget) -> Result<()> {
        let (count, horizontal, origin) = (self.count(), self.horizontal, self.origin);
        if let Some(h) = &mut self.hinted {
            h.build_initial(count, horizontal, origin, work)?;
        }
        if !self.mask.valid {
            self.mask.set_all(self.count())?;
        }
        if horizontal > self.mask.count {
            if let Some(h) = &mut self.hinted {
                h.current.count = 0;
            }
            return Ok(());
        }
        if let Some(h) = &mut self.hinted {
            h.place(Map::Current, horizontal, &self.mask, origin, work)?;
        }
        self.built = true;
        self.mask.new = false;
        Ok(())
    }

    /// Keeps the current map for the subpath's first point.
    pub(super) fn save_first(&mut self) {
        if let Some(h) = &mut self.hinted {
            h.first.copy_from(&h.current);
        }
    }
}
