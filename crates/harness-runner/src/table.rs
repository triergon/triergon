//! Workers that serve a table of operations: each entry owns a command prefix and
//! prepares its operation from the whole input file and the words after it.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::process::ExitCode;

use crate::{ITERATIONS, Mode, Refusals, Result, Totals, failed, measure_dyn};

/// The largest input file a table worker reads.
const MAX_INPUT: u64 = 256 * 1024 * 1024;

const USAGE: &str =
    "usage: PREFIX{fold|trace|memory} FILE [ARG...] | PREFIXbench FILE ITERATIONS [ARG...]";

/// One run of a prepared operation: it folds its records into the totals and, in
/// trace mode, prints them to the writer.
pub type Action<'a> =
    Box<dyn FnMut(&mut Totals, Option<&mut dyn Write>) -> std::result::Result<(), String> + 'a>;

/// Prepares the operation of a command prefix from the input file and the words after
/// the file (after the iteration count in bench mode).
pub type Prepare =
    for<'a> fn(&str, &'a [u8], &[OsString]) -> std::result::Result<Operation<'a>, String>;

/// A prepared operation and the name its result line reports.
pub struct Operation<'a> {
    /// The operation name of the result line.
    pub name: &'static str,
    /// One run of the operation.
    pub run: Action<'a>,
}

impl std::fmt::Debug for Operation<'_> {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Operation")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Serve `PREFIX{fold|trace|memory} FILE ARGS` and `PREFIXbench FILE ITERATIONS ARGS`
/// with the first entry of `operations` whose prefix the command starts with:
/// prepare the operation once, then run it as [`crate::measure`] does. `args` are the
/// process arguments, the program name first. A failure prints its message on stderr
/// and returns status 2.
pub fn serve(args: impl IntoIterator<Item = OsString>, operations: &[(&str, Prepare)]) -> ExitCode {
    let args: Vec<_> = args.into_iter().skip(1).collect();
    match dispatch(&args, operations) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => failed(None, 2, error),
    }
}

/// [`serve`] on the words after the program name, for a worker that serves some of its
/// commands from a table.
///
/// # Errors
///
/// The usage line when no entry serves the command, or the input's, the preparation's
/// or the operation's error.
pub fn dispatch(args: &[OsString], operations: &[(&str, Prepare)]) -> Result<()> {
    let [command, file, tail @ ..] = args else {
        return Err(USAGE.into());
    };
    let (prefix, prepare, mode) = operations
        .iter()
        .find_map(|&(prefix, prepare)| {
            let mode = match command.to_str()?.strip_prefix(prefix)? {
                "fold" => Mode::Fold,
                "trace" => Mode::Trace,
                "bench" => Mode::Bench,
                "memory" => Mode::Memory,
                _ => return None,
            };
            Some((prefix, prepare, mode))
        })
        .ok_or(USAGE)?;
    let (iterations, arguments) = if mode == Mode::Bench {
        let (count, rest) = tail.split_first().ok_or(USAGE)?;
        (iterations(count).ok_or("invalid iteration count")?, rest)
    } else {
        (1, tail)
    };
    let input = input(file)?;
    let mut operation = prepare(prefix, &input, arguments)?;
    let trace = mode == Mode::Trace;
    measure_dyn(
        mode,
        iterations,
        operation.name,
        None,
        &[],
        Refusals::Stop,
        &mut || {
            let mut totals = Totals::default();
            if trace {
                (operation.run)(&mut totals, Some(&mut io::stdout().lock()))?;
            } else {
                (operation.run)(&mut totals, None)?;
            }
            Ok(totals)
        },
    )
}

/// A bench count: decimal digits for a number of [`ITERATIONS`].
fn iterations(count: &OsStr) -> Option<u64> {
    count
        .to_str()
        .filter(|count| count.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|count| count.parse().ok())
        .filter(|count| ITERATIONS.contains(count))
}

/// The input file, refused when it is larger than [`MAX_INPUT`].
fn input(file: &OsStr) -> Result<Vec<u8>> {
    let mut input = Vec::new();
    File::open(file)
        .and_then(|opened| opened.take(MAX_INPUT + 1).read_to_end(&mut input))
        .map_err(|error| format!("{}: {error}", file.to_string_lossy()))?;
    if input.len() as u64 > MAX_INPUT {
        return Err("input exceeds the worker limit".into());
    }
    Ok(input)
}

/// `views-smoke-`: one record of the input's length and Adler-32 checksum, which
/// proves the command line, the input, the result line and the trace of a table
/// worker against the reference's.
///
/// # Errors
///
/// When any argument follows the file.
pub fn smoke<'a>(
    _prefix: &str,
    bytes: &'a [u8],
    arguments: &[OsString],
) -> std::result::Result<Operation<'a>, String> {
    if !arguments.is_empty() {
        return Err("views-smoke takes no arguments".into());
    }
    Ok(Operation {
        name: "views-smoke",
        run: Box::new(move |work, trace| {
            let (mut a, mut b) = (1_u32, 0_u32);
            for &byte in bytes {
                a = (a + u32::from(byte)) % 65521;
                b = (a + b) % 65521;
            }
            let checksum = u64::from(b << 16 | a);
            work.record(3, &[bytes.len() as u64, checksum], &[]);
            if let Some(trace) = trace {
                writeln!(trace, "3 {} {checksum}", bytes.len())
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        }),
    })
}
