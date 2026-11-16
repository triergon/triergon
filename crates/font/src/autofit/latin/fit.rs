//! Grid-fitting a glyph with the latin writing system, as FreeType's
//! default load (`FT_LOAD_TARGET_NORMAL`) runs it: stems are adjusted to
//! whole pixels, without snapping to standard widths and without the light,
//! monochrome and LCD modes' variations.

use super::super::hints::{Dimension, Edge, GlyphHints, Point, Segment, edge};
use super::super::style::NONBASE;
use super::super::tables::{SCRIPTS, STYLES};
use super::adjust;
use super::{Axis, Metrics, analysis};
use crate::long::{abs, mul_div, pix_round, wrap};
use crate::work::charge;
use triergon_core::{Result, WorkBudget};

/// How much of a glyph to hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in super::super) enum Scope {
    /// Both dimensions and every point: the hinted outline.
    Outline,
    /// The horizontal edges only, which are all the advance depends on.
    Advance,
}

/// What hinting needs to know of the glyph besides its outline.
#[derive(Clone, Copy, Debug)]
pub(in super::super) struct Glyph {
    /// Its entry in the style coverage, with its `NONBASE` bit.
    pub(in super::super) coverage: u16,
    /// Its flags in the adjustment database (`AF_ADJUST_*`), through the
    /// reverse charmap; zero for none.
    pub(in super::super) adjustment: u16,
    /// The size's pixels per em (`x_ppem`).
    pub(in super::super) ppem: u16,
    /// Whether the face is italic, which turns horizontal hinting off.
    pub(in super::super) italic: bool,
}

/// `af_latin_hints_apply` after `af_latin_hints_init`: hints the glyph
/// loaded into `hints` (scaled by the metrics' scales, `x_scale` and the x
/// height adjusted `y_scale`) with `metrics`, scaled.
///
/// Horizontally, unless the face is italic: vertical edges are found and
/// fitted. Vertically: accents are first prepared as the adjustment
/// database asks (tildes stretched and aligned, blue zones kept off
/// diacritics), horizontal edges are found and given their blue zones
/// (base characters only), then fitted. In each dimension the points then
/// follow the edges, strong points are interpolated between them and weak
/// points between those; vertically, diacritics are last kept a pixel away
/// from their base.
///
/// With [`Scope::Advance`] only the vertical edges are fitted, with each
/// point's coordinates set as the vertical analysis leaves them, which the
/// serif test of the horizontal fitting reads.
pub(in super::super) fn apply(
    hints: &mut GlyphHints,
    metrics: &Metrics,
    glyph: Glyph,
    scope: Scope,
    work: &mut WorkBudget,
) -> Result<()> {
    let script = &SCRIPTS[STYLES[metrics.style].script];
    let horizontal = !glyph.italic;
    if horizontal {
        analysis::detect_features(
            hints,
            Dimension::Horizontal,
            metrics,
            script.top_to_bottom,
            work,
        )?;
    }
    if scope == Scope::Advance {
        if horizontal {
            for p in hints.points.as_mut_slice() {
                (p.u, p.v) = (i64::from(p.fy), i64::from(p.fx));
            }
            hint_edges(
                hints,
                Dimension::Horizontal,
                metrics,
                glyph.ppem,
                script.top_to_bottom,
                work,
            )?;
        }
        return Ok(());
    }
    let accent_height_limit =
        adjust::prepare(hints, metrics, glyph.adjustment, script.top_to_bottom, work)?;
    if glyph.coverage & NONBASE == 0 {
        analysis::compute_blue_edges(hints, metrics, work)?;
    }
    for dim in Dimension::BOTH {
        if dim == Dimension::Horizontal && !horizontal {
            continue;
        }
        hint_edges(hints, dim, metrics, glyph.ppem, script.top_to_bottom, work)?;
        hints.align_edge_points(dim, work)?;
        hints.align_strong_points(dim, work)?;
        hints.align_weak_points(dim, work)?;
        if dim == Dimension::Vertical {
            adjust::separate_vertically(hints, glyph.adjustment, accent_height_limit, work)?;
        }
    }
    Ok(())
}

/// `af_latin_compute_stem_width` for the default load, with stem
/// adjustment and without snapping: the width of a stem from an edge with
/// flags `base_flags` moved by `base_delta`, lightly quantized. Widths of
/// extra light axes and of vertical serifs under three pixels stay;
/// others grow to 1 pixel (round edges under 1.25 pixel) or 7/8, snap to
/// the standard width within 5/8 pixel, keep small fractions under three
/// pixels, and round above, less part of the base edge's move at under 30
/// pixels per em. A width of `FT_LONG_MIN` stays negative when made
/// positive, as FreeType's does.
fn stem_width(
    axis: &Axis,
    dim: Dimension,
    ppem: u16,
    width: i64,
    base_delta: i64,
    base_flags: u8,
    stem_flags: u8,
) -> i64 {
    if axis.extra_light {
        return width;
    }
    let vertical = dim == Dimension::Vertical;
    let mut dist = abs(width);
    if !(stem_flags & edge::SERIF != 0 && vertical && dist < 3 * 64) {
        if base_flags & edge::ROUND != 0 {
            if dist < 80 {
                dist = 64;
            }
        } else if dist < 56 {
            dist = 56;
        }
        if let Some(standard) = axis.stems.widths().first() {
            if abs(dist - standard.cur) < 40 {
                dist = standard.cur.max(48);
            } else if dist < 3 * 64 {
                let fraction = dist & 63;
                let kept = if fraction < 10 {
                    fraction
                } else if fraction < 32 {
                    10
                } else if fraction < 54 {
                    54
                } else {
                    fraction
                };
                dist = wrap((dist & !63) + kept);
            } else {
                // Rounding both the stem's start and its length can move its
                // end far at small sizes: take back part of the start's move
                // when both go the same way.
                let mut bdelta = 0;
                if (width > 0 && base_delta > 0) || (width < 0 && base_delta < 0) {
                    let ppem = i64::from(ppem);
                    if ppem < 10 {
                        bdelta = base_delta;
                    } else if ppem < 30 {
                        bdelta = wrap(base_delta * (30 - ppem)) / 20;
                    }
                    bdelta = abs(bdelta);
                }
                dist = wrap((dist - bdelta + 32) & !63);
            }
        }
    }
    if width < 0 { wrap(-dist) } else { dist }
}

/// The edges of one dimension and what fitting them reads.
struct Fitter<'a> {
    edges: &'a mut [Edge],
    segments: &'a [Segment],
    points: &'a [Point],
    axis: &'a Axis,
    dim: Dimension,
    ppem: u16,
}

impl Fitter<'_> {
    /// `af_latin_align_linked_edge`: places edge `stem` at its fitted stem
    /// width from edge `base`.
    fn align_linked(&mut self, base: usize, stem: usize) {
        let (base_edge, stem_edge) = (self.edges[base], self.edges[stem]);
        let dist = wrap(stem_edge.opos - base_edge.opos);
        let base_delta = wrap(base_edge.pos - base_edge.opos);
        let fitted = stem_width(
            self.axis,
            self.dim,
            self.ppem,
            dist,
            base_delta,
            base_edge.flags,
            stem_edge.flags,
        );
        self.edges[stem].pos = wrap(base_edge.pos + fitted);
    }

    /// `af_latin_compute_stem_width` between two edges, from nowhere.
    fn width(&self, from: usize, to: usize) -> i64 {
        let (a, b) = (self.edges[from], self.edges[to]);
        stem_width(
            self.axis,
            self.dim,
            self.ppem,
            wrap(b.opos - a.opos),
            0,
            a.flags,
            b.flags,
        )
    }

    /// The extent along the edge of `e`'s first and last segments: the `v`
    /// of their end points, as the points hold them now.
    fn extent(&self, e: usize) -> (i64, i64) {
        let edge = self.edges[e];
        let ends = [self.segments[edge.first], self.segments[edge.last]];
        ends.iter()
            .flat_map(|s| [self.points[s.first].v, self.points[s.last].v])
            .fold((i64::MAX, i64::MIN), |(min, max), v| {
                (min.min(v), max.max(v))
            })
    }
}

/// `af_latin_hint_edges`: fits the edges of `dim`.
///
/// Horizontal edges in blue zones go to the zones' fitted positions first,
/// with the other edge of their stem at the stem's width. Then every stem
/// is fitted in order: the first one centred on the pixel grid (stems under
/// 1.5 pixels between half pixels), later ones keeping their distance to it
/// rounded, and none crossing the stem before it unless that would almost
/// close it. A lowercase m's three vertical stems are kept evenly spaced.
/// Last, serifs follow their stem, and remaining edges are interpolated
/// between fitted ones or rounded from the first.
fn hint_edges(
    hints: &mut GlyphHints,
    dim: Dimension,
    metrics: &Metrics,
    ppem: u16,
    top_to_bottom: bool,
    work: &mut WorkBudget,
) -> Result<()> {
    let axis_hints = &mut hints.axis[dim.index()];
    let count = axis_hints.edges.len();
    charge(work, count * count + 1)?;
    let mut fit = Fitter {
        edges: axis_hints.edges.as_mut_slice(),
        segments: axis_hints.segments.as_slice(),
        points: hints.points.as_slice(),
        axis: &metrics.axis[dim.index()],
        dim,
        ppem,
    };
    let top_to_bottom = dim == Dimension::Vertical && top_to_bottom;
    // Whether `pos` lies before the position `limit` in the edges' order.
    let before = |pos: i64, limit: i64| {
        if top_to_bottom {
            pos > limit
        } else {
            pos < limit
        }
    };
    let mut anchor: Option<usize> = None;
    let mut has_non_stem_edges = false;

    if dim == Dimension::Vertical {
        for e in 0..count {
            if fit.edges[e].flags & edge::DONE != 0 {
                continue;
            }
            let mut link = fit.edges[e].link;
            // A stem with a neutral and another blue zone keeps the other;
            // with two neutral ones, the first.
            if let Some(l) = link
                && fit.edges[e].blue_edge.is_some()
                && fit.edges[l].blue_edge.is_some()
            {
                if fit.edges[l].flags & edge::NEUTRAL != 0 {
                    fit.edges[l].blue_edge = None;
                    fit.edges[l].flags &= !edge::NEUTRAL;
                } else if fit.edges[e].flags & edge::NEUTRAL != 0 {
                    fit.edges[e].blue_edge = None;
                    fit.edges[e].flags &= !edge::NEUTRAL;
                }
            }
            let (blued, blue) = if let Some(blue) = fit.edges[e].blue_edge {
                (e, blue)
            } else if let Some((l, blue)) =
                link.and_then(|l| fit.edges[l].blue_edge.map(|b| (l, b)))
            {
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
            if anchor.is_none() {
                anchor = Some(e);
            }
        }
    }

    // The other stems, in order.
    for e in 0..count {
        if fit.edges[e].flags & edge::DONE != 0 {
            continue;
        }
        let Some(link) = fit.edges[e].link else {
            has_non_stem_edges = true;
            continue;
        };
        if fit.edges[link].blue_edge.is_some() {
            // FreeType does not expect this.
            fit.align_linked(link, e);
            fit.edges[e].flags |= edge::DONE;
            continue;
        }
        let (edge, other) = (fit.edges[e], fit.edges[link]);
        let org_len = wrap(other.opos - edge.opos);
        let cur_len = fit.width(e, link);
        // Stems up to a pixel wide centre between pixels; up to 1.5 pixels,
        // off centre.
        let (u_off, d_off) = if cur_len <= 64 { (32, 32) } else { (38, 26) };
        let centred = |org_center: i64| {
            let pos = pix_round(org_center);
            let error1 = abs(org_center - (pos - u_off));
            let error2 = abs(org_center - (pos + d_off));
            wrap(if error1 < error2 {
                pos - u_off
            } else {
                pos + d_off
            })
        };
        let Some(a) = anchor else {
            // The first stem.
            if cur_len < 96 {
                let center = centred(wrap(edge.opos + (org_len >> 1)));
                fit.edges[e].pos = wrap(center - cur_len / 2);
                fit.edges[link].pos = wrap(fit.edges[e].pos + cur_len);
            } else {
                fit.edges[e].pos = pix_round(edge.opos);
            }
            anchor = Some(e);
            fit.edges[e].flags |= edge::DONE;
            fit.align_linked(e, link);
            continue;
        };
        let anchor_edge = fit.edges[a];
        let org_pos = wrap(anchor_edge.pos + (edge.opos - anchor_edge.opos));
        let org_center = wrap(org_pos + (org_len >> 1));
        if other.flags & edge::DONE != 0 {
            fit.edges[e].pos = wrap(other.pos - cur_len);
        } else if cur_len < 96 {
            let center = centred(org_center);
            fit.edges[e].pos = wrap(center - cur_len / 2);
            fit.edges[link].pos = wrap(center + cur_len / 2);
        } else {
            // Round the nearer end, keeping the width.
            let pos1 = pix_round(org_pos);
            let delta1 = abs(pos1 + (cur_len >> 1) - org_center);
            let pos2 = wrap(pix_round(org_pos + org_len) - cur_len);
            let delta2 = abs(pos2 + (cur_len >> 1) - org_center);
            fit.edges[e].pos = if delta1 < delta2 { pos1 } else { pos2 };
            fit.edges[link].pos = wrap(fit.edges[e].pos + cur_len);
        }
        fit.edges[e].flags |= edge::DONE;
        fit.edges[link].flags |= edge::DONE;
        if e > 0 && before(fit.edges[e].pos, fit.edges[e - 1].pos) {
            // Unless the stem would (almost, by 1/4 pixel) disappear.
            let previous = fit.edges[e - 1].pos;
            if abs(fit.edges[link].pos - previous) > 16 {
                fit.edges[e].pos = previous;
            }
        }
    }

    // A lowercase m: six vertical edges sans serif, twelve with. Its three
    // stems keep their even spacing if they had it.
    if dim == Dimension::Horizontal && (count == 6 || count == 12) {
        let (e1, e2, e3) = if count == 6 { (0, 2, 4) } else { (1, 5, 9) };
        let dist1 = fit.edges[e2].opos - fit.edges[e1].opos;
        let dist2 = fit.edges[e3].opos - fit.edges[e2].opos;
        if abs(dist1 - dist2) < 8 {
            let delta = fit.edges[e3].pos - (2 * fit.edges[e2].pos - fit.edges[e1].pos);
            let shift = |edge: &mut Edge| edge.pos = wrap(edge.pos - delta);
            shift(&mut fit.edges[e3]);
            if let Some(link) = fit.edges[e3].link {
                shift(&mut fit.edges[link]);
            }
            // The serifs move with the stem.
            if count == 12 {
                shift(&mut fit.edges[8]);
                shift(&mut fit.edges[11]);
            }
            fit.edges[e3].flags |= edge::DONE;
            if let Some(link) = fit.edges[e3].link {
                fit.edges[link].flags |= edge::DONE;
            }
        }
    }

    if has_non_stem_edges || anchor.is_none() {
        // Serifs and single edges. None of them has a link, so FreeType's
        // checks against crossing a neighbour never apply here.
        for e in 0..count {
            if fit.edges[e].flags & edge::DONE != 0 {
                continue;
            }
            let mut delta = 1000;
            if let Some(serif) = fit.edges[e].serif {
                delta = wrap(fit.edges[serif].opos - fit.edges[e].opos);
                let (top, bottom) = if delta < 0 {
                    delta = wrap(-delta);
                    (e, serif)
                } else {
                    (serif, e)
                };
                if delta < 64 + 32 {
                    // Not a real serif if an edge between the two overlaps
                    // them, such as an accent near its base.
                    let (top_min, top_max) = fit.extent(top);
                    let (bottom_min, bottom_max) = fit.extent(bottom);
                    let (min_pos, max_pos) = (top_min.min(bottom_min), top_max.max(bottom_max));
                    let overlapped = (bottom + 1..top).any(|between| {
                        let (e_min, e_max) = fit.extent(between);
                        !((e_min < min_pos && e_max < min_pos)
                            || (e_min > max_pos && e_max > max_pos))
                    });
                    if overlapped {
                        continue;
                    }
                }
            }
            let opos = fit.edges[e].opos;
            if delta < 64 + 16 {
                // `af_latin_align_serif_edge`: follow the serif's base.
                let base = fit.edges[fit.edges[e].serif.unwrap_or(e)];
                fit.edges[e].pos = wrap(base.pos + (opos - base.opos));
            } else if let Some(a) = anchor {
                let done = |i: &usize| fit.edges[*i].flags & edge::DONE != 0;
                let previous = (0..e).rev().find(done);
                let next = (e + 1..count).find(done);
                fit.edges[e].pos = wrap(if let (Some(b), Some(n)) = (previous, next) {
                    // Between the fitted edges around it.
                    let (b, n) = (fit.edges[b], fit.edges[n]);
                    if n.opos == b.opos {
                        b.pos
                    } else {
                        b.pos + mul_div(opos - b.opos, n.pos - b.pos, n.opos - b.opos)
                    }
                } else {
                    // From the anchor, in half pixels.
                    let anchor = fit.edges[a];
                    anchor.pos + ((opos - anchor.opos + 16) & !31)
                });
            } else {
                fit.edges[e].pos = pix_round(opos);
                anchor = Some(e);
            }
            fit.edges[e].flags |= edge::DONE;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::super::hints::{Dimension, Width, edge};
    use super::super::Axis;
    use super::stem_width;

    fn axis(standard: Option<i64>, extra_light: bool) -> Axis {
        let mut axis = Axis::EMPTY;
        if let Some(cur) = standard {
            axis.stems.widths[0] = Width {
                org: 0,
                cur,
                fit: cur,
            };
            axis.stems.count = 1;
        }
        axis.extra_light = extra_light;
        axis
    }

    #[test]
    fn stems_are_quantized_lightly() {
        let h = Dimension::Horizontal;
        let standard = axis(Some(100), false);
        // Near the standard width: snapped to it.
        assert_eq!(stem_width(&standard, h, 20, 120, 0, 0, 0), 100);
        assert_eq!(stem_width(&standard, h, 20, -120, 0, 0, 0), -100);
        // Under three pixels: whole pixels plus small fractions.
        assert_eq!(stem_width(&standard, h, 20, 150, 0, 0, 0), 128 + 10);
        assert_eq!(stem_width(&standard, h, 20, 170, 0, 0, 0), 128 + 54);
        // Above: rounded, less part of the base edge's move at 20 ppem.
        assert_eq!(
            stem_width(&standard, h, 20, 300, 40, 0, 0),
            (300 - 20 + 32) & !63
        );
        // Without standard widths only the minimum applies.
        assert_eq!(
            stem_width(&axis(None, false), h, 20, 30, 0, edge::ROUND, 0),
            64
        );
        assert_eq!(stem_width(&axis(None, false), h, 20, 30, 0, 0, 0), 56);
        // Extra light axes and short vertical serifs keep their widths.
        assert_eq!(stem_width(&axis(Some(100), true), h, 20, 30, 0, 0, 0), 30);
        let v = Dimension::Vertical;
        assert_eq!(stem_width(&standard, v, 20, 30, 0, 0, edge::SERIF), 30);
    }

    #[test]
    fn a_width_of_ft_long_min_stays_negative() {
        // FT_ABS leaves FT_LONG_MIN negative, under the 7/8 pixel minimum,
        // which then applies: -56, where 64-bit arithmetic keeps the width.
        let width = i64::from(i32::MIN);
        let h = Dimension::Horizontal;
        assert_eq!(stem_width(&axis(None, false), h, 20, width, 0, 0, 0), -56);
    }
}
