//! Finding a glyph's features for the latin writing system: segments along
//! a dimension, the stems and serifs they form, the edges they group into
//! and the blue zones those edges lie in.

use super::super::hints::{Dimension, Edge, GlyphHints, Segment, Width, direction, edge, point};
use super::{Axis, Metrics, blue, constant};
use crate::long::{abs, div_fix, mul_fix, wrap};
use crate::work::charge;
use triergon_core::{Result, WorkBudget};

/// Where a segment ends while it is being found, and FreeType's copy of
/// those values for the previous segment of the contour.
#[derive(Clone, Copy, Debug)]
struct Extent {
    min_pos: i64,
    max_pos: i64,
    min_coord: i64,
    max_coord: i64,
    min_flags: u16,
    max_flags: u16,
    min_on_coord: i64,
    max_on_coord: i64,
}

impl Extent {
    const EMPTY: Self = Self {
        min_pos: 32000,
        max_pos: -32000,
        min_coord: 32000,
        max_coord: -32000,
        min_flags: 0,
        max_flags: 0,
        min_on_coord: 32000,
        max_on_coord: -32000,
    };

    /// Whether a segment spanning this extent is round: it starts or ends
    /// at a control point and its on points stay within the flat threshold.
    fn round(&self, flat_threshold: i64) -> bool {
        (self.min_flags | self.max_flags) & point::CONTROL != 0
            && self.max_on_coord - self.min_on_coord < flat_threshold
    }

    /// Sets `segment`'s position, spread and coordinates from the extent,
    /// and its round flag when the extent is round.
    fn close(&self, segment: &mut Segment, flat_threshold: i64) {
        segment.pos = ((self.min_pos + self.max_pos) >> 1) as i16;
        segment.delta = ((self.max_pos - self.min_pos) >> 1) as i16;
        if self.round(flat_threshold) {
            segment.flags |= edge::ROUND;
        }
        segment.min_coord = self.min_coord as i16;
        segment.max_coord = self.max_coord as i16;
        segment.height = (i32::from(segment.max_coord) - i32::from(segment.min_coord)) as i16;
    }
}

/// `af_latin_hints_compute_segments`: the runs of points along `dim` whose
/// out direction is the dimension's major direction (or its opposite), in
/// font units, with their positions across the run (`u`) and their extent
/// along it (`v`); one-point contours give one-point segments. A glyph of
/// more than 1000 segments gets none. Segment heights then grow by half the
/// distance to the points just outside them, which helps tell serifs.
pub(super) fn compute_segments(
    hints: &mut GlyphHints,
    dim: Dimension,
    units_per_em: u16,
    work: &mut WorkBudget,
) -> Result<()> {
    let flat_threshold = i64::from(units_per_em / 14);
    let axis = &mut hints.axis[dim.index()];
    let points = hints.points.as_mut_slice();
    let major_dir = axis.major_dir.abs();
    let mut segment_dir = major_dir;
    axis.segments.clear();
    charge(work, 2 * points.len())?;
    for p in points.iter_mut() {
        let (x, y) = (i64::from(p.fx), i64::from(p.fy));
        (p.u, p.v) = match dim {
            Dimension::Horizontal => (x, y),
            Dimension::Vertical => (y, x),
        };
    }
    let segments = &mut axis.segments;
    for &contour in hints.contours.as_slice() {
        let mut point = contour;
        let mut last = points[point].prev;
        let mut on_edge = false;
        let mut extent = Extent::EMPTY;
        let mut previous_extent = Extent::EMPTY;
        let mut previous: Option<usize> = None;
        let mut current: Option<usize> = None;
        // Starting on an edge: go back to its start.
        if points[point].flags & point::IGNORE == 0
            && points[last].out_dir.abs() == major_dir
            && points[point].out_dir.abs() == major_dir
        {
            last = point;
            loop {
                point = points[point].prev;
                if points[point].out_dir.abs() != major_dir {
                    point = points[point].next;
                    break;
                }
                if point == last {
                    break;
                }
            }
        }
        last = point;
        let mut passed = false;
        loop {
            if on_edge && let Some(index) = current {
                let p = points[point];
                extent.min_pos = extent.min_pos.min(p.u);
                extent.max_pos = extent.max_pos.max(p.u);
                if p.v < extent.min_coord {
                    extent.min_coord = p.v;
                    extent.min_flags = p.flags;
                }
                if p.v > extent.max_coord {
                    extent.max_coord = p.v;
                    extent.max_flags = p.flags;
                }
                if p.flags & point::CONTROL == 0 {
                    extent.min_on_coord = extent.min_on_coord.min(p.v);
                    extent.max_on_coord = extent.max_on_coord.max(p.v);
                }
                if p.flags & point::IGNORE != 0 || p.out_dir != segment_dir || point == last {
                    // Leaving the edge. A segment starting where the
                    // previous one ended (a spike) merges with it.
                    let merge = previous.filter(|&prev| {
                        p.flags & point::IGNORE == 0 && segments[index].first == segments[prev].last
                    });
                    match merge {
                        None => {
                            let segment = &mut segments[index];
                            segment.last = point;
                            extent.close(segment, flat_threshold);
                            previous = Some(index);
                            previous_extent = extent;
                        }
                        Some(prev) => {
                            let same_direction = points[segments[prev].last].in_dir == p.in_dir;
                            merge_segments(
                                segments.as_mut_slice(),
                                prev,
                                index,
                                point,
                                same_direction,
                                &mut extent,
                                &mut previous_extent,
                                flat_threshold,
                            );
                            segments.pop();
                        }
                    }
                    on_edge = false;
                    current = None;
                }
            }
            if point == last {
                if passed {
                    break;
                }
                passed = true;
            }
            let p = points[point];
            // A new segment where the out direction is the major one, or
            // at a one-point contour.
            if p.flags & point::IGNORE == 0
                && !on_edge
                && (p.out_dir.abs() == major_dir || point == p.prev)
            {
                if segments.len() > 1000 {
                    // Too many to hint sensibly.
                    segments.clear();
                    return Ok(());
                }
                segment_dir = p.out_dir;
                charge(work, 1)?;
                let index = segments.len();
                segments
                    .try_push(Segment {
                        dir: segment_dir,
                        first: point,
                        last: point,
                        ..Segment::NEW
                    })
                    .map_err(|e| e.error)?;
                extent = Extent {
                    min_pos: p.u,
                    max_pos: p.u,
                    min_coord: p.v,
                    max_coord: p.v,
                    min_flags: p.flags,
                    max_flags: p.flags,
                    min_on_coord: if p.flags & point::CONTROL != 0 {
                        32000
                    } else {
                        p.v
                    },
                    max_on_coord: if p.flags & point::CONTROL != 0 {
                        -32000
                    } else {
                        p.v
                    },
                };
                on_edge = true;
                current = Some(index);
                if point == p.prev {
                    // A one-point contour, without a direction.
                    let segment = &mut segments[index];
                    segment.pos = p.u as i16;
                    if p.flags & point::CONTROL != 0 {
                        segment.flags |= edge::ROUND;
                    }
                    segment.min_coord = p.v as i16;
                    segment.max_coord = p.v as i16;
                    segment.height = 0;
                    on_edge = false;
                    current = None;
                }
            }
            point = p.next;
        }
    }
    // Grow segments by half the distance to the points beyond their ends.
    for segment in segments.iter_mut() {
        let (first, last) = (points[segment.first], points[segment.last]);
        let (before, after) = (points[first.prev].v, points[last.next].v);
        let grow = |height: i16, by: i64| (i64::from(height) + (by >> 1)) as i16;
        if first.v < last.v {
            if before < first.v {
                segment.height = grow(segment.height, first.v - before);
            }
            if after > last.v {
                segment.height = grow(segment.height, after - last.v);
            }
        } else {
            if before > first.v {
                segment.height = grow(segment.height, before - first.v);
            }
            if after < last.v {
                segment.height = grow(segment.height, last.v - after);
            }
        }
    }
    Ok(())
}

/// Merges segment `current`, which ends at `point`, with `previous`, which
/// ended where it starts, as `af_latin_hints_compute_segments` does: with
/// identical directions the two become one; otherwise the longer one stays,
/// in `previous`'s place.
#[expect(clippy::too_many_arguments, reason = "the state of the segment walk")]
fn merge_segments(
    segments: &mut [Segment],
    previous: usize,
    current: usize,
    point: usize,
    same_direction: bool,
    extent: &mut Extent,
    previous_extent: &mut Extent,
    flat_threshold: i64,
) {
    let (old, new) = (*previous_extent, *extent);
    if same_direction {
        extent.min_pos = old.min_pos.min(new.min_pos);
        extent.max_pos = old.max_pos.max(new.max_pos);
        if old.min_coord < new.min_coord {
            extent.min_coord = old.min_coord;
            extent.min_flags = old.min_flags;
        }
        if old.max_coord > new.max_coord {
            extent.max_coord = old.max_coord;
            extent.max_flags = old.max_flags;
        }
        extent.min_on_coord = old.min_on_coord.min(new.min_on_coord);
        extent.max_on_coord = old.max_on_coord.max(new.max_on_coord);
        let segment = &mut segments[previous];
        segment.last = point;
        segment.flags &= !edge::ROUND;
        extent.close(segment, flat_threshold);
    } else if (old.max_coord - old.min_coord).abs() > (new.max_coord - new.min_coord).abs() {
        // Keep the previous segment.
        previous_extent.min_pos = new.min_pos.min(old.min_pos);
        previous_extent.max_pos = new.max_pos.max(old.max_pos);
        let segment = &mut segments[previous];
        segment.last = point;
        segment.pos = ((previous_extent.min_pos + previous_extent.max_pos) >> 1) as i16;
        segment.delta = ((previous_extent.max_pos - previous_extent.min_pos) >> 1) as i16;
    } else {
        // Keep the current one, in the previous one's place.
        extent.min_pos = old.min_pos.min(new.min_pos);
        extent.max_pos = old.max_pos.max(new.max_pos);
        let segment = &mut segments[current];
        segment.last = point;
        extent.close(segment, flat_threshold);
        segments[previous] = segments[current];
        *previous_extent = *extent;
    }
}

/// `af_latin_hints_link_segments`: pairs each segment in the major
/// direction with the facing segment of opposite direction to its right
/// that scores best, by overlap along the axis and by distance against the
/// widest standard width `widths` gives (or the plain distance without
/// widths). A segment whose best partner prefers another becomes a serif of
/// that other segment.
pub(super) fn link_segments(
    hints: &mut GlyphHints,
    dim: Dimension,
    units_per_em: u16,
    widths: &[Width],
    work: &mut WorkBudget,
) -> Result<()> {
    let axis = &mut hints.axis[dim.index()];
    let major_dir = axis.major_dir;
    let segments = axis.segments.as_mut_slice();
    charge(work, segments.len() * segments.len())?;
    let max_width = widths.last().map_or(0, |w| w.org);
    let len_threshold = constant(units_per_em, 8).max(1);
    let len_score = constant(units_per_em, 6000);
    let dist_score = 3000;
    for i in 0..segments.len() {
        if segments[i].dir != major_dir {
            continue;
        }
        for j in 0..segments.len() {
            let (first, second) = (segments[i], segments[j]);
            let (pos1, pos2) = (i64::from(first.pos), i64::from(second.pos));
            if i32::from(first.dir) + i32::from(second.dir) != 0 || pos2 <= pos1 {
                continue;
            }
            // How much the two overlap along the axis.
            let min = first.min_coord.max(second.min_coord);
            let max = first.max_coord.min(second.max_coord);
            let len = i64::from(max) - i64::from(min);
            if len < len_threshold {
                continue;
            }
            let dist = pos2 - pos1;
            let dist_demerit = if max_width != 0 {
                // Multiples of the widest width, in 1/1024.
                let delta = (dist << 10) / max_width - (1 << 10);
                if delta > 10000 {
                    32000
                } else if delta > 0 {
                    delta * delta / dist_score
                } else {
                    0
                }
            } else {
                dist
            };
            let score = dist_demerit + len_score / len;
            if score < segments[i].score {
                segments[i].score = score;
                segments[i].link = Some(j);
            }
            if score < segments[j].score {
                segments[j].score = score;
                segments[j].link = Some(i);
            }
        }
    }
    for i in 0..segments.len() {
        if let Some(link) = segments[i].link
            && segments[link].link != Some(i)
        {
            segments[i].link = None;
            segments[i].serif = segments[link].link;
        }
    }
    Ok(())
}

/// `af_latin_hints_compute_edges`: groups the segments of `dim` into edges,
/// sorted by position: a segment joins the first edge of its direction
/// closer than the axis's edge threshold (at most 1/4 pixel), else starts
/// one. Segments shorter than a pixel (horizontal edges only), wider than
/// a pixel, or serifs shorter than 1.5 pixels are left out; one-point
/// segments join an edge of any direction near them. Each edge then takes
/// its links and serifs from its segments and is round where most of its
/// segments are.
pub(super) fn compute_edges(
    hints: &mut GlyphHints,
    dim: Dimension,
    latin: &Axis,
    top_to_bottom: bool,
    work: &mut WorkBudget,
) -> Result<()> {
    let scale = match dim {
        Dimension::Horizontal => hints.x_scale,
        Dimension::Vertical => hints.y_scale,
    };
    let top_to_bottom = dim == Dimension::Vertical && top_to_bottom;
    let segment_length_threshold = match dim {
        Dimension::Horizontal => div_fix(64, hints.y_scale),
        Dimension::Vertical => 0,
    };
    let segment_width_threshold = div_fix(32, scale);
    let edge_distance_threshold = div_fix(
        mul_fix(latin.edge_distance_threshold, scale).min(64 / 4),
        scale,
    );
    let axis = &mut hints.axis[dim.index()];
    axis.edges.clear();
    let count = axis.segments.len();
    for s in 0..count {
        let segment = axis.segments[s];
        let height = i64::from(segment.height);
        if height < segment_length_threshold
            || i64::from(segment.delta) > segment_width_threshold
            || segment.dir == direction::NONE
        {
            continue;
        }
        if segment.serif.is_some() && 2 * height < wrap(3 * segment_length_threshold) {
            continue;
        }
        charge(work, axis.edges.len() + 1)?;
        let found = axis.edges.iter().position(|e| {
            (i64::from(segment.pos) - i64::from(e.fpos)).abs() < edge_distance_threshold
                && e.dir == segment.dir
        });
        match found {
            None => {
                let at = axis.new_edge(segment.pos, segment.dir, top_to_bottom)?;
                let opos = mul_fix(i64::from(segment.pos), scale);
                axis.edges[at] = Edge {
                    fpos: segment.pos,
                    opos,
                    pos: opos,
                    dir: segment.dir,
                    first: s,
                    last: s,
                    ..Edge::default()
                };
                axis.segments[s].edge_next = Some(s);
            }
            Some(e) => add_to_edge(axis.segments.as_mut_slice(), &mut axis.edges[e], s),
        }
    }
    // One-point segments without a direction join an edge near them.
    for s in 0..count {
        let segment = axis.segments[s];
        if segment.dir != direction::NONE {
            continue;
        }
        charge(work, axis.edges.len() + 1)?;
        let found = axis.edges.iter().position(|e| {
            (i64::from(segment.pos) - i64::from(e.fpos)).abs() < edge_distance_threshold
        });
        if let Some(e) = found {
            add_to_edge(axis.segments.as_mut_slice(), &mut axis.edges[e], s);
        }
    }
    let segments = axis.segments.as_mut_slice();
    let edges = axis.edges.as_mut_slice();
    for (e, edge) in edges.iter().enumerate() {
        let mut s = edge.first;
        loop {
            segments[s].edge = Some(e);
            s = segments[s].edge_next.unwrap_or(edge.first);
            if s == edge.first {
                break;
            }
        }
    }
    charge(work, count)?;
    for e in 0..edges.len() {
        let (mut round, mut straight) = (0, 0);
        let mut s = edges[e].first;
        loop {
            let segment = segments[s];
            if segment.flags & edge::ROUND != 0 {
                round += 1;
            } else {
                straight += 1;
            }
            // A serif segment's link is ignored.
            let serif = segment
                .serif
                .filter(|&serif| segments[serif].edge.is_some_and(|other| other != e));
            let link = segment.link.filter(|&link| segments[link].edge.is_some());
            if let Some(other) = serif.or(link) {
                let current = if serif.is_some() {
                    edges[e].serif
                } else {
                    edges[e].link
                };
                // The nearer of the edge found so far and the other
                // segment's edge.
                let target = match current {
                    Some(found) => {
                        let edge_delta =
                            (i64::from(edges[e].fpos) - i64::from(edges[found].fpos)).abs();
                        let seg_delta =
                            (i64::from(segment.pos) - i64::from(segments[other].pos)).abs();
                        if seg_delta < edge_delta {
                            segments[other].edge
                        } else {
                            Some(found)
                        }
                    }
                    None => segments[other].edge,
                };
                if serif.is_some() {
                    edges[e].serif = target;
                    if let Some(target) = target {
                        edges[target].flags |= edge::SERIF;
                    }
                } else {
                    edges[e].link = target;
                }
            }
            s = segment.edge_next.unwrap_or(edges[e].first);
            if s == edges[e].first {
                break;
            }
        }
        edges[e].flags = if round > 0 && round >= straight {
            edge::ROUND
        } else {
            0
        };
        // Serifs give way to links.
        if edges[e].serif.is_some() && edges[e].link.is_some() {
            edges[e].serif = None;
        }
    }
    Ok(())
}

/// Adds segment `s` to the circular segment list of `edge`.
fn add_to_edge(segments: &mut [Segment], edge: &mut Edge, s: usize) {
    segments[s].edge_next = Some(edge.first);
    segments[edge.last].edge_next = Some(s);
    edge.last = s;
}

/// `af_latin_hints_detect_features`: segments, their links and edges.
pub(super) fn detect_features(
    hints: &mut GlyphHints,
    dim: Dimension,
    metrics: &Metrics,
    top_to_bottom: bool,
    work: &mut WorkBudget,
) -> Result<()> {
    let latin = &metrics.axis[dim.index()];
    compute_segments(hints, dim, metrics.units_per_em, work)?;
    link_segments(hints, dim, metrics.units_per_em, latin.widths(), work)?;
    compute_edges(hints, dim, latin, top_to_bottom, work)
}

/// `af_latin_hints_compute_blue_edges`: gives each horizontal edge the
/// active blue zone edge nearest it within 1/40 em (at most half a pixel):
/// the reference, or for a round edge beyond it the overshoot. Top zones
/// take edges against the major direction, bottom zones edges along it, and
/// neutral zones both.
pub(super) fn compute_blue_edges(
    hints: &mut GlyphHints,
    metrics: &Metrics,
    work: &mut WorkBudget,
) -> Result<()> {
    let latin = &metrics.axis[Dimension::Vertical.index()];
    let scale = latin.scale;
    let axis = &mut hints.axis[Dimension::Vertical.index()];
    let major_dir = axis.major_dir;
    charge(work, axis.edges.len() * (latin.blue_count + 1))?;
    for edge in axis.edges.iter_mut() {
        if edge.flags & edge::NO_BLUE != 0 {
            continue;
        }
        let mut best_dist = mul_fix(i64::from(metrics.units_per_em / 40), scale).min(64 / 2);
        let mut best: Option<(i64, bool)> = None;
        let fpos = i64::from(edge.fpos);
        for zone in latin.blues() {
            if zone.flags & blue::ACTIVE == 0 {
                continue;
            }
            let top = zone.flags & (blue::TOP | blue::SUB_TOP) != 0;
            let neutral = zone.flags & blue::NEUTRAL != 0;
            let major = edge.dir == major_dir;
            if !(top != major || neutral) {
                continue;
            }
            let dist = mul_fix(abs(fpos - zone.reference.org), scale);
            if dist < best_dist {
                best_dist = dist;
                best = Some((zone.reference.fit, neutral));
            }
            // The overshoot, for a round edge beyond the reference.
            if edge.flags & edge::ROUND != 0 && dist != 0 && !neutral {
                let under = fpos < zone.reference.org;
                if top != under {
                    let dist = mul_fix(abs(fpos - zone.shoot.org), scale);
                    if dist < best_dist {
                        best_dist = dist;
                        best = Some((zone.shoot.fit, neutral));
                    }
                }
            }
        }
        if let Some((fit, neutral)) = best {
            edge.blue_edge = Some(fit);
            if neutral {
                edge.flags |= edge::NEUTRAL;
            }
        }
    }
    Ok(())
}
