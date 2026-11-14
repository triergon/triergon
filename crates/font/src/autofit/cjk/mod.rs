//! The CJK writing system (`afcjk.c`), which hints CJK characters and every
//! glyph no script covers: the style's global metrics (standard stem
//! widths, blue zones and whether the digits share one advance), measured
//! on the face once and scaled for each size, and the hinting of one glyph
//! with them in [`apply`]. Its stem widths, digits and segments are the
//! latin writing system's.

mod analysis;
mod fit;

pub(super) use fit::apply;

use super::Face;
use super::hints::{Dimension, Width};
use super::latin::{self, Stems};
use super::style::{MAX_BLUES, MAX_CLUSTERS, clusters};
use super::tables::STYLES;
use crate::long::{abs, div_fix, mul_fix, pix_round, wrap};
use crate::outline::{Outline, OutlinePoint};
use triergon_core::{Result, WorkBudget};

/// The properties of a blue string (`AF_BLUE_PROPERTY_CJK_*`).
mod property {
    /// A top zone, or with [`HORIZ`] a right one.
    pub(super) const TOP: u8 = 1 << 0;
    /// A zone of x coordinates.
    pub(super) const HORIZ: u8 = 1 << 1;
}

/// The flags of a blue zone (`AF_CJK_BLUE_*`).
mod blue {
    /// Less than 3/4 pixel tall at the current size.
    pub(super) const ACTIVE: u8 = 1 << 0;
    /// A top (or right) zone.
    pub(super) const TOP: u8 = 1 << 1;
}

/// A blue zone (`AF_CJKBlueRec`): its reference and overshoot positions.
/// The overshoot of a CJK zone is inside it: below a top zone's reference.
#[derive(Clone, Copy, Debug, Default)]
struct Blue {
    reference: Width,
    shoot: Width,
    flags: u8,
}

/// One dimension's metrics (`AF_CJKAxisRec`): standard widths and blue
/// zones. The widths stay in font units: FreeType never scales them for
/// this writing system.
#[derive(Clone, Copy, Debug)]
struct Axis {
    scale: i64,
    stems: Stems,
    blue_count: usize,
    blues: [Blue; MAX_BLUES],
}

impl Axis {
    const EMPTY: Self = Self {
        scale: 0,
        stems: Stems::EMPTY,
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
            flags: 0,
        }; MAX_BLUES],
    };

    fn blues(&self) -> &[Blue] {
        &self.blues[..self.blue_count]
    }
}

/// The global metrics of a style of the CJK writing system
/// (`AF_CJKMetricsRec`), in font units as measured on the face, or scaled
/// to a size by [`Self::scaled`].
#[derive(Clone, Debug)]
pub(crate) struct Metrics {
    units_per_em: u16,
    axis: [Axis; 2],
    digits_have_same_width: bool,
}

impl Metrics {
    /// `af_cjk_metrics_init`: the style's metrics as measured on the face:
    /// its stem widths and digits as the latin writing system measures
    /// them, and its blue zones.
    ///
    /// Without a Unicode charmap nothing is measured, and the metrics stay
    /// empty.
    pub(super) fn new(face: &Face<'_>, style: usize, work: &mut WorkBudget) -> Result<Self> {
        let mut metrics = Self {
            units_per_em: face.font.units_per_em(),
            axis: [Axis::EMPTY; 2],
            digits_have_same_width: false,
        };
        if face.charmap.is_none() {
            return Ok(metrics);
        }
        let mut outline = Outline::new(face.budget);
        // `af_cjk_metrics_init_widths`.
        let stems = Stems::measure(face, style, &mut outline, work)?;
        for (axis, stems) in metrics.axis.iter_mut().zip(stems) {
            axis.stems = stems;
        }
        metrics.init_blues(face, style, &mut outline, work)?;
        // `af_cjk_metrics_check_digits`.
        metrics.digits_have_same_width = latin::digits_have_same_width(face);
        Ok(metrics)
    }

    /// `af_cjk_metrics_init_blues`: one blue zone per blue string of the
    /// style that finds a glyph, of x coordinates for a horizontal string
    /// and of y otherwise. Each glyph gives its extremum: the highest (or
    /// rightmost) point for a top (or right) zone, the lowest (or leftmost)
    /// otherwise. The median extremum of the characters before the string's
    /// `|` (FreeType's fill values) is the zone's reference and that of the
    /// characters after it (the flat values) its overshoot; with only one
    /// kind both are its median, and an overshoot outside its reference
    /// makes both their mean.
    fn init_blues(
        &mut self,
        face: &Face<'_>,
        style: usize,
        outline: &mut Outline,
        work: &mut WorkBudget,
    ) -> Result<()> {
        for string in STYLES[style].blues {
            let horizontal = string.properties & property::HORIZ != 0;
            let top = string.properties & property::TOP != 0;
            let (mut fills, mut flats) = ([0_i64; MAX_CLUSTERS], [0_i64; MAX_CLUSTERS]);
            let (mut fill_count, mut flat_count) = (0, 0);
            let mut fill = true;
            for c in clusters(string.text) {
                if c == Some('|') {
                    fill = false;
                    continue;
                }
                // Clusters of several characters are unavailable.
                let Some(glyph) = c.and_then(|c| face.glyph(c)) else {
                    continue;
                };
                if !face.load_units(glyph, outline, work)? || outline.points().len() <= 2 {
                    continue;
                }
                let best = extremum(outline.points(), outline.contour_ends(), horizontal, top);
                let (list, count) = if fill {
                    (&mut fills, &mut fill_count)
                } else {
                    (&mut flats, &mut flat_count)
                };
                if *count < MAX_CLUSTERS {
                    list[*count] = best;
                    *count += 1;
                }
            }
            if fill_count == 0 && flat_count == 0 {
                continue;
            }
            // `af_sort_pos`; equal positions are indistinguishable.
            fills[..fill_count].sort_unstable();
            flats[..flat_count].sort_unstable();
            let (mut reference, mut shoot) = match (fill_count, flat_count) {
                (_, 0) => (fills[fill_count / 2], fills[fill_count / 2]),
                (0, _) => (flats[flat_count / 2], flats[flat_count / 2]),
                _ => (fills[fill_count / 2], flats[flat_count / 2]),
            };
            // An overshoot on the wrong side of its reference: the mean.
            if shoot != reference && top != (shoot < reference) {
                reference = wrap(shoot + reference) / 2;
                shoot = reference;
            }
            let dim = if horizontal {
                Dimension::Horizontal
            } else {
                Dimension::Vertical
            };
            let axis = &mut self.axis[dim.index()];
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
                    flags: if top { blue::TOP } else { 0 },
                };
                axis.blue_count += 1;
            }
        }
        Ok(())
    }

    /// `af_cjk_metrics_scale`: the metrics at 16.16 scales `x_scale` and
    /// `y_scale` (font units to 26.6 pixels), in the 32 bits of an
    /// `FT_Fixed`, which this writing system hints with unadjusted.
    pub(super) fn scaled(&self, x_scale: i64, y_scale: i64) -> Self {
        let mut scaled = self.clone();
        scaled.scale_dim(Dimension::Horizontal, wrap(x_scale));
        scaled.scale_dim(Dimension::Vertical, wrap(y_scale));
        scaled
    }

    /// `af_cjk_metrics_scale_dim`, without the deltas the default load
    /// leaves at zero: blue zones are scaled, and a zone under 3/4 pixel
    /// tall is active, with its reference rounded and its overshoot the
    /// rounded distance from it (none under half a pixel).
    ///
    /// FreeType skips the scaling when the scale is the one it last scaled
    /// the style's metrics to, which only matters for a scale of zero
    /// before any other.
    fn scale_dim(&mut self, dim: Dimension, scale: i64) {
        let axis = &mut self.axis[dim.index()];
        axis.scale = scale;
        for zone in &mut axis.blues[..axis.blue_count] {
            zone.reference.cur = mul_fix(zone.reference.org, scale);
            zone.reference.fit = zone.reference.cur;
            zone.shoot.cur = mul_fix(zone.shoot.org, scale);
            zone.shoot.fit = zone.shoot.cur;
            zone.flags &= !blue::ACTIVE;
            let distance = mul_fix(zone.reference.org - zone.shoot.org, scale);
            if (-48..=48).contains(&distance) {
                zone.reference.fit = pix_round(zone.reference.cur);
                let delta1 = wrap(div_fix(zone.reference.fit, scale) - zone.shoot.org);
                let mut delta2 = mul_fix(abs(delta1), scale);
                delta2 = if delta2 < 32 { 0 } else { pix_round(delta2) };
                if delta1 < 0 {
                    delta2 = wrap(-delta2);
                }
                zone.shoot.fit = wrap(zone.reference.fit - delta2);
                zone.flags |= blue::ACTIVE;
            }
        }
    }

    /// The 16.16 horizontal scale of the scaled metrics.
    pub(super) fn x_scale(&self) -> i64 {
        self.axis[Dimension::Horizontal.index()].scale
    }

    /// The 16.16 vertical scale of the scaled metrics.
    pub(super) fn y_scale(&self) -> i64 {
        self.axis[Dimension::Vertical.index()].scale
    }

    pub(super) const fn digits_have_same_width(&self) -> bool {
        self.digits_have_same_width
    }
}

/// The extremum one glyph gives a blue zone, part of
/// `af_cjk_metrics_init_blues`: the largest (for a top or right zone) or
/// smallest x (for a horizontal zone) or y of the points of its contours of
/// two or more points, or zero without such a contour.
fn extremum(points: &[OutlinePoint], ends: &[u32], horizontal: bool, top: bool) -> i64 {
    let coordinate = |p: &OutlinePoint| i64::from(if horizontal { p.point.x } else { p.point.y });
    let mut best: Option<i64> = None;
    let mut first = 0;
    for &end in ends {
        let contour = points.get(first..end as usize).unwrap_or_default();
        first = end as usize;
        // Single points are never drawn; in some fonts they are mark
        // anchors far outside the glyph.
        if contour.len() < 2 {
            continue;
        }
        for v in contour.iter().map(coordinate) {
            let better = best.is_none_or(|b| if top { v > b } else { v < b });
            if better {
                best = Some(v);
            }
        }
    }
    best.unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::super::hints::Dimension;
    use super::{Axis, Blue, Metrics, blue, extremum};
    use crate::outline::{OutlinePoint, Point, PointKind};

    fn points(coordinates: &[(i32, i32)]) -> Vec<OutlinePoint> {
        coordinates
            .iter()
            .map(|&(x, y)| OutlinePoint {
                point: Point::new(x, y),
                kind: PointKind::On,
            })
            .collect()
    }

    #[test]
    fn extrema_skip_single_points() {
        // A one-point contour above a triangle, as a mark anchor would be.
        let glyph = points(&[(50, 900), (0, 0), (100, 0), (60, 700)]);
        let ends = [1, 4];
        assert_eq!(extremum(&glyph, &ends, false, true), 700);
        assert_eq!(extremum(&glyph, &ends, false, false), 0);
        assert_eq!(extremum(&glyph, &ends, true, true), 100);
        // Without a contour of two points FreeType keeps its initial zero.
        assert_eq!(extremum(&glyph[..1], &ends[..1], false, true), 0);
    }

    #[test]
    fn zones_under_three_quarters_of_a_pixel_are_fitted() {
        let mut axis = Axis::EMPTY;
        // A top zone with its overshoot 10 units inside its reference at
        // 830 units, and a bottom zone 40 units tall.
        axis.blues[0] = Blue {
            flags: blue::TOP,
            ..Blue::default()
        };
        axis.blues[0].reference.org = 830;
        axis.blues[0].shoot.org = 820;
        axis.blues[1].reference.org = -80;
        axis.blues[1].shoot.org = -40;
        axis.blue_count = 2;
        let metrics = Metrics {
            units_per_em: 1000,
            axis: [Axis::EMPTY, axis],
            digits_have_same_width: false,
        };
        // 0.8 26.6 units per font unit (12.5 pixels per em at 1000).
        let scaled = metrics.scaled(0xCCCC, 0xCCCC);
        let zones = scaled.axis[Dimension::Vertical.index()].blues();
        // 830 units are 664 26.6 units: the reference rounds to 640.
        // FT_DivFix(640, 0xCCCC) is 800 units, 20 under the overshoot, 16
        // 26.6 units: under half a pixel, so the overshoot joins it.
        assert_eq!(zones[0].reference.cur, 664);
        assert_eq!((zones[0].reference.fit, zones[0].shoot.fit), (640, 640));
        assert_ne!(zones[0].flags & blue::ACTIVE, 0);
        // The bottom zone is 32 26.6 units tall, FT_MulFix rounding -31.99
        // away from zero: active. Its reference, at -64, is 40 units under
        // the overshoot, 32 26.6 units, which round to a pixel above it.
        assert_eq!((zones[1].reference.fit, zones[1].shoot.fit), (-64, 0));
        // At 1.6 26.6 units per font unit the bottom zone is a pixel tall
        // and inactive, keeping its scaled positions.
        let scaled = metrics.scaled(0x1_9999, 0x1_9999);
        let zones = scaled.axis[Dimension::Vertical.index()].blues();
        assert_eq!(zones[1].flags & blue::ACTIVE, 0);
        assert_eq!((zones[1].reference.fit, zones[1].shoot.fit), (-128, -64));
    }
}
