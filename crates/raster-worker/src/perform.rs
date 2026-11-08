//! Performing cases with triergon-raster and recording their results, in the
//! order of `harness/reference_raster.h`.

use std::io;

use triergon_core::{Budget, Error, Result as CoreResult, WorkBudget};
use triergon_raster::{
    Bitmap, Border, Clip, ClipFrame, Colour, Effects, FrameScale, GlyphBitmaps, GlyphRequest,
    Limits, OUTLINE_MAX, Outline, Pen, Placement, Raster, Rect, Run, RunBorder, Shadow, Shape,
    Vector,
};

use crate::case::{Case, Glyph, Random, RunCase, Source};
use crate::record::{Kind, Sink};

/// Status values of a record. The first three are the reference's: the source
/// or parameters are invalid, the operation succeeded, or it failed as libass
/// fails it (`Error::Invalid`). The reference never records the last two, so a
/// case that reaches them fails: the crate refused under a limit (the case's
/// work, [`Limits`] or a fixed size bound: `Error::Policy` and
/// `Error::Limit`), or because its storage budget ran out.
const INVALID: i64 = 0;
const OK: i64 = 1;
const FAILED: i64 = 2;
const POLICY: i64 = 3;
const BUDGET: i64 = 4;

/// The work one case may do: the work of a frame, which a case stands for.
const CASE_WORK: u64 = 1 << 32;

/// The status of an operation that failed with `error`.
fn refused(error: &Error) -> i64 {
    match error {
        Error::Invalid(_) => FAILED,
        Error::Policy(_) | Error::Limit { .. } => POLICY,
        // The storage budget ran out (`Backpressure`, `Allocation`).
        _ => BUDGET,
    }
}

/// The status of a source that does not convert: invalid as libass rejects it,
/// or refused.
fn unconverted(error: &Error) -> i64 {
    match error {
        Error::Invalid(_) => INVALID,
        error => refused(error),
    }
}

/// The raster state reused across cases, and the glyphs of a font case.
#[derive(Debug)]
pub struct Runner<'g> {
    budget: Budget,
    raster: Raster,
    /// The work of the case being performed.
    work: WorkBudget,
    /// The selected glyph ids and outlines of a font case.
    font_glyphs: &'g [(u16, Option<Glyph>)],
}

impl<'g> Runner<'g> {
    pub fn new(budget: &Budget, font_glyphs: &'g [(u16, Option<Glyph>)]) -> Self {
        Self {
            budget: budget.clone(),
            raster: Raster::new(budget, Limits::default()),
            work: WorkBudget::new(CASE_WORK),
            font_glyphs,
        }
    }

    /// The converted outline of `source`: `Invalid` when libass rejects it or
    /// it names no loaded glyph.
    fn convert(&self, source: &Source) -> CoreResult<Outline> {
        const MISSING: Error = Error::Invalid("no such glyph");
        let glyph = match source {
            Source::Glyph(index) => self
                .font_glyphs
                .get(*index)
                .and_then(|(_, glyph)| glyph.as_ref())
                .ok_or(MISSING)?,
            Source::Inline(glyph) => glyph,
        };
        let mut outline = Outline::new(&self.budget);
        outline.push_glyph(&glyph.points, &glyph.ends)?;
        Ok(outline)
    }

    pub fn perform(&mut self, case: Option<&Case>, sink: &mut impl Sink) -> io::Result<()> {
        self.work = WorkBudget::new(CASE_WORK);
        let Some(case) = case else {
            sink.begin(Kind::Unparsed)?;
            return sink.end();
        };
        match case {
            Case::Convert(source) => {
                sink.begin(Kind::Convert)?;
                match self.convert(source) {
                    Ok(outline) => {
                        sink.value(OK)?;
                        sink.outline(&outline)?;
                    }
                    Err(error) => sink.value(unconverted(&error))?,
                }
            }
            Case::Rotate(offset, source) => {
                sink.begin(Kind::Rotate)?;
                match self.convert(source) {
                    Err(error) => sink.value(unconverted(&error))?,
                    Ok(mut outline) => match outline.rotate_90(*offset) {
                        Ok(()) => {
                            sink.value(OK)?;
                            sink.outline(&outline)?;
                        }
                        Err(error) => sink.value(refused(&error))?,
                    },
                }
            }
            Case::Stroke([bx, by, eps], source) => {
                sink.begin(Kind::Stroke)?;
                self.stroke(Vector::new(*bx, *by), *eps, source, sink)?;
            }
            Case::Transform(projection, m, source) => {
                sink.begin(Kind::Transform)?;
                let outline = match self.convert(source) {
                    Ok(outline) => outline,
                    Err(error) => {
                        sink.value(unconverted(&error))?;
                        return sink.end();
                    }
                };
                let mut out = Outline::new(&self.budget);
                match outline.transform_into(m, *projection, &mut out) {
                    Ok(()) => {
                        sink.value(OK)?;
                        sink.outline(&out)?;
                    }
                    Err(error) => sink.value(refused(&error))?,
                }
            }
            Case::Raster(projection, m, sources) => {
                sink.begin(Kind::Raster)?;
                let mut outlines = Vec::with_capacity(2);
                for source in sources {
                    let outline = match self.convert(source) {
                        Ok(outline) => outline,
                        Err(error) => {
                            sink.value(unconverted(&error))?;
                            return sink.end();
                        }
                    };
                    let mut out = Outline::new(&self.budget);
                    // A transform out of range leaves the outline empty; a
                    // refusal for storage is recorded.
                    match outline.transform_into(m, *projection, &mut out) {
                        Ok(()) | Err(Error::Invalid(_)) => {}
                        Err(error) => {
                            sink.value(refused(&error))?;
                            return sink.end();
                        }
                    }
                    outlines.push(out);
                }
                let second = outlines.pop().unwrap_or_else(|| Outline::new(&self.budget));
                let first = outlines.pop().unwrap_or_else(|| Outline::new(&self.budget));
                let shape = Shape::from_outlines(first, second);
                self.result(sink, |runner| {
                    runner.raster.rasterize(&shape, &mut runner.work)
                })?;
            }
            Case::Run(run) => {
                sink.begin(Kind::Run)?;
                self.run(run, sink)?;
            }
            Case::Blur {
                bitmap,
                variance,
                be,
            } => {
                sink.begin(Kind::Blur)?;
                self.result(sink, |runner| {
                    let mut out = runner.random(*bitmap)?;
                    runner
                        .raster
                        .blur(&mut out, *be, *variance, &mut runner.work)?;
                    Ok(Some(out))
                })?;
            }
            Case::Add {
                seed,
                target,
                added,
                at,
            } => {
                sink.begin(Kind::Add)?;
                self.result(sink, |runner| {
                    let mut rng = Rng::seeded(*seed);
                    let mut out = rng.bitmap(&runner.budget, *target)?;
                    let added = rng.bitmap(&runner.budget, *added)?;
                    out.add(&added, at[0], at[1]);
                    Ok(Some(out))
                })?;
            }
            Case::Multiply { bitmap, inverse } => {
                sink.begin(if *inverse {
                    Kind::MultiplyInverse
                } else {
                    Kind::Multiply
                })?;
                self.result(sink, |runner| {
                    let mut rng = Rng::seeded(bitmap.seed);
                    let size = [bitmap.width, bitmap.height];
                    let mut out = rng.bitmap(&runner.budget, size)?;
                    let other = rng.bitmap(&runner.budget, size)?;
                    if *inverse {
                        out.multiply_inverse(&other, 0, 0);
                    } else {
                        out.multiply(&other, 0, 0);
                    }
                    Ok(Some(out))
                })?;
            }
            Case::Fix { seed, fill, border } => {
                sink.begin(Kind::Fix)?;
                self.result(sink, |runner| {
                    let mut rng = Rng::seeded(*seed);
                    let mut placed = |(size, [left, top]): ([usize; 2], [i32; 2])| {
                        let mut bitmap = rng.bitmap(&runner.budget, size)?;
                        bitmap.position = Vector::new(left, top);
                        CoreResult::Ok(bitmap)
                    };
                    let fill = placed(*fill)?;
                    let mut out = placed(*border)?;
                    out.subtract_fill(&fill);
                    Ok(Some(out))
                })?;
            }
            Case::Shift { bitmap, by } => {
                sink.begin(Kind::Shift)?;
                self.result(sink, |runner| {
                    let mut out = runner.random(*bitmap)?;
                    out.shift_by(Vector::new(by[0], by[1]));
                    Ok(Some(out))
                })?;
            }
            Case::Clip { bitmap, values } => {
                sink.begin(Kind::Clip)?;
                self.clip(*bitmap, values, sink)?;
            }
        }
        sink.end()
    }

    /// A bitmap result: `OK` and the bitmap, or the status of the crate's
    /// refusal; libass draws every one.
    fn result(
        &mut self,
        sink: &mut impl Sink,
        operation: impl FnOnce(&mut Self) -> CoreResult<Option<Bitmap>>,
    ) -> io::Result<()> {
        match operation(self) {
            Ok(bitmap) => {
                sink.value(OK)?;
                sink.bitmap(bitmap.as_ref())
            }
            Err(error) => sink.value(refused(&error)),
        }
    }

    fn random(&self, bitmap: Random) -> CoreResult<Bitmap> {
        Rng::seeded(bitmap.seed).bitmap(&self.budget, [bitmap.width, bitmap.height])
    }

    fn stroke(
        &mut self,
        width: Vector,
        tolerance: i32,
        source: &Source,
        sink: &mut impl Sink,
    ) -> io::Result<()> {
        let radius = width.x.max(width.y);
        let outline = match self.convert(source) {
            Ok(outline) => outline,
            Err(error) => return sink.value(unconverted(&error)),
        };
        if !(tolerance >= 1 && radius >= tolerance && radius <= OUTLINE_MAX) {
            return sink.value(INVALID);
        }
        let pen = Pen { width, tolerance };
        let mut border = Shape::new(&self.budget);
        match self
            .raster
            .stroke(&outline, &pen, &mut border, &mut self.work)
        {
            Ok(()) => {
                sink.value(OK)?;
                let [outer, inner] = border.outlines();
                sink.outline(outer)?;
                sink.outline(inner)
            }
            Err(error) => sink.value(refused(&error)),
        }
    }

    fn clip(&self, bitmap: Random, v: &[i32; 16], sink: &mut impl Sink) -> io::Result<()> {
        let mut source = match self.random(bitmap) {
            Ok(source) => source,
            Err(error) => return sink.value(refused(&error)),
        };
        source.position = Vector::new(v[0], v[1]);
        let rect = |at: usize| Rect::new(v[at], v[at + 1], v[at + 2], v[at + 3]);
        let clip = Clip {
            rect: rect(5),
            inverse: v[9] != 0,
        };
        let frame = ClipFrame {
            width: v[10],
            height: v[11],
            content: rect(12),
        };
        let parts = clip.parts(&source, Vector::new(v[2], v[3]), v[4], &frame);
        sink.value(OK)?;
        sink.value(parts.len() as i64)?;
        for part in parts {
            let colour = match part.colour {
                Colour::First => 1,
                Colour::Second => 2,
            };
            sink.values(&[
                part.position.x.into(),
                part.position.y.into(),
                part.source_x as i64,
                part.source_y as i64,
                part.width as i64,
                part.height as i64,
                colour,
            ])?;
        }
        Ok(())
    }

    /// One run: every glyph's origin and bitmaps, the run's karaoke leftmost
    /// x, then the composite's fill, border and shadow. The case's flags are
    /// the style's: an opaque box, a border, a shadow, a shadow and a border
    /// that keep the fill, and `\kf` karaoke.
    fn run(&mut self, case: &RunCase, sink: &mut impl Sink) -> io::Result<()> {
        let bit = |b: u32| case.flags & b != 0;
        let r = &case.reals;
        let frame = FrameScale {
            blur: [r[3], r[4]],
            border: [r[5], r[6]],
            pixel_aspect: r[7],
        };
        let effects = Effects {
            border: if bit(1) {
                RunBorder::OpaqueBox { shown: bit(2) }
            } else if bit(2) {
                RunBorder::Outline {
                    keeps_fill: bit(16),
                }
            } else {
                RunBorder::None
            },
            shadow: Shadow {
                offset: if bit(4) { [r[1], r[2]] } else { [0.0, 0.0] },
                keeps_fill: bit(8),
            },
            blur: r[0],
            be: case.be,
        };
        // libass draws a glyph's box when the box is shown or casts a shadow,
        // and a stroked border when the run has one.
        let [x, y] = effects.shadow.offset;
        let casts = x != 0.0 || y != 0.0;
        let mut run = Run::default();
        let mut drawn = Vec::new();
        for glyph in &case.glyphs {
            let outline = match self.convert(&glyph.source) {
                Ok(outline) => outline,
                Err(Error::Invalid(_)) => {
                    sink.value(INVALID)?;
                    continue;
                }
                Err(error) => return sink.value(refused(&error)),
            };
            let shape = Shape::from_outlines(outline, Outline::new(&self.budget));
            let (gr, gi) = (&glyph.reals, &glyph.ints);
            let size = [gr[11], gr[12]];
            let border = match effects.border {
                RunBorder::OpaqueBox { shown } if shown || casts => Border::OpaqueBox {
                    size,
                    spacing: gi[6],
                    advance: gi[7],
                    scale_fix: gr[13],
                },
                RunBorder::Outline { .. } => Border::Outline { size },
                _ => Border::None,
            };
            let request = GlyphRequest {
                shape: &shape,
                placement: Placement {
                    rotation: [gr[0], gr[1], gr[2]],
                    shear: [gr[3], gr[4]],
                    scale: [gr[5], gr[6]],
                    outline_scale: [gr[7], gr[8]],
                    outline_offset: [gr[9], gr[10]],
                    shift: Vector::new(gi[0], gi[1]),
                    ascender: gi[2],
                    descender: gi[3],
                    position: Vector::new(gi[4], gi[5]),
                },
                frame,
                border,
                karaoke: bit(32),
            };
            let bitmaps = match self.raster.render(&request, &mut run, &mut self.work) {
                Ok(bitmaps) => bitmaps,
                Err(error) => return sink.value(refused(&error)),
            };
            sink.value(OK)?;
            let origin = bitmaps
                .as_ref()
                .map_or(Vector::default(), GlyphBitmaps::origin);
            sink.values(&[origin.x.into(), origin.y.into()])?;
            sink.bitmap(bitmaps.as_ref().and_then(|b| b.glyph()))?;
            sink.bitmap(bitmaps.as_ref().and_then(|b| b.border()))?;
            drawn.extend(bitmaps);
        }
        sink.value(run.leftmost_x().into())?;
        if drawn.is_empty() {
            for _ in 0..3 {
                sink.absent()?;
            }
            return Ok(());
        }
        let composite = match self
            .raster
            .compose(&effects, &frame, &drawn, &mut self.work)
        {
            Ok(composite) => composite,
            Err(error) => return sink.value(refused(&error)),
        };
        for bitmap in [&composite.fill, &composite.border, &composite.shadow] {
            sink.bitmap(bitmap.as_ref())?;
        }
        Ok(())
    }
}

/// The deterministic bitmap content of the bitmap cases (xorshift64): runs of
/// empty, full and arbitrary coverage.
struct Rng(u64);

impl Rng {
    fn seeded(seed: i64) -> Self {
        Self((seed as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn bitmap(&mut self, budget: &Budget, [width, height]: [usize; 2]) -> CoreResult<Bitmap> {
        let mut bitmap = Bitmap::new(budget, width, height, &Limits::default())?;
        let mut value = 0u8;
        for row in bitmap.rows_mut() {
            for v in row {
                let r = self.next();
                if r.is_multiple_of(5) {
                    value = match (r >> 8) % 3 {
                        0 => 0,
                        1 => 255,
                        _ => (r >> 16) as u8,
                    };
                }
                *v = value;
            }
        }
        Ok(bitmap)
    }
}
