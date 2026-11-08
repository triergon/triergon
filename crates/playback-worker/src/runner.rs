//! The command line and measured loop of every playback-worker mode.
//!
//! Each runner owns one mode prefix. It prepares its input outside the measurement,
//! then [`measure`] runs the operation and prints one JSON result line. Reading the
//! input, process start-up and the result line are outside the timed loop and the
//! allocation window.

use std::io::{self, Write};
use std::ops::RangeInclusive;
use std::time::Instant;
use triergon_core::Budget;
use triergon_harness_protocol::digest::fold;
use triergon_harness_protocol::push_quoted;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The largest input file a runner reads.
pub const MAX_INPUT: u64 = 64 * 1024 * 1024;

type Run = fn(&[String]) -> Result<()>;

const RUNNERS: [(&str, Run); 9] = [
    (crate::matroska_video::PREFIX, crate::matroska_video::run),
    (crate::matroska_audio::PREFIX, crate::matroska_audio::run),
    (crate::hls::PREFIX, crate::hls::run),
    (crate::mp4_range::PREFIX, crate::mp4_range::run),
    (crate::mp4_forward::PREFIX, crate::mp4_forward::run),
    (crate::progressive::PREFIX, crate::progressive::run),
    (crate::ts_program::PREFIX, crate::ts_program::run),
    (crate::ts::PREFIX, crate::ts::run),
    (crate::raw_video::PREFIX, crate::raw_video::run),
];

/// Run the mode in `args[0]` with the runner that owns its prefix.
pub fn dispatch(args: &[String]) -> Result<()> {
    let mode = args.first().map_or("", String::as_str);
    let Some((_, run)) = RUNNERS.iter().find(|(prefix, _)| mode.starts_with(prefix)) else {
        let prefixes: Vec<&str> = RUNNERS.iter().map(|(prefix, _)| *prefix).collect();
        return Err(format!(
            "usage: playback-worker PREFIX{{fold,bench,memory}} FILE ...; prefixes: {}",
            prefixes.join(" ")
        )
        .into());
    };
    run(args)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// One operation.
    Fold,
    /// The given number of operations, timed.
    Bench,
    /// One operation inside the allocation-window markers.
    Memory,
    /// One operation that prints what it folds.
    Trace,
}

/// One runner command line: `{prefix}MODE FILE [ITERATIONS] ARGS...`.
pub struct Invocation<'a> {
    pub mode: Mode,
    pub path: &'a str,
    pub iterations: u64,
    pub arguments: &'a [String],
}

impl<'a> Invocation<'a> {
    /// Parse `{prefix}fold FILE ARGS`, `{prefix}bench FILE ITERATIONS ARGS`,
    /// `{prefix}memory FILE ARGS` and, when the runner traces, `{prefix}trace FILE
    /// ARGS`. `usage` names the ARGS, whose count lies in `arguments`.
    pub fn parse(
        args: &'a [String],
        prefix: &str,
        usage: &str,
        arguments: RangeInclusive<usize>,
        trace: bool,
    ) -> Result<Self> {
        let usage = || {
            let mut line = format!(
                "usage: playback-worker {prefix}fold FILE{usage} | {prefix}bench FILE ITERATIONS{usage} | {prefix}memory FILE{usage}"
            );
            if trace {
                line += &format!(" | {prefix}trace FILE{usage}");
            }
            line
        };
        let (mode, rest) = args.split_first().ok_or_else(usage)?;
        let (path, rest) = rest.split_first().ok_or_else(usage)?;
        let (mode, iterations, rest) = match mode.strip_prefix(prefix) {
            Some("bench") => {
                let (count, rest) = rest.split_first().ok_or_else(usage)?;
                (Mode::Bench, count.parse::<u64>()?, rest)
            }
            Some("fold") => (Mode::Fold, 1, rest),
            Some("memory") => (Mode::Memory, 1, rest),
            Some("trace") if trace => (Mode::Trace, 1, rest),
            _ => return Err(usage().into()),
        };
        if !arguments.contains(&rest.len()) || !(1..=1_000_000).contains(&iterations) {
            return Err(usage().into());
        }
        Ok(Self {
            mode,
            path,
            iterations,
            arguments: rest,
        })
    }

    /// The input file, refused when it is larger than `limit` bytes.
    pub fn input(&self, limit: u64) -> Result<Vec<u8>> {
        if std::fs::metadata(self.path)?.len() > limit {
            return Err("input limit".into());
        }
        Ok(std::fs::read(self.path)?)
    }
}

/// What one operation adds to the result line.
#[derive(Default)]
pub struct Totals {
    pub packets: u64,
    pub bytes: u64,
    pub digest: u64,
    /// Why the operation ended without completing its work, if it did.
    pub refusal: Option<Box<Refusal>>,
}

/// An operation that ended early: its error and the input event it is attributed
/// to, as a JSON object.
pub struct Refusal {
    pub reason: String,
    pub cause: String,
}

/// Run `operation` as often as the invocation asks and print the result line: the
/// summed packets and bytes and the ordered fold of each operation's digest.
/// `held` is the pool of a preloaded input; an operation that leaves more of it
/// charged fails. A refused operation ends the loop; the line then counts the
/// operations that ran and carries the refusal and its cause.
pub fn measure(
    invocation: &Invocation<'_>,
    name: &str,
    held: Option<&Budget>,
    mut operation: impl FnMut() -> Result<Totals>,
) -> Result<()> {
    let retained = held.map(Budget::used);
    let memory = invocation.mode == Mode::Memory;
    let mut out = io::stdout().lock();
    if memory {
        out.write_all(b"TRIERGON_BEGIN_V1\n")?;
        out.flush()?;
    }
    let start = (invocation.mode == Mode::Bench).then(Instant::now);
    let (mut packets, mut bytes, mut digest) = (0, 0, 0);
    let (mut runs, mut refusal) = (0, None);
    while runs < invocation.iterations {
        let totals = operation()?;
        runs += 1;
        packets += totals.packets;
        bytes += totals.bytes;
        fold(&mut digest, totals.digest);
        if totals.refusal.is_some() {
            refusal = totals.refusal;
            break;
        }
        if held.map(Budget::used) != retained {
            return Err("operation retained charged storage".into());
        }
    }
    let elapsed = start.map_or(0, |begin| begin.elapsed().as_nanos());
    if memory {
        out.write_all(b"TRIERGON_END_V1\n")?;
        out.flush()?;
    }
    let mut line = format!(
        "{{\"schema\":1,\"operation\":\"{name}\",\"iterations\":{runs},\"packets\":{packets},\"bytes\":{bytes},\"digest\":{digest},\"elapsed_ns\":{elapsed},\"eof\":{}",
        refusal.is_none()
    );
    if let Some(refusal) = refusal {
        line.push_str(",\"refusal\":");
        push_quoted(&mut line, &refusal.reason);
        line.push_str(",\"cause\":");
        line.push_str(&refusal.cause);
    }
    line.push('}');
    writeln!(out, "{line}")?;
    out.flush()?;
    Ok(())
}
