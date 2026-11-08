//! Rasterising measurement worker: libass's outline, stroker, rasteriser, blur,
//! composition and clipping stages through triergon-raster, with glyph outlines
//! from triergon-font, in the order of `harness/reference_raster.h`. Reading the
//! file, loading the glyphs, parsing the cases, printing and process startup are
//! outside the timed loop.
#![forbid(unsafe_code)]

mod case;
mod perform;
mod record;

use case::{Case, Glyph};
use perform::Runner;
use record::{Dump, Kind, Sink, Work};
use std::error::Error;
use std::io::{self, BufWriter, Write};
use std::process::ExitCode;
use std::time::Instant;
use triergon_core::{Budget, Bytes};
use triergon_font::{Font, GlyphId, Limits};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const MAX_INPUT: u64 = 64 << 20;
/// Glyphs one font case may select.
const MAX_GLYPHS: usize = 4096;
/// The storage budget: an accounting ceiling of 16 GiB, above the largest
/// bitmap libass draws and its scratch, so only parity decides. A 32-bit
/// target's address space is smaller, so there the ceiling is the whole of
/// it and the allocator refuses first.
const BUDGET_BYTES: usize = {
    let ceiling: u64 = 1 << 34;
    if ceiling > usize::MAX as u64 {
        usize::MAX
    } else {
        ceiling as usize
    }
};
const USAGE: &str = "usage: raster-worker OPERATION-fold FILE MODE...
       | OPERATION-bench FILE ITERATIONS MODE... | OPERATION-memory FILE MODE...
       | OPERATION-dump FILE MODE...
  OPERATION: raster or raster-simd (the same work; the reference's engine differs)
  MODE: glyphs HEIGHT COUNT CASE... (FILE a font) or lines (FILE one case per line)";

/// Where a run's cases and glyphs come from.
enum Input {
    /// Cases given as arguments over `count` glyphs of face 0 of a font file,
    /// loaded unhinted at a 26.6 pixel `height`.
    Glyphs {
        height: i32,
        count: usize,
        cases: Vec<String>,
    },
    /// One case per line of the file.
    Lines,
}

/// The glyphs of a font case: the selected ids and outlines, or `None` when
/// the face or size does not load.
type Selection = Option<Vec<(u16, Option<Glyph>)>>;

/// `count` glyph ids spread evenly over `total`: the k-th is `k · total / count`,
/// or every glyph when there are fewer.
fn spread(total: usize, count: usize) -> Vec<u16> {
    if total <= count {
        return (0..total).map(|g| g as u16).collect();
    }
    (0..count)
        .map(|k| ((k as u64 * total as u64) / count as u64) as u16)
        .collect()
}

/// Loads the selection as libass does with hinting off: face 0 at `height`
/// after its GDI metrics, each outline unhinted (`FT_LOAD_NO_HINTING`).
fn load(file: &Bytes, budget: &Budget, height: i32, count: usize) -> Selection {
    let font = Font::new(file.clone(), 0, budget, Limits::default()).ok()?;
    let sized = font.at_size(height).ok()?;
    let mut outline = triergon_font::Outline::new(budget);
    let glyphs = spread(usize::from(font.glyph_count()), count)
        .into_iter()
        .map(|id| {
            let glyph = sized
                .outline_into(GlyphId(id), &mut outline)
                .ok()
                .map(|_| Glyph {
                    points: outline.points().to_vec(),
                    ends: outline.contour_ends().to_vec(),
                });
            (id, glyph)
        })
        .collect();
    Some(glyphs)
}

/// The records of the selection, before the cases.
fn record_selection(selection: &Selection, sink: &mut impl Sink) -> io::Result<()> {
    sink.begin(Kind::Glyphs)?;
    let Some(glyphs) = selection else {
        sink.absent()?;
        return sink.end();
    };
    sink.value(glyphs.len() as i64)?;
    sink.end()?;
    for (id, glyph) in glyphs {
        sink.begin(Kind::Glyph)?;
        sink.value((*id).into())?;
        match glyph {
            Some(glyph) => sink.value(glyph.points.len() as i64)?,
            None => sink.absent()?,
        }
        sink.end()?;
    }
    Ok(())
}

/// One operation: the selection's records, then every case.
fn read(
    budget: &Budget,
    selection: Option<&Selection>,
    cases: &[Option<Case>],
    sink: &mut impl Sink,
) -> io::Result<()> {
    if let Some(selection) = selection {
        record_selection(selection, sink)?;
    }
    let glyphs = selection.and_then(Option::as_deref).unwrap_or_default();
    let mut runner = Runner::new(budget, glyphs);
    for case in cases {
        runner.perform(case.as_ref(), sink)?;
    }
    Ok(())
}

/// The lines of a text file, without their newlines; a final newline ends the
/// last line.
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

enum Action {
    Fold,
    Bench(u64),
    Memory,
    Dump,
}

fn parse(args: &[String]) -> Result<(Action, &str, Input)> {
    let (mode, rest) = args.split_first().ok_or(USAGE)?;
    // The crate computes what libass's C engine does; `raster-simd` names the
    // same work for the feature whose reference runs the host's SIMD engine.
    let action = mode
        .strip_prefix("raster-simd-")
        .or_else(|| mode.strip_prefix("raster-"))
        .ok_or(USAGE)?;
    let (action, path, rest) = match (action, rest) {
        ("bench", [path, count, rest @ ..]) => (Action::Bench(count.parse()?), path, rest),
        ("fold", [path, rest @ ..]) => (Action::Fold, path, rest),
        ("memory", [path, rest @ ..]) => (Action::Memory, path, rest),
        ("dump", [path, rest @ ..]) => (Action::Dump, path, rest),
        _ => return Err(USAGE.into()),
    };
    let input = match rest {
        [mode, height, count, cases @ ..] if mode == "glyphs" => {
            let height: i32 = height.parse()?;
            let count: usize = count.parse()?;
            if !(1..=MAX_GLYPHS).contains(&count) || height < 1 {
                return Err(USAGE.into());
            }
            Input::Glyphs {
                height,
                count,
                cases: cases.to_vec(),
            }
        }
        [mode] if mode == "lines" => Input::Lines,
        _ => return Err(USAGE.into()),
    };
    Ok((action, path, input))
}

fn run(args: &[String]) -> Result<()> {
    let (action, path, input) = parse(args)?;
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
    let budget = Budget::new(BUDGET_BYTES)?;
    let file = Bytes::try_from_vec(&budget, std::fs::read(path)?).map_err(|e| e.error())?;
    let (selection, cases): (Option<Selection>, Vec<Option<Case>>) = match &input {
        Input::Glyphs {
            height,
            count,
            cases,
        } => (
            Some(load(&file, &budget, *height, *count)),
            cases.iter().map(|c| case::parse(c)).collect(),
        ),
        Input::Lines => {
            let cases = lines(file.as_ref())
                .map(|line| std::str::from_utf8(line).ok().and_then(case::parse))
                .collect();
            (None, cases)
        }
    };
    let retained = budget.used();
    let mut out = io::stdout().lock();
    if let Action::Dump = action {
        let mut dump = Dump(BufWriter::new(out));
        read(&budget, selection.as_ref(), &cases, &mut dump)?;
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
        read(
            &budget,
            selection.as_ref(),
            std::hint::black_box(&cases),
            &mut work,
        )?;
        total.accumulate(&work);
        if budget.used() != retained {
            return Err("raster operation retained charged storage".into());
        }
    }
    let elapsed = start.map_or(0, |begin| begin.elapsed().as_nanos());
    if memory {
        out.write_all(b"TRIERGON_END_V1\n")?;
        out.flush()?;
    }
    writeln!(
        out,
        "{{\"schema\":1,\"operation\":\"raster\",\"iterations\":{iterations},\"packets\":{},\"bytes\":{},\"digest\":{},\"elapsed_ns\":{elapsed},\"eof\":true}}",
        total.packets, total.bytes, total.digest,
    )?;
    Ok(out.flush()?)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("raster-worker: {error}");
            ExitCode::FAILURE
        }
    }
}
