//! Prints every face of a font file in the line format of `harness/freetype-font.c`,
//! the FreeType reference, so `cargo xtask fonts` can compare them.
//!
//! Usage: font-dump FILE
//!
//! Outline coordinates are font units times 64, rounded to the nearest integer.

use std::io::{self, BufWriter, Write};
use std::process::ExitCode;
use triergon_core::{Budget, Bytes};
use triergon_font::{Font, GlyphId, Limits, NameRecord, Outline, PointKind, platform};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [path] = &args[..] else {
        eprintln!("usage: font-dump FILE");
        return ExitCode::from(2);
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
            Ok(font) => dump(&font, &budget, &mut out).expect("stdout"),
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

/// FreeType's PostScript form: printable ASCII other than `[](){}<>/%`.
fn postscript(record: Option<NameRecord<'_>>) -> String {
    let Some(record) = record else {
        return "-".into();
    };
    let text: String = units(&record)
        .filter_map(|u| u8::try_from(u).ok())
        .filter(|&b| (33..=126).contains(&b) && !b"[](){}<>/%".contains(&b))
        .map(char::from)
        .collect();
    if text.is_empty() { "-".into() } else { text }
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
        font.max_advance(),
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
            r.platform, r.encoding, r.language, r.name_id
        )?;
    }
    for charmap in font.charmaps() {
        writeln!(
            out,
            "charmap {} {} {}",
            charmap.platform, charmap.encoding, charmap.format
        )?;
        for (code, glyph) in font.mappings(charmap) {
            writeln!(out, "map {code} {}", glyph.0)?;
        }
    }
    match font.text_charmap() {
        Some(c) => writeln!(out, "textmap {} {} {}", c.platform, c.encoding, c.format)?,
        None => writeln!(out, "textmap -")?,
    }
    let mut kerned: Vec<GlyphId> = (0..font.glyph_count().min(256)).map(GlyphId).collect();
    for code in 0x20..=0xff_u8 {
        if let Some(glyph) = font.glyph_index(char::from(code)).filter(|g| g.0 >= 256) {
            kerned.push(glyph);
        }
    }
    for &a in &kerned {
        for &b in &kerned {
            let value = font.kerning(a, b);
            if value != 0 {
                writeln!(out, "kern {} {} {value}", a.0, b.0)?;
            }
        }
    }
    let mut outline = Outline::new(budget);
    for glyph in (0..font.glyph_count()).map(GlyphId) {
        let advance = font.advance(glyph).map_or(-1, i64::from);
        match font.outline_into(glyph, &mut outline) {
            Ok(metrics) => {
                write!(
                    out,
                    "glyph {} {advance} {} {} |",
                    glyph.0,
                    metrics.advance,
                    outline.contour_ends().len()
                )?;
                for end in outline.contour_ends() {
                    write!(out, " {end}")?;
                }
                write!(out, " |")?;
                for p in outline.points() {
                    let tag = match p.kind {
                        PointKind::On => 1,
                        PointKind::Quadratic => 0,
                        PointKind::Cubic => 2,
                    };
                    let scale = |v: f32| (f64::from(v) * 64.0).round() as i64;
                    write!(out, " {} {} {tag}", scale(p.point.x), scale(p.point.y))?;
                }
                writeln!(out)?;
            }
            Err(error) => writeln!(out, "glyph {} error {error}", glyph.0)?,
        }
    }
    Ok(())
}
