//! The values one font operation reads, and the two ways of consuming them: the
//! ordered work digest of the harness protocol and the readable line format.

use std::io::{self, Write};
use triergon_font::{Charmap, Outline, PointKind};
use triergon_harness_protocol::digest::{fold, fold_framed, signed};

/// One group of values, in the order `harness/reference_font.h` produces them.
pub enum Record<'a> {
    Face {
        index: u32,
        glyphs: u32,
        units_per_em: u16,
        ascender: i32,
        descender: i32,
        height: i32,
        max_advance: u16,
        bold: bool,
        italic: bool,
    },
    /// A face the reader rejects.
    FaceError(u32),
    /// The family, style and PostScript names in their ASCII forms.
    Family(Option<&'a str>),
    Style(Option<&'a str>),
    PostScript(Option<&'a str>),
    Weight(u16),
    Name {
        platform: u16,
        encoding: u16,
        language: u16,
        id: u16,
        bytes: &'a [u8],
    },
    Charmap(&'a Charmap),
    Map {
        code: u32,
        glyph: u32,
    },
    TextCharmap(Option<&'a Charmap>),
    Kern {
        left: u32,
        right: u32,
        value: i32,
    },
    Glyph {
        index: u32,
        /// The `hmtx` advance, or -1 when it cannot be read.
        advance: i64,
        /// The advance of the loaded glyph.
        loaded: i64,
        outline: &'a Outline,
    },
    /// A glyph the reader rejects.
    GlyphError(u32),
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
const ABSENT: u64 = u64::MAX;

/// Outline coordinates at 1/64 font unit, as FreeType's 26.6 values at a size of
/// one pixel per font unit.
fn units_64(value: f32) -> i64 {
    (f64::from(value) * 64.0).round() as i64
}

const fn point_tag(kind: PointKind) -> u8 {
    match kind {
        PointKind::Quadratic => 0,
        PointKind::On => 1,
        PointKind::Cubic => 2,
    }
}

/// The work of one or more operations: glyphs, outline points and the digest.
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
                max_advance.into(),
                bold.into(),
                italic.into(),
            ]),
            Record::FaceError(index) => self.words(&[FACE_ERROR, index.into()]),
            Record::Family(text) => self.text(FAMILY, text),
            Record::Style(text) => self.text(STYLE, text),
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
            Record::Charmap(c) => {
                self.words(&[
                    CHARMAP,
                    c.platform.into(),
                    c.encoding.into(),
                    c.format.into(),
                ]);
            }
            Record::Map { code, glyph } => self.words(&[MAP, code.into(), glyph.into()]),
            Record::TextCharmap(Some(c)) => {
                self.words(&[
                    TEXT_CHARMAP,
                    c.platform.into(),
                    c.encoding.into(),
                    c.format.into(),
                ]);
            }
            Record::TextCharmap(None) => self.words(&[TEXT_CHARMAP, ABSENT]),
            Record::Kern { left, right, value } => {
                self.words(&[KERN, left.into(), right.into(), signed(value.into())]);
            }
            Record::Glyph {
                index,
                advance,
                loaded,
                outline,
            } => {
                let (ends, points) = (outline.contour_ends(), outline.points());
                self.words(&[
                    GLYPH,
                    index.into(),
                    signed(advance),
                    signed(loaded),
                    ends.len() as u64,
                    points.len() as u64,
                ]);
                for &end in ends {
                    fold(&mut self.digest, end.into());
                }
                for p in points {
                    self.words(&[
                        signed(units_64(p.point.x)),
                        signed(units_64(p.point.y)),
                        point_tag(p.kind).into(),
                    ]);
                }
                self.packets += 1;
                self.bytes += points.len() as u64;
            }
            Record::GlyphError(index) => self.words(&[GLYPH_ERROR, index.into()]),
        }
        Ok(())
    }
}

/// Writes each record as one line, for diffing against the reference's `font-dump`.
pub struct Dump<W>(pub W);

impl<W: Write> Sink for Dump<W> {
    fn record(&mut self, record: Record<'_>) -> io::Result<()> {
        let out = &mut self.0;
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
                out,
                "face {index} {glyphs} {units_per_em} {ascender} {descender} {height} {max_advance} {} {}",
                u8::from(bold),
                u8::from(italic)
            ),
            Record::FaceError(index) => writeln!(out, "face {index} error"),
            Record::Family(name) => writeln!(out, "family {}", name.unwrap_or("-")),
            Record::Style(name) => writeln!(out, "style {}", name.unwrap_or("-")),
            Record::PostScript(name) => writeln!(out, "postscript {}", name.unwrap_or("-")),
            Record::Weight(weight) => writeln!(out, "weight {weight}"),
            Record::Name {
                platform,
                encoding,
                language,
                id,
                bytes,
            } => {
                write!(out, "name {platform} {encoding} {language} {id} ")?;
                triergon_harness_protocol::write_hex(out, bytes)?;
                writeln!(out)
            }
            Record::Charmap(c) => {
                writeln!(out, "charmap {} {} {}", c.platform, c.encoding, c.format)
            }
            Record::Map { code, glyph } => writeln!(out, "map {code} {glyph}"),
            Record::TextCharmap(Some(c)) => {
                writeln!(out, "textmap {} {} {}", c.platform, c.encoding, c.format)
            }
            Record::TextCharmap(None) => writeln!(out, "textmap -"),
            Record::Kern { left, right, value } => writeln!(out, "kern {left} {right} {value}"),
            Record::Glyph {
                index,
                advance,
                loaded,
                outline,
            } => {
                let ends = outline.contour_ends();
                write!(out, "glyph {index} {advance} {loaded} {} |", ends.len())?;
                for end in ends {
                    write!(out, " {end}")?;
                }
                write!(out, " |")?;
                for p in outline.points() {
                    write!(
                        out,
                        " {} {} {}",
                        units_64(p.point.x),
                        units_64(p.point.y),
                        point_tag(p.kind)
                    )?;
                }
                writeln!(out)
            }
            Record::GlyphError(index) => writeln!(out, "glyph {index} error"),
        }
    }
}
