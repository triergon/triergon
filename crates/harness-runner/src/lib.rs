#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_docs,
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]

use std::io::{self, Write};
use std::ops::RangeInclusive;
use std::path::Path;
use std::time::Instant;
use triergon_core::Budget;
use triergon_harness_protocol::digest::fold;
use triergon_harness_protocol::push_quoted;

/// A worker result; errors end the process with their message.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The running worker's file name, for usage lines.
fn program() -> String {
    std::env::args()
        .next()
        .as_deref()
        .and_then(|path| Path::new(path).file_name())
        .map_or_else(
            || "worker".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        )
}

/// What one invocation does with the operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// One worker command line: `{prefix}MODE FILE [ITERATIONS] ARGS...`.
#[derive(Clone, Copy, Debug)]
pub struct Invocation<'a> {
    /// What to do with the operation.
    pub mode: Mode,
    /// The input file.
    pub path: &'a str,
    /// How often to run the operation: 1 outside `bench`.
    pub iterations: u64,
    /// The words after the file (and the iteration count).
    pub arguments: &'a [String],
}

impl<'a> Invocation<'a> {
    /// Parse `{prefix}fold FILE ARGS`, `{prefix}bench FILE ITERATIONS ARGS`,
    /// `{prefix}memory FILE ARGS` and, when the runner traces, `{prefix}trace FILE
    /// ARGS`. `usage` names the ARGS, whose count lies in `arguments`.
    ///
    /// # Errors
    ///
    /// The usage line, when `args` is not such a command line.
    pub fn parse(
        args: &'a [String],
        prefix: &str,
        usage: &str,
        arguments: RangeInclusive<usize>,
        trace: bool,
    ) -> Result<Self> {
        let usage = || {
            let worker = program();
            let mut line = format!(
                "usage: {worker} {prefix}fold FILE{usage} | {prefix}bench FILE ITERATIONS{usage} | {prefix}memory FILE{usage}"
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
    ///
    /// # Errors
    ///
    /// When the file cannot be read or is larger than `limit`.
    pub fn input(&self, limit: u64) -> Result<Vec<u8>> {
        if std::fs::metadata(self.path)?.len() > limit {
            return Err("input limit".into());
        }
        Ok(std::fs::read(self.path)?)
    }
}

/// What one operation adds to the result line.
#[derive(Debug, Default)]
pub struct Totals {
    /// Packets (or events) the operation produced.
    pub packets: u64,
    /// Bytes the operation folded.
    pub bytes: u64,
    /// The operation's ordered digest.
    pub digest: u64,
    /// Why the operation ended without completing its work, if it did.
    pub refusal: Option<Box<Refusal>>,
}

/// An operation that ended early: its error and the input event it is attributed
/// to, as a JSON object.
#[derive(Debug)]
pub struct Refusal {
    /// The error, printed as a JSON string.
    pub reason: String,
    /// The input event it is attributed to, a JSON object.
    pub cause: String,
}

/// Run `operation` as often as the invocation asks and print the result line: the
/// summed packets and bytes and the ordered fold of each operation's digest.
/// `held` is the pool of a preloaded input; an operation that leaves more of it
/// charged fails. A refused operation ends the loop; the line then counts the
/// operations that ran and carries the refusal and its cause.
///
/// # Errors
///
/// When the operation fails, retains charged storage, or stdout fails.
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
