//! Finding a glyph's features for the CJK writing system: the latin
//! writing system's segments, the stems and serifs they form, the edges
//! they group into and the blue zones those edges lie in.

use super::super::hints::{Dimension, GlyphHints};
use super::super::latin::{self, add_to_edge, edge_properties, start_edge};
use super::{Axis, Metrics, blue};
use crate::long::{abs, div_fix, mul_fix};
use crate::work::charge;
use triergon_core::{Result, WorkBudget};

/// `af_cjk_hints_detect_features`: segments, their links and edges.
///
/// The segments are the latin writing system's
/// (`af_cjk_hints_compute_segments`): FreeType's pass that then recomputes
/// their roundness walks the segments counted before they were computed,
/// none, and changes nothing.
pub(super) fn detect_features(
    hints: &mut GlyphHints,
    dim: Dimension,
    metrics: &Metrics,
    work: &mut WorkBudget,
) -> Result<()> {
    latin::compute_segments(hints, dim, metrics.units_per_em, work)?;
    link_segments(hints, dim, metrics.units_per_em, work)?;
    compute_edges(hints, dim, &metrics.axis[dim.index()], work)
}

/// `af_cjk_hints_link_segments`: pairs each segment in the major direction
/// with a facing segment of opposite direction at or after its position
/// that overlaps it by 8/2048 em or more: the nearest, unless a slightly
/// farther one overlaps longer. Then, as strokes of Hanzi widen at their
/// ends, a stem nested in a wider one and nearer than 3 pixels either
/// turns the wider one's segments into serifs of its own (if three times as
/// long) or loses its link. A segment whose partner prefers another loses
/// its link and becomes a serif of that other segment if the partner's
/// distance to it is under 3 pixels or over a quarter of its own.
fn link_segments(
    hints: &mut GlyphHints,
    dim: Dimension,
    units_per_em: u16,
    work: &mut WorkBudget,
) -> Result<()> {
    let scale = match dim {
        Dimension::Horizontal => hints.x_scale,
        Dimension::Vertical => hints.y_scale,
    };
    let axis = &mut hints.axis[dim.index()];
    let major_dir = axis.major_dir;
    let segments = axis.segments.as_mut_slice();
    let count = segments.len();
    charge(work, 2 * count * count + count)?;
    let len_threshold = latin::constant(units_per_em, 8);
    let dist_threshold = div_fix(64 * 3, scale);
    for i in 0..count {
        if segments[i].dir != major_dir {
            continue;
        }
        for j in 0..count {
            let (first, second) = (segments[i], segments[j]);
            if j == i || i32::from(first.dir) + i32::from(second.dir) != 0 {
                continue;
            }
            let dist = i64::from(second.pos) - i64::from(first.pos);
            if dist < 0 {
                continue;
            }
            // How much the two overlap along the axis.
            let min = first.min_coord.max(second.min_coord);
            let max = first.max_coord.min(second.max_coord);
            let len = i64::from(max) - i64::from(min);
            if len < len_threshold {
                continue;
            }
            // Nearer by an eighth, or nearer by less but overlapping longer.
            let better = |score: i64, kept: i64| {
                dist * 8 < score * 9 && (dist * 8 < score * 7 || kept < len)
            };
            if better(first.score, first.len) {
                let segment = &mut segments[i];
                (segment.score, segment.len, segment.link) = (dist, len, Some(j));
            }
            let second = segments[j];
            if better(second.score, second.len) {
                let segment = &mut segments[j];
                (segment.score, segment.len, segment.link) = (dist, len, Some(i));
            }
        }
    }
    // Stems ending in wider strokes: seg2 < seg1 < link1 < link2.
    for i in 0..count {
        let Some(link1) = segments[i].link else {
            continue;
        };
        if segments[link1].link != Some(i) || segments[link1].pos <= segments[i].pos {
            continue;
        }
        if segments[i].score >= dist_threshold {
            continue;
        }
        for j in 0..count {
            let (seg1, seg2) = (segments[i], segments[j]);
            if seg2.pos > seg1.pos || j == i {
                continue;
            }
            let Some(link2) = seg2.link else {
                continue;
            };
            if segments[link2].link != Some(j) || segments[link2].pos < segments[link1].pos {
                continue;
            }
            if seg1.pos == seg2.pos && segments[link1].pos == segments[link2].pos {
                continue;
            }
            if seg2.score <= seg1.score || seg1.score * 4 <= seg2.score {
                continue;
            }
            if seg1.len >= seg2.len * 3 {
                // The wider stem's segments become serifs of the narrower.
                for segment in segments.iter_mut() {
                    if segment.link == Some(j) {
                        segment.link = None;
                        segment.serif = Some(link1);
                    } else if segment.link == Some(link2) {
                        segment.link = None;
                        segment.serif = Some(i);
                    }
                }
            } else {
                segments[link1].link = None;
                segments[i].link = None;
                break;
            }
        }
    }
    for i in 0..count {
        let Some(other) = segments[i].link else {
            continue;
        };
        if segments[other].link != Some(i) {
            segments[i].link = None;
            if segments[other].score < dist_threshold
                || segments[i].score < segments[other].score * 4
            {
                segments[i].serif = segments[other].link;
            }
        }
    }
    Ok(())
}

/// `af_cjk_hints_compute_edges`: groups every segment of `dim` into edges,
/// sorted by position: a segment joins the nearest edge of its direction
/// closer than the axis's edge threshold (at most 1/4 pixel) whose linked
/// segments all lie that close to its own link, else starts one. Each edge
/// then takes its links and serifs from its segments and is round where
/// most of its segments are.
fn compute_edges(
    hints: &mut GlyphHints,
    dim: Dimension,
    cjk: &Axis,
    work: &mut WorkBudget,
) -> Result<()> {
    let scale = match dim {
        Dimension::Horizontal => hints.x_scale,
        Dimension::Vertical => hints.y_scale,
    };
    let threshold = cjk.stems.edge_distance_threshold;
    let edge_distance_threshold = if mul_fix(threshold, scale) > 64 / 4 {
        div_fix(64 / 4, scale)
    } else {
        threshold
    };
    let axis = &mut hints.axis[dim.index()];
    axis.edges.clear();
    let count = axis.segments.len();
    for s in 0..count {
        // The edges, and the segments of each candidate.
        charge(work, axis.edges.len() + s + 1)?;
        let segment = axis.segments[s];
        let segments = axis.segments.as_slice();
        let mut found = None;
        let mut best = 0xFFFF;
        for (e, edge) in axis.edges.iter().enumerate() {
            if edge.dir != segment.dir {
                continue;
            }
            let dist = (i64::from(segment.pos) - i64::from(edge.fpos)).abs();
            if dist >= edge_distance_threshold || dist >= best {
                continue;
            }
            if let Some(link) = segment.link {
                // The links of the edge's segments must all lie that close
                // to the segment's own link.
                let mut dist2 = 0;
                let mut s1 = edge.first;
                loop {
                    if let Some(link1) = segments[s1].link {
                        dist2 =
                            (i64::from(segments[link].pos) - i64::from(segments[link1].pos)).abs();
                        if dist2 >= edge_distance_threshold {
                            break;
                        }
                    }
                    s1 = segments[s1].edge_next.unwrap_or(edge.first);
                    if s1 == edge.first {
                        break;
                    }
                }
                if dist2 >= edge_distance_threshold {
                    continue;
                }
            }
            best = dist;
            found = Some(e);
        }
        match found {
            None => start_edge(axis, s, scale, false)?,
            Some(e) => add_to_edge(axis.segments.as_mut_slice(), &mut axis.edges[e], s),
        }
    }
    edge_properties(
        axis.segments.as_mut_slice(),
        axis.edges.as_mut_slice(),
        work,
    )
}

/// `af_cjk_hints_compute_blue_edges`: gives each edge of `dim` the active
/// blue zone edge nearest it within 1/40 em (at most half a pixel): of
/// each zone, whichever of its reference and overshoot is nearer. Top (and
/// right) zones take edges against the major direction, bottom (and left)
/// zones edges along it.
pub(super) fn compute_blue_edges(
    hints: &mut GlyphHints,
    metrics: &Metrics,
    dim: Dimension,
    work: &mut WorkBudget,
) -> Result<()> {
    let cjk = &metrics.axis[dim.index()];
    let scale = cjk.scale;
    let best_dist0 = mul_fix(i64::from(metrics.units_per_em / 40), scale).min(64 / 2);
    let axis = &mut hints.axis[dim.index()];
    let major_dir = axis.major_dir;
    charge(work, axis.edges.len() * (cjk.blue_count + 1))?;
    for edge in axis.edges.iter_mut() {
        let fpos = i64::from(edge.fpos);
        let mut best_dist = best_dist0;
        let mut best = None;
        for zone in cjk.blues() {
            if zone.flags & blue::ACTIVE == 0 {
                continue;
            }
            let top = zone.flags & blue::TOP != 0;
            if top == (edge.dir == major_dir) {
                continue;
            }
            let compare = if abs(fpos - zone.reference.org) > abs(fpos - zone.shoot.org) {
                zone.shoot
            } else {
                zone.reference
            };
            let dist = mul_fix(abs(fpos - compare.org), scale);
            if dist < best_dist {
                best_dist = dist;
                best = Some(compare.fit);
            }
        }
        if best.is_some() {
            edge.blue_edge = best;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::super::hints::{Dimension, GlyphHints, Segment, direction};
    use super::link_segments;
    use triergon_core::{Budget, WorkBudget};

    /// The links and serifs `link_segments` leaves on vertical segments at
    /// a scale of one, each `(dir, pos, min_coord, max_coord)`.
    fn linked(segments: &[(i8, i16, i16, i16)]) -> Vec<(Option<usize>, Option<usize>)> {
        let budget = Budget::new(1 << 20).unwrap();
        let mut hints = GlyphHints::new(&budget);
        hints.x_scale = 0x1_0000;
        let axis = &mut hints.axis[0];
        axis.major_dir = direction::UP;
        for &(dir, pos, min_coord, max_coord) in segments {
            let segment = Segment {
                dir,
                pos,
                min_coord,
                max_coord,
                ..Segment::NEW
            };
            axis.segments.try_push(segment).unwrap();
        }
        let mut work = WorkBudget::new(1 << 20);
        link_segments(&mut hints, Dimension::Horizontal, 1000, &mut work).unwrap();
        let axis = &hints.axis[0];
        axis.segments.iter().map(|s| (s.link, s.serif)).collect()
    }

    #[test]
    fn stems_ending_in_wider_strokes_become_serifs_or_unlink() {
        let (up, down) = (direction::UP, direction::DOWN);
        // A stem 200 wide over 0..100 around one 100 wide over 200..800,
        // which nothing else overlaps: two stems. The narrow one is under
        // the 3-pixel threshold, nearer than the wide one but by less than
        // four times, and six times as long: the wide stem's segments
        // become serifs of the narrow one's.
        let nested = [
            (up, 100, 0, 100),
            (down, 300, 0, 100),
            (up, 150, 200, 800),
            (down, 250, 200, 800),
        ];
        assert_eq!(
            linked(&nested),
            [
                (None, Some(2)),
                (None, Some(3)),
                (Some(3), None),
                (Some(2), None)
            ]
        );
        // Twice as long only: the narrow stem loses its links instead.
        let mut short = nested;
        (short[2].3, short[3].3) = (400, 400);
        assert_eq!(
            linked(&short),
            [(Some(1), None), (Some(0), None), (None, None), (None, None)]
        );
    }

    #[test]
    fn a_segment_whose_partner_prefers_another_becomes_its_serif() {
        let (up, down) = (direction::UP, direction::DOWN);
        // Two up segments facing one down one, the second nearer: the
        // first loses its link and, its partner being nearer than 3 pixels
        // to the second, becomes a serif of the second.
        let segments = [(up, 100, 0, 500), (up, 300, 0, 500), (down, 400, 0, 500)];
        assert_eq!(
            linked(&segments),
            [(None, Some(1)), (Some(2), None), (Some(1), None)]
        );
    }
}
