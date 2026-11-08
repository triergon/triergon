//! The ASS drawing language (`\p`, vector `\clip`) as libass's
//! `ass_drawing.c` reads it into outlines.
//!
//! Coordinates are 26.6 fixed point (1/64 script pixel, `lrint(v * 64)`),
//! with y growing downwards as on screen. Outlines are unscaled: the `\p`
//! level divides them by [`drawing_scale`] when they are placed.

use triergon_core::{Result, WorkBudget};
use triergon_raster::{Outline, Rect, SegmentKind, Vector, pixels_26_6};

use crate::number::strtod;
use crate::{Limits, charge};

/// The factor libass applies to a drawing at `\p` level `level`:
/// 1 / 2^(level - 1), with the shift wrapping as in libass, and 0 when the
/// shifted value is not positive.
pub fn drawing_scale(level: i32) -> f64 {
    let base = (1_u32 << (level.wrapping_sub(1) & 31)) as i32;
    if base > 0 { 1.0 / f64::from(base) } else { 0.0 }
}

/// The token kinds of libass's drawing tokenizer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    Move,
    MoveNoClose,
    Line,
    Cubic,
    BSpline,
    ExtendSpline,
}

/// A parsed drawing: its outline and bounding box.
#[derive(Debug)]
pub struct Drawing<'o> {
    /// Lines and cubic splines in closed contours, 26.6, y down.
    pub outline: &'o Outline,
    /// The box of every point named, control points and B-spline points
    /// included; [`Rect::EMPTY`] for a drawing without points. libass sizes a
    /// drawing glyph by it.
    pub cbox: Rect,
}

impl Drawing<'_> {
    /// Parses drawing commands into `outline` (cleared first), as libass's
    /// `ass_drawing_parse` does: `m` moves (closing the open contour), `n`
    /// moves without closing, `l` lines, `b` cubic Béziers, `s` uniform cubic
    /// B-splines, `p` extends them and `c` closes them; other bytes are
    /// skipped. Commands before the first valid `m` are ignored, and an `n`
    /// first rejects the whole drawing (an empty outline).
    ///
    /// Returns `None` when a point lies beyond libass's coordinate bound
    /// ([`OUTLINE_MAX`](triergon_raster::OUTLINE_MAX)), for which libass
    /// discards the drawing; the outline is then empty.
    ///
    /// # Errors
    ///
    /// `Policy` past the work bound of `limits` (a unit per command and
    /// point); `Limit` or `Backpressure` when the outline's budget is
    /// exhausted.
    pub fn parse<'o>(
        text: &[u8],
        outline: &'o mut Outline,
        limits: &Limits,
    ) -> Result<Option<Drawing<'o>>> {
        outline.clear();
        let mut work = limits.work();
        let mut builder = Builder {
            outline,
            cbox: Rect::EMPTY,
            pen: Vector::default(),
            start: None,
            recent: [Vector::default(); 4],
            group: 0,
            valid: true,
        };
        tokenize(text, &mut work, &mut |token, point| {
            builder.token(token, point)
        })?;
        builder.close()?;
        let Builder {
            outline,
            cbox,
            valid,
            ..
        } = builder;
        if !valid {
            outline.clear();
            return Ok(None);
        }
        Ok(Some(Drawing { outline, cbox }))
    }
}

/// libass's `drawing_tokenize`, emitting tokens as it finds them.
fn tokenize(
    text: &[u8],
    work: &mut WorkBudget,
    emit: &mut dyn FnMut(Token, Vector) -> Result<()>,
) -> Result<()> {
    let mut t = Tokenizer {
        text,
        at: 0,
        last: Vector::default(),
        emit,
    };
    let mut have_root = false;
    let mut m_seen = false;
    let mut tokens = 0_usize; // libass's `points`, which `c` does not count
    // The current point when `s` started and the spline's first two points.
    let mut spline: Option<[Vector; 3]> = None;
    while let Some(&command) = text.get(t.at) {
        t.at += 1;
        charge(work, 1)?;
        match command {
            b'm' | b'n' => {
                let token = if command == b'm' {
                    m_seen = true;
                    Token::Move
                } else {
                    Token::MoveNoClose
                };
                if !have_root {
                    let Some(p) = t.point() else {
                        continue;
                    };
                    if !m_seen {
                        return Ok(());
                    }
                    t.emit(token, p)?;
                    have_root = true;
                    tokens = 1;
                }
                tokens += t.many(token, 1, work)?;
            }
            b'l' if have_root => tokens += t.many(Token::Line, 1, work)?,
            b'b' if have_root => tokens += t.many(Token::Cubic, 3, work)?,
            b's' if have_root => {
                let start = t.last;
                // Like libass, stop reading at the first point that fails.
                let Some([a, b, c]) = (|| Some([t.point()?, t.point()?, t.point()?]))() else {
                    spline = None;
                    continue;
                };
                for p in [a, b, c] {
                    t.emit(Token::BSpline, p)?;
                }
                spline = Some([start, a, b]);
                tokens += 3 + t.many(Token::ExtendSpline, 1, work)?;
            }
            b'p' if tokens >= 3 => tokens += t.many(Token::ExtendSpline, 1, work)?,
            b'c' => {
                for p in spline.take().into_iter().flatten() {
                    t.emit(Token::ExtendSpline, p)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

struct Tokenizer<'t, 'e> {
    text: &'t [u8],
    at: usize,
    /// The point of the last token.
    last: Vector,
    emit: &'e mut dyn FnMut(Token, Vector) -> Result<()>,
}

impl Tokenizer<'_, '_> {
    /// libass's `get_point`: two numbers, each consumed if it converts.
    fn point(&mut self) -> Option<Vector> {
        let x = self.coordinate()?;
        let y = self.coordinate()?;
        Some(Vector::new(x, y))
    }

    fn coordinate(&mut self) -> Option<i32> {
        let (value, used) = strtod(&self.text[self.at..]);
        self.at += used;
        // libass's `double_to_d6` narrows `lrint` to `int`; out of `int`'s
        // range C leaves the result to the target, and libass on 32-bit ARM
        // with GCC saturates, which puts the point out of bounds.
        (used != 0).then(|| pixels_26_6(value))
    }

    fn emit(&mut self, token: Token, p: Vector) -> Result<()> {
        self.last = p;
        (self.emit)(token, p)
    }

    /// Points while they convert, emitted in complete groups of `batch`; a
    /// final partial group is consumed and dropped.
    fn many(&mut self, token: Token, batch: usize, work: &mut WorkBudget) -> Result<usize> {
        let mut group = [Vector::default(); 3];
        let (mut filled, mut added) = (0, 0);
        while self.at < self.text.len() {
            let Some(p) = self.point() else { break };
            charge(work, 1)?;
            group[filled] = p;
            filled += 1;
            if filled == batch {
                for &p in &group[..batch] {
                    self.emit(token, p)?;
                }
                added += batch;
                filled = 0;
            }
        }
        Ok(added)
    }
}

/// libass's conversion of the token list into an outline, fed one token at a
/// time; curves look back at the most recent points.
///
/// libass adds each point as it comes and tags a segment onto the point that
/// ends it; here a segment is pushed with the points it owns once its end is
/// known, so `start` holds the last point added, which starts the next
/// segment. The result is the same outline, and a point beyond the bound
/// makes the drawing invalid either way.
struct Builder<'o> {
    outline: &'o mut Outline,
    cbox: Rect,
    pen: Vector,
    /// The open contour's current point, `None` before a contour starts
    /// (libass's `started`).
    start: Option<Vector>,
    /// The last four token points, the newest last.
    recent: [Vector; 4],
    /// Points seen of the current `b` or `s` group of three.
    group: u8,
    valid: bool,
}

impl Builder<'_> {
    fn token(&mut self, token: Token, p: Vector) -> Result<()> {
        self.recent.rotate_left(1);
        self.recent[3] = p;
        if !self.valid {
            return Ok(());
        }
        match token {
            Token::MoveNoClose | Token::Move => {
                self.pen = p;
                self.cbox.include_point(p);
                if token == Token::Move {
                    self.close()?;
                }
            }
            Token::Line => {
                self.cbox.include_point(p);
                let start = self.start.unwrap_or(self.pen);
                self.push(SegmentKind::Line, &[start], p)?;
            }
            Token::Cubic | Token::BSpline => {
                self.group += 1;
                if self.group == 3 {
                    self.group = 0;
                    self.curve(token == Token::BSpline)?;
                }
            }
            Token::ExtendSpline => self.curve(true)?,
        }
        Ok(())
    }

    /// libass's `drawing_add_curve` over the last four points.
    fn curve(&mut self, spline: bool) -> Result<()> {
        let mut p = self.recent;
        for &q in &p {
            self.cbox.include_point(q);
        }
        if spline {
            // Uniform B-spline to Bézier, in libass's integer arithmetic.
            let third = |a: i32, b: i32| b.wrapping_sub(a) / 3;
            let (x01, y01) = (third(p[0].x, p[1].x), third(p[0].y, p[1].y));
            let (x12, y12) = (third(p[1].x, p[2].x), third(p[1].y, p[2].y));
            let (x23, y23) = (third(p[2].x, p[3].x), third(p[2].y, p[3].y));
            p[0].x = p[1].x.wrapping_add(x12.wrapping_sub(x01) >> 1);
            p[0].y = p[1].y.wrapping_add(y12.wrapping_sub(y01) >> 1);
            p[3].x = p[2].x.wrapping_add(x23.wrapping_sub(x12) >> 1);
            p[3].y = p[2].y.wrapping_add(y23.wrapping_sub(y12) >> 1);
            p[1].x = p[1].x.wrapping_add(x12);
            p[1].y = p[1].y.wrapping_add(y12);
            p[2].x = p[2].x.wrapping_sub(x12);
            p[2].y = p[2].y.wrapping_sub(y12);
        }
        let start = self.start.unwrap_or(p[0]);
        self.push(SegmentKind::Cubic, &[start, p[1], p[2]], p[3])
    }

    /// Adds a segment owning `points`, ending at `end`, which starts the
    /// next one.
    fn push(&mut self, kind: SegmentKind, points: &[Vector], end: Vector) -> Result<()> {
        if !points.iter().all(|p| p.in_outline_range()) || !end.in_outline_range() {
            self.valid = false;
            return Ok(());
        }
        self.outline.push_segment(kind, points)?;
        self.start = Some(end);
        Ok(())
    }

    /// Closes the open contour with a line back to its first point.
    fn close(&mut self) -> Result<()> {
        if let Some(start) = self.start.take()
            && self.valid
        {
            self.outline.push_segment(SegmentKind::Line, &[start])?;
            self.outline.close_contour();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Drawing, drawing_scale};
    use crate::Limits;
    use triergon_core::Budget;
    use triergon_raster::{Outline, Rect, SegmentKind, Vector};

    type Parsed = (Vec<Vector>, Vec<(SegmentKind, bool)>, Rect);

    fn parse(text: &str) -> Option<Parsed> {
        let budget = Budget::new(1 << 20).unwrap();
        let mut outline = Outline::new(&budget);
        let drawing = Drawing::parse(text.as_bytes(), &mut outline, &Limits::default()).unwrap()?;
        let segments = drawing
            .outline
            .segments()
            .iter()
            .map(|s| (s.kind, s.closes_contour))
            .collect();
        Some((drawing.outline.points().to_vec(), segments, drawing.cbox))
    }

    #[test]
    fn a_square_is_one_closed_contour() {
        let (points, segments, cbox) = parse("m 0 0 l 10 0 10 10 0 10").unwrap();
        assert_eq!(points.len(), 4);
        assert_eq!(points[2], Vector::new(640, 640));
        assert_eq!(segments.len(), 4);
        assert_eq!(segments[3], (SegmentKind::Line, true));
        assert_eq!(cbox, Rect::new(0, 0, 640, 640));
    }

    #[test]
    fn libass_tolerances_hold() {
        // Commands before the first m are skipped; an n first rejects all.
        assert_eq!(parse("l 5 5 m 0 0 l 1 1").unwrap().0.len(), 2);
        assert!(parse("n 0 0 m 0 0 l 1 1").unwrap().0.is_empty());
        // A coordinate beyond 2^28/64 discards the drawing, as does one
        // whose 26.6 value is out of `int`'s range or lands on i32::MIN.
        assert!(parse("m 0 0 l 4194304 0").is_none());
        assert!(parse("m 0 0 l 33554432 0").is_none());
        assert!(parse("m 0 0 l 0 -33554432").is_none());
        assert!(parse("m 1e400 0 l 1 1").is_none());
        assert!(parse("m -1e400 0 l 1 1").is_none());
        assert!(parse("m 0 0 l 67108864.5 0").is_none());
        // Partial groups of a b command are consumed and dropped.
        let (points, segments, _) = parse("m 0 0 b 1 1 2 2").unwrap();
        assert!(points.is_empty() && segments.is_empty());
        let (points, segments, _) = parse("m 0 0 s 1 0 1 1 0 1 c").unwrap();
        assert_eq!(segments.len(), 5);
        assert_eq!(points.len(), 13);
    }

    #[test]
    fn scales_wrap_as_libass_shifts() {
        assert_eq!(drawing_scale(1), 1.0);
        assert_eq!(drawing_scale(3), 0.25);
        assert_eq!(drawing_scale(0), 0.0); // 1 << 31 is negative
        assert_eq!(drawing_scale(33), 1.0);
    }
}
