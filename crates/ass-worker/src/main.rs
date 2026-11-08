//! ASS parsing measurement worker: whole scripts, Matroska packet streams and
//! event walks through triergon-ass, in the order of `harness/reference_ass.h`.
//! Reading the file, its directives and commands, printing and process
//! startup are outside the timed loop; so is reading the script an event walk
//! runs over.
#![forbid(unsafe_code)]

mod fixture;
mod perform;
mod record;

use fixture::Fixture;
use perform::{Result, Walker, parse, record_script, run_stream};
use record::{Dump, Kind, Sink, Work};
use std::io::{self, BufWriter, Write};
use std::process::ExitCode;
use std::time::Instant;
use triergon_ass::{Options, Script};
use triergon_core::Budget;

const MAX_INPUT: u64 = 64 << 20;
const USAGE: &str = "usage: ass-worker ass-fold FILE MODE | ass-bench FILE ITERATIONS MODE
       | ass-memory FILE MODE | ass-dump FILE MODE
  MODE: script | chunks, either followed by events";

enum Action {
    Fold,
    Bench(u64),
    Memory,
    Dump,
}

/// What one operation does with the fixture.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Read it as a whole script and record the track.
    Script,
    /// Run its stream commands, recording the track at each dump and at the end.
    Chunks,
    /// Walk every event of the script or stream at the fixture's times.
    Events { stream: bool },
}

fn parse_args(args: &[String]) -> Result<(Action, &str, Mode)> {
    let (action, rest) = args.split_first().ok_or(USAGE)?;
    let (action, path, rest) = match (action.as_str(), rest) {
        ("ass-bench", [path, count, rest @ ..]) => (Action::Bench(count.parse()?), path, rest),
        ("ass-fold", [path, rest @ ..]) => (Action::Fold, path, rest),
        ("ass-memory", [path, rest @ ..]) => (Action::Memory, path, rest),
        ("ass-dump", [path, rest @ ..]) => (Action::Dump, path, rest),
        _ => return Err(USAGE.into()),
    };
    let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
    let mode = match rest[..] {
        ["script"] => Mode::Script,
        ["chunks"] => Mode::Chunks,
        ["script", "events"] => Mode::Events { stream: false },
        ["chunks", "events"] => Mode::Events { stream: true },
        _ => return Err(USAGE.into()),
    };
    Ok((action, path, mode))
}

/// One operation.
fn operation(
    sink: &mut impl Sink,
    budget: &Budget,
    data: &[u8],
    fixture: &Fixture<'_>,
    options: &Options<'_>,
    mode: Mode,
    walked: Option<&Script>,
) -> Result<()> {
    let script = match mode {
        Mode::Chunks => {
            run_stream(sink, budget, options, &fixture.commands)?;
            return Ok(());
        }
        Mode::Script => parse(budget, data, options)?,
        Mode::Events { .. } => None,
    };
    if let Some(script) = &script {
        record_script(sink, script)?;
    } else if let Some(script) = walked {
        Walker::new(budget).walk_all(sink, script, &fixture.times)?;
    } else {
        sink.begin(Kind::NoTrack)?;
        sink.end()?;
    }
    Ok(())
}

fn run(args: &[String]) -> Result<()> {
    let (action, path, mode) = parse_args(args)?;
    let iterations = match action {
        Action::Bench(count) => count,
        _ => 1,
    };
    if !(1..=1_000_000).contains(&iterations) {
        return Err(USAGE.into());
    }
    if std::fs::metadata(path)?.len() > MAX_INPUT {
        return Err("input exceeds the size bound".into());
    }
    let data = std::fs::read(path)?;
    let stream = matches!(mode, Mode::Chunks | Mode::Events { stream: true });
    let fixture = Fixture::read(&data, stream);
    let options = Options {
        extract_fonts: fixture.extract_fonts,
        style_overrides: &fixture.overrides,
        ..Options::default()
    };
    // An accounting ceiling far above any script of the corpus, so only parity
    // decides; it fits a 32-bit `usize`.
    let budget = Budget::new(1 << 30)?;
    // An event walk runs over a script read before it.
    let walked = match mode {
        Mode::Events { stream: false } => parse(&budget, &data, &options)?,
        Mode::Events { stream: true } => Some(run_stream(
            &mut Work::default(),
            &budget,
            &options,
            &fixture.commands,
        )?),
        _ => None,
    };
    let retained = budget.used();
    let mut out = io::stdout().lock();
    if let Action::Dump = action {
        let mut dump = Dump(BufWriter::new(out));
        operation(
            &mut dump,
            &budget,
            &data,
            &fixture,
            &options,
            mode,
            walked.as_ref(),
        )?;
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
        operation(
            &mut work,
            &budget,
            std::hint::black_box(&data),
            &fixture,
            &options,
            mode,
            walked.as_ref(),
        )?;
        total.accumulate(&work);
        if budget.used() != retained {
            return Err("ass operation retained charged storage".into());
        }
    }
    let elapsed = start.map_or(0, |begin| begin.elapsed().as_nanos());
    if memory {
        out.write_all(b"TRIERGON_END_V1\n")?;
        out.flush()?;
    }
    writeln!(
        out,
        "{{\"schema\":1,\"operation\":\"ass-parse\",\"iterations\":{iterations},\"packets\":{},\"bytes\":{},\"digest\":{},\"elapsed_ns\":{elapsed},\"eof\":true}}",
        total.packets, total.bytes, total.digest,
    )?;
    Ok(out.flush()?)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ass-worker: {error}");
            ExitCode::FAILURE
        }
    }
}
