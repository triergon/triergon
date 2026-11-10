//! The accent adjustments of the latin writing system (`afadjust.c` and
//! the vertical adjustment code of `aflatin.c`): characters the adjustment
//! database lists have their tildes stretched and aligned to the pixel
//! grid, the blue zones of their base kept off their diacritics, and their
//! diacritics kept a pixel away from the base after hinting.
//!
//! The adjustments work on whole contours: the database assumes one
//! enclosing contour for the base glyph and one for each diacritic.

use super::super::hints::{Dimension, GlyphHints, edge, point};
use super::super::tables::ADJUSTMENTS;
use super::{Blue, Metrics, analysis, blue};
use crate::fixed::{mul_fix, pix_round};
use crate::work::charge;
use triergon_core::{Result, WorkBudget};

/// The adjustment flags (`AF_ADJUST_*` and `AF_IGNORE_*`).
mod flag {
    /// Push the topmost contour up, clear of the rest.
    pub(super) const UP: u16 = 0x01;
    /// Push the bottommost contour down.
    pub(super) const DOWN: u16 = 0x02;
    /// Push the contour below the topmost one up, with it.
    pub(super) const UP2: u16 = 0x04;
    /// Push the contour above the bottommost one down, with it.
    pub(super) const DOWN2: u16 = 0x08;
    /// The topmost contour is a tilde.
    pub(super) const TILDE_TOP: u16 = 0x10;
    pub(super) const TILDE_BOTTOM: u16 = 0x20;
    /// The contour below the topmost one is a tilde.
    pub(super) const TILDE_TOP2: u16 = 0x40;
    pub(super) const TILDE_BOTTOM2: u16 = 0x80;
    /// Ignore diacritics on top of a capital letter for blue zones.
    pub(super) const IGNORE_CAPITAL_TOP: u16 = 0x100;
    pub(super) const IGNORE_CAPITAL_BOTTOM: u16 = 0x200;
    pub(super) const IGNORE_SMALL_TOP: u16 = 0x400;
    pub(super) const IGNORE_SMALL_BOTTOM: u16 = 0x800;
    /// Adjust whatever the diacritic's height.
    pub(super) const NO_HEIGHT_CHECK: u16 = 0x1000;
}

/// `af_adjustment_database_lookup`: the flags of `code`, by FreeType's
/// binary search over the database in its order, which is not sorted
/// throughout (so that a few code points are never found).
pub(in super::super) fn adjustment(code: u32) -> u16 {
    let (mut low, mut high) = (0, ADJUSTMENTS.len() - 1);
    while high >= low {
        let mid = (low + high) / 2;
        let (entry, flags) = ADJUSTMENTS[mid];
        if entry < code {
            low = mid + 1;
        } else if entry > code {
            // FreeType's unsigned index would wrap here; only code points
            // below the first entry, which the database never asks for,
            // reach it.
            let Some(lower) = mid.checked_sub(1) else {
                return 0;
            };
            high = lower;
        } else {
            return flags;
        }
    }
    0
}

/// The vertical preparation of `af_latin_hints_apply`: tildes stretched
/// and aligned, horizontal edges found, and blue zones kept off diacritics
/// as `adjustment` asks. Returns the limit on a diacritic's height for the
/// separation after hinting.
pub(super) fn prepare(
    hints: &mut GlyphHints,
    metrics: &Metrics,
    adjustment: u16,
    top_to_bottom: bool,
    work: &mut WorkBudget,
) -> Result<i64> {
    let has = |f: u16| adjustment & f != 0;
    let tilde = flag::TILDE_TOP | flag::TILDE_BOTTOM | flag::TILDE_TOP2 | flag::TILDE_BOTTOM2;
    if has(tilde) {
        compute_vertical_extrema(hints, work)?;
    }
    // Inner tildes first.
    if has(flag::TILDE_TOP2) {
        let contour = second_highest_contour(hints);
        let offset = stretch_tilde(hints, contour, true) + align_tilde(hints, contour, true);
        move_contours_up(hints, hints.y_minima[contour], offset);
    }
    if has(flag::TILDE_BOTTOM2) {
        let contour = second_lowest_contour(hints);
        let offset = stretch_tilde(hints, contour, false) - align_tilde(hints, contour, false);
        move_contours_down(hints, hints.y_maxima[contour], offset);
    }
    if has(flag::TILDE_TOP) {
        let contour = highest_contour(hints);
        stretch_tilde(hints, contour, true);
        align_tilde(hints, contour, true);
    }
    if has(flag::TILDE_BOTTOM) {
        let contour = lowest_contour(hints);
        stretch_tilde(hints, contour, false);
        align_tilde(hints, contour, false);
    }
    analysis::detect_features(hints, Dimension::Vertical, metrics, top_to_bottom, work)?;
    let (mut capital, mut small) = ((None, None), (None, None));
    let mut accent_height_limit = 0;
    if adjustment != 0 {
        capital = base_glyph_blues(metrics, true);
        small = base_glyph_blues(metrics, false);
        if !has(flag::NO_HEIGHT_CHECK) {
            // A heuristic height an accent stays under: 2/3 of the small
            // letters' zone, else half the capitals', else 0.4 em.
            accent_height_limit = match (small, capital) {
                ((Some(top), Some(bottom)), _) => 2 * (top.shoot.cur - bottom.shoot.cur) / 3,
                (_, (Some(top), Some(bottom))) => (top.shoot.cur - bottom.shoot.cur) / 2,
                _ => mul_fix(i64::from(metrics.units_per_em) * 4 / 10, metrics.y_scale()),
            };
        }
    }
    for ((top, bottom), ignore_top, ignore_bottom) in [
        (
            capital,
            flag::IGNORE_CAPITAL_TOP,
            flag::IGNORE_CAPITAL_BOTTOM,
        ),
        (small, flag::IGNORE_SMALL_TOP, flag::IGNORE_SMALL_BOTTOM),
    ] {
        let (Some(top), Some(bottom)) = (top, bottom) else {
            continue;
        };
        // Above or below the base by more than about 14% of its height
        // and a quarter pixel, edges take no blue zone.
        let height = top.shoot.cur - bottom.shoot.cur;
        let axis = &mut hints.axis[Dimension::Vertical.index()];
        if has(ignore_top) {
            let limit = top.shoot.cur + height / 7 + 16;
            for e in axis.edges.iter_mut().filter(|e| e.pos > limit) {
                e.flags |= edge::NO_BLUE;
            }
        }
        if has(ignore_bottom) {
            let limit = bottom.shoot.cur - height / 7 - 16;
            for e in axis.edges.iter_mut().filter(|e| e.pos < limit) {
                e.flags |= edge::NO_BLUE;
            }
        }
    }
    Ok(accent_height_limit)
}

/// `af_latin_get_base_glyph_blues`: the first active top and bottom blue
/// zones of capital (else small) letters.
fn base_glyph_blues(metrics: &Metrics, capital: bool) -> (Option<Blue>, Option<Blue>) {
    let (top, bottom) = if capital {
        (blue::TOP, blue::BOTTOM)
    } else {
        (blue::ADJUSTMENT, blue::BOTTOM_SMALL)
    };
    let blues = metrics.axis[Dimension::Vertical.index()].blues();
    let find = |f: u8| {
        let f = f | blue::ACTIVE;
        blues.iter().find(|b| b.flags & f == f).copied()
    };
    (find(top), find(bottom))
}

/// `af_compute_vertical_extrema`: each contour's lowest and highest point;
/// contours of one or two points keep extrema that compare as empty.
fn compute_vertical_extrema(hints: &mut GlyphHints, work: &mut WorkBudget) -> Result<()> {
    let count = hints.contours.len();
    charge(work, hints.points.len() + count)?;
    hints.y_minima.clear();
    hints.y_maxima.clear();
    hints.y_minima.try_resize(count, i64::MAX)?;
    hints.y_maxima.try_resize(count, i64::MIN)?;
    let points = hints.points.as_slice();
    for (c, &first) in hints.contours.as_slice().iter().enumerate() {
        if points[points[first].next].next == first {
            continue;
        }
        let (mut min, mut max) = (i64::MAX, i64::MIN);
        let mut p = first;
        loop {
            min = min.min(points[p].y);
            max = max.max(points[p].y);
            p = points[p].next;
            if p == first {
                break;
            }
        }
        hints.y_minima[c] = min;
        hints.y_maxima[c] = max;
    }
    Ok(())
}

/// The extrema of contour `c`, as last computed.
fn extrema(hints: &GlyphHints, c: usize) -> (i64, i64) {
    (hints.y_minima[c], hints.y_maxima[c])
}

/// `af_find_highest_contour`: the contour reaching highest, of two the
/// shorter one.
fn highest_contour(hints: &GlyphHints) -> usize {
    let (mut best, mut best_min, mut best_max) = (0, i64::MAX, i64::MIN);
    for c in 0..hints.contours.len() {
        let (min, max) = extrema(hints, c);
        if max > best_max || (max == best_max && min > best_min) {
            (best, best_min, best_max) = (c, min, max);
        }
    }
    best
}

/// `af_find_second_highest_contour`: of the contours reaching below the
/// highest one's bottom, the one reaching highest; contour 0 for fewer than
/// three contours.
fn second_highest_contour(hints: &GlyphHints) -> usize {
    if hints.contours.len() < 3 {
        return 0;
    }
    let highest = highest_contour(hints);
    let highest_min = hints.y_minima[highest];
    let (mut best, mut best_max) = (0, i64::MIN);
    for c in (0..hints.contours.len()).filter(|&c| c != highest) {
        let (min, max) = extrema(hints, c);
        if max > best_max && min < highest_min {
            (best, best_max) = (c, max);
        }
    }
    best
}

/// `af_find_lowest_contour`: the contour reaching lowest, of two the
/// shorter one.
fn lowest_contour(hints: &GlyphHints) -> usize {
    let (mut best, mut best_min, mut best_max) = (0, i64::MAX, i64::MIN);
    for c in 0..hints.contours.len() {
        let (min, max) = extrema(hints, c);
        if min < best_min || (min == best_min && max < best_max) {
            (best, best_min, best_max) = (c, min, max);
        }
    }
    best
}

/// `af_find_second_lowest_contour`, the counterpart of
/// [`second_highest_contour`].
fn second_lowest_contour(hints: &GlyphHints) -> usize {
    if hints.contours.len() < 3 {
        return 0;
    }
    let lowest = lowest_contour(hints);
    let lowest_max = hints.y_maxima[lowest];
    let (mut best, mut best_min) = (0, i64::MAX);
    for c in (0..hints.contours.len()).filter(|&c| c != lowest) {
        let (min, max) = extrema(hints, c);
        if min < best_min && max > lowest_max {
            (best, best_min) = (c, min);
        }
    }
    best
}

/// `af_move_contour_vertically`.
fn move_contour(hints: &mut GlyphHints, c: usize, by: i64) {
    let first = hints.contours[c];
    let points = hints.points.as_mut_slice();
    let mut p = first;
    loop {
        points[p].y = points[p].y.wrapping_add(by);
        p = points[p].next;
        if p == first {
            break;
        }
    }
}

/// `af_move_contours_up`: moves every contour above `limit` by `delta`.
fn move_contours_up(hints: &mut GlyphHints, limit: i64, delta: i64) {
    for c in 0..hints.contours.len() {
        let (min, max) = extrema(hints, c);
        if min < max && min > limit {
            move_contour(hints, c, delta);
        }
    }
}

/// `af_move_contours_down`: moves every contour below `limit` down by
/// `delta`.
fn move_contours_down(hints: &mut GlyphHints, limit: i64, delta: i64) {
    for c in 0..hints.contours.len() {
        let (min, max) = extrema(hints, c);
        if min < max && max < limit {
            move_contour(hints, c, delta.wrapping_neg());
        }
    }
}

/// `af_touch_top_contours` and `af_touch_bottom_contours`: marks the
/// points of the contours at or above (below) `tilde`'s bottom so that
/// hinting leaves them, as `af_touch_contour` does: kept out of segments,
/// and the points on the curve touched vertically.
fn touch_contours(hints: &mut GlyphHints, tilde: usize, top: bool) {
    let limit = hints.y_minima[tilde];
    for c in 0..hints.contours.len() {
        let (min, max) = extrema(hints, c);
        if min < max && (if top { min >= limit } else { max <= limit }) {
            let first = hints.contours[c];
            let points = hints.points.as_mut_slice();
            let mut p = first;
            loop {
                p = points[p].next;
                points[p].flags |= point::IGNORE;
                if points[p].flags & point::CONTROL == 0 {
                    points[p].flags |= point::TOUCH_Y;
                }
                if p == first {
                    break;
                }
            }
        }
    }
}

/// `af_latin_stretch_top_tilde` and `af_latin_stretch_bottom_tilde`:
/// stretches the tilde contour so that its wiggle (the smallest distance of
/// a round on-curve extremum from the far side, at least an eighth of the
/// height) spans more than a pixel, and protects it and the contours beyond
/// it from hinting. Returns how much taller it got.
fn stretch_tilde(hints: &mut GlyphHints, tilde: usize, top: bool) -> i64 {
    let (min_y, max_y) = extrema(hints, tilde);
    if min_y == max_y {
        return 0;
    }
    let height = max_y.wrapping_sub(min_y);
    let extremum_threshold = height / 8;
    let first = hints.contours[tilde];
    let points = hints.points.as_slice();
    let control = |p: usize| points[p].flags & point::CONTROL != 0;
    let mut measurement: Option<i64> = None;
    let mut p = first;
    loop {
        p = points[p].next;
        let (prev, next, y) = (points[p].prev, points[p].next, points[p].y);
        if !control(p)
            && points[prev].y == y
            && points[next].y == y
            && y != min_y
            && y != max_y
            && control(prev)
            && control(next)
        {
            // A candidate: the on points around it must both be above or
            // both below.
            let (mut prev_on, mut next_on) = (prev, next);
            while control(prev_on) {
                prev_on = points[prev_on].prev;
            }
            while control(next_on) {
                next_on = points[next_on].next;
            }
            let found = if points[next_on].y > y && points[prev_on].y > y {
                Some(y.wrapping_sub(min_y))
            } else if points[next_on].y < y && points[prev_on].y < y {
                Some(max_y.wrapping_sub(y))
            } else {
                None
            };
            if let Some(found) = found.filter(|&m| m >= extremum_threshold)
                && measurement.is_none_or(|m| found < m)
            {
                measurement = Some(found);
            }
        }
        if p == first {
            break;
        }
    }
    touch_contours(hints, tilde, top);
    let target_height = measurement.unwrap_or(0).wrapping_add(64);
    if height >= target_height {
        return 0;
    }
    // Scale the diacritic about its far side.
    let base = if top { min_y } else { max_y };
    let points = hints.points.as_mut_slice();
    let mut p = first;
    loop {
        p = points[p].next;
        // `height` is positive: the contour's extrema differ.
        let scaled = points[p].y.wrapping_sub(base).wrapping_mul(target_height);
        points[p].y = scaled.wrapping_div(height).wrapping_add(base);
        if p == first {
            break;
        }
    }
    target_height.wrapping_sub(height)
}

/// `af_latin_align_top_tilde` and `af_latin_align_bottom_tilde`: moves the
/// tilde contour so that its bottom (top) is on a pixel, or its centre for
/// a tilde under three pixels tall. Returns the move.
fn align_tilde(hints: &mut GlyphHints, tilde: usize, top: bool) -> i64 {
    let first = hints.contours[tilde];
    let points = hints.points.as_slice();
    let (mut min_y, mut max_y) = (points[first].y, points[first].y);
    let mut p = first;
    loop {
        p = points[p].next;
        min_y = min_y.min(points[p].y);
        max_y = max_y.max(points[p].y);
        if p == first {
            break;
        }
    }
    let height = max_y.wrapping_sub(min_y);
    let centring = if height < 64 * 3 {
        (pix_round(height) - height) / 2
    } else {
        0
    };
    let delta = if top {
        pix_round(min_y).wrapping_sub(min_y) + centring
    } else {
        pix_round(max_y).wrapping_sub(max_y) - centring
    };
    move_contour(hints, tilde, delta);
    delta
}

/// `af_check_contour_horizontal_overlap`: whether contour `c` overlaps the
/// horizontal extent of the other contours of three or more points.
fn overlaps_horizontally(hints: &GlyphHints, c: usize) -> bool {
    let points = hints.points.as_slice();
    let (mut own_min, mut own_max) = (i64::MAX, i64::MIN);
    let (mut other_min, mut other_max) = (i64::MAX, i64::MIN);
    for (i, &first) in hints.contours.as_slice().iter().enumerate() {
        if points[points[first].next].next == first {
            continue;
        }
        let (min, max) = if i == c {
            (&mut own_min, &mut own_max)
        } else {
            (&mut other_min, &mut other_max)
        };
        let mut p = first;
        loop {
            p = points[p].next;
            *min = (*min).min(points[p].x);
            *max = (*max).max(points[p].x);
            if p == first {
                break;
            }
        }
    }
    (other_min <= own_max && own_max <= other_max)
        || (other_min <= own_min && own_min <= other_max)
        || (own_max >= other_max && own_min <= other_min)
}

/// `af_glyph_hints_apply_vertical_separation_adjustments` after the
/// vertical hinting: a diacritic above (below) its base that overlaps it
/// horizontally and is no taller than `accent_height_limit` moves until a
/// pixel separates it from the contours below (above), with every contour
/// in its range or beyond it; a tilde whose bottom then lands on a pixel is
/// re-centred.
pub(super) fn separate_vertically(
    hints: &mut GlyphHints,
    adjustment: u16,
    accent_height_limit: i64,
    work: &mut WorkBudget,
) -> Result<()> {
    let has = |f: u16| adjustment & f != 0;
    let contours = hints.contours.len();
    let (up, up2, down, down2) = (
        has(flag::UP),
        has(flag::UP2),
        has(flag::DOWN),
        has(flag::DOWN2),
    );
    let (top_wanted, bottom_wanted) = (
        (up && contours >= 2) || (up2 && contours >= 3),
        (down && contours >= 2) || (down2 && contours >= 3),
    );
    if top_wanted || bottom_wanted {
        // Again, on the hinted outline.
        compute_vertical_extrema(hints, work)?;
    }
    if top_wanted {
        let high = if up2 {
            second_highest_contour(hints)
        } else {
            highest_contour(hints)
        };
        if !overlaps_horizontally(hints, high) {
            return Ok(());
        }
        let (high_min, high_max) = extrema(hints, high);
        let high_height = high_max.wrapping_sub(high_min);
        if high_height > accent_height_limit {
            return Ok(());
        }
        // The least gap, under a pixel, below the diacritic.
        let mut min_distance = 64;
        for c in (0..contours).filter(|&c| c != high) {
            let (min, max) = extrema(hints, c);
            let distance = high_min.wrapping_sub(max);
            if distance < 64 && distance < min_distance && min < high_min {
                min_distance = distance;
            }
        }
        let adjustment_amount = 64 - min_distance;
        let (tilde, tilde2) = (has(flag::TILDE_TOP), has(flag::TILDE_TOP2));
        let mut centring = 0;
        if tilde || tilde2 {
            let contour = if up || tilde2 {
                high
            } else {
                highest_contour(hints)
            };
            let (tilde_min, tilde_max) = extrema(hints, contour);
            let tilde_height = tilde_max.wrapping_sub(tilde_min);
            let mut pos = high_min.wrapping_add(adjustment_amount);
            if up2 && tilde {
                pos = pos.wrapping_add(high_height);
            }
            if pos % 64 == 0 && tilde_height < 3 * 64 {
                centring = (pix_round(tilde_height) - tilde_height) / 2;
            }
        }
        let calculated = if (up && tilde) || (up2 && tilde2) {
            adjustment_amount + centring
        } else {
            adjustment_amount
        };
        // A delta of 2/64 pixel allows for rounding.
        if calculated != 0 && calculated >= -2 && (calculated <= 66 || adjustment_amount <= 66) {
            let min_y_limit = high_min.wrapping_sub(high_height / 8);
            move_contours_up(hints, min_y_limit, adjustment_amount);
            if up2 && tilde {
                move_contours_up(hints, min_y_limit.wrapping_add(high_height), centring);
            }
        }
    }
    if bottom_wanted {
        let low = if down2 {
            second_lowest_contour(hints)
        } else {
            lowest_contour(hints)
        };
        if !overlaps_horizontally(hints, low) {
            return Ok(());
        }
        let (low_min, low_max) = extrema(hints, low);
        let low_height = low_max.wrapping_sub(low_min);
        if low_height > accent_height_limit {
            return Ok(());
        }
        let mut min_distance = 64;
        for c in (0..contours).filter(|&c| c != low) {
            let (min, max) = extrema(hints, c);
            let distance = min.wrapping_sub(low_max);
            if distance < 64 && distance < min_distance && max > low_max {
                min_distance = distance;
            }
        }
        let adjustment_amount = 64 - min_distance;
        let (tilde, tilde2) = (has(flag::TILDE_BOTTOM), has(flag::TILDE_BOTTOM2));
        let mut centring = 0;
        if tilde || tilde2 {
            let contour = if down || tilde2 {
                low
            } else {
                lowest_contour(hints)
            };
            let (tilde_min, tilde_max) = extrema(hints, contour);
            let tilde_height = tilde_max.wrapping_sub(tilde_min);
            let mut pos = low_max.wrapping_sub(adjustment_amount);
            if down2 && tilde {
                pos = pos.wrapping_sub(low_height);
            }
            if pos % 64 == 0 && tilde_height < 3 * 64 {
                centring = (pix_round(tilde_height) - tilde_height) / 2;
            }
        }
        let calculated = if (down && tilde) || (down2 && tilde2) {
            adjustment_amount + centring
        } else {
            adjustment_amount
        };
        if calculated != 0 && calculated >= -2 && (calculated <= 66 || adjustment_amount <= 66) {
            let max_y_limit = low_max.wrapping_add(low_height / 8);
            move_contours_down(hints, max_y_limit, adjustment_amount);
            if down2 && tilde {
                move_contours_down(hints, max_y_limit.wrapping_sub(low_height), centring);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::adjustment;

    #[test]
    fn the_database_is_searched_in_freetypes_order() {
        // `!` and `?` need no height check.
        assert_eq!(adjustment(0x21), 0x1001);
        assert_eq!(adjustment(0x3F), 0x1001);
        // `Q` is listed out of order, before `?`, and never found.
        assert_eq!(adjustment(0x51), 0);
        assert_eq!(adjustment(0x69), 0x01);
        assert_eq!(adjustment(0x20), 0);
        assert_eq!(adjustment(0xF_FFFF), 0);
    }
}
