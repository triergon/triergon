//! Shaping measurement worker: text shaped with every face of a font file, bidi
//! resolved and Unicode properties read through triergon-shape, in the order of
//! `harness/reference_shape.h`. Reading the file, printing and process startup are
//! outside the timed loop.
#![forbid(unsafe_code)]

mod record;

use record::{Dump, Record, Sink, Work};
use std::error::Error;
use std::io::{self, BufWriter, Write};
use std::ops::Range;
use std::process::ExitCode;
use std::time::Instant;
use triergon_core::{Budget, Bytes};
use triergon_font::{Font, Limits};
use triergon_shape::bidi::{self, BaseDirection, BidiClass, Bracket, Level, Resolver};
use triergon_shape::{
    Buffer, Direction, Face, Feature, Language, Metrics, OtMetrics, Plan, Scale, Script,
    SizedMetrics, Tag,
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
  simple (BASE) or unicode";

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
                let Ok(metrics) = font.at_size(height).and_then(SizedMetrics::new) else {
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
    let plan = Language::new(&settings.language).ok().and_then(|language| {
        let (direction, script) = (settings.direction, settings.script);
        Plan::new(
            &face,
            direction,
            script,
            &language,
            &settings.features,
            budget,
        )
        .ok()
    });
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
            let p = triergon_shape::reference_properties(cp);
            let optional = |v: Option<u32>| v.map_or(-1, i64::from);
            let (a, b) = p.decomposition.unzip();
            sink.record(Record::Point([
                cp.into(),
                p.general_category.into(),
                p.combining_class.into(),
                p.script.into(),
                p.mirroring.into(),
                p.bidi_type.into(),
                p.bracket.into(),
                optional(p.mirror),
                p.joining_type.into(),
                optional(a),
                optional(b),
            ]))?;
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
    }
}

// --------------------------------------------------------------------- main

enum Action {
    Fold,
    Bench(u64),
    Memory,
    Dump,
}

fn parse(args: &[String]) -> Result<(Action, Operation, &str)> {
    let (mode, rest) = args.split_first().ok_or(USAGE)?;
    let (operation, action) = mode.rsplit_once('-').ok_or(USAGE)?;
    let (action, path, arguments) = match (action, rest) {
        ("bench", [path, count, rest @ ..]) => (Action::Bench(count.parse()?), path, rest),
        ("fold", [path, rest @ ..]) => (Action::Fold, path, rest),
        ("memory", [path, rest @ ..]) => (Action::Memory, path, rest),
        ("dump", [path, rest @ ..]) => (Action::Dump, path, rest),
        _ => return Err(USAGE.into()),
    };
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
        _ => return Err(USAGE.into()),
    };
    Ok((action, operation, path))
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
        return Err("input exceeds the size bound".into());
    }
    let budget = Budget::new(1 << 30)?;
    let file = Bytes::try_from_vec(&budget, std::fs::read(path)?).map_err(|e| e.error())?;
    let retained = budget.used();
    let mut out = io::stdout().lock();
    if let Action::Dump = action {
        let mut dump = Dump(BufWriter::new(out));
        read(&file, &budget, &operation, &mut dump)?;
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
        read(std::hint::black_box(&file), &budget, &operation, &mut work)?;
        total.accumulate(&work);
        if budget.used() != retained {
            return Err("shaping operation retained charged storage".into());
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
            eprintln!("shape-worker: {error}");
            ExitCode::FAILURE
        }
    }
}
