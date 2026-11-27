//! Facade selection over a supplied prefix and through its input adapters.

use std::cell::Cell;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use triergon::isobmff::ReaderEvent;
use triergon::{
    AnyOther, AnyPacket, AnyReader, Budget, Error, Event, PacketInfo, Probed, SourceFormat,
    open_blocking, open_sequential,
};
use triergon_harness_protocol::digest::signed;

use triergon_harness_runner::{Operation, Outcome};

#[derive(Clone, Copy)]
enum Mode {
    Bytes,
    Blocking,
    Sequential,
}

pub(super) fn prepare<'a>(input: &'a [u8], arguments: &[&str]) -> Result<Operation<'a>, String> {
    // With a trailing `--report-refusal`, a movie read the reader refuses as unsupported
    // names its detail on the result line.
    let (proof, arguments) = match arguments.split_last() {
        Some((&"--report-refusal", rest)) => (true, rest),
        _ => (false, arguments),
    };
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
    let whole = length == input.len();
    Ok(Operation {
        name: "facade-probe",
        run: Box::new(move |work, trace| {
            let case = Case {
                input: &input[..length],
                whole,
                window,
                mode,
                proof,
            };
            run(&case, &pool, work, trace)
        }),
    })
}

fn parse(text: &str, max: usize) -> Result<usize, String> {
    text.parse::<usize>()
        .ok()
        .filter(|value| (1..=max).contains(value))
        .ok_or_else(|| format!("expected an integer in 1..={max}"))
}

struct Case<'a> {
    input: &'a [u8],
    whole: bool,
    window: usize,
    mode: Mode,
    proof: bool,
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
    case: &Case<'_>,
    pool: &Budget,
    work: &mut Outcome,
    mut trace: Option<&mut dyn Write>,
) -> Result<(), String> {
    let &Case {
        input,
        whole,
        window,
        mode,
        proof,
    } = case;
    let high_water = Cell::new(0);
    let source = Reads {
        cursor: Cursor::new(input),
        high_water: &high_water,
        window,
    };
    let mut more = false;
    let (verdict, width, replay) = match mode {
        Mode::Bytes => {
            // A whole input is decided with the bytes it has; a shorter prefix may
            // answer that more bytes are wanted.
            let probed = if whole {
                triergon::probe_whole(input).map_or(Probed::Unrecognised, Probed::Format)
            } else {
                triergon::probe(input)
            };
            more = matches!(probed, Probed::Need(_));
            let (verdict, width) = format(probed);
            (verdict, width, None)
        }
        Mode::Blocking => match open_blocking(source, pool) {
            Ok(opened) => {
                let verdict = reader_format(opened.reader());
                check_replay(
                    opened.into_inner(),
                    input,
                    high_water.get(),
                    trace.is_some(),
                )?;
                (verdict, 0, Some(input.len()))
            }
            Err(error) => refusal(error, trace.is_some())?,
        },
        Mode::Sequential => match open_sequential(source, pool) {
            Ok(opened) => {
                let verdict = reader_format(opened.reader());
                check_replay(
                    opened.into_inner(),
                    input,
                    high_water.get(),
                    trace.is_some(),
                )?;
                (verdict, 0, Some(input.len()))
            }
            Err(error) => refusal(error, trace.is_some())?,
        },
    };
    let words = [
        verdict,
        width,
        // Bytes mode: whether the prefix asks for more bytes. The adapters: a replay.
        u64::from(replay.is_some() || more),
        replay.map_or(0, |length| length as u64),
    ];
    let words = &words[..if replay.is_some() { 4 } else { 3 }];
    record(work, &mut trace, 3, words, &[])?;
    if matches!(mode, Mode::Sequential) && verdict == 6 {
        let source = Reads {
            cursor: Cursor::new(input),
            high_water: &high_water,
            window,
        };
        if let Some(Error::Unsupported(detail)) = read_movie(source, pool, work, &mut trace)?
            && proof
        {
            work.unsupported = Some(detail);
        }
    }
    Ok(())
}

fn record(
    work: &mut Outcome,
    trace: &mut Option<&mut dyn Write>,
    kind: u64,
    words: &[u64],
    payload: &[u8],
) -> Result<(), String> {
    work.record(kind, words, payload);
    if let Some(trace) = trace {
        write!(trace, "{kind}").map_err(|error| error.to_string())?;
        for word in words {
            write!(trace, " {word}").map_err(|error| error.to_string())?;
        }
        writeln!(trace).map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Reads an ISO BMFF movie from a sequential source through the facade's adapter: one
/// record of kind 5 per track (its `track_ID` and time base), one of kind 4 per packet
/// (track, size, presentation and decode time, with the payload), and one of kind 6
/// that is 0 where the source was read to its end and 1 where the read was refused or
/// failed. Returns the error that ended the read.
fn read_movie(
    source: Reads<'_>,
    pool: &Budget,
    work: &mut Outcome,
    trace: &mut Option<&mut dyn Write>,
) -> Result<Option<Error>, String> {
    let mut movie = open_sequential(source, pool).map_err(|error| error.to_string())?;
    let ended = loop {
        match movie.next_event() {
            Ok(Some(Event::Packet(packet))) => {
                let AnyPacket::Isobmff(_) = &packet else {
                    return Err("the facade returned a packet of another format".into());
                };
                let words = [
                    packet.track().0,
                    packet.payload().len() as u64,
                    signed(packet.pts().unwrap_or(i64::MIN)),
                    signed(packet.dts().unwrap_or(i64::MIN)),
                ];
                record(work, trace, 4, &words, packet.payload())?;
            }
            Ok(Some(Event::Other(AnyOther::Isobmff(ReaderEvent::Movie)))) => {
                let AnyReader::Isobmff(reader) = movie.reader() else {
                    return Err("the facade opened a reader of another format".into());
                };
                for track in reader.tracks() {
                    let header = track.header();
                    let words = [u64::from(header.track_id), 1, u64::from(header.timescale)];
                    record(work, trace, 5, &words, &[])?;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break None,
            Err(error) => break Some(error),
        }
    };
    if let (Some(error), true) = (ended, trace.is_some()) {
        eprintln!("movie read ended: {error}");
    }
    record(work, trace, 6, &[u64::from(ended.is_some())], &[])?;
    Ok(ended)
}

fn check_replay(
    mut source: impl Read,
    mut expected: &[u8],
    consumed: usize,
    trace: bool,
) -> Result<(), String> {
    if trace {
        eprintln!("probe consumed {consumed} bytes before handoff");
    }
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
    if source
        .read(&mut buffer)
        .map_err(|error| error.to_string())?
        != 0
    {
        return Err("probe handoff extended the source prefix".into());
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

fn format(value: Probed) -> (u64, u64) {
    let Probed::Format(format) = value else {
        return (0, 0);
    };
    match format {
        SourceFormat::Flac => (1, 0),
        SourceFormat::MpegAudio => (2, 0),
        SourceFormat::Adts => (3, 0),
        SourceFormat::Ac3 => (4, 0),
        SourceFormat::Matroska => (5, 0),
        SourceFormat::Isobmff => (6, 0),
        SourceFormat::Ogg => (7, 0),
        SourceFormat::Ts => (8, 188),
        SourceFormat::M2ts => (8, 192),
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
