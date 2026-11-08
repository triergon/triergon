//! Prints every face of a font file in the line format of `harness/freetype-font.c`,
//! the FreeType reference, so `cargo xtask fonts` can compare them.
//!
//! Usage: font-dump FILE
//!
//! Outline coordinates are font units times 64, rounded to the nearest integer.

use std::io::{self, BufWriter, Write};
use std::process::ExitCode;
use triergon_core::{Budget, Bytes, Error};
use triergon_font::{
    Coordinate, Font, GlyphId, Limits, NameRecord, Outline, PointKind, ScaledOutline, Synthesis,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (sizes, path) = match &args[..] {
        [path] => (None, path),
        [flag, sizes, path] if flag == "--scaled" => {
            let parsed: Result<Vec<i32>, _> = sizes.split(',').map(str::parse).collect();
            let Ok(sizes) = parsed else {
                eprintln!("sizes must be 26.6 heights separated by commas");
                return ExitCode::from(2);
            };
            (Some(sizes), path)
        }
        _ => {
            eprintln!("usage: font-dump [--scaled HEIGHT,...] FILE");
            return ExitCode::from(2);
        }
    };
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(error) => {
            eprintln!("{path}: {error}");
            return ExitCode::from(2);
        }
    };
    let budget = Budget::new(1 << 30).expect("budget");
    let bytes = Bytes::copy_from_slice(&budget, &data).expect("font bytes");
    let faces = match Font::face_count(&data) {
        Ok(faces) => faces,
        Err(error) => {
            eprintln!("{path}: {error}");
            return ExitCode::from(2);
        }
    };
    let mut out = BufWriter::new(io::stdout().lock());
    let mut failed = false;
    for index in 0..faces {
        match Font::new(bytes.clone(), index, &budget, Limits::default()) {
            Ok(font) => match &sizes {
                None => dump(&font, &budget, &mut out).expect("stdout"),
                Some(sizes) => {
                    writeln!(out, "face {index}").expect("stdout");
                    for (i, &height) in sizes.iter().enumerate() {
                        dump_size(&font, height, i == 0, &budget, &mut out).expect("stdout");
                    }
                }
            },
            Err(error) if sizes.is_some() => {
                writeln!(out, "face {index} error").expect("stdout");
                eprintln!("face {index}: {error}");
                failed = true;
            }
            Err(error) => {
                writeln!(out, "face {index} error {error}").expect("stdout");
                failed = true;
            }
        }
    }
    out.flush().expect("stdout");
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// FreeType's ASCII form of a name: UTF-16 units, or bytes for Macintosh records,
/// up to a NUL, with units outside 32..=127 shown as `?`.
fn ascii(record: Option<NameRecord<'_>>) -> String {
    let Some(record) = record else {
        return "-".into();
    };
    record
        .code_units()
        .map(u32::from)
        .take_while(|&u| u != 0)
        .map(|u| {
            u8::try_from(u)
                .ok()
                .filter(|b| (32..=127).contains(b))
                .map_or('?', char::from)
        })
        .collect()
}

/// FreeType's PostScript form: printable ASCII other than `[](){}<>/%`.
fn postscript(record: Option<NameRecord<'_>>) -> String {
    let Some(record) = record else {
        return "-".into();
    };
    let text: String = record
        .code_units()
        .map(u32::from)
        .filter_map(|u| u8::try_from(u).ok())
        .filter(|&b| (33..=126).contains(&b) && !b"[](){}<>/%".contains(&b))
        .map(char::from)
        .collect();
    if text.is_empty() { "-".into() } else { text }
}

/// The glyphs whose kerning is compared: the first 256 and the text glyphs of
/// U+0020..U+00FF.
fn kerned_glyphs(font: &Font) -> Vec<GlyphId> {
    let mut kerned: Vec<GlyphId> = (0..font.glyph_count().min(256)).map(GlyphId).collect();
    for code in 0x20..=0xff_u8 {
        if let Some(glyph) = font.glyph_index(char::from(code)).filter(|g| g.0 >= 256) {
            kerned.push(glyph);
        }
    }
    kerned
}

/// The outline part of a `glyph` line: `n | ends | x y tag ...`.
fn outline_text<C: Coordinate + Into<f64>>(outline: &Outline<C>, scale: f64, text: &mut Vec<u8>) {
    text.clear();
    write!(text, "{} |", outline.contour_ends().len()).expect("buffer");
    for end in outline.contour_ends() {
        write!(text, " {end}").expect("buffer");
    }
    write!(text, " |").expect("buffer");
    for p in outline.points() {
        let tag = match p.kind {
            PointKind::On => 1,
            PointKind::Quadratic => 0,
            PointKind::Cubic => 2,
        };
        let v = |c: C| (c.into() * scale).round() as i64;
        write!(text, " {} {} {tag}", v(p.point.x), v(p.point.y)).expect("buffer");
    }
}

/// FNV-1a, 64 bits.
fn digest(text: &[u8]) -> u64 {
    text.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

/// One size in the format of the reference's `--scaled` mode.
fn dump_size(
    font: &Font,
    height: i32,
    detailed: bool,
    budget: &Budget,
    out: &mut impl Write,
) -> io::Result<()> {
    let Ok(sized) = font.at_size(height) else {
        return writeln!(out, "size {height} error");
    };
    let m = sized.metrics();
    let (ascent, descent) = sized.ascent_descent();
    let (hb_x, hb_y) = sized.harfbuzz_scale();
    let strength = sized.embolden_strength();
    write!(
        out,
        "size {height} {} {} {} {} {} {} {} {} {ascent} {descent} {hb_x} {hb_y} {strength} {}",
        m.x_ppem,
        m.y_ppem,
        m.x_scale,
        m.y_scale,
        m.ascender,
        m.descender,
        m.height,
        m.max_advance,
        u8::from(font.is_postscript())
    )?;
    for (key, band) in [
        ("underline", sized.underline()),
        ("strikeout", sized.strikeout()),
    ] {
        match band {
            Some(b) => write!(out, " {key} {} {}", b.y_min, b.y_max)?,
            None => write!(out, " {key} -")?,
        }
    }
    writeln!(out)?;
    let kerned = kerned_glyphs(font);
    for &a in &kerned {
        for &b in &kerned {
            let value = sized.kerning(a, b);
            if value != 0 {
                writeln!(out, "kern {} {} {value}", a.0, b.0)?;
            }
        }
    }
    let (mut outline, mut styled) = (ScaledOutline::new(budget), ScaledOutline::new(budget));
    let mut text = Vec::new();
    let synthetic = |oblique, embolden| Synthesis { oblique, embolden };
    for glyph in (0..font.glyph_count()).map(GlyphId) {
        match sized.hinted_advance(glyph) {
            Ok(advance) => writeln!(out, "hinted {} {advance}", glyph.0)?,
            Err(Error::Unsupported(_)) => writeln!(out, "hinted {} unsupported", glyph.0)?,
            Err(_) => writeln!(out, "hinted {} error", glyph.0)?,
        }
        let Ok(metrics) = sized.outline_into(glyph, &mut outline) else {
            writeln!(out, "glyph {} error", glyph.0)?;
            continue;
        };
        outline_text(&outline, 1.0, &mut text);
        write!(out, "glyph {} {} ", glyph.0, metrics.advance)?;
        if detailed {
            out.write_all(&text)?;
        } else {
            write!(out, "{:016x}", digest(&text))?;
        }
        for (key, oblique, embolden) in [
            ("bold", false, true),
            ("oblique", true, false),
            ("both", true, true),
        ] {
            if styled.copy_from(&outline).is_err()
                || sized
                    .synthesize(&mut styled, synthetic(oblique, embolden))
                    .is_err()
            {
                write!(out, " {key} error")?;
                continue;
            }
            outline_text(&styled, 1.0, &mut text);
            write!(out, " {key} {:016x}", digest(&text))?;
        }
        writeln!(out)?;
    }
    Ok(())
}

fn dump(font: &Font, budget: &Budget, out: &mut impl Write) -> io::Result<()> {
    let v = font.vertical_metrics();
    let style = font.style();
    writeln!(
        out,
        "face {} {} {} {} {} {} {} {} {}",
        font.index(),
        font.glyph_count(),
        font.units_per_em(),
        v.ascender,
        v.descender,
        v.height,
        font.horizontal_header().advance_max,
        u8::from(style.bold),
        u8::from(style.italic)
    )?;
    writeln!(out, "family {}", ascii(font.family_name()))?;
    writeln!(out, "style {}", ascii(font.style_name()))?;
    writeln!(out, "postscript {}", postscript(font.postscript_name()))?;
    writeln!(out, "weight {}", font.os2().map_or(0, |o| o.weight_class))?;
    for r in font.names() {
        let hex: String = r.bytes.iter().map(|b| format!("{b:02x}")).collect();
        writeln!(
            out,
            "name {} {} {} {} {hex}",
            r.platform.id(),
            r.encoding_id,
            r.language_id,
            r.name_id.id()
        )?;
    }
    for charmap in font.charmaps() {
        writeln!(
            out,
            "charmap {} {} {}",
            charmap.platform().id(),
            charmap.encoding_id(),
            charmap.format().number()
        )?;
        for mapping in charmap.mappings() {
            let (code, glyph) = mapping.map_err(io::Error::other)?;
            writeln!(out, "map {code} {}", glyph.0)?;
        }
    }
    match font.text_charmap() {
        Some(c) => writeln!(
            out,
            "textmap {} {} {}",
            c.platform().id(),
            c.encoding_id(),
            c.format().number()
        )?,
        None => writeln!(out, "textmap -")?,
    }
    let kerned = kerned_glyphs(font);
    for &a in &kerned {
        for &b in &kerned {
            let value = font.kerning(a, b);
            if value != 0 {
                writeln!(out, "kern {} {} {value}", a.0, b.0)?;
            }
        }
    }
    let mut outline = Outline::new(budget);
    let mut text = Vec::new();
    for glyph in (0..font.glyph_count()).map(GlyphId) {
        let advance = font.advance(glyph).map_or(-1, i64::from);
        match font.outline_into(glyph, &mut outline) {
            Ok(metrics) => {
                outline_text(&outline, 64.0, &mut text);
                write!(out, "glyph {} {advance} {} ", glyph.0, metrics.advance)?;
                out.write_all(&text)?;
                writeln!(out)?;
            }
            Err(error) => writeln!(out, "glyph {} error {error}", glyph.0)?,
        }
    }
    Ok(())
}
