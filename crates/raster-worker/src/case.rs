//! The case grammar shared with `harness/reference_raster.h`: one case is one
//! line of tokens separated by single spaces.
//!
//! ```text
//! SOURCE    g INDEX                      a glyph of the font case
//!         | O NC END... NP (X Y KIND)... exclusive contour ends; KIND 0 quadratic,
//!                                        1 on, 2 cubic
//! MATRIX    9 doubles, row by row        each the 16 hex digits of its bits
//! convert   SOURCE
//! rotate    X Y SOURCE
//! stroke    XBORD YBORD EPS SOURCE
//! transform a|p MATRIX SOURCE
//! raster    a|p MATRIX SOURCE SOURCE
//! run       FLAGS BE BLUR XSHAD YSHAD BLURSCALEX BLURSCALEY BORDERSCALEX BORDERSCALEY PAR
//!           N (FRX FRY FRZ FAX FAY SCALEX SCALEY OSCALEX OSCALEY OOFFX OOFFY XBORD YBORD
//!              SCALEFIX SHIFTX SHIFTY ASC DESC POSX POSY SPACING ADVANCE SOURCE)×N
//! blur      W H SEED R2X R2Y BE
//! add       SEED W1 H1 W2 H2 X Y
//! mul|imul  SEED W H
//! fix       SEED W1 H1 LEFT1 TOP1 W2 H2 LEFT2 TOP2
//! shift     SEED W H SX SY
//! clip      SEED W H LEFT TOP DX DY SPLIT X0 Y0 X1 Y1 INVERSE FW FH CX0 CY0 CX1 CY1
//! ```
//!
//! Integers are decimal and fit 32 bits (seeds 64); run FLAGS are libass's
//! filter flags (1 opaque box, 2 border, 4 shadow, 8 fill in shadow, 16 fill in
//! border) plus 32 for `\kf` karaoke glyphs. Bitmap sizes are 1 to 4096, `BE`
//! 0 to 255, shifts 0 to 63, rotation offsets within ±2³⁰, an added bitmap lies
//! inside its target, clip values within ±2²⁴. A line outside the grammar is recorded as unparsed.

use triergon_font::{OutlinePoint, Point, PointKind};
use triergon_raster::{Matrix, Projection, Vector};

/// Bounds on one case, so no input can demand unbounded work or reach the
/// reference's unchecked arithmetic.
const MAX_SIDE: i64 = 4096;
const MAX_POINTS: i64 = 65535;
const MAX_CLIP: i64 = 1 << 24;
const MAX_RUN: i64 = 8;
/// The largest rotation offset libass accepts (`INT32_MAX - OUTLINE_MAX`), rounded down.
const MAX_OFFSET: i64 = 1 << 30;

/// An outline a case reads.
#[derive(Clone, Debug)]
pub enum Source {
    /// A glyph of the font case, by its place in the selection.
    Glyph(usize),
    Inline(Glyph),
}

/// Tagged points and exclusive contour ends, as a font outline gives them.
#[derive(Clone, Debug, Default)]
pub struct Glyph {
    pub points: Vec<OutlinePoint>,
    pub ends: Vec<u32>,
}

/// One glyph of a run case.
#[derive(Clone, Debug)]
pub struct RunGlyph {
    /// FRX .. SCALEFIX.
    pub reals: [f64; 14],
    /// SHIFTX .. ADVANCE.
    pub ints: [i32; 8],
    pub source: Source,
}

#[derive(Clone, Debug)]
pub struct RunCase {
    pub flags: u32,
    pub be: u32,
    /// BLUR .. PAR.
    pub reals: [f64; 8],
    pub glyphs: Vec<RunGlyph>,
}

/// A bitmap of random content: its seed and size.
#[derive(Clone, Copy, Debug)]
pub struct Random {
    pub seed: i64,
    pub width: usize,
    pub height: usize,
}

#[derive(Clone, Debug)]
pub enum Case {
    Convert(Source),
    Rotate(Vector, Source),
    Stroke([i32; 3], Source),
    Transform(Projection, Matrix, Source),
    Raster(Projection, Matrix, [Source; 2]),
    Run(RunCase),
    Blur {
        bitmap: Random,
        variance: [f64; 2],
        be: u32,
    },
    Add {
        seed: i64,
        target: [usize; 2],
        added: [usize; 2],
        at: [i32; 2],
    },
    Multiply {
        bitmap: Random,
        inverse: bool,
    },
    Fix {
        seed: i64,
        fill: ([usize; 2], [i32; 2]),
        border: ([usize; 2], [i32; 2]),
    },
    Shift {
        bitmap: Random,
        by: [i32; 2],
    },
    Clip {
        bitmap: Random,
        /// LEFT TOP DX DY SPLIT X0 Y0 X1 Y1 INVERSE FW FH CX0 CY0 CX1 CY1.
        values: [i32; 16],
    },
}

struct Tokens<'a>(std::str::Split<'a, char>);

impl Tokens<'_> {
    fn text(&mut self) -> Option<&str> {
        self.0.next()
    }

    fn int(&mut self, min: i64, max: i64) -> Option<i64> {
        let text = self.text()?;
        let digits = text.strip_prefix('-').unwrap_or(text);
        if digits.is_empty() || digits.len() > 19 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let value: i64 = text.parse().ok()?;
        (min..=max).contains(&value).then_some(value)
    }

    fn i32(&mut self) -> Option<i32> {
        self.int(i32::MIN.into(), i32::MAX.into()).map(|v| v as i32)
    }

    fn side(&mut self) -> Option<usize> {
        self.int(1, MAX_SIDE).map(|v| v as usize)
    }

    fn seed(&mut self) -> Option<i64> {
        self.int(i64::MIN, i64::MAX)
    }

    fn real(&mut self) -> Option<f64> {
        let text = self.text()?;
        if text.len() != 16 || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return None;
        }
        u64::from_str_radix(text, 16).ok().map(f64::from_bits)
    }

    fn reals<const N: usize>(&mut self) -> Option<[f64; N]> {
        let mut values = [0.0; N];
        for v in &mut values {
            *v = self.real()?;
        }
        Some(values)
    }

    fn i32s<const N: usize>(&mut self) -> Option<[i32; N]> {
        let mut values = [0; N];
        for v in &mut values {
            *v = self.i32()?;
        }
        Some(values)
    }

    fn projection(&mut self) -> Option<Projection> {
        match self.text()? {
            "a" => Some(Projection::Affine),
            "p" => Some(Projection::Perspective),
            _ => None,
        }
    }

    fn random(&mut self) -> Option<Random> {
        Some(Random {
            seed: self.seed()?,
            width: self.side()?,
            height: self.side()?,
        })
    }

    fn source(&mut self) -> Option<Source> {
        match self.text()? {
            "g" => Some(Source::Glyph(self.int(0, MAX_POINTS)? as usize)),
            "O" => {
                let contours = self.int(0, MAX_POINTS)?;
                let ends = (0..contours)
                    .map(|_| self.int(0, MAX_POINTS).map(|e| e as u32))
                    .collect::<Option<_>>()?;
                let count = self.int(0, MAX_POINTS)?;
                let points = (0..count)
                    .map(|_| {
                        let (x, y) = (self.i32()?, self.i32()?);
                        let kind = match self.int(0, 2)? {
                            0 => PointKind::Quadratic,
                            1 => PointKind::On,
                            _ => PointKind::Cubic,
                        };
                        Some(OutlinePoint {
                            point: Point::new(x, y),
                            kind,
                        })
                    })
                    .collect::<Option<_>>()?;
                Some(Source::Inline(Glyph { points, ends }))
            }
            _ => None,
        }
    }

    fn run(&mut self) -> Option<RunCase> {
        let flags = self.int(0, 63)? as u32;
        let be = self.int(0, 255)? as u32;
        let reals = self.reals()?;
        let count = self.int(1, MAX_RUN)?;
        let glyphs = (0..count)
            .map(|_| {
                Some(RunGlyph {
                    reals: self.reals()?,
                    ints: self.i32s()?,
                    source: self.source()?,
                })
            })
            .collect::<Option<_>>()?;
        Some(RunCase {
            flags,
            be,
            reals,
            glyphs,
        })
    }

    fn case(&mut self) -> Option<Case> {
        let case = match self.text()? {
            "convert" => Case::Convert(self.source()?),
            "rotate" => {
                let (x, y) = (
                    self.int(-MAX_OFFSET, MAX_OFFSET)?,
                    self.int(-MAX_OFFSET, MAX_OFFSET)?,
                );
                let (x, y) = (x as i32, y as i32);
                Case::Rotate(Vector::new(x, y), self.source()?)
            }
            "stroke" => Case::Stroke(self.i32s()?, self.source()?),
            "transform" => {
                let projection = self.projection()?;
                Case::Transform(projection, self.matrix()?, self.source()?)
            }
            "raster" => {
                let projection = self.projection()?;
                let m = self.matrix()?;
                Case::Raster(projection, m, [self.source()?, self.source()?])
            }
            "run" => Case::Run(self.run()?),
            "blur" => {
                let (width, height) = (self.side()?, self.side()?);
                let seed = self.seed()?;
                Case::Blur {
                    bitmap: Random {
                        seed,
                        width,
                        height,
                    },
                    variance: self.reals()?,
                    be: self.int(0, 255)? as u32,
                }
            }
            "add" => {
                let seed = self.seed()?;
                let target = [self.side()?, self.side()?];
                let added = [self.side()?, self.side()?];
                if added[0] > target[0] || added[1] > target[1] {
                    return None;
                }
                let room = |i: usize| (target[i] - added[i]) as i64;
                let at = [self.int(0, room(0))? as i32, self.int(0, room(1))? as i32];
                Case::Add {
                    seed,
                    target,
                    added,
                    at,
                }
            }
            "mul" => Case::Multiply {
                bitmap: self.random()?,
                inverse: false,
            },
            "imul" => Case::Multiply {
                bitmap: self.random()?,
                inverse: true,
            },
            "fix" => {
                let seed = self.seed()?;
                let mut placed = || Some(([self.side()?, self.side()?], self.i32s::<2>()?));
                let fill = placed()?;
                Case::Fix {
                    seed,
                    fill,
                    border: placed()?,
                }
            }
            "shift" => Case::Shift {
                bitmap: self.random()?,
                by: [self.int(0, 63)? as i32, self.int(0, 63)? as i32],
            },
            "clip" => {
                let bitmap = self.random()?;
                let mut values = [0; 16];
                for (i, v) in values.iter_mut().enumerate() {
                    let range = if i == 9 {
                        (0, 1)
                    } else {
                        (-MAX_CLIP, MAX_CLIP)
                    };
                    *v = self.int(range.0, range.1)? as i32;
                }
                Case::Clip { bitmap, values }
            }
            _ => return None,
        };
        self.0.next().is_none().then_some(case)
    }

    fn matrix(&mut self) -> Option<Matrix> {
        let [a, b, c, d, e, f, g, h, i] = self.reals()?;
        Some(Matrix([[a, b, c], [d, e, f], [g, h, i]]))
    }
}

/// One case, or `None` outside the grammar.
pub fn parse(line: &str) -> Option<Case> {
    Tokens(line.split(' ')).case()
}

#[cfg(test)]
mod tests {
    use super::{Case, Source, parse};

    #[test]
    fn cases_parse_strictly() {
        assert!(matches!(
            parse("convert O 1 3 3 0 0 1 64 0 1 0 64 1"),
            Some(Case::Convert(Source::Inline(_)))
        ));
        assert!(matches!(
            parse("convert g 3"),
            Some(Case::Convert(Source::Glyph(3)))
        ));
        for broken in [
            "convert O 1 3 3 0 0 1 64 0 1 0 64 3",
            "convert g 3 ",
            "convert  g 3",
            "shift 1 4 4 64 0",
            "add 1 4 4 5 1 0 0",
            "add 1 4 4 2 2 3 0",
            "blur 0 4 1 0000000000000000 0000000000000000 0",
        ] {
            assert!(parse(broken).is_none(), "{broken}");
        }
        assert!(parse("add 1 4 4 2 2 2 2").is_some());
    }
}
