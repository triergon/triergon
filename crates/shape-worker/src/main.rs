//! Shaping measurement worker: text shaped with every face of a font file, bidi
//! resolved and Unicode properties read through triergon-shape, in the order of
//! `harness/reference_shape.h`. Reading the file, printing and process startup are
//! outside the timed loop.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]

mod record;
mod unicode;

use record::{Dump, Record, Sink, Work};
use std::error::Error;
use std::io::{self, BufWriter, Write};
use std::ops::Range;
use std::process::ExitCode;
use triergon_core::{Budget, Bytes, WorkBudget};
use triergon_font::compat::libass;
use triergon_font::{Font, Limits};
use triergon_harness_runner::{
    Boundary, Invocation, Mode as RunMode, Timer, Totals, write_summary,
};
use triergon_shape::bidi::{self, BaseDirection, BidiClass, Bracket, Level, Resolver};
use triergon_shape::compat::libass::SizedMetrics;
use triergon_shape::{
    Buffer, Direction, Face, Feature, Language, Metrics, OtMetrics, Plan, Scale, Script, Tag,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const MAX_INPUT: u64 = 64 << 20;
/// Code points of one text, as the reference bounds them.
const MAX_TEXT: usize = 65536;
const MAX_FEATURES: usize = 64;
const USAGE: &str = "usage: shape-worker OPERATION-fold FILE ARGUMENT...
       | OPERATION-bench FILE ITERATIONS ARGUMENT...
       | OPERATION-memory FILE ARGUMENT... | OPERATION-dump FILE ARGUMENT...
  OPERATION: shape (DIRECTION SCRIPT LANGUAGE FEATURES TEXT...),
  shape-libass (SIZE DIRECTION SCRIPT LANGUAGE FEATURES TEXT...), bidi (BASE BRACKETS),
  simple (BASE), unicode or extents";

/// What one operation does with its file.
enum Operation {
    /// Shape every text with every face of the font file.
    Shape(Settings),
    /// Resolve and reorder every line of the text file.
    Bidi(BaseDirection, bool),
    /// libass's simple shaping of every line of the text file.
    Simple(BaseDirection),
    /// The properties of every code point of the file's ranges.
    Unicode,
    /// Every glyph's extents from every face of the font file.
    Extents,
}

impl Operation {
    const fn name(&self) -> &'static str {
        match self {
            Self::Shape(settings) => match settings.functions {
                Functions::OpenType => "shape-text",
                Functions::Libass(_) => "shape-libass",
            },
            Self::Bidi(..) => "shape-bidi",
            Self::Simple(_) => "shape-simple",
            Self::Unicode => "shape-unicode",
            Self::Extents => "shape-extents",
        }
    }
}

/// The font functions a shape operation shapes with.
#[derive(Clone, Copy)]
enum Functions {
    /// HarfBuzz's own, at units per em.
    OpenType,
    /// libass's, at a 26.6 pixel height.
    Libass(i32),
}

/// The font functions, segment properties and features of a shape operation,
/// and its texts.
struct Settings {
    functions: Functions,
    direction: Direction,
    script: Script,
    language: String,
    features: Vec<Feature>,
    texts: Vec<String>,
}

// ----------------------------------------------------------------- parsing

/// One to six upper-case hex digits naming a Unicode scalar value.
fn scalar(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() || digits.len() > 6 {
        return None;
    }
    let mut value = 0_u32;
    for &d in digits {
        let digit = match d {
            b'0'..=b'9' => d - b'0',
            b'A'..=b'F' => d - b'A' + 10,
            _ => return None,
        };
        value = value * 16 + u32::from(digit);
    }
    (value <= 0x10FFFF && !(0xD800..=0xDFFF).contains(&value)).then_some(value)
}

/// A decimal number of one to seven digits.
fn decimal(digits: &[u8]) -> Option<usize> {
    if digits.is_empty() || digits.len() > 7 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(
        digits
            .iter()
            .fold(0, |n, &d| n * 10 + usize::from(d - b'0')),
    )
}

/// Scalar values separated by single spaces: at least one, at most `MAX_TEXT`.
fn text(field: &[u8]) -> Option<Vec<char>> {
    let mut chars = Vec::new();
    for token in field.split(|&b| b == b' ') {
        if chars.len() == MAX_TEXT {
            return None;
        }
        chars.push(char::from_u32(scalar(token)?)?);
    }
    Some(chars)
}

/// A text argument, `[START,LENGTH:]CODE POINTS`: the text and its item.
fn item(arg: &str) -> Option<(Vec<char>, Range<usize>)> {
    let bytes = arg.as_bytes();
    let (head, points) = match bytes.iter().position(|&b| b == b':') {
        Some(colon) => (Some(&bytes[..colon]), &bytes[colon + 1..]),
        None => (None, bytes),
    };
    let text = text(points)?;
    let Some(head) = head else {
        let len = text.len();
        return Some((text, 0..len));
    };
    let comma = head.iter().position(|&b| b == b',')?;
    let (start, length) = (decimal(&head[..comma])?, decimal(&head[comma + 1..])?);
    (start <= text.len() && length <= text.len() - start).then_some((text, start..start + length))
}

fn tag(field: &[u8]) -> Option<[u8; 4]> {
    let tag: [u8; 4] = field.try_into().ok()?;
    tag.iter().all(|b| (0x20..=0x7E).contains(b)).then_some(tag)
}

/// `-`, or comma-separated `TAG=VALUE`.
fn features(field: &str) -> Option<Vec<Feature>> {
    if field == "-" {
        return Some(Vec::new());
    }
    let mut features = Vec::new();
    for feature in field.split(',') {
        if features.len() == MAX_FEATURES {
            return None;
        }
        let (name, value) = feature.split_once('=')?;
        features.push(Feature {
            tag: Tag::new(&tag(name.as_bytes())?),
            value: u32::try_from(decimal(value.as_bytes())?).ok()?,
        });
    }
    Some(features)
}

fn base(field: &str) -> Option<BaseDirection> {
    match field {
        "ltr" => Some(BaseDirection::LeftToRight),
        "rtl" => Some(BaseDirection::RightToLeft),
        "on" => Some(BaseDirection::Auto),
        _ => None,
    }
}

/// The lines of a text file, without their newlines.
fn lines(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = data;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let end = rest.iter().position(|&b| b == b'\n');
        let line = &rest[..end.unwrap_or(rest.len())];
        rest = end.map_or(&[][..], |at| &rest[at + 1..]);
        Some(line)
    })
}

/// A text line, with `|` and comma-separated line starts in `1..len` when
/// `with_starts`.
fn bidi_line(line: &[u8], with_starts: bool) -> Option<(Vec<char>, Vec<usize>)> {
    let bar = with_starts
        .then(|| line.iter().position(|&b| b == b'|'))
        .flatten();
    let text = text(&line[..bar.unwrap_or(line.len())])?;
    let mut starts = Vec::new();
    if let Some(bar) = bar {
        for field in line[bar + 1..].split(|&b| b == b',') {
            let at = decimal(field)?;
            if at == 0 || at >= text.len() {
                return None;
            }
            starts.push(at);
        }
    }
    starts.sort_unstable();
    starts.dedup();
    Some((text, starts))
}

// -------------------------------------------------------------- operations

fn shape_text(
    file: &Bytes,
    budget: &Budget,
    settings: &Settings,
    sink: &mut impl Sink,
) -> Result<()> {
    let faces = Font::face_count(file.as_ref()).unwrap_or(0);
    let mut buffer = Buffer::new(budget);
    for index in 0..faces {
        let Ok(font) = Font::new(file.clone(), index, budget, Limits::default()) else {
            sink.record(Record::FaceError(index))?;
            continue;
        };
        match settings.functions {
            Functions::OpenType => {
                sink.record(Record::Face(index))?;
                let metrics = OtMetrics::new(&font, Scale::units(font.units_per_em()));
                shape_texts(&font, &metrics, budget, settings, &mut buffer, sink)?;
            }
            Functions::Libass(height) => {
                // The comparison measures answers, not work: a face's runs
                // share an unbounded budget.
                let mut work = WorkBudget::new(u64::MAX);
                let Ok(metrics) = libass::at_size(&font, height)
                    .and_then(|sized| SizedMetrics::new(sized, &mut work))
                else {
                    sink.record(Record::FaceError(index))?;
                    continue;
                };
                sink.record(Record::Face(index))?;
                shape_texts(&font, &metrics, budget, settings, &mut buffer, sink)?;
            }
        }
    }
    Ok(())
}

/// Every text of the operation shaped as one run with `font` and `metrics`.
fn shape_texts(
    font: &Font,
    metrics: &dyn Metrics,
    budget: &Budget,
    settings: &Settings,
    buffer: &mut Buffer,
    sink: &mut impl Sink,
) -> Result<()> {
    let face = Face::new(font);
    let plan = Plan::new(
        &face,
        settings.direction,
        settings.script,
        &Language::new(&settings.language),
        &settings.features,
        budget,
    )
    .ok();
    for (t, argument) in settings.texts.iter().enumerate() {
        let Some((text, item)) = item(argument) else {
            sink.record(Record::TextError(t))?;
            continue;
        };
        let Some(plan) = &plan else {
            sink.record(Record::ShapeError(t))?;
            continue;
        };
        let length = item.len();
        if triergon_shape::shape(&face, metrics, plan, &text, item, buffer).is_err() {
            sink.record(Record::ShapeError(t))?;
            continue;
        }
        sink.record(Record::Text(buffer.len()))?;
        for (glyph, p) in buffer.glyphs() {
            sink.record(Record::Glyph {
                id: glyph.id.0,
                cluster: glyph.cluster,
                x_advance: p.x_advance,
                y_advance: p.y_advance,
                x_offset: p.x_offset,
                y_offset: p.y_offset,
            })?;
        }
        sink.count(buffer.len(), length);
    }
    Ok(())
}

/// Every glyph's extents through [`OtMetrics`] at units per em, the font
/// functions `shape` uses, for every face of the file.
fn shape_extents(file: &Bytes, budget: &Budget, sink: &mut impl Sink) -> Result<()> {
    let faces = Font::face_count(file.as_ref()).unwrap_or(0);
    for index in 0..faces {
        let Ok(font) = Font::new(file.clone(), index, budget, Limits::default()) else {
            sink.record(Record::FaceError(index))?;
            continue;
        };
        sink.record(Record::Face(index))?;
        sink.record(Record::Glyphs(font.glyph_count()))?;
        let metrics = OtMetrics::new(&font, Scale::units(font.units_per_em()));
        for glyph in font.glyphs() {
            let record = match metrics.extents(glyph) {
                Ok(extents) => Record::Extents {
                    id: glyph.0,
                    extents: extents.map(|e| [e.x_bearing, e.y_bearing, e.width, e.height]),
                },
                Err(_) => Record::ExtentsError(glyph.0),
            };
            sink.record(record)?;
        }
        sink.count(usize::from(font.glyph_count()), 0);
    }
    Ok(())
}

/// Levels per paragraph, then each line reordered with its paragraph's
/// direction, as libass resolves a whole event.
fn resolve(
    resolver: &mut Resolver,
    text: &[char],
    starts: &[usize],
    base: BaseDirection,
    brackets: bool,
) -> Result<(Vec<Direction>, Vec<Level>, Vec<usize>)> {
    let classes: Vec<BidiClass> = text.iter().map(|&c| BidiClass::of(c)).collect();
    let pairs: Option<Vec<Option<Bracket>>> =
        brackets.then(|| text.iter().map(|&c| Bracket::of(c)).collect());
    let mut levels = vec![Level::default(); text.len()];
    let mut directions = Vec::new();
    for paragraph in bidi::paragraphs(&classes) {
        let pairs = pairs.as_deref().map(|p| &p[paragraph.clone()]);
        let classes = &classes[paragraph.clone()];
        directions.push(resolver.resolve(classes, pairs, base, &mut levels[paragraph])?);
    }
    let mut map: Vec<usize> = (0..text.len()).collect();
    let (mut start, mut paragraph) = (0, 0);
    for end in 1..=text.len() {
        let separator = classes[end - 1] == BidiClass::B;
        if end < text.len() && !separator && starts.binary_search(&end).is_err() {
            continue;
        }
        let line = start..end;
        let direction = directions[paragraph];
        bidi::reorder_line(
            &classes[line.clone()],
            direction,
            &mut levels[line.clone()],
            &mut map[line],
        )?;
        start = end;
        if separator && paragraph + 1 < directions.len() {
            paragraph += 1;
        }
    }
    Ok((directions, levels, map))
}

fn shape_bidi(
    data: &[u8],
    budget: &Budget,
    base: BaseDirection,
    brackets: bool,
    sink: &mut impl Sink,
) -> Result<()> {
    let mut resolver = Resolver::new(budget);
    for (index, line) in lines(data).enumerate() {
        let Some((text, starts)) = bidi_line(line, true) else {
            sink.record(Record::BidiLineError(index))?;
            continue;
        };
        let (directions, levels, map) = resolve(&mut resolver, &text, &starts, base, brackets)?;
        sink.record(Record::BidiLine(text.len()))?;
        for direction in directions {
            sink.record(Record::Paragraph(direction == Direction::RightToLeft))?;
        }
        for level in levels {
            sink.record(Record::Level(level.get()))?;
        }
        for at in map {
            sink.record(Record::Map(at))?;
        }
        sink.count(1, text.len());
    }
    Ok(())
}

fn shape_simple(
    data: &[u8],
    budget: &Budget,
    base: BaseDirection,
    sink: &mut impl Sink,
) -> Result<()> {
    let mut resolver = Resolver::new(budget);
    for (index, line) in lines(data).enumerate() {
        let Some((mut text, _)) = bidi_line(line, false) else {
            sink.record(Record::SimpleLineError(index))?;
            continue;
        };
        let classes: Vec<BidiClass> = text.iter().map(|&c| BidiClass::of(c)).collect();
        let mut levels = vec![Level::default(); text.len()];
        resolver.resolve(&classes, None, base, &mut levels)?;
        bidi::shape_simple(&mut text, &classes, &levels)?;
        sink.record(Record::SimpleLine(text.len()))?;
        for c in &text {
            sink.record(Record::Char(u32::from(*c)))?;
        }
        sink.count(1, text.len());
    }
    Ok(())
}

fn shape_unicode(data: &[u8], sink: &mut impl Sink) -> Result<()> {
    for (index, line) in lines(data).enumerate() {
        let range = line.iter().position(|&b| b == b' ').and_then(|space| {
            let (first, last) = (scalar(&line[..space])?, scalar(&line[space + 1..])?);
            (first <= last).then_some((first, last))
        });
        let Some((first, last)) = range else {
            sink.record(Record::RangeError(index))?;
            continue;
        };
        sink.record(Record::Range(first, last))?;
        for cp in first..=last {
            sink.record(Record::Point(unicode::point(cp)))?;
            sink.count(1, 0);
        }
    }
    Ok(())
}

fn read(file: &Bytes, budget: &Budget, operation: &Operation, sink: &mut impl Sink) -> Result<()> {
    match operation {
        Operation::Shape(settings) => shape_text(file, budget, settings, sink),
        Operation::Bidi(base, brackets) => {
            shape_bidi(file.as_ref(), budget, *base, *brackets, sink)
        }
        Operation::Simple(base) => shape_simple(file.as_ref(), budget, *base, sink),
        Operation::Unicode => shape_unicode(file.as_ref(), sink),
        Operation::Extents => shape_extents(file, budget, sink),
    }
}

// --------------------------------------------------------------------- main

fn parse(args: &[String]) -> Result<(Invocation<'_>, Operation)> {
    let (mode, rest) = args.split_first().ok_or(USAGE)?;
    let (operation, action) = mode.rsplit_once('-').ok_or(USAGE)?;
    let invocation = Invocation::parse_mode(action, rest, Some("dump"), || USAGE)?;
    let arguments = invocation.arguments;
    let operation = match (operation, arguments) {
        ("shape", arguments) => Operation::Shape(shape_settings(Functions::OpenType, arguments)?),
        ("shape-libass", [size, arguments @ ..]) => {
            let height = decimal(size.as_bytes())
                .and_then(|h| i32::try_from(h).ok())
                .filter(|&h| h > 0)
                .ok_or(USAGE)?;
            Operation::Shape(shape_settings(Functions::Libass(height), arguments)?)
        }
        ("bidi", [base, brackets]) if matches!(brackets.as_str(), "0" | "1") => {
            Operation::Bidi(self::base(base).ok_or(USAGE)?, brackets == "1")
        }
        ("simple", [base]) => Operation::Simple(self::base(base).ok_or(USAGE)?),
        ("unicode", []) => Operation::Unicode,
        ("extents", []) => Operation::Extents,
        _ => return Err(USAGE.into()),
    };
    Ok((invocation, operation))
}

/// `DIRECTION SCRIPT LANGUAGE FEATURES TEXT...`.
fn shape_settings(functions: Functions, arguments: &[String]) -> Result<Settings> {
    let [direction, script, language, features, texts @ ..] = arguments else {
        return Err(USAGE.into());
    };
    let direction = match direction.as_str() {
        "ltr" => Direction::LeftToRight,
        "rtl" => Direction::RightToLeft,
        _ => return Err(USAGE.into()),
    };
    let script = tag(script.as_bytes()).ok_or(USAGE)?;
    if language.is_empty() {
        return Err(USAGE.into());
    }
    Ok(Settings {
        functions,
        direction,
        script: Script::from_iso15924(script),
        language: language.clone(),
        features: self::features(features).ok_or(USAGE)?,
        texts: texts.to_vec(),
    })
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
        return Err("input exceeds the size bound".into());
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
            return Err("shaping operation retained charged storage".into());
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
    triergon_harness_runner::command(Some("shape-worker"), 1, run)
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
