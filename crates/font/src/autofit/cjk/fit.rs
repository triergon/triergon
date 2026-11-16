//! Grid-fitting a glyph with the CJK writing system, as FreeType's default
//! load (`FT_LOAD_TARGET_NORMAL`) runs it: stems are adjusted to whole
//! pixels, without snapping to standard widths and without the light,
//! monochrome and LCD modes' variations.

use super::super::hints::{Dimension, Edge, GlyphHints, edge, point};
use super::{Axis, Metrics, analysis};
use crate::long::{abs, mul_div, wrap};
use crate::work::charge;
use triergon_core::{Result, WorkBudget};

/// `af_cjk_hints_apply` after `af_cjk_hints_init`: hints the glyph loaded
/// into `hints` (scaled by the metrics' scales) with `metrics`, scaled.
///
/// In both dimensions edges are found and given their blue zones; then
/// each dimension's edges are fitted, the points of their segments moved
/// with them, and strong points interpolated between the edges and weak
/// points between those.
pub(in super::super) fn apply(
    hints: &mut GlyphHints,
    metrics: &Metrics,
    work: &mut WorkBudget,
) -> Result<()> {
    for dim in Dimension::BOTH {
        analysis::detect_features(hints, dim, metrics, work)?;
        analysis::compute_blue_edges(hints, metrics, dim, work)?;
    }
    for dim in Dimension::BOTH {
        hint_edges(hints, dim, &metrics.axis[dim.index()], work)?;
        align_edge_points(hints, dim, work)?;
        hints.align_strong_points(dim, work)?;
        hints.align_weak_points(dim, work)?;
    }
    Ok(())
}

/// `af_cjk_compute_stem_width` for the default load, with stem adjustment
/// and without snapping: the width of a stem, lightly quantized. Where the
/// axis has standard widths, a stem within 40/64 pixel of the first one's
/// scaled width (which the CJK metrics leave at zero) becomes 3/4 pixel;
/// others under 54/64 pixel grow halfway to that, and others under three
/// pixels keep their whole pixels and a fraction under 10/64 or from 22/64
/// to 42/64, but a fraction up to 22/64 becomes 10/64 and one up to 54/64
/// becomes 54/64. A width of `FT_LONG_MIN` stays negative when made
/// positive, as FreeType's does.
fn stem_width(axis: &Axis, width: i64) -> i64 {
    let mut dist = abs(width);
    if axis.stems.count > 0 && abs(dist) < 40 {
        dist = 48;
    } else if dist < 54 {
        dist = wrap(dist + wrap(54 - dist) / 2);
    } else if dist < 3 * 64 {
        let fraction = dist & 63;
        let kept = match fraction {
            0..10 => fraction,
            10..22 => 10,
            22..42 => fraction,
            42..54 => 54,
            _ => fraction,
        };
        dist = (dist & !63) + kept;
    }
    if width < 0 { wrap(-dist) } else { dist }
}

/// The edges of one dimension and the axis metrics fitting them reads.
struct Fitter<'a> {
    edges: &'a mut [Edge],
    axis: &'a Axis,
}

impl Fitter<'_> {
    /// `af_cjk_align_linked_edge`: places edge `stem` at its fitted stem
    /// width from edge `base`.
    fn align_linked(&mut self, base: usize, stem: usize) {
        let (base_edge, stem_edge) = (self.edges[base], self.edges[stem]);
        let fitted = stem_width(self.axis, wrap(stem_edge.opos - base_edge.opos));
        self.edges[stem].pos = wrap(base_edge.pos + fitted);
    }

    /// `af_cjk_align_serif_edge`: moves edge `serif` as edge `base` moved.
    fn align_serif(&mut self, base: usize, serif: usize) {
        let base = self.edges[base];
        let serif = &mut self.edges[serif];
        serif.pos = wrap(base.pos + (serif.opos - base.opos));
    }

    /// `af_hint_normal_stem` for the default load: fits the stem between
    /// edges `e` and `link` at its fitted width, centred on its original
    /// centre moved by `anchor`, then shifts it as [`stem_shift`] says.
    /// Returns the shift.
    fn normal_stem(&mut self, e: usize, link: usize, anchor: i64) -> i64 {
        let (edge1, edge2) = (self.edges[e], self.edges[link]);
        let cur_len = stem_width(self.axis, wrap(edge2.opos - edge1.opos));
        let org_center = wrap(wrap(edge1.opos + edge2.opos) / 2 + anchor);
        let start = wrap(org_center - cur_len / 2);
        let delta = stem_shift(start, cur_len);
        let start = wrap(start + delta);
        let end = wrap(start + cur_len);
        if edge1.opos < edge2.opos {
            self.edges[e].pos = start;
            self.edges[link].pos = end;
        } else {
            self.edges[e].pos = end;
            self.edges[link].pos = start;
        }
        delta
    }
}

/// The shift `af_hint_normal_stem` gives a stem from `start` of length
/// `len`, with the default load's threshold of a whole pixel (the light
/// mode's smaller ones do not apply): none when an end is on the pixel
/// grid. A stem of up to a pixel across a pixel boundary moves wholly to
/// the side of it the stem covers more of (the upper one on a tie). A
/// longer stem whose length's fraction is under half a pixel and reaches
/// the start's distance up or the end's distance down stays; otherwise it
/// takes the smaller of two moves, each the smaller of a move down and one
/// up as FreeType pairs them, each putting one end on the grid, the first
/// on a tie.
fn stem_shift(start: i64, len: i64) -> i64 {
    let threshold = 64;
    let end = wrap(start + len);
    // The distances down to the pixel boundaries, and up from the start.
    let (d_off1, d_off2) = (start & 63, end & 63);
    let u_off1 = 64 - d_off1;
    if d_off1 == 0 || d_off2 == 0 {
        return 0;
    }
    if len <= threshold {
        return if d_off2 >= len {
            0
        } else if u_off1 <= d_off2 {
            u_off1
        } else {
            -d_off2
        };
    }
    let mut offset = len & 63;
    if offset < 32 {
        if u_off1 <= offset || d_off2 <= offset {
            return 0;
        }
    } else {
        offset = 64 - threshold;
    }
    let up1 = u_off1 - offset;
    let down1 = threshold - u_off1;
    let up2 = threshold - d_off2;
    let down2 = d_off2 - offset;
    let shift1 = if down1 <= up1 { -down1 } else { up1 };
    let shift2 = if down2 <= up2 { -down2 } else { up2 };
    if abs(shift1) <= abs(shift2) {
        shift1
    } else {
        shift2
    }
}

/// `af_cjk_hint_edges`: fits the edges of `dim`.
///
/// Edges in blue zones go to the zones' fitted positions first, with the
/// other edge of their stem at the stem's width. Then every stem is fitted
/// in order, centred as [`Fitter::normal_stem`] places it, each later
/// vertical stem moved as the first one was; a stem with an edge less than
/// a pixel after the last stem fitted is left, to keep the space between
/// them. A lowercase m's three vertical stems are kept evenly spaced. Last,
/// serifs follow their stem, and remaining edges are interpolated between
/// fitted ones, or follow the one fitted edge on their side.
fn hint_edges(
    hints: &mut GlyphHints,
    dim: Dimension,
    axis: &Axis,
    work: &mut WorkBudget,
) -> Result<()> {
    let axis_hints = &mut hints.axis[dim.index()];
    let count = axis_hints.edges.len();
    charge(work, count * count + 1)?;
    let mut fit = Fitter {
        edges: axis_hints.edges.as_mut_slice(),
        axis,
    };
    let done = |fitted: &Edge| fitted.flags & edge::DONE != 0;
    // FreeType's anchor only says whether an edge was fitted yet.
    let mut anchored = false;
    let mut delta = 0;
    // Edges left to the serif and interpolation passes (FreeType's `FT_Int`).
    let mut skipped = 0_i32;
    let mut last_stem: Option<i64> = None;

    for e in 0..count {
        if done(&fit.edges[e]) {
            continue;
        }
        let mut link = fit.edges[e].link;
        let (blued, blue) = if let Some(blue) = fit.edges[e].blue_edge {
            (e, blue)
        } else if let Some((l, blue)) = link.and_then(|l| fit.edges[l].blue_edge.map(|b| (l, b))) {
            // The other edge of the stem is in a zone.
            link = Some(e);
            (l, blue)
        } else {
            continue;
        };
        fit.edges[blued].pos = blue;
        fit.edges[blued].flags |= edge::DONE;
        if let Some(other) = link
            && fit.edges[other].blue_edge.is_none()
        {
            fit.align_linked(blued, other);
            fit.edges[other].flags |= edge::DONE;
        }
        anchored = true;
    }

    // The stems, in order.
    for e in 0..count {
        if done(&fit.edges[e]) {
            continue;
        }
        let Some(link) = fit.edges[e].link else {
            skipped += 1;
            continue;
        };
        // An edge too close to the last stem would merge with it: it is
        // interpolated below instead.
        if let Some(last) = last_stem {
            let limit = wrap(last + 64);
            if fit.edges[e].pos < limit || fit.edges[link].pos < limit {
                skipped += 1;
                continue;
            }
        }
        if fit.edges[link].blue_edge.is_some() {
            // FreeType does not expect this.
            fit.align_linked(link, e);
            fit.edges[e].flags |= edge::DONE;
            continue;
        }
        if link < e {
            fit.align_linked(link, e);
            fit.edges[e].flags |= edge::DONE;
            last_stem = Some(fit.edges[e].pos);
            continue;
        }
        if dim == Dimension::Horizontal && !anchored {
            delta = fit.normal_stem(e, link, 0);
        } else {
            fit.normal_stem(e, link, delta);
        }
        anchored = true;
        fit.edges[e].flags |= edge::DONE;
        fit.edges[link].flags |= edge::DONE;
        last_stem = Some(fit.edges[link].pos);
    }

    // A lowercase m: six vertical edges sans serif, twelve with. Its three
    // stems keep their even spacing if they had it.
    if dim == Dimension::Horizontal && (count == 6 || count == 12) {
        let (e1, e2, e3) = if count == 6 { (0, 2, 4) } else { (1, 5, 9) };
        let span = abs(wrap(
            (fit.edges[e2].opos - fit.edges[e1].opos) - (fit.edges[e3].opos - fit.edges[e2].opos),
        ));
        let stem = |e: usize| fit.edges[e].link == Some(e + 1);
        if stem(e1) && stem(e2) && stem(e3) && span < 8 {
            let delta = wrap(fit.edges[e3].pos - wrap(2 * fit.edges[e2].pos - fit.edges[e1].pos));
            let mut shift = |e: usize| fit.edges[e].pos = wrap(fit.edges[e].pos - delta);
            shift(e3);
            shift(e3 + 1);
            // The serifs move with the stem.
            if count == 12 {
                shift(8);
                shift(11);
            }
            fit.edges[e3].flags |= edge::DONE;
            fit.edges[e3 + 1].flags |= edge::DONE;
        }
    }
    if skipped == 0 {
        return Ok(());
    }

    // Serifs follow their base.
    for e in 0..count {
        if done(&fit.edges[e]) {
            continue;
        }
        if let Some(serif) = fit.edges[e].serif {
            fit.align_serif(serif, e);
            fit.edges[e].flags |= edge::DONE;
            skipped -= 1;
        }
    }
    if skipped == 0 {
        return Ok(());
    }

    // The other edges, between the fitted ones around them; these are not
    // marked fitted, so later ones look past them.
    for e in 0..count {
        if done(&fit.edges[e]) {
            continue;
        }
        let before = (0..e).rev().find(|&b| done(&fit.edges[b]));
        let after = (e + 1..count).find(|&a| done(&fit.edges[a]));
        match (before, after) {
            (None, None) => {}
            (None, Some(a)) => fit.align_serif(a, e),
            (Some(b), None) => fit.align_serif(b, e),
            (Some(b), Some(a)) => {
                let (b, a) = (fit.edges[b], fit.edges[a]);
                let fpos = i64::from(fit.edges[e].fpos);
                let (b_fpos, a_fpos) = (i64::from(b.fpos), i64::from(a.fpos));
                fit.edges[e].pos = if a_fpos == b_fpos {
                    b.pos
                } else {
                    wrap(b.pos + mul_div(fpos - b_fpos, a.pos - b.pos, a_fpos - b_fpos))
                };
            }
        }
    }
    Ok(())
}

/// `af_cjk_align_edge_points` without snapping: moves the points of each
/// segment of each edge by the edge's move and marks them touched. A point
/// two segments share moves twice, as in FreeType.
fn align_edge_points(hints: &mut GlyphHints, dim: Dimension, work: &mut WorkBudget) -> Result<()> {
    let axis = &hints.axis[dim.index()];
    let points = hints.points.as_mut_slice();
    for edge in axis.edges.as_slice() {
        let delta = wrap(edge.pos - edge.opos);
        let mut s = edge.first;
        loop {
            let segment = axis.segments[s];
            let mut p = segment.first;
            loop {
                charge(work, 1)?;
                let target = &mut points[p];
                match dim {
                    Dimension::Horizontal => {
                        target.x = wrap(target.x + delta);
                        target.flags |= point::TOUCH_X;
                    }
                    Dimension::Vertical => {
                        target.y = wrap(target.y + delta);
                        target.flags |= point::TOUCH_Y;
                    }
                }
                if p == segment.last {
                    break;
                }
                p = target.next;
            }
            s = segment.edge_next.unwrap_or(edge.first);
            if s == edge.first {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::super::hints::{Dimension, Edge, GlyphHints};
    use super::super::Axis;
    use super::{hint_edges, stem_shift, stem_width};
    use triergon_core::{Budget, WorkBudget};

    fn axis(with_widths: bool) -> Axis {
        let mut axis = Axis::EMPTY;
        axis.stems.count = usize::from(with_widths);
        axis
    }

    #[test]
    fn stems_are_quantized_lightly() {
        let standard = axis(true);
        // Within 40/64 pixel of the standard width's unscaled zero: 3/4
        // pixel, with the stem's sign.
        assert_eq!(stem_width(&standard, 30), 48);
        assert_eq!(stem_width(&standard, -30), -48);
        // Without standard widths, and from 40/64: halfway to 54/64.
        assert_eq!(stem_width(&axis(false), 30), 42);
        assert_eq!(stem_width(&standard, 45), 49);
        // Under three pixels the fraction is kept, or set to 10/64 or 54/64.
        assert_eq!(stem_width(&standard, 100), 100);
        assert_eq!(stem_width(&standard, 80), 74);
        assert_eq!(stem_width(&standard, 110), 118);
        assert_eq!(stem_width(&standard, 180), 182);
        assert_eq!(stem_width(&standard, 200), 200);
    }

    #[test]
    fn a_width_of_ft_long_min_wraps_as_on_32_bit_arm() {
        let width = i64::from(i32::MIN);
        // FT_ABS leaves FT_LONG_MIN negative: within 40 of zero.
        assert_eq!(stem_width(&axis(true), width), -48);
        // Under 54: 54 - FT_LONG_MIN wraps to 54 - 2^31, half of which
        // added to FT_LONG_MIN wraps to 2^30 + 27, then negated.
        assert_eq!(stem_width(&axis(false), width), -(1 << 30) - 27);
    }

    #[test]
    fn stems_shift_onto_the_pixel_grid() {
        // An end on the grid: no shift.
        assert_eq!(stem_shift(128, 100), 0);
        // Up to a pixel: within one pixel it stays, across a boundary it
        // moves to the side it covers more of.
        assert_eq!(stem_shift(70, 40), 0);
        assert_eq!(stem_shift(120, 20), 8);
        assert_eq!(stem_shift(110, 30), -12);
        assert_eq!(stem_shift(100, 60), 28);
        // A fraction under half a pixel reaching the start's distance up
        // keeps the stem where it is.
        assert_eq!(stem_shift(120, 140), 0);
        // Otherwise the smaller move: 16 up puts the end on 256.
        assert_eq!(stem_shift(100, 140), 16);
        // A fraction of half a pixel or more: the start up by 19, onto 128.
        assert_eq!(stem_shift(109, 182), 19);
    }

    #[test]
    fn stems_too_close_to_the_last_one_are_interpolated() {
        let budget = Budget::new(1 << 20).unwrap();
        let mut hints = GlyphHints::new(&budget);
        // Stems at 100..160, 200..260 and 400..460, and a single edge at 300
        // between them, in font units at a scale of one; each with the
        // index of the edge it links to.
        let edges: [(i16, Option<usize>); 7] = [
            (100, Some(1)),
            (160, Some(0)),
            (200, Some(3)),
            (260, Some(2)),
            (300, None),
            (400, Some(6)),
            (460, Some(5)),
        ];
        for (index, (at, link)) in edges.into_iter().enumerate() {
            let edge = Edge {
                fpos: at,
                opos: i64::from(at),
                pos: i64::from(at),
                link,
                first: index,
                last: index,
                ..Edge::default()
            };
            hints.axis[0].edges.try_push(edge).unwrap();
        }
        let mut work = WorkBudget::new(1 << 20);
        hint_edges(&mut hints, Dimension::Horizontal, &axis(true), &mut work).unwrap();
        let positions: Vec<i64> = hints.axis[0].edges.iter().map(|e| e.pos).collect();
        // The first stem keeps its 60/64 width and moves 28 up, across a
        // boundary, to end at 188. The second starts less than a pixel
        // after that and is left; the third, moved 28 as the first was,
        // then 20 more onto the grid, lands at 448..508. The edges between
        // are interpolated between 188 (at 160) and 448 (at 400).
        assert_eq!(positions, [128, 188, 231, 296, 340, 448, 508]);
    }
}
