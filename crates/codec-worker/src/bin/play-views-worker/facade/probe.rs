//! Facade selection over a supplied prefix and through its input adapters.

use std::cell::Cell;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use triergon::{AnyReader, Budget, Error, Probe, open_blocking, open_sequential};

use super::super::protocol::{Operation, Work};

#[derive(Clone, Copy)]
enum Mode {
    Bytes,
    Blocking,
    Sequential,
}

pub(super) fn prepare<'a>(input: &'a [u8], arguments: &[&str]) -> Result<Operation<'a>, String> {
    let [mode, length, window] = arguments else {
        return Err("usage: facade-probe-* FILE bytes|blocking|sequential PREFIX WINDOW".into());
    };
    let mode = match *mode {
        "bytes" => Mode::Bytes,
        "blocking" => Mode::Blocking,
        "sequential" => Mode::Sequential,
        _ => return Err("invalid facade probe adapter".into()),
    };
    let length = if *length == "whole" {
        input.len()
    } else {
        parse(length, 256 << 20)?.min(input.len())
    };
    let window = parse(window, 65536)?;
    let pool = Budget::new(16 << 20).map_err(|error| error.to_string())?;
    Ok(Operation {
        name: "facade-probe",
        run: Box::new(move |work, trace| run(&input[..length], window, mode, &pool, work, trace)),
    })
}

fn parse(text: &str, max: usize) -> Result<usize, String> {
    text.parse::<usize>()
        .ok()
        .filter(|value| (1..=max).contains(value))
        .ok_or_else(|| format!("expected an integer in 1..={max}"))
}

struct Reads<'a> {
    cursor: Cursor<&'a [u8]>,
    high_water: &'a Cell<usize>,
    window: usize,
}

impl Read for Reads<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let size = buffer.len().min(self.window);
        let count = self.cursor.read(&mut buffer[..size])?;
        let at = self.cursor.position() as usize;
        self.high_water.set(at.max(self.high_water.get()));
        Ok(count)
    }
}

impl Seek for Reads<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.cursor.seek(position)
    }
}

fn run(
    input: &[u8],
    window: usize,
    mode: Mode,
    pool: &Budget,
    work: &mut Work,
    trace: Option<&mut dyn Write>,
) -> Result<(), String> {
    let high_water = Cell::new(0);
    let source = Reads {
        cursor: Cursor::new(input),
        high_water: &high_water,
        window,
    };
    let (verdict, width, replay) = match mode {
        Mode::Bytes => {
            let (verdict, width) = format(triergon::probe(input));
            (verdict, width, None)
        }
        Mode::Blocking => match open_blocking(source, pool) {
            Ok(opened) => {
                let verdict = reader_format(opened.reader());
                let consumed = high_water.get();
                check_replay(opened.into_inner(), &input[..consumed])?;
                (verdict, 0, Some(consumed))
            }
            Err(error) => refusal(error, trace.is_some())?,
        },
        Mode::Sequential => match open_sequential(source, pool) {
            Ok(opened) => {
                let verdict = reader_format(opened.reader());
                let consumed = high_water.get();
                check_replay(opened.into_inner(), &input[..consumed])?;
                (verdict, 0, Some(consumed))
            }
            Err(error) => refusal(error, trace.is_some())?,
        },
    };
    let words = [
        verdict,
        width,
        u64::from(replay.is_some()),
        replay.unwrap_or(0) as u64,
    ];
    let words = &words[..if replay.is_some() { 4 } else { 3 }];
    work.record(3, words, &[]);
    if let Some(trace) = trace {
        write!(trace, "3").map_err(|error| error.to_string())?;
        for word in words {
            write!(trace, " {word}").map_err(|error| error.to_string())?;
        }
        writeln!(trace).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn check_replay(mut source: impl Read, mut expected: &[u8]) -> Result<(), String> {
    let mut buffer = [0; 4096];
    while !expected.is_empty() {
        let count = expected.len().min(buffer.len());
        source
            .read_exact(&mut buffer[..count])
            .map_err(|error| error.to_string())?;
        if buffer[..count] != expected[..count] {
            return Err("probe handoff changed the source prefix".into());
        }
        expected = &expected[count..];
    }
    Ok(())
}

fn refusal(error: Error, trace: bool) -> Result<(u64, u64, Option<usize>), String> {
    match error {
        Error::Unsupported(_)
        | Error::Invalid(_)
        | Error::Truncated
        | Error::Policy(_)
        | Error::Limit { .. } => {
            if trace {
                eprintln!("probe refusal: {error}");
            }
            Ok((0, 0, None))
        }
        _ => Err(error.to_string()),
    }
}

fn format(value: Probe) -> (u64, u64) {
    match value {
        Probe::Unknown => (0, 0),
        Probe::Flac => (1, 0),
        Probe::MpegAudio => (2, 0),
        Probe::Adts => (3, 0),
        Probe::Ac3 => (4, 0),
        Probe::Matroska => (5, 0),
        Probe::Isobmff => (6, 0),
        Probe::Ogg => (7, 0),
        Probe::Ts => (8, 188),
        Probe::M2ts => (8, 192),
    }
}

fn reader_format(reader: &AnyReader) -> u64 {
    match reader {
        AnyReader::Flac(_) => 1,
        AnyReader::MpegAudio(_) => 2,
        AnyReader::Adts(_) => 3,
        AnyReader::Ac3(_) => 4,
        AnyReader::Matroska(_) => 5,
        AnyReader::Isobmff(_) => 6,
        AnyReader::Ogg(_) => 7,
        AnyReader::MpegTs(_) => 8,
    }
}
