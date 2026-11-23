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

pub mod records;
mod table;

pub use table::{Action, Operation, Prepare, dispatch, serve, smoke};

use std::fmt::{self, Display};
use std::io::{self, Write};
use std::ops::RangeInclusive;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};
use triergon_core::Budget;
use triergon_harness_protocol::digest::{fold, fold_bytes};
use triergon_harness_protocol::push_quoted;

/// How often one invocation may run its operation.
const ITERATIONS: RangeInclusive<u64> = 1..=1_000_000;

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
        let action = mode.strip_prefix(prefix).ok_or_else(usage)?;
        let invocation = Self::parse_mode(action, rest, trace.then_some("trace"), usage)?;
        if !arguments.contains(&invocation.arguments.len()) {
            return Err(usage().into());
        }
        invocation.check_iterations(usage)?;
        Ok(invocation)
    }

    /// Parse a mode and the following file, optional bench count and operation arguments.
    /// `trace` gives the spelling for record output, if supported. The caller validates
    /// operation arguments and the iteration bound in the order its command line requires.
    ///
    /// # Errors
    ///
    /// Returns the usage message for missing words or unknown modes, or the integer
    /// parser's error for a malformed bench count.
    pub fn parse_mode<T: Into<Box<dyn std::error::Error>>>(
        action: &str,
        args: &'a [String],
        trace: Option<&str>,
        usage: impl Fn() -> T,
    ) -> Result<Self> {
        let usage = || -> Box<dyn std::error::Error> { usage().into() };
        let (path, rest) = args.split_first().ok_or_else(usage)?;
        let (mode, iterations, arguments) = match action {
            "bench" => {
                let (count, rest) = rest.split_first().ok_or_else(usage)?;
                (Mode::Bench, count.parse::<u64>()?, rest)
            }
            "fold" => (Mode::Fold, 1, rest),
            "memory" => (Mode::Memory, 1, rest),
            action if trace == Some(action) => (Mode::Trace, 1, rest),
            _ => return Err(usage()),
        };
        Ok(Self {
            mode,
            path,
            iterations,
            arguments,
        })
    }

    /// Require between one and one million iterations, inclusive.
    ///
    /// # Errors
    ///
    /// Returns the supplied usage message when the count is out of range.
    pub fn check_iterations<T: Into<Box<dyn std::error::Error>>>(
        &self,
        usage: impl FnOnce() -> T,
    ) -> Result<()> {
        if ITERATIONS.contains(&self.iterations) {
            Ok(())
        } else {
            Err(usage().into())
        }
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

/// Run a command on the words after its program name and report its error.
pub fn command(
    name: Option<&str>,
    failure: u8,
    run: impl FnOnce(&[String]) -> Result<()>,
) -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => failed(name, failure, error),
    }
}

/// Write one diagnostic to stderr and return the worker's failure status.
#[expect(
    clippy::print_stderr,
    reason = "the worker CLI reports failures on stderr"
)]
pub fn failed(name: Option<&str>, status: u8, error: impl Display) -> ExitCode {
    match name {
        Some(name) => eprintln!("{name}: {error}"),
        None => eprintln!("{error}"),
    }
    ExitCode::from(status)
}

/// One boundary of an allocation observation.
#[derive(Clone, Copy, Debug)]
pub enum Boundary {
    /// Start observing allocations.
    Begin,
    /// Stop observing allocations.
    End,
}

impl Boundary {
    /// The protocol marker, including its line feed.
    pub const fn line(self) -> &'static [u8] {
        match self {
            Self::Begin => b"TRIERGON_BEGIN_V1\n",
            Self::End => b"TRIERGON_END_V1\n",
        }
    }

    /// Write and flush this boundary before the worker continues.
    ///
    /// # Errors
    ///
    /// Returns the stream's write or flush error.
    pub fn mark(self, out: &mut impl Write) -> io::Result<()> {
        out.write_all(self.line())?;
        out.flush()
    }
}

/// Monotonic time between operation boundaries chosen by the worker.
#[derive(Debug)]
pub struct Timer(Option<Instant>);

impl Timer {
    /// Start the timed interval, or leave it inactive for an untimed mode.
    #[inline]
    pub fn start(timed: bool) -> Self {
        Self(timed.then(Instant::now))
    }

    /// Read the elapsed duration, or zero for an inactive timer.
    #[inline]
    pub fn elapsed(&self) -> Duration {
        self.0.map_or(Duration::ZERO, |begin| begin.elapsed())
    }
}

/// The timing line of the decoder and media workers, with elapsed nanoseconds before
/// the work counters.
#[derive(Debug)]
pub struct BenchLine {
    /// Include the leading schema field used by the decoder and media workers.
    pub schema: bool,
    /// Number of operations.
    pub iterations: u64,
    /// Time spent in the measured interval, in nanoseconds.
    pub elapsed_ns: u128,
    /// Output units.
    pub packets: u64,
    /// Output bytes.
    pub bytes: u64,
    /// Digest of the work.
    pub digest: u64,
}

impl BenchLine {
    /// Build a schema-bearing timing line from the packet, byte and digest totals.
    pub const fn with_schema(
        iterations: u64,
        elapsed_ns: u128,
        (packets, bytes, digest): (u64, u64, u64),
    ) -> Self {
        Self {
            schema: true,
            iterations,
            elapsed_ns,
            packets,
            bytes,
            digest,
        }
    }
}

impl Display for BenchLine {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            schema,
            iterations,
            elapsed_ns,
            packets,
            bytes,
            digest,
        } = self;
        let schema = if *schema { "\"schema\":1," } else { "" };
        write!(
            out,
            "{{{schema}\"iterations\":{iterations},\"elapsed_ns\":{elapsed_ns},\"packets\":{packets},\"bytes\":{bytes},\"digest\":{digest}}}"
        )
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
    /// An unsupported feature the operation met and reports while completing.
    pub unsupported: Option<&'static str>,
}

impl Totals {
    /// Fold one record: its kind, its words and, when it has one, its payload, which
    /// it counts as one packet of the payload's length.
    pub fn record(&mut self, kind: u64, words: &[u64], payload: &[u8]) {
        fold(&mut self.digest, kind);
        for &word in words {
            fold(&mut self.digest, word);
        }
        if !payload.is_empty() {
            fold_bytes(&mut self.digest, payload);
        }
        self.packets += 1;
        self.bytes += payload.len() as u64;
    }
}

/// An operation that ended early: its error and the input event it is attributed
/// to, as a JSON object.
#[derive(Debug, PartialEq, Eq)]
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
    operation: impl FnMut() -> Result<Totals>,
) -> Result<()> {
    measure_with(invocation, name, held, &[], operation)
}

/// [`measure`], with `fields` printed after the digest: named values the
/// worker works out outside the operation, such as a digest of its prepared
/// input.
///
/// # Errors
///
/// When the operation fails, retains charged storage, or stdout fails.
pub fn measure_with(
    invocation: &Invocation<'_>,
    name: &str,
    held: Option<&Budget>,
    fields: &[(&str, u64)],
    mut operation: impl FnMut() -> Result<Totals>,
) -> Result<()> {
    measure_dyn(
        invocation.mode,
        invocation.iterations,
        name,
        held,
        fields,
        Refusals::Stop,
        &mut operation,
    )
}

/// Measure a workload whose required outcome may be a deterministic refusal: run
/// every planned operation, each of which must end alike, and report the first
/// refusal.
///
/// # Errors
///
/// When the outcome changes between repetitions, the operation fails, retains
/// charged storage, or stdout fails.
pub fn measure_repeated(
    invocation: &Invocation<'_>,
    name: &str,
    held: Option<&Budget>,
    mut operation: impl FnMut() -> Result<Totals>,
) -> Result<()> {
    measure_dyn(
        invocation.mode,
        invocation.iterations,
        name,
        held,
        &[],
        Refusals::Repeat,
        &mut operation,
    )
}

/// What the measured loop does with an operation that ends in a refusal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Refusals {
    /// End the loop.
    Stop,
    /// Run the remaining operations, which must refuse alike.
    Repeat,
}

/// [`measure_with`] with the operation behind a reference, so each worker binary
/// holds one copy of the loop and of the result line.
fn measure_dyn(
    mode: Mode,
    iterations: u64,
    name: &str,
    held: Option<&Budget>,
    fields: &[(&str, u64)],
    refusals: Refusals,
    operation: &mut dyn FnMut() -> Result<Totals>,
) -> Result<()> {
    let retained = held.map(Budget::used);
    let memory = mode == Mode::Memory;
    let mut out = io::stdout().lock();
    if memory {
        Boundary::Begin.mark(&mut out)?;
    }
    let start = (mode == Mode::Bench).then(Instant::now);
    let (mut packets, mut bytes, mut digest) = (0, 0, 0);
    let (mut runs, mut refusal, mut unsupported) = (0, None, None);
    while runs < iterations {
        let totals = operation()?;
        runs += 1;
        packets += totals.packets;
        bytes += totals.bytes;
        fold(&mut digest, totals.digest);
        unsupported = unsupported.or(totals.unsupported);
        match refusals {
            Refusals::Stop if totals.refusal.is_some() => {
                refusal = totals.refusal;
                break;
            }
            Refusals::Stop => {}
            Refusals::Repeat if runs == 1 => refusal = totals.refusal,
            Refusals::Repeat if totals.refusal != refusal => {
                return Err("operation outcome changed across repetitions".into());
            }
            Refusals::Repeat => {}
        }
        if held.map(Budget::used) != retained {
            return Err("operation retained charged storage".into());
        }
    }
    let elapsed = start.map_or(0, |begin| begin.elapsed().as_nanos());
    if memory {
        Boundary::End.mark(&mut out)?;
    }
    write_result(
        &mut out,
        name,
        runs,
        elapsed,
        &Totals {
            packets,
            bytes,
            digest,
            refusal,
            unsupported,
        },
        fields,
    )?;
    Ok(())
}

/// Write and flush the standard summary after the operation has completed.
///
/// # Errors
///
/// Returns the stream's write or flush error.
pub fn write_summary(
    out: &mut impl Write,
    name: &str,
    iterations: u64,
    elapsed_ns: u128,
    totals: &Totals,
) -> io::Result<()> {
    write_result(out, name, iterations, elapsed_ns, totals, &[])
}

fn write_result(
    out: &mut impl Write,
    name: &str,
    runs: u64,
    elapsed: u128,
    totals: &Totals,
    fields: &[(&str, u64)],
) -> io::Result<()> {
    let Totals {
        packets,
        bytes,
        digest,
        refusal,
        unsupported,
    } = totals;
    let mut line = format!(
        "{{\"schema\":1,\"operation\":\"{name}\",\"iterations\":{runs},\"packets\":{packets},\"bytes\":{bytes},\"digest\":{digest},\"elapsed_ns\":{elapsed},\"eof\":{}",
        refusal.is_none()
    );
    for (key, value) in fields {
        line.push_str(&format!(",\"{key}\":{value}"));
    }
    if let Some(detail) = unsupported {
        line.push_str(",\"unsupported\":");
        push_quoted(&mut line, detail);
    }
    if let Some(refusal) = refusal {
        line.push_str(",\"refusal\":");
        push_quoted(&mut line, &refusal.reason);
        line.push_str(",\"cause\":");
        line.push_str(&refusal.cause);
    }
    line.push('}');
    writeln!(out, "{line}")?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_refusals_execute_every_operation() -> Result<()> {
        let invocation = Invocation {
            mode: Mode::Fold,
            path: "",
            iterations: 50,
            arguments: &[],
        };
        let mut executed = 0;
        measure_repeated(&invocation, "test-refusal", None, || {
            executed += 1;
            Ok(Totals {
                refusal: Some(Box::new(Refusal {
                    reason: "control refusal".into(),
                    cause: "{}".into(),
                })),
                ..Totals::default()
            })
        })?;
        assert_eq!(executed, invocation.iterations);
        Ok(())
    }

    #[test]
    fn a_refusal_that_changes_between_repetitions_fails() {
        let invocation = Invocation {
            mode: Mode::Fold,
            path: "",
            iterations: 3,
            arguments: &[],
        };
        let mut executed = 0;
        let error = measure_repeated(&invocation, "test-refusal", None, || {
            executed += 1;
            Ok(Totals {
                refusal: (executed < 3).then(|| {
                    Box::new(Refusal {
                        reason: "control refusal".into(),
                        cause: "{}".into(),
                    })
                }),
                ..Totals::default()
            })
        })
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "operation outcome changed across repetitions"
        );
    }
}
