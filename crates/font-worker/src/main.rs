//! Font measurement worker: every face of a font file read through triergon-font,
//! in the order of `harness/reference_font.h`. Reading the file, printing and process
//! startup are outside the timed loop.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]

mod record;

use record::{Dump, Record, Sink, Size, Style, Work};
use std::error::Error;
use std::io::{self, BufWriter, Write};
use std::process::ExitCode;
use triergon_core::{Budget, Bytes, WorkBudget};
use triergon_font::compat::libass::{self, Synthesis};
use triergon_font::{Font, GlyphId, Limits, Load, NameRecord, Outline, OutlineFormat, SizedFont};
use triergon_harness_runner::{
    Boundary, Invocation, Mode as RunMode, Timer, Totals, write_summary,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const MAX_INPUT: u64 = 64 << 20;
const USAGE: &str = "usage: font-worker OPERATION-fold FILE [MODE HEIGHT]
       | OPERATION-bench FILE ITERATIONS [MODE HEIGHT]
       | OPERATION-memory FILE [MODE HEIGHT] | OPERATION-dump FILE [MODE HEIGHT]
  OPERATION is font (no MODE or HEIGHT) or font-sized, with MODE outlines, advances,
  vertical or default and HEIGHT in 26.6 pixels, or several heights separated by
  commas, requested in turn";

/// The most heights one operation requests in turn.
const MAX_HEIGHTS: usize = 64;

/// What one operation reads from every face.
enum Operation {
    /// Face data and unscaled outlines.
    Faces,
    /// What a face reads at each of the heights, requested in turn on the same
    /// face as libass resizes one.
    Sized(Mode, Vec<i32>),
}

/// What a `font-sized` operation reads at a size.
#[derive(Clone, Copy)]
enum Mode {
    /// What libass obtains with hinting off: size metrics, kerning, and every
    /// glyph's unhinted outline and advance with its synthetic styles, each
    /// outline with its orientation.
    Scaled,
    /// The default-load (hinted) advance of every glyph.
    Hinted,
    /// The vertical advances of every glyph, unhinted and of the default load.
    Vertical,
    /// The default-load metrics and outline of every glyph.
    Default,
}

impl Operation {
    const fn name(&self) -> &'static str {
        match self {
            Self::Faces => "font-outlines",
            Self::Sized(Mode::Scaled, _) => "font-sized-outlines",
            Self::Sized(Mode::Hinted, _) => "font-sized-advances",
            Self::Sized(Mode::Vertical, _) => "font-sized-vertical",
            Self::Sized(Mode::Default, _) => "font-sized-default",
        }
    }
}

/// The ASCII form of a family or style name.
fn ascii(record: NameRecord<'_>) -> String {
    record.ascii().collect()
}

/// The PostScript form of a name, absent when nothing of it remains.
fn postscript(record: NameRecord<'_>) -> Option<String> {
    let text: String = record.postscript().collect();
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
fn read(file: &Bytes, budget: &Budget, operation: &Operation, sink: &mut impl Sink) -> Result<()> {
    let faces = Font::face_count(file.as_ref())?;
    for index in 0..faces {
        let Ok(font) = Font::new(file.clone(), index, budget, Limits::default()) else {
            sink.record(Record::FaceError(index))?;
            continue;
        };
        let Operation::Sized(mode, heights) = operation else {
            face(&font, &mut Outline::new(budget), sink)?;
            continue;
        };
        sink.record(Record::SizedFace(index))?;
        for &height in heights {
            if heights.len() > 1 {
                sink.record(Record::Request(height))?;
            }
            let Ok(sized) = libass::at_size(&font, height) else {
                sink.record(Record::SizeError(height))?;
                continue;
            };
            let measured = match mode {
                Mode::Scaled => {
                    scaled(&font, &sized, height, budget, sink)?;
                    continue;
                }
                Mode::Hinted => hinted(&font, &sized, sink),
                Mode::Vertical => vertical(&font, &sized, &mut Outline::new(budget), sink),
                Mode::Default => default(&font, &sized, &mut Outline::new(budget), sink),
            };
            measured.map_err(|e| format!("face {index} at {height}: {e}"))?;
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
    sink.record(Record::TextCharmap(libass::text_charmap(font)))?;
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
        font.outline_format() == OutlineFormat::Cff,
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
            // `synthesize` answers `false` where it refuses to embolden: an
            // outline without an orientation.
            let applied = styled.copy_from(&outline).is_ok()
                && matches!(libass::synthesize(sized, &mut styled, synthesis), Ok(true));
            sink.record(Record::Styled {
                style,
                index: glyph.0,
                outline: applied.then_some(&styled),
            })?;
        }
    }
    Ok(())
}

/// What the default load gives for `glyph`, absent where it fails. What the
/// crate cannot give (`Unsupported`) ends the operation with an error naming
/// `what`: it is not a value to compare.
fn supported<T>(loaded: triergon_core::Result<T>, glyph: GlyphId, what: &str) -> Result<Option<T>> {
    match loaded {
        Ok(value) => Ok(Some(value)),
        Err(triergon_core::Error::Unsupported(why)) => {
            Err(format!("glyph {}: {what} unsupported: {why}", glyph.0).into())
        }
        Err(_) => Ok(None),
    }
}

/// The default-load advances.
fn hinted(font: &Font, sized: &SizedFont<'_>, sink: &mut impl Sink) -> Result<()> {
    for glyph in font.glyphs() {
        let advance = supported(sized.default_advance(glyph), glyph, "hinted advance")?;
        sink.record(Record::Hinted {
            index: glyph.0,
            advance,
        })?;
    }
    Ok(())
}

/// The vertical advances: of the unhinted load where the glyph's outline
/// loads, and of the default load.
fn vertical(
    font: &Font,
    sized: &SizedFont<'_>,
    outline: &mut Outline,
    sink: &mut impl Sink,
) -> Result<()> {
    for glyph in font.glyphs() {
        let unhinted = sized.outline_into(glyph, outline).ok();
        let hinted = supported(
            sized.default_vertical_advance(glyph),
            glyph,
            "hinted vertical advance",
        )?;
        sink.record(Record::Vertical {
            index: glyph.0,
            unhinted,
            hinted,
        })?;
    }
    Ok(())
}

/// The default-load metrics and outlines.
fn default(
    font: &Font,
    sized: &SizedFont<'_>,
    outline: &mut Outline,
    sink: &mut impl Sink,
) -> Result<()> {
    for glyph in font.glyphs() {
        let mut work = WorkBudget::new(Limits::default().work);
        let load = sized.load(glyph, Load::Default, outline, &mut work);
        let loaded = supported(load, glyph, "default load")?;
        sink.record(Record::Default {
            index: glyph.0,
            loaded: loaded.map(|m| (m, &*outline)),
        })?;
    }
    Ok(())
}

/// A height, or several separated by commas.
fn heights(text: &str) -> Result<Vec<i32>> {
    let heights = text
        .split(',')
        .map(str::parse)
        .collect::<std::result::Result<Vec<i32>, _>>()?;
    if heights.len() > MAX_HEIGHTS {
        return Err(USAGE.into());
    }
    Ok(heights)
}

fn parse(args: &[String]) -> Result<(Invocation<'_>, Operation)> {
    let (mode, rest) = args.split_first().ok_or(USAGE)?;
    let (operation, action) = mode.rsplit_once('-').ok_or(USAGE)?;
    let invocation = Invocation::parse_mode(action, rest, Some("dump"), || USAGE)?;
    let arguments = invocation.arguments;
    let operation = match (operation, arguments) {
        ("font", []) => Operation::Faces,
        ("font-sized", [mode, height]) => {
            let mode = match mode.as_str() {
                "outlines" => Mode::Scaled,
                "advances" => Mode::Hinted,
                "vertical" => Mode::Vertical,
                "default" => Mode::Default,
                _ => return Err(USAGE.into()),
            };
            Operation::Sized(mode, heights(height)?)
        }
        _ => return Err(USAGE.into()),
    };
    Ok((invocation, operation))
}

fn run(args: &[String]) -> Result<()> {
    let (invocation, operation) = parse(args)?;
    invocation.check_iterations(|| USAGE)?;
    let Invocation {
        mode: action,
        path,
        iterations,
        ..
    } = invocation;
    let size = std::fs::metadata(path)?.len();
    if size > MAX_INPUT {
        return Err("input exceeds the font size bound".into());
    }
    let budget = Budget::new(1 << 30)?;
    let file = Bytes::try_from_vec(&budget, std::fs::read(path)?).map_err(|e| e.error)?;
    let retained = budget.used();
    let mut out = io::stdout().lock();
    if action == RunMode::Trace {
        let mut dump = Dump(BufWriter::new(out));
        read(&file, &budget, &operation, &mut dump)?;
        return Ok(dump.0.flush()?);
    }
    let memory = action == RunMode::Memory;
    if memory {
        Boundary::Begin.mark(&mut out)?;
    }
    let start = Timer::start(action == RunMode::Bench);
    let mut total = Work::default();
    for _ in 0..iterations {
        let mut work = Work::default();
        read(std::hint::black_box(&file), &budget, &operation, &mut work)?;
        total.accumulate(&work);
        if budget.used() != retained {
            return Err("font operation retained charged storage".into());
        }
    }
    let elapsed = start.elapsed().as_nanos();
    if memory {
        Boundary::End.mark(&mut out)?;
    }
    Ok(write_summary(
        &mut out,
        operation.name(),
        iterations,
        elapsed,
        &Totals {
            packets: total.packets,
            bytes: total.bytes,
            digest: total.digest,
            refusal: None,
        },
    )?)
}

#[cfg_attr(test, allow(dead_code))]
fn worker_main() -> ExitCode {
    triergon_harness_runner::command(Some("font-worker"), 1, run)
}

/// C's entry point, the one exception to the worker's `deny(unsafe_code)`
/// (`triergon-worker-entry`).
#[cfg(not(test))]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
extern "C" fn main(
    _argc: std::ffi::c_int,
    _argv: *const *const std::ffi::c_char,
) -> std::ffi::c_int {
    triergon_worker_entry::enter(worker_main)
}
