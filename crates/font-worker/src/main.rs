//! Font measurement worker: every face of a font file read through triergon-font,
//! in the order of `harness/reference_font.h`. Reading the file, printing and process
//! startup are outside the timed loop.
#![forbid(unsafe_code)]

mod record;

use record::{Dump, Record, Sink, Size, Style, Work};
use std::error::Error;
use std::io::{self, BufWriter, Write};
use std::process::ExitCode;
use std::time::Instant;
use triergon_core::{Budget, Bytes};
use triergon_font::{Font, GlyphId, Limits, NameRecord, Outline, SizedFont, Synthesis};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const MAX_INPUT: u64 = 64 << 20;
const USAGE: &str =
    "usage: font-worker OPERATION-fold FILE [HEIGHT] | OPERATION-bench FILE ITERATIONS [HEIGHT]
       | OPERATION-memory FILE [HEIGHT] | OPERATION-dump FILE [HEIGHT]
  OPERATION is font (no HEIGHT), font-scaled or font-hinted (HEIGHT in 26.6 pixels)";

/// What one operation reads from every face.
#[derive(Clone, Copy)]
enum Operation {
    /// Face data and unscaled outlines.
    Faces,
    /// What libass obtains at a size with hinting off: size metrics, kerning, and
    /// every glyph's unhinted outline and advance with its synthetic styles.
    Scaled(i32),
    /// The default-load (hinted) advance of every glyph at a size.
    Hinted(i32),
}

impl Operation {
    const fn name(self) -> &'static str {
        match self {
            Self::Faces => "font-outlines",
            Self::Scaled(_) => "font-scaled",
            Self::Hinted(_) => "font-hinted",
        }
    }
}

/// FreeType's ASCII form of a name: UTF-16 units, or bytes for Macintosh records,
/// up to a NUL, with units outside 32..=127 shown as `?`.
fn ascii(record: NameRecord<'_>) -> String {
    record
        .code_units()
        .take_while(|&u| u != 0)
        .map(|u| {
            u8::try_from(u)
                .ok()
                .filter(|b| (32..=127).contains(b))
                .map_or('?', char::from)
        })
        .collect()
}

/// FreeType's PostScript form: printable ASCII other than `[](){}<>/%`, absent
/// when nothing remains.
fn postscript(record: NameRecord<'_>) -> Option<String> {
    let text: String = record
        .code_units()
        .filter_map(|u| u8::try_from(u).ok())
        .filter(|&b| (33..=126).contains(&b) && !b"[](){}<>/%".contains(&b))
        .map(char::from)
        .collect();
    (!text.is_empty()).then_some(text)
}

/// Every pair the `kern` subtables list, once, in glyph order.
fn kerning_pairs(font: &Font) -> Vec<(GlyphId, GlyphId)> {
    let mut pairs: Vec<(GlyphId, GlyphId)> = font.kerning_pairs().collect();
    pairs.sort_unstable();
    pairs.dedup();
    pairs
}

/// One operation: parse every face of `file` and read what `operation` covers.
fn read(file: &Bytes, budget: &Budget, operation: Operation, sink: &mut impl Sink) -> Result<()> {
    let faces = Font::face_count(file.as_ref())?;
    for index in 0..faces {
        let Ok(font) = Font::new(file.clone(), index, budget, Limits::default()) else {
            sink.record(Record::FaceError(index))?;
            continue;
        };
        match operation {
            Operation::Faces => face(&font, &mut Outline::new(budget), sink)?,
            Operation::Scaled(height) => {
                sink.record(Record::SizedFace(index))?;
                match font.at_size(height) {
                    Ok(sized) => scaled(&font, &sized, height, budget, sink)?,
                    Err(_) => sink.record(Record::SizeError(height))?,
                }
            }
            Operation::Hinted(height) => {
                sink.record(Record::SizedFace(index))?;
                match font.at_size(height) {
                    Ok(sized) => hinted(&font, &sized, sink)
                        .map_err(|e| format!("face {index} at {height}: {e}"))?,
                    Err(_) => sink.record(Record::SizeError(height))?,
                }
            }
        }
    }
    Ok(())
}

fn face(font: &Font, outline: &mut Outline, sink: &mut impl Sink) -> io::Result<()> {
    let metrics = font.vertical_metrics();
    let style = font.style();
    sink.record(Record::Face {
        index: font.index(),
        glyphs: font.glyph_count(),
        units_per_em: font.units_per_em(),
        ascender: metrics.ascender,
        descender: metrics.descender,
        height: metrics.height,
        max_advance: font.horizontal_header().advance_max.cast_signed(),
        bold: style.bold,
        italic: style.italic,
    })?;
    sink.record(Record::Family(font.family_name().map(ascii).as_deref()))?;
    sink.record(Record::StyleName(font.style_name().map(ascii).as_deref()))?;
    sink.record(Record::PostScript(
        font.postscript_name().and_then(postscript).as_deref(),
    ))?;
    sink.record(Record::Weight(font.os2().map_or(0, |o| o.weight_class)))?;
    for name in font.names() {
        sink.record(Record::Name {
            platform: name.platform.id(),
            encoding: name.encoding_id,
            language: name.language.0,
            id: name.name_id.id(),
            bytes: name.bytes,
        })?;
    }
    for charmap in font.charmaps() {
        sink.record(Record::Charmap(charmap))?;
        for mapping in charmap.mappings() {
            let Ok((code, glyph)) = mapping else {
                sink.record(Record::MapError)?;
                break;
            };
            sink.record(Record::Map {
                code,
                glyph: glyph.0,
            })?;
        }
    }
    sink.record(Record::TextCharmap(font.text_charmap()))?;
    for (left, right) in kerning_pairs(font) {
        let value = font.kerning(left, right);
        if value != 0 {
            sink.record(Record::Kern {
                left: left.0,
                right: right.0,
                value,
            })?;
        }
    }
    for glyph in font.glyphs() {
        let advance = font.advance(glyph).map_or(-1, i64::from);
        match font.outline_into(glyph, outline) {
            Ok(loaded) => sink.record(Record::Glyph {
                index: glyph.0,
                advance,
                loaded: loaded.advance,
                outline,
            })?,
            Err(_) => sink.record(Record::GlyphError(glyph.0))?,
        }
    }
    Ok(())
}

fn scaled(
    font: &Font,
    sized: &SizedFont<'_>,
    height: i32,
    budget: &Budget,
    sink: &mut impl Sink,
) -> io::Result<()> {
    sink.record(Record::Size(&Size::new(
        height,
        sized,
        font.is_postscript(),
    )))?;
    for (left, right) in kerning_pairs(font) {
        let value = sized.kerning(left, right);
        if value != 0 {
            sink.record(Record::Kern {
                left: left.0,
                right: right.0,
                value,
            })?;
        }
    }
    let (mut outline, mut styled) = (Outline::new(budget), Outline::new(budget));
    for glyph in font.glyphs() {
        let Ok(metrics) = sized.outline_into(glyph, &mut outline) else {
            sink.record(Record::GlyphError(glyph.0))?;
            continue;
        };
        sink.record(Record::ScaledGlyph {
            index: glyph.0,
            advance: metrics.advance,
            outline: &outline,
        })?;
        for style in Style::ALL {
            let synthesis = Synthesis {
                oblique: !matches!(style, Style::Bold),
                embolden: !matches!(style, Style::Oblique),
            };
            let applied = styled.copy_from(&outline).is_ok()
                && sized.synthesize(&mut styled, synthesis).is_ok();
            sink.record(Record::Styled {
                style,
                index: glyph.0,
                outline: applied.then_some(&styled),
            })?;
        }
    }
    Ok(())
}

/// The default-load advances. An advance the crate cannot give (`Unsupported`)
/// ends the operation with an error: it is not a value to compare.
fn hinted(font: &Font, sized: &SizedFont<'_>, sink: &mut impl Sink) -> Result<()> {
    for glyph in font.glyphs() {
        let advance = match sized.hinted_advance(glyph) {
            Ok(advance) => Some(advance),
            Err(triergon_core::Error::Unsupported(what)) => {
                return Err(
                    format!("glyph {}: hinted advance unsupported: {what}", glyph.0).into(),
                );
            }
            Err(_) => None,
        };
        sink.record(Record::Hinted {
            index: glyph.0,
            advance,
        })?;
    }
    Ok(())
}

enum Action {
    Fold,
    Bench(u64),
    Memory,
    Dump,
}

fn parse(args: &[String]) -> Result<(Action, Operation, &str)> {
    let (mode, rest) = args.split_first().ok_or(USAGE)?;
    let (operation, action) = mode.rsplit_once('-').ok_or(USAGE)?;
    let (action, rest) = match (action, rest) {
        ("bench", [path, count, rest @ ..]) => (Action::Bench(count.parse()?), (path, rest)),
        ("fold", [path, rest @ ..]) => (Action::Fold, (path, rest)),
        ("memory", [path, rest @ ..]) => (Action::Memory, (path, rest)),
        ("dump", [path, rest @ ..]) => (Action::Dump, (path, rest)),
        _ => return Err(USAGE.into()),
    };
    let (path, arguments) = rest;
    let operation = match (operation, arguments) {
        ("font", []) => Operation::Faces,
        ("font-scaled", [height]) => Operation::Scaled(height.parse()?),
        ("font-hinted", [height]) => Operation::Hinted(height.parse()?),
        _ => return Err(USAGE.into()),
    };
    Ok((action, operation, path))
}

fn run(args: &[String]) -> Result<()> {
    let (action, operation, path) = parse(args)?;
    let iterations = match action {
        Action::Bench(count) => count,
        _ => 1,
    };
    if !(1..=1_000_000).contains(&iterations) {
        return Err(USAGE.into());
    }
    let size = std::fs::metadata(path)?.len();
    if size > MAX_INPUT {
        return Err("input exceeds the font size bound".into());
    }
    let budget = Budget::new(1 << 30)?;
    let file = Bytes::try_from_vec(&budget, std::fs::read(path)?).map_err(|e| e.error())?;
    let retained = budget.used();
    let mut out = io::stdout().lock();
    if let Action::Dump = action {
        let mut dump = Dump(BufWriter::new(out));
        read(&file, &budget, operation, &mut dump)?;
        return Ok(dump.0.flush()?);
    }
    let memory = matches!(action, Action::Memory);
    if memory {
        out.write_all(b"TRIERGON_BEGIN_V1\n")?;
        out.flush()?;
    }
    let start = matches!(action, Action::Bench(_)).then(Instant::now);
    let mut total = Work::default();
    for _ in 0..iterations {
        let mut work = Work::default();
        read(std::hint::black_box(&file), &budget, operation, &mut work)?;
        total.accumulate(&work);
        if budget.used() != retained {
            return Err("font operation retained charged storage".into());
        }
    }
    let elapsed = start.map_or(0, |begin| begin.elapsed().as_nanos());
    if memory {
        out.write_all(b"TRIERGON_END_V1\n")?;
        out.flush()?;
    }
    writeln!(
        out,
        "{{\"schema\":1,\"operation\":\"{}\",\"iterations\":{iterations},\"packets\":{},\"bytes\":{},\"digest\":{},\"elapsed_ns\":{elapsed},\"eof\":true}}",
        operation.name(),
        total.packets,
        total.bytes,
        total.digest,
    )?;
    Ok(out.flush()?)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("font-worker: {error}");
            ExitCode::FAILURE
        }
    }
}
