//! Font measurement worker: every face of a font file read through triergon-font,
//! in the order of `harness/reference_font.h`. Reading the file, printing and process
//! startup are outside the timed loop.
#![forbid(unsafe_code)]

mod record;

use record::{Dump, Record, Sink, Work};
use std::error::Error;
use std::io::{self, BufWriter, Write};
use std::process::ExitCode;
use std::time::Instant;
use triergon_core::{Budget, Bytes};
use triergon_font::{Font, GlyphId, Limits, NameRecord, Outline, platform};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const OPERATION: &str = "font-outlines";
const MAX_INPUT: u64 = 64 << 20;
const USAGE: &str = "usage: font-worker font-fold FILE | font-bench FILE ITERATIONS | font-memory FILE | font-dump FILE";

/// FreeType's ASCII form of a name: UTF-16 units, or bytes for Macintosh records,
/// up to a NUL, with units outside 32..=127 shown as `?`.
fn ascii(record: NameRecord<'_>) -> String {
    units(&record)
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
    let text: String = units(&record)
        .filter_map(|u| u8::try_from(u).ok())
        .filter(|&b| (33..=126).contains(&b) && !b"[](){}<>/%".contains(&b))
        .map(char::from)
        .collect();
    (!text.is_empty()).then_some(text)
}

fn units<'a>(record: &NameRecord<'a>) -> Box<dyn Iterator<Item = u32> + 'a> {
    if record.platform == platform::MACINTOSH {
        Box::new(record.bytes.iter().map(|&b| u32::from(b)))
    } else {
        Box::new(
            record
                .bytes
                .chunks_exact(2)
                .map(|u| u32::from(u16::from_be_bytes([u[0], u[1]]))),
        )
    }
}

/// One operation: parse every face of `file` and read everything about it.
fn read(file: &Bytes, budget: &Budget, sink: &mut impl Sink) -> Result<()> {
    let faces = Font::face_count(file.as_ref())?;
    let mut outline = Outline::new(budget);
    for index in 0..faces {
        match Font::new(file.clone(), index, budget, Limits::default()) {
            Ok(font) => face(&font, &mut outline, sink)?,
            Err(_) => sink.record(Record::FaceError(index))?,
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
        max_advance: font.max_advance(),
        bold: style.bold,
        italic: style.italic,
    })?;
    sink.record(Record::Family(font.family_name().map(ascii).as_deref()))?;
    sink.record(Record::Style(font.style_name().map(ascii).as_deref()))?;
    sink.record(Record::PostScript(
        font.postscript_name().and_then(postscript).as_deref(),
    ))?;
    sink.record(Record::Weight(font.os2().map_or(0, |o| o.weight_class)))?;
    for name in font.names() {
        sink.record(Record::Name {
            platform: name.platform,
            encoding: name.encoding,
            language: name.language,
            id: name.name_id,
            bytes: name.bytes,
        })?;
    }
    for charmap in font.charmaps() {
        sink.record(Record::Charmap(charmap))?;
        for (code, glyph) in font.mappings(charmap) {
            sink.record(Record::Map {
                code,
                glyph: glyph.0,
            })?;
        }
    }
    sink.record(Record::TextCharmap(font.text_charmap()))?;
    // Every pair the kerning subtables list, once, in glyph order.
    let mut pairs: Vec<(GlyphId, GlyphId)> = font.kerning_pairs().collect();
    pairs.sort_unstable();
    pairs.dedup();
    for (left, right) in pairs {
        let value = font.kerning(left, right);
        if value != 0 {
            sink.record(Record::Kern {
                left: left.0,
                right: right.0,
                value,
            })?;
        }
    }
    for glyph in (0..font.glyph_count()).map(GlyphId) {
        let advance = font.advance(glyph).map_or(-1, i64::from);
        match font.outline_into(glyph, outline) {
            Ok(loaded) => sink.record(Record::Glyph {
                index: glyph.0,
                advance,
                loaded: loaded.advance.into(),
                outline,
            })?,
            Err(_) => sink.record(Record::GlyphError(glyph.0))?,
        }
    }
    Ok(())
}

fn run(args: &[String]) -> Result<()> {
    let (mode, path, iterations) = match args {
        [mode, path] if matches!(mode.as_str(), "font-fold" | "font-memory" | "font-dump") => {
            (mode.as_str(), path, 1)
        }
        [mode, path, count] if mode == "font-bench" => (mode.as_str(), path, count.parse::<u64>()?),
        _ => return Err(USAGE.into()),
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
    if mode == "font-dump" {
        let mut dump = Dump(BufWriter::new(out));
        read(&file, &budget, &mut dump)?;
        return Ok(dump.0.flush()?);
    }
    let memory = mode == "font-memory";
    if memory {
        out.write_all(b"TRIERGON_BEGIN_V1\n")?;
        out.flush()?;
    }
    let start = (mode == "font-bench").then(Instant::now);
    let mut total = Work::default();
    for _ in 0..iterations {
        let mut work = Work::default();
        read(std::hint::black_box(&file), &budget, &mut work)?;
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
        "{{\"schema\":1,\"operation\":\"{OPERATION}\",\"iterations\":{iterations},\"packets\":{},\"bytes\":{},\"digest\":{},\"elapsed_ns\":{elapsed},\"eof\":true}}",
        total.packets, total.bytes, total.digest,
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
