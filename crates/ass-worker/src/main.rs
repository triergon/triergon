//! ASS parsing measurement worker: whole scripts, Matroska packet streams and
//! event walks through triergon-ass, in the order of `harness/reference_ass.h`.
//! Reading the file, its directives and commands, printing and process
//! startup are outside the timed loop; so is reading the script an event walk
//! runs over.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]

mod fixture;
mod perform;
mod record;
mod render;

use fixture::Fixture;
use perform::{Result, Walker, parse, record_script, run_stream};
use record::{Dump, Kind, Sink, Work};
use std::io::{self, BufWriter, Write};
use std::process::ExitCode;
use triergon_ass::{Options, Script};
use triergon_core::Budget;
use triergon_harness_runner::{
    Boundary, Invocation, Mode as RunMode, Timer, Totals, write_summary,
};

const MAX_INPUT: u64 = 64 << 20;
const USAGE: &str = "usage: ass-worker ass-fold FILE MODE | ass-bench FILE ITERATIONS MODE
       | ass-memory FILE MODE | ass-dump FILE MODE
  MODE: script | chunks, either followed by events";

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

fn parse_args(args: &[String]) -> Result<(Invocation<'_>, Mode)> {
    let (action, rest) = args.split_first().ok_or(USAGE)?;
    let action = action.strip_prefix("ass-").ok_or(USAGE)?;
    let invocation = Invocation::parse_mode(action, rest, Some("dump"), || USAGE)?;
    let rest: Vec<&str> = invocation.arguments.iter().map(String::as_str).collect();
    let mode = match rest[..] {
        ["script"] => Mode::Script,
        ["chunks"] => Mode::Chunks,
        ["script", "events"] => Mode::Events { stream: false },
        ["chunks", "events"] => Mode::Events { stream: true },
        _ => return Err(USAGE.into()),
    };
    Ok((invocation, mode))
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
    if args
        .first()
        .is_some_and(|action| action.starts_with("ass-render-"))
    {
        return render::run(args);
    }
    let (invocation, mode) = parse_args(args)?;
    invocation.check_iterations(|| USAGE)?;
    let Invocation {
        mode: action,
        path,
        iterations,
        ..
    } = invocation;
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
    if action == RunMode::Trace {
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
    let memory = action == RunMode::Memory;
    if memory {
        Boundary::Begin.mark(&mut out)?;
    }
    let start = Timer::start(action == RunMode::Bench);
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
    let elapsed = start.elapsed().as_nanos();
    if memory {
        Boundary::End.mark(&mut out)?;
    }
    Ok(write_summary(
        &mut out,
        "ass-parse",
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
    triergon_harness_runner::command(Some("ass-worker"), 1, run)
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
