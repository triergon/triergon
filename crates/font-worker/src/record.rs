//! The values the font operations read, and the two ways of consuming them: the
//! ordered work digest of the harness protocol and the readable line format.

use std::io::{self, Write};
use triergon_font::{Band, Charmap, GlyphMetrics, Outline, PointKind, SizeMetrics, SizedFont};
use triergon_harness_protocol::digest::{fold, fold_framed, signed};

/// What one size reports besides its glyphs.
pub struct Size {
    pub height: i32,
    pub metrics: SizeMetrics,
    pub ascent: i32,
    pub descent: i32,
    pub harfbuzz: (i32, i32),
    pub strength: i32,
    pub postscript: bool,
    pub underline: Option<Band>,
    pub strikeout: Option<Band>,
}

impl Size {
    pub fn new(height: i32, sized: &SizedFont<'_>, postscript: bool) -> Self {
        let (ascent, descent) = sized.ascent_descent();
        Self {
            height,
            metrics: sized.metrics(),
            ascent,
            descent,
            harfbuzz: sized.harfbuzz_scale(),
            strength: sized.embolden_strength(),
            postscript,
            underline: sized.underline(),
            strikeout: sized.strikeout(),
        }
    }
}

/// libass's synthetic styles, in the order it applies them to a glyph.
#[derive(Clone, Copy)]
pub enum Style {
    Bold,
    Oblique,
    Both,
}

impl Style {
    pub const ALL: [Self; 3] = [Self::Bold, Self::Oblique, Self::Both];

    const fn name(self) -> &'static str {
        match self {
            Self::Bold => "bold",
            Self::Oblique => "oblique",
            Self::Both => "both",
        }
    }
}

/// One group of values, in the order `harness/reference_font.h` produces them.
pub enum Record<'a> {
    Face {
        index: u32,
        glyphs: u16,
        units_per_em: u16,
        ascender: i32,
        descender: i32,
        height: i32,
        /// `advanceWidthMax` as FreeType exposes it, an `FT_Short`.
        max_advance: i16,
        bold: bool,
        italic: bool,
    },
    /// A face the reader rejects.
    FaceError(u32),
    /// The family, style and PostScript names in their ASCII forms.
    Family(Option<&'a str>),
    StyleName(Option<&'a str>),
    PostScript(Option<&'a str>),
    Weight(u16),
    Name {
        platform: u16,
        encoding: u16,
        language: u16,
        id: u16,
        bytes: &'a [u8],
    },
    Charmap(Charmap<'a>),
    Map {
        code: u32,
        glyph: u16,
    },
    /// The mappings of a charmap ended with an error.
    MapError,
    TextCharmap(Option<Charmap<'a>>),
    Kern {
        left: u16,
        right: u16,
        value: i32,
    },
    Glyph {
        index: u16,
        /// The `hmtx` advance, or -1 when it cannot be read.
        advance: i64,
        /// The advance of the loaded glyph.
        loaded: i32,
        outline: &'a Outline,
    },
    /// A glyph the reader rejects.
    GlyphError(u16),
    /// A face of the scaled and hinted operations.
    SizedFace(u32),
    Size(&'a Size),
    /// A size the face cannot be loaded at.
    SizeError(i32),
    ScaledGlyph {
        index: u16,
        /// The unhinted advance in 26.6.
        advance: i32,
        outline: &'a Outline,
    },
    /// A synthetic style of the last scaled glyph; `None` when it cannot be applied.
    Styled {
        style: Style,
        index: u16,
        outline: Option<&'a Outline>,
    },
    /// The default-load advance in 26.6; `None` when the glyph cannot be loaded.
    Hinted {
        index: u16,
        advance: Option<i32>,
    },
    /// The vertical advances of the unhinted load and of the default load;
    /// `None` where the glyph cannot be loaded.
    Vertical {
        index: u16,
        unhinted: Option<GlyphMetrics>,
        hinted: Option<i32>,
    },
}

/// Consumes the records of one operation.
pub trait Sink {
    fn record(&mut self, record: Record<'_>) -> io::Result<()>;
}

/// Each record starts with its tag; absent values fold as `ABSENT`.
const FACE: u64 = 1;
const FACE_ERROR: u64 = 2;
const FAMILY: u64 = 3;
const STYLE: u64 = 4;
const POSTSCRIPT: u64 = 5;
const WEIGHT: u64 = 6;
const NAME: u64 = 7;
const CHARMAP: u64 = 8;
const MAP: u64 = 9;
const TEXT_CHARMAP: u64 = 10;
const KERN: u64 = 11;
const GLYPH: u64 = 12;
const GLYPH_ERROR: u64 = 13;
const MAP_ERROR: u64 = 14;
const SIZED_FACE: u64 = 20;
const SIZE: u64 = 21;
const SIZE_ERROR: u64 = 22;
const SCALED_GLYPH: u64 = 23;
const STYLED: u64 = 24;
const HINTED: u64 = 25;
const VERTICAL: u64 = 26;
const ABSENT: u64 = u64::MAX;

const fn point_tag(kind: PointKind) -> u8 {
    match kind {
        PointKind::Quadratic => 0,
        PointKind::On => 1,
        PointKind::Cubic => 2,
    }
}

/// The work of one or more operations: glyph outlines (or advances), their
/// points and the digest.
#[derive(Default)]
pub struct Work {
    pub packets: u64,
    pub bytes: u64,
    pub digest: u64,
}

impl Work {
    fn words(&mut self, words: &[u64]) {
        for &word in words {
            fold(&mut self.digest, word);
        }
    }

    fn text(&mut self, tag: u64, text: Option<&str>) {
        fold(&mut self.digest, tag);
        match text {
            Some(text) => fold_framed(&mut self.digest, text.as_bytes()),
            None => fold(&mut self.digest, ABSENT),
        }
    }

    fn charmap(&mut self, tag: u64, charmap: Option<Charmap<'_>>) {
        match charmap {
            Some(c) => self.words(&[
                tag,
                c.platform().id().into(),
                c.encoding_id().into(),
                // FreeType's `FT_Get_CMap_Format`: -1 for a charmap it adds.
                signed(c.kind().format_number().map_or(-1, i64::from)),
            ]),
            None => self.words(&[tag, ABSENT]),
        }
    }

    fn band(&mut self, band: Option<Band>) {
        match band {
            Some(b) => self.words(&[signed(b.y_min.into()), signed(b.y_max.into())]),
            None => fold(&mut self.digest, ABSENT),
        }
    }

    /// Contours, points, contour ends, then x, y and tag of every point.
    fn outline(&mut self, outline: &Outline) {
        let (ends, points) = (outline.contour_ends(), outline.points());
        self.words(&[ends.len() as u64, points.len() as u64]);
        for &end in ends {
            fold(&mut self.digest, end.into());
        }
        for p in points {
            self.words(&[
                signed(p.point.x.into()),
                signed(p.point.y.into()),
                point_tag(p.kind).into(),
            ]);
        }
        self.packets += 1;
        self.bytes += points.len() as u64;
    }

    /// Adds the work of one operation, its digest folded as one value.
    pub fn accumulate(&mut self, operation: &Work) {
        self.packets += operation.packets;
        self.bytes += operation.bytes;
        fold(&mut self.digest, operation.digest);
    }
}

impl Sink for Work {
    #[inline]
    fn record(&mut self, record: Record<'_>) -> io::Result<()> {
        match record {
            Record::Face {
                index,
                glyphs,
                units_per_em,
                ascender,
                descender,
                height,
                max_advance,
                bold,
                italic,
            } => self.words(&[
                FACE,
                index.into(),
                glyphs.into(),
                units_per_em.into(),
                signed(ascender.into()),
                signed(descender.into()),
                signed(height.into()),
                signed(max_advance.into()),
                bold.into(),
                italic.into(),
            ]),
            Record::FaceError(index) => self.words(&[FACE_ERROR, index.into()]),
            Record::Family(text) => self.text(FAMILY, text),
            Record::StyleName(text) => self.text(STYLE, text),
            Record::PostScript(text) => self.text(POSTSCRIPT, text),
            Record::Weight(weight) => self.words(&[WEIGHT, weight.into()]),
            Record::Name {
                platform,
                encoding,
                language,
                id,
                bytes,
            } => {
                self.words(&[
                    NAME,
                    platform.into(),
                    encoding.into(),
                    language.into(),
                    id.into(),
                ]);
                fold_framed(&mut self.digest, bytes);
            }
            Record::Charmap(c) => self.charmap(CHARMAP, Some(c)),
            Record::Map { code, glyph } => self.words(&[MAP, code.into(), glyph.into()]),
            Record::MapError => self.words(&[MAP_ERROR]),
            Record::TextCharmap(c) => self.charmap(TEXT_CHARMAP, c),
            Record::Kern { left, right, value } => {
                self.words(&[KERN, left.into(), right.into(), signed(value.into())]);
            }
            Record::Glyph {
                index,
                advance,
                loaded,
                outline,
            } => {
                self.words(&[GLYPH, index.into(), signed(advance), signed(loaded.into())]);
                self.outline(outline);
            }
            Record::GlyphError(index) => self.words(&[GLYPH_ERROR, index.into()]),
            Record::SizedFace(index) => self.words(&[SIZED_FACE, index.into()]),
            Record::Size(size) => {
                let m = &size.metrics;
                self.words(&[
                    SIZE,
                    signed(size.height.into()),
                    m.x_ppem.into(),
                    m.y_ppem.into(),
                    signed(m.x_scale),
                    signed(m.y_scale),
                    signed(m.ascender),
                    signed(m.descender),
                    signed(m.height),
                    signed(m.max_advance),
                    signed(size.ascent.into()),
                    signed(size.descent.into()),
                    signed(size.harfbuzz.0.into()),
                    signed(size.harfbuzz.1.into()),
                    signed(size.strength.into()),
                    size.postscript.into(),
                ]);
                self.band(size.underline);
                self.band(size.strikeout);
            }
            Record::SizeError(height) => self.words(&[SIZE_ERROR, signed(height.into())]),
            Record::ScaledGlyph {
                index,
                advance,
                outline,
            } => {
                self.words(&[SCALED_GLYPH, index.into(), signed(advance.into())]);
                self.outline(outline);
            }
            Record::Styled {
                style,
                index,
                outline,
            } => {
                self.words(&[STYLED, style as u64, index.into()]);
                match outline {
                    Some(outline) => self.outline(outline),
                    None => fold(&mut self.digest, ABSENT),
                }
            }
            Record::Hinted { index, advance } => {
                let advance = advance.map_or(ABSENT, |a| signed(a.into()));
                self.words(&[HINTED, index.into(), advance]);
                self.packets += 1;
            }
            Record::Vertical {
                index,
                unhinted,
                hinted,
            } => {
                let (linear, advance) = unhinted.map_or((ABSENT, ABSENT), |m| {
                    (
                        signed(m.linear_vertical_advance),
                        signed(m.vertical_advance.into()),
                    )
                });
                let hinted = hinted.map_or(ABSENT, |a| signed(a.into()));
                self.words(&[VERTICAL, index.into(), linear, advance, hinted]);
                self.packets += 1;
            }
        }
        Ok(())
    }
}

/// Writes each record as one line, for diffing against the reference's dump.
pub struct Dump<W>(pub W);

impl<W: Write> Dump<W> {
    fn outline(&mut self, outline: &Outline) -> io::Result<()> {
        let out = &mut self.0;
        let ends = outline.contour_ends();
        write!(out, "{} |", ends.len())?;
        for end in ends {
            write!(out, " {end}")?;
        }
        write!(out, " |")?;
        for p in outline.points() {
            let (x, y, tag) = (p.point.x, p.point.y, point_tag(p.kind));
            write!(out, " {x} {y} {tag}")?;
        }
        writeln!(out)
    }

    fn band(&mut self, key: &str, band: Option<Band>) -> io::Result<()> {
        match band {
            Some(b) => write!(self.0, " {key} {} {}", b.y_min, b.y_max),
            None => write!(self.0, " {key} -"),
        }
    }
}

fn charmap_text(charmap: Option<Charmap<'_>>) -> String {
    charmap.map_or_else(
        || "-".into(),
        |c| {
            format!(
                "{} {} {}",
                c.platform().id(),
                c.encoding_id(),
                c.kind().format_number().map_or(-1, i32::from)
            )
        },
    )
}

impl<W: Write> Sink for Dump<W> {
    fn record(&mut self, record: Record<'_>) -> io::Result<()> {
        match record {
            Record::Face {
                index,
                glyphs,
                units_per_em,
                ascender,
                descender,
                height,
                max_advance,
                bold,
                italic,
            } => writeln!(
                self.0,
                "face {index} {glyphs} {units_per_em} {ascender} {descender} {height} {max_advance} {} {}",
                u8::from(bold),
                u8::from(italic)
            ),
            Record::FaceError(index) => writeln!(self.0, "face {index} error"),
            Record::Family(name) => writeln!(self.0, "family {}", name.unwrap_or("-")),
            Record::StyleName(name) => writeln!(self.0, "style {}", name.unwrap_or("-")),
            Record::PostScript(name) => writeln!(self.0, "postscript {}", name.unwrap_or("-")),
            Record::Weight(weight) => writeln!(self.0, "weight {weight}"),
            Record::Name {
                platform,
                encoding,
                language,
                id,
                bytes,
            } => {
                write!(self.0, "name {platform} {encoding} {language} {id} ")?;
                triergon_harness_protocol::write_hex(&mut self.0, bytes)?;
                writeln!(self.0)
            }
            Record::Charmap(c) => writeln!(self.0, "charmap {}", charmap_text(Some(c))),
            Record::Map { code, glyph } => writeln!(self.0, "map {code} {glyph}"),
            Record::MapError => writeln!(self.0, "map error"),
            Record::TextCharmap(c) => writeln!(self.0, "textmap {}", charmap_text(c)),
            Record::Kern { left, right, value } => writeln!(self.0, "kern {left} {right} {value}"),
            Record::Glyph {
                index,
                advance,
                loaded,
                outline,
            } => {
                write!(self.0, "glyph {index} {advance} {loaded} ")?;
                self.outline(outline)
            }
            Record::GlyphError(index) => writeln!(self.0, "glyph {index} error"),
            Record::SizedFace(index) => writeln!(self.0, "face {index}"),
            Record::Size(size) => {
                let m = &size.metrics;
                write!(
                    self.0,
                    "size {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
                    size.height,
                    m.x_ppem,
                    m.y_ppem,
                    m.x_scale,
                    m.y_scale,
                    m.ascender,
                    m.descender,
                    m.height,
                    m.max_advance,
                    size.ascent,
                    size.descent,
                    size.harfbuzz.0,
                    size.harfbuzz.1,
                    size.strength,
                    u8::from(size.postscript)
                )?;
                self.band("underline", size.underline)?;
                self.band("strikeout", size.strikeout)?;
                writeln!(self.0)
            }
            Record::SizeError(height) => writeln!(self.0, "size {height} error"),
            Record::ScaledGlyph {
                index,
                advance,
                outline,
            } => {
                write!(self.0, "glyph {index} {advance} ")?;
                self.outline(outline)
            }
            Record::Styled {
                style,
                index,
                outline,
            } => match outline {
                Some(outline) => {
                    write!(self.0, "{} {index} ", style.name())?;
                    self.outline(outline)
                }
                None => writeln!(self.0, "{} {index} error", style.name()),
            },
            Record::Hinted { index, advance } => match advance {
                Some(advance) => writeln!(self.0, "hinted {index} {advance}"),
                None => writeln!(self.0, "hinted {index} error"),
            },
            Record::Vertical {
                index,
                unhinted,
                hinted,
            } => {
                write!(self.0, "vertical {index}")?;
                match unhinted {
                    Some(m) => write!(
                        self.0,
                        " {} {}",
                        m.linear_vertical_advance, m.vertical_advance
                    )?,
                    None => write!(self.0, " error")?,
                }
                match hinted {
                    Some(advance) => writeln!(self.0, " {advance}"),
                    None => writeln!(self.0, " error"),
                }
            }
        }
    }
}
