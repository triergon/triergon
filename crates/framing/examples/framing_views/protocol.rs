use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::time::Instant;

#[path = "fold.rs"]
mod fold;
pub(super) use fold::Work;

const MAXIMUM_INPUT_BYTES: u64 = 256 * 1024 * 1024;
const MAXIMUM_ITERATIONS: usize = 1_000_000;

pub(super) type Action<'a> =
    Box<dyn FnMut(&mut Work, Option<&mut dyn Write>) -> Result<(), String> + 'a>;
pub(super) type Prepare = for<'a> fn(&str, &'a [u8], &[OsString]) -> Result<Operation<'a>, String>;

pub(super) struct Operation<'a> {
    pub name: &'static str,
    pub run: Action<'a>,
}

pub(super) fn main(
    args: impl IntoIterator<Item = OsString>,
    operations: &[(&str, Prepare)],
) -> ExitCode {
    match execute(args, operations) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}

fn execute(
    args: impl IntoIterator<Item = OsString>,
    operations: &[(&str, Prepare)],
) -> Result<(), String> {
    let args: Vec<_> = args.into_iter().skip(1).collect();
    let usage =
        "usage: PREFIX{fold|trace|memory} FILE [ARG...] | PREFIXbench FILE ITERATIONS [ARG...]";
    let [command, file, tail @ ..] = args.as_slice() else {
        return Err(usage.into());
    };
    let (prefix, prepare, mode) = operations
        .iter()
        .find_map(|&(prefix, prepare)| {
            let mode = command.to_str()?.strip_prefix(prefix)?;
            matches!(mode, "fold" | "trace" | "bench" | "memory").then_some((prefix, prepare, mode))
        })
        .ok_or(usage)?;
    let (iterations, arguments) = if mode == "bench" {
        let (count, rest) = tail.split_first().ok_or(usage)?;
        let count = OsStr::to_str(count)
            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|n| (1..=MAXIMUM_ITERATIONS).contains(n))
            .ok_or("invalid iteration count")?;
        (count, rest)
    } else {
        (1, tail)
    };
    let mut input = Vec::new();
    File::open(file)
        .and_then(|file| file.take(MAXIMUM_INPUT_BYTES + 1).read_to_end(&mut input))
        .map_err(|error| format!("{}: {error}", file.to_string_lossy()))?;
    if input.len() as u64 > MAXIMUM_INPUT_BYTES {
        return Err("input exceeds the worker limit".into());
    }
    let mut operation = prepare(prefix, &input, arguments)?;
    let mut output = io::stdout().lock();
    if mode == "memory" {
        marker(&mut output, b"TRIERGON_BEGIN_V1\n")?;
    }
    let mut total = Work::default();
    let start = (mode == "bench").then(Instant::now);
    for _ in 0..iterations {
        let mut once = Work::default();
        let trace = (mode == "trace").then_some(&mut output as &mut dyn Write);
        (operation.run)(&mut once, trace)?;
        total.iteration(&once);
    }
    let elapsed = start.map_or(0, |start| start.elapsed().as_nanos());
    if mode == "memory" {
        marker(&mut output, b"TRIERGON_END_V1\n")?;
    }
    writeln!(
        output,
        "{{\"schema\":1,\"operation\":\"{}\",\"iterations\":{iterations},\"packets\":{},\"bytes\":{},\"digest\":{},\"elapsed_ns\":{elapsed},\"eof\":true}}",
        operation.name, total.packets, total.bytes, total.digest,
    )
    .map_err(|error| error.to_string())
}

fn marker(output: &mut impl Write, line: &[u8]) -> Result<(), String> {
    output
        .write_all(line)
        .and_then(|()| output.flush())
        .map_err(|error| error.to_string())
}

pub(super) fn smoke<'a>(
    _prefix: &str,
    bytes: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
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
