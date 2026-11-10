//! The latin writing system (`aflatin.c`), which hints most scripts: the
//! style's global metrics (standard stem widths, blue zones and whether the
//! digits share one advance), measured on the face once and scaled for each
//! size, and the hinting of one glyph with them in [`apply`].

mod adjust;
mod analysis;
mod fit;

pub(super) use adjust::adjustment;
pub(super) use fit::{Glyph, Scope, apply};

use super::Face;
use super::hints::{Dimension, GlyphHints, Width, sort_and_quantize_widths, sort_positions};
use super::style::clusters;
use super::tables::{SCRIPTS, STYLES};
use crate::fixed::{mul_div, mul_fix, pix_round};
use crate::outline::{Outline, OutlinePoint, PointKind};
use triergon_core::{Result, WorkBudget};

/// `AF_LATIN_MAX_WIDTHS`: the standard widths kept per dimension.
const MAX_WIDTHS: usize = 16;
/// `AF_BLUE_STRINGSET_MAX_LEN`: the blue zones of one style.
const MAX_BLUES: usize = 8;
/// `AF_BLUE_STRING_MAX_LEN`: the clusters of one blue string.
const MAX_CLUSTERS: usize = 51;

/// The properties of a blue string (`AF_BLUE_PROPERTY_LATIN_*`).
mod property {
    pub(super) const TOP: u8 = 1 << 0;
    pub(super) const SUB_TOP: u8 = 1 << 1;
    pub(super) const NEUTRAL: u8 = 1 << 2;
    pub(super) const X_HEIGHT: u8 = 1 << 3;
    pub(super) const LONG: u8 = 1 << 4;
    pub(super) const CAPITAL_BOTTOM: u8 = 1 << 5;
    pub(super) const SMALL_BOTTOM: u8 = 1 << 6;
}

/// The flags of a blue zone (`AF_LATIN_BLUE_*`).
mod blue {
    /// Less than 3/4 pixel tall at the current size.
    pub(super) const ACTIVE: u8 = 1 << 0;
    pub(super) const TOP: u8 = 1 << 1;
    pub(super) const SUB_TOP: u8 = 1 << 2;
    pub(super) const NEUTRAL: u8 = 1 << 3;
    /// The x height, which the vertical scale is fitted to.
    pub(super) const ADJUSTMENT: u8 = 1 << 4;
    pub(super) const BOTTOM: u8 = 1 << 5;
    pub(super) const BOTTOM_SMALL: u8 = 1 << 6;
}

/// `AF_LATIN_CONSTANT`: `value`, given for 2048 units per em, for the face's
/// units per em.
fn constant(units_per_em: u16, value: i64) -> i64 {
    value * i64::from(units_per_em) / 2048
}

/// A blue zone (`AF_LatinBlueRec`): its reference and overshoot positions,
/// and the extent of the characters it was measured on.
#[derive(Clone, Copy, Debug, Default)]
struct Blue {
    reference: Width,
    shoot: Width,
    ascender: i64,
    descender: i64,
    flags: u8,
}

/// One dimension's metrics (`AF_LatinAxisRec`): standard widths, and for
/// the vertical dimension the blue zones.
#[derive(Clone, Copy, Debug)]
struct Axis {
    scale: i64,
    width_count: usize,
    widths: [Width; MAX_WIDTHS],
    /// Segments closer than this, in font units, share an edge.
    edge_distance_threshold: i64,
    standard_width: i64,
    /// Whether the standard width is under 5/8 pixel at the size.
    extra_light: bool,
    blue_count: usize,
    blues: [Blue; MAX_BLUES],
}

impl Axis {
    const EMPTY: Self = Self {
        scale: 0,
        width_count: 0,
        widths: [Width {
            org: 0,
            cur: 0,
            fit: 0,
        }; MAX_WIDTHS],
        edge_distance_threshold: 0,
        standard_width: 0,
        extra_light: false,
        blue_count: 0,
        blues: [Blue {
            reference: Width {
                org: 0,
                cur: 0,
                fit: 0,
            },
            shoot: Width {
                org: 0,
                cur: 0,
                fit: 0,
            },
            ascender: 0,
            descender: 0,
            flags: 0,
        }; MAX_BLUES],
    };

    fn widths(&self) -> &[Width] {
        &self.widths[..self.width_count]
    }

    fn blues(&self) -> &[Blue] {
        &self.blues[..self.blue_count]
    }
}

/// The global metrics of a style of the latin writing system
/// (`AF_LatinMetricsRec`), in font units as measured on the face, or scaled
/// to a size by [`Self::scaled`].
#[derive(Clone, Debug)]
pub(crate) struct Metrics {
    style: usize,
    units_per_em: u16,
    axis: [Axis; 2],
    digits_have_same_width: bool,
}

impl Metrics {
    /// `af_latin_metrics_init`: the style's metrics as measured on the
    /// face, or `None` when none of its blue strings gives a zone; FreeType
    /// then leaves the style's glyphs to the dummy writing system.
    ///
    /// Without a Unicode charmap nothing is measured, and the metrics stay
    /// empty.
    pub(super) fn new(
        face: &Face<'_>,
        style: usize,
        work: &mut WorkBudget,
    ) -> Result<Option<Self>> {
        let mut metrics = Self {
            style,
            units_per_em: face.font.units_per_em(),
            axis: [Axis::EMPTY; 2],
            digits_have_same_width: false,
        };
        if face.charmap.is_none() {
            return Ok(Some(metrics));
        }
        let mut outline = Outline::new(face.budget);
        metrics.init_widths(face, &mut outline, work)?;
        if !metrics.init_blues(face, &mut outline, work)? {
            return Ok(None);
        }
        metrics.check_digits(face);
        Ok(Some(metrics))
    }

    /// `af_latin_metrics_init_widths`: the widths of the stems of the
    /// first standard character of the script the face maps, in both
    /// dimensions, quantized; then each dimension's standard width (the
    /// smallest, or 50/2048 em without any) and edge threshold (a fifth of
    /// it).
    fn init_widths(
        &mut self,
        face: &Face<'_>,
        outline: &mut Outline,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let script = &SCRIPTS[STYLES[self.style].script];
        let glyph = clusters(script.standard).find_map(|c| c.and_then(|c| face.glyph(c)));
        if let Some(glyph) = glyph
            && face.load_units(glyph, outline, work)?
            && !outline.is_empty()
        {
            // The analysis runs in font units: FreeType's hints keep their
            // zero scale here.
            let mut hints = GlyphHints::new(face.budget);
            hints.reload(outline, self.units_per_em, work)?;
            for dim in Dimension::BOTH {
                analysis::compute_segments(&mut hints, dim, self.units_per_em, work)?;
                analysis::link_segments(&mut hints, dim, self.units_per_em, &[], work)?;
                let axis = &mut self.axis[dim.index()];
                let segments = hints.axis[dim.index()].segments.as_slice();
                let mut count = 0;
                for (index, segment) in segments.iter().enumerate() {
                    // Stems only: segments linked to each other, once.
                    let Some(link) = segment.link.filter(|&l| l > index) else {
                        continue;
                    };
                    if segments[link].link == Some(index) && count < MAX_WIDTHS {
                        let distance = i64::from(segment.pos) - i64::from(segments[link].pos);
                        axis.widths[count].org = distance.abs();
                        count += 1;
                    }
                }
                let threshold = i64::from(self.units_per_em / 100);
                axis.width_count = sort_and_quantize_widths(&mut axis.widths, count, threshold);
            }
        }
        for axis in &mut self.axis {
            let standard = if axis.width_count > 0 {
                axis.widths[0].org
            } else {
                constant(self.units_per_em, 50)
            };
            axis.edge_distance_threshold = standard / 5;
            axis.standard_width = standard;
            axis.extra_light = false;
        }
        Ok(())
    }

    /// `af_latin_metrics_init_blues`: one blue zone per blue string of the
    /// style that finds a glyph, its reference the median of the flat
    /// extrema of its characters and its overshoot the median of the round
    /// ones, then the zones' tops lowered where zones overlap. Whether any
    /// zone was found.
    fn init_blues(
        &mut self,
        face: &Face<'_>,
        outline: &mut Outline,
        work: &mut WorkBudget,
    ) -> Result<bool> {
        let units_per_em = i64::from(self.units_per_em);
        let axis = &mut self.axis[Dimension::Vertical.index()];
        for string in STYLES[self.style].blues {
            let properties = string.properties;
            let (mut flats, mut rounds) = ([0_i64; MAX_CLUSTERS], [0_i64; MAX_CLUSTERS]);
            let (mut flat_count, mut round_count) = (0, 0);
            let (mut ascender, mut descender) = (0, 0);
            for c in clusters(string.text) {
                // Clusters of several characters are unavailable.
                let Some(c) = c else {
                    continue;
                };
                let Some(glyph) = face.glyph(c) else {
                    continue;
                };
                // Glyphs that render nothing are skipped.
                if !face.load_units(glyph, outline, work)? || outline.points().len() <= 2 {
                    continue;
                }
                let found = extremum(
                    outline.points(),
                    outline.contour_ends(),
                    properties,
                    units_per_em,
                    &mut ascender,
                    &mut descender,
                );
                let Some((y, round)) = found else {
                    continue;
                };
                // One glyph per cluster: its extremum always beats the
                // initial one.
                let (list, count) = if round {
                    (&mut rounds, &mut round_count)
                } else {
                    (&mut flats, &mut flat_count)
                };
                if *count < MAX_CLUSTERS {
                    list[*count] = y;
                    *count += 1;
                }
            }
            if flat_count == 0 && round_count == 0 {
                continue;
            }
            sort_positions(&mut rounds[..round_count]);
            sort_positions(&mut flats[..flat_count]);
            let (mut reference, mut shoot) = match (flat_count, round_count) {
                (0, _) => (rounds[round_count / 2], rounds[round_count / 2]),
                (_, 0) => (flats[flat_count / 2], flats[flat_count / 2]),
                _ => (flats[flat_count / 2], rounds[round_count / 2]),
            };
            // An overshoot on the wrong side of its reference: the mean.
            let upward = properties & (property::TOP | property::SUB_TOP) != 0;
            if shoot != reference && upward != (shoot > reference) {
                reference = (shoot + reference) / 2;
                shoot = reference;
            }
            let mut flags = 0;
            for (from, to) in [
                (property::TOP, blue::TOP),
                (property::SUB_TOP, blue::SUB_TOP),
                (property::NEUTRAL, blue::NEUTRAL),
                (property::CAPITAL_BOTTOM, blue::BOTTOM),
                (property::SMALL_BOTTOM, blue::BOTTOM_SMALL),
                (property::X_HEIGHT, blue::ADJUSTMENT),
            ] {
                if properties & from != 0 {
                    flags |= to;
                }
            }
            if axis.blue_count < MAX_BLUES {
                axis.blues[axis.blue_count] = Blue {
                    reference: Width {
                        org: reference,
                        ..Width::default()
                    },
                    shoot: Width {
                        org: shoot,
                        ..Width::default()
                    },
                    ascender,
                    descender,
                    flags,
                };
                axis.blue_count += 1;
            }
        }
        if axis.blue_count == 0 {
            return Ok(false);
        }
        // Sort the zones bottom to top (`af_latin_sort_blue`), then lower
        // each top that reaches into the next zone.
        let upward = |b: &Blue| b.flags & (blue::TOP | blue::SUB_TOP) != 0;
        let bottom = |b: &Blue| {
            if upward(b) {
                b.reference.org
            } else {
                b.shoot.org
            }
        };
        let mut sorted = [0_usize; MAX_BLUES];
        for (i, slot) in sorted.iter_mut().enumerate() {
            *slot = i;
        }
        let sorted = &mut sorted[..axis.blue_count];
        for i in 1..sorted.len() {
            let mut j = i;
            while j > 0 && bottom(&axis.blues[sorted[j]]) < bottom(&axis.blues[sorted[j - 1]]) {
                sorted.swap(j, j - 1);
                j -= 1;
            }
        }
        for i in 0..sorted.len() - 1 {
            let next = &axis.blues[sorted[i + 1]];
            let b = if upward(next) {
                next.shoot.org
            } else {
                next.reference.org
            };
            let zone = &mut axis.blues[sorted[i]];
            let a = if upward(zone) {
                &mut zone.shoot.org
            } else {
                &mut zone.reference.org
            };
            if *a > b {
                *a = b;
            }
        }
        Ok(true)
    }

    /// `af_latin_metrics_check_digits`: whether the glyphs of the ASCII
    /// digits the face maps share one `hmtx` advance.
    fn check_digits(&mut self, face: &Face<'_>) {
        let mut first: Option<u16> = None;
        let mut same = true;
        for digit in '0'..='9' {
            let Some(glyph) = face.glyph(digit) else {
                continue;
            };
            let advance = face.font.advance(glyph).unwrap_or_default();
            match first {
                Some(first) if first != advance => {
                    same = false;
                    break;
                }
                Some(_) => {}
                None => first = Some(advance),
            }
        }
        self.digits_have_same_width = same;
    }

    /// `af_latin_metrics_scale`: the metrics at 16.16 scales `x_scale` and
    /// `y_scale` (font units to 26.6 pixels).
    pub(super) fn scaled(&self, x_scale: i64, y_scale: i64) -> Self {
        let mut scaled = self.clone();
        scaled.scale_dim(Dimension::Horizontal, x_scale);
        scaled.scale_dim(Dimension::Vertical, y_scale);
        scaled
    }

    /// `af_latin_metrics_scale_dim`. The vertical scale is first adjusted
    /// so that the x height blue zone's overshoot lands on a pixel, rounding
    /// up from 40/64, unless that moves any extent by two pixels or more.
    /// Widths and blue zones are scaled; a zone under 3/4 pixel tall is
    /// active, with its reference rounded and its overshoot at 0, 1/2 or 1
    /// pixel from it. A sub-top zone overlapping another active zone is
    /// inactive.
    fn scale_dim(&mut self, dim: Dimension, mut scale: i64) {
        let vertical = &self.axis[Dimension::Vertical.index()];
        if dim == Dimension::Vertical
            && let Some(x_height) = vertical
                .blues()
                .iter()
                .find(|b| b.flags & blue::ADJUSTMENT != 0)
        {
            let scaled = mul_fix(x_height.shoot.org, scale);
            // The `increase-x-height` property is off.
            let fitted = (scaled + 40) & !63;
            if scaled != fitted {
                let new_scale = mul_div(scale, fitted, scaled);
                let max_height = vertical
                    .blues()
                    .iter()
                    .fold(i64::from(self.units_per_em), |height, b| {
                        height.max(b.ascender).max(-b.descender)
                    });
                let distance = mul_fix(max_height, new_scale - scale);
                if -128 < distance && distance < 128 {
                    scale = new_scale;
                }
            }
        }
        let axis = &mut self.axis[dim.index()];
        axis.scale = scale;
        for width in &mut axis.widths[..axis.width_count] {
            width.cur = mul_fix(width.org, scale);
            width.fit = width.cur;
        }
        axis.extra_light = mul_fix(axis.standard_width, scale) < 32 + 8;
        if dim == Dimension::Horizontal {
            return;
        }
        for zone in &mut axis.blues[..axis.blue_count] {
            zone.reference.cur = mul_fix(zone.reference.org, scale);
            zone.reference.fit = zone.reference.cur;
            zone.shoot.cur = mul_fix(zone.shoot.org, scale);
            zone.shoot.fit = zone.shoot.cur;
            zone.flags &= !blue::ACTIVE;
            let distance = mul_fix(zone.reference.org - zone.shoot.org, scale);
            if (-48..=48).contains(&distance) {
                let step = if distance.abs() < 32 {
                    0
                } else if distance.abs() < 48 {
                    32
                } else {
                    64
                };
                let step = if distance < 0 { -step } else { step };
                zone.reference.fit = pix_round(zone.reference.cur);
                zone.shoot.fit = zone.reference.fit - step;
                zone.flags |= blue::ACTIVE;
            }
        }
        // A sub-top zone only acts where it does not overlap another zone.
        for n in 0..axis.blue_count {
            let zone = axis.blues[n];
            if zone.flags & blue::SUB_TOP == 0 || zone.flags & blue::ACTIVE == 0 {
                continue;
            }
            let overlaps = axis.blues().iter().any(|b| {
                b.flags & blue::SUB_TOP == 0
                    && b.flags & blue::ACTIVE != 0
                    && b.reference.fit <= zone.shoot.fit
                    && b.shoot.fit >= zone.reference.fit
            });
            if overlaps {
                axis.blues[n].flags &= !blue::ACTIVE;
            }
        }
    }

    /// The 16.16 horizontal scale of the scaled metrics.
    pub(super) fn x_scale(&self) -> i64 {
        self.axis[Dimension::Horizontal.index()].scale
    }

    /// The 16.16 vertical scale of the scaled metrics, adjusted to the x
    /// height.
    pub(super) fn y_scale(&self) -> i64 {
        self.axis[Dimension::Vertical.index()].scale
    }

    pub(super) const fn digits_have_same_width(&self) -> bool {
        self.digits_have_same_width
    }
}

/// The extremum one glyph gives a blue zone, part of
/// `af_latin_metrics_init_blues`: the highest point (for a top or sub-top
/// zone) or lowest of its contours of two or more points, and whether the
/// segment around it is round. Every point also widens `ascender` and
/// `descender`. `None` for a glyph FreeType skips: a degenerate one for a
/// long zone, or a round extremum for a neutral one.
fn extremum(
    points: &[OutlinePoint],
    ends: &[u32],
    properties: u8,
    units_per_em: i64,
    ascender: &mut i64,
    descender: &mut i64,
) -> Option<(i64, bool)> {
    let upward = properties & (property::TOP | property::SUB_TOP) != 0;
    let x = |i: i64| i64::from(points[i as usize].point.x);
    let y = |i: i64| i64::from(points[i as usize].point.y);
    let on = |i: i64| points[i as usize].kind == PointKind::On;
    let (mut best_point, mut best_first, mut best_last) = (-1_i64, -1_i64, -1_i64);
    let mut best_y = 0;
    let mut last = -1_i64;
    for &end in ends {
        let first = last + 1;
        last = i64::from(end) - 1;
        // Single points are never drawn; in some fonts they are mark
        // anchors far outside the glyph.
        if last <= first {
            continue;
        }
        for p in first..=last {
            let better = if upward { y(p) > best_y } else { y(p) < best_y };
            if best_point < 0 || better {
                best_point = p;
                best_y = y(p);
                if upward {
                    *ascender = (*ascender).max(best_y);
                } else {
                    *descender = (*descender).min(best_y);
                }
            } else if upward {
                *descender = (*descender).min(y(p));
            } else {
                *ascender = (*ascender).max(y(p));
            }
        }
        if best_point > best_last {
            best_first = first;
            best_last = last;
        }
    }
    if best_point < 0 {
        // FreeType then keeps its initial flat zero.
        return Some((0, false));
    }
    let (contour_first, contour_last) = (best_first, best_last);
    let previous = |p: i64| {
        if p > contour_first {
            p - 1
        } else {
            contour_last
        }
    };
    let following = |p: i64| {
        if p < contour_last {
            p + 1
        } else {
            contour_first
        }
    };
    // Accept a small distance or a small angle (20 is about 2.9 degrees).
    let leaves = |from: i64, to: i64| {
        let distance = (y(to) - y(from)).abs();
        distance > 5 && (x(to) - x(from)).abs() <= 20 * distance
    };
    let best_x = x(best_point);
    let (mut segment_first, mut segment_last) = (best_point, best_point);
    let (mut on_first, mut on_last) = if on(best_point) {
        (best_point, best_point)
    } else {
        (-1, -1)
    };
    // The points before and after the extremum at about its height.
    let mut prev = best_point;
    loop {
        prev = previous(prev);
        let distance = (y(prev) - best_y).abs();
        if distance > 5 && (x(prev) - best_x).abs() <= 20 * distance {
            break;
        }
        segment_first = prev;
        if on(prev) {
            on_first = prev;
            if on_last < 0 {
                on_last = prev;
            }
        }
        if prev == best_point {
            break;
        }
    }
    let mut next = best_point;
    loop {
        next = following(next);
        let distance = (y(next) - best_y).abs();
        if distance > 5 && (x(next) - best_x).abs() <= 20 * distance {
            break;
        }
        segment_last = next;
        if on(next) {
            on_last = next;
            if on_first < 0 {
                on_first = next;
            }
        }
        if next == best_point {
            break;
        }
    }
    if properties & property::LONG != 0 {
        // The segment must be long enough; else look along the contour for
        // one that is, in the same direction and not too far below.
        let length_threshold = units_per_em / 25;
        let length = (x(segment_last) - x(segment_first)).abs();
        if length < length_threshold
            && segment_last - segment_first + 2 <= contour_last - contour_first
        {
            let height_threshold = units_per_em / 4;
            let mut prev = best_point;
            loop {
                prev = previous(prev);
                if x(prev) != best_x || prev == best_point {
                    break;
                }
            }
            if prev == best_point {
                return None;
            }
            let left_to_right = x(prev) < x(best_point);
            let (mut first, mut last) = (segment_last, segment_last);
            let (mut p_first, mut p_last) = (0, 0);
            let mut hit = false;
            loop {
                if !hit {
                    first = last;
                    (p_first, p_last) = if on(first) { (first, first) } else { (-1, -1) };
                    hit = true;
                }
                last = following(last);
                if (best_y - y(first)).abs() > height_threshold || leaves(first, last) {
                    hit = false;
                } else {
                    let distance = (y(last) - y(first)).abs();
                    if on(last) {
                        p_last = last;
                        if p_first < 0 {
                            p_first = last;
                        }
                    }
                    if (x(first) < x(last)) == left_to_right
                        && (x(last) - x(first)).abs() >= length_threshold
                    {
                        // Extend the segment to its end. FreeType tests the
                        // x of `next` and the earlier distance here.
                        loop {
                            last = following(last);
                            let d = (y(last) - y(first)).abs();
                            if d > 5 && (x(next) - x(first)).abs() <= 20 * distance {
                                last = previous(last);
                                break;
                            }
                            p_last = last;
                            if on(last) && p_first < 0 {
                                p_first = last;
                            }
                            if last == segment_first {
                                break;
                            }
                        }
                        best_y = y(first);
                        segment_first = first;
                        segment_last = last;
                        on_first = p_first;
                        on_last = p_last;
                        break;
                    }
                }
                if last == segment_first {
                    break;
                }
            }
        }
    }
    // Flat if the on points span more than the flat threshold, else round
    // if the segment starts or ends off the curve.
    let flat_threshold = units_per_em / 14;
    let round =
        if on_first >= 0 && on_last >= 0 && (x(on_last) - x(on_first)).abs() > flat_threshold {
            false
        } else {
            !on(segment_first) || !on(segment_last)
        };
    if round && properties & property::NEUTRAL != 0 {
        return None;
    }
    Some((best_y, round))
}
