//! The command line and measured loop shared by the per-input runners: the packet
//! folds (`mkv-audio-`, `mp4-forward-`, `ts-program-` and `raw-video-`), `pgs-` and
//! `subtitle-text-`.
//!
//! A runner prepares its input outside the measurement, then [`measure`] runs the
//! operation and prints one JSON result line. Reading the input, process start-up
//! and the result line are outside the timed loop and the allocation window.

use std::io::{self, Write};
use std::ops::RangeInclusive;
use std::time::Instant;
use triergon_core::Budget;
use triergon_harness_protocol::digest::fold;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

type Run = fn(&[String]) -> Result<()>;

const RUNNERS: [(&str, Run); 6] = [
    (crate::matroska_audio::PREFIX, crate::matroska_audio::run),
    (crate::mp4_forward::PREFIX, crate::mp4_forward::run),
    (crate::ts_program::PREFIX, crate::ts_program::run),
    (crate::raw_video::PREFIX, crate::raw_video::run),
    (crate::pgs::PREFIX, crate::pgs::run),
    (crate::subtitle_text::PREFIX, crate::subtitle_text::run),
];

/// The runner that owns the mode in `args[0]`, if any.
pub fn dispatch(args: &[String]) -> Option<Result<()>> {
    let mode = args.first()?;
    RUNNERS
        .iter()
        .find(|(prefix, _)| mode.starts_with(prefix))
        .map(|(_, run)| run(args))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// One operation.
    Fold,
    /// The given number of operations, timed.
    Bench,
    /// One operation inside the allocation-window markers.
    Memory,
    /// One operation that prints each folded record.
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
pub struct Totals {
    pub packets: u64,
    pub bytes: u64,
    pub digest: u64,
}

/// Run `operation` as often as the invocation asks and print the result line: the
/// summed packets and bytes and the ordered fold of each operation's digest. `pool`
/// holds the prepared input; an operation that leaves more charged fails.
pub fn measure(
    invocation: &Invocation<'_>,
    name: &str,
    pool: &Budget,
    mut operation: impl FnMut() -> Result<Totals>,
) -> Result<()> {
    let retained = pool.used();
    let memory = invocation.mode == Mode::Memory;
    let mut out = io::stdout().lock();
    if memory {
        out.write_all(b"TRIERGON_BEGIN_V1\n")?;
        out.flush()?;
    }
    let start = (invocation.mode == Mode::Bench).then(Instant::now);
    let (mut packets, mut bytes, mut digest) = (0, 0, 0);
    for _ in 0..invocation.iterations {
        let totals = operation()?;
        packets += totals.packets;
        bytes += totals.bytes;
        fold(&mut digest, totals.digest);
        if pool.used() != retained {
            return Err("operation retained charged storage".into());
        }
    }
    let elapsed = start.map_or(0, |begin| begin.elapsed().as_nanos());
    if memory {
        out.write_all(b"TRIERGON_END_V1\n")?;
        out.flush()?;
    }
    writeln!(
        out,
        "{{\"schema\":1,\"operation\":\"{name}\",\"iterations\":{},\"packets\":{packets},\"bytes\":{bytes},\"digest\":{digest},\"elapsed_ns\":{elapsed},\"eof\":true}}",
        invocation.iterations,
    )?;
    out.flush()?;
    Ok(())
}
