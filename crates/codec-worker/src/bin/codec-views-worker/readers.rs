use std::ffi::OsString;
use std::io::Write;

use triergon::Probe;
use triergon::elementary::{self, Kind};
use triergon::ogg::page::{PageHeader, write_page};

use super::protocol::{Operation, Work};

mod decode;
mod metadata;
mod metadata_wire;
mod packets;
mod tags;

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    match prefix {
        "media-probe-" => {
            let [family, length] = arguments else {
                return Err(
                    "media-probe takes elementary|container and a prefix length or whole".into(),
                );
            };
            let length = length.to_str().ok_or("invalid prefix length")?;
            let prefix = &input[..if length == "whole" {
                input.len()
            } else {
                length
                    .parse::<usize>()
                    .map_err(|_| "invalid prefix length")?
                    .min(input.len())
            }];
            let elementary = match family.to_str() {
                Some("elementary") => true,
                Some("container") => false,
                _ => return Err("unknown probe family".into()),
            };
            Ok(Operation {
                name: "media-probe",
                run: Box::new(move |work, trace| {
                    let verdict = if elementary {
                        match elementary::probe(prefix) {
                            Some(Kind::Flac) => 1,
                            Some(Kind::MpegAudio) => 2,
                            Some(Kind::Adts) => 3,
                            Some(Kind::Ac3) => 4,
                            None => 0,
                        }
                    } else {
                        match triergon::probe(prefix) {
                            Probe::Matroska => 5,
                            Probe::Isobmff => 6,
                            Probe::Ogg => 7,
                            Probe::Ts | Probe::M2ts => 8,
                            _ => 0,
                        }
                    };
                    record(work, trace, 3, &[verdict], &[])
                }),
            })
        }
        "ogg-tracks-" if arguments.is_empty() => Ok(Operation {
            name: "ogg-tracks",
            run: Box::new(move |work, trace| metadata::ogg_tracks(input, work, trace)),
        }),
        "elementary-metadata-" => {
            let (format, wire) = match arguments {
                [format] => (format, false),
                [format, wire] if wire == "wire" => (format, true),
                _ => return Err("elementary-metadata takes a format and optional wire mode".into()),
            };
            let format = format
                .to_str()
                .ok_or("invalid elementary format")?
                .to_owned();
            let run = if wire {
                metadata_wire::run
            } else {
                metadata::elementary
            };
            Ok(Operation {
                name: "elementary-metadata",
                run: Box::new(move |work, trace| run(input, &format, work, trace)),
            })
        }
        "ogg-pages-" if arguments.is_empty() || arguments == ["wire"] => Ok(Operation {
            name: "ogg-pages",
            run: Box::new(move |work, trace| pages(input, work, trace)),
        }),
        _ => Err(format!("usage: {prefix} operation is unavailable")),
    }
}

fn record(
    work: &mut Work,
    trace: Option<&mut dyn Write>,
    kind: u64,
    words: &[u64],
    payload: &[u8],
) -> Result<(), String> {
    work.record(kind, words, payload);
    if let Some(trace) = trace {
        let mut emit = || -> std::io::Result<()> {
            write!(trace, "{kind}")?;
            for word in words {
                write!(trace, " {word}")?;
            }
            if !payload.is_empty() {
                write!(trace, " ")?;
                for byte in payload {
                    write!(trace, "{byte:02x}")?;
                }
            }
            writeln!(trace)
        };
        emit().map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn pages(
    mut input: &[u8],
    work: &mut Work,
    mut trace: Option<&mut dyn Write>,
) -> Result<(), String> {
    let mut rewritten = Vec::new();
    while !input.is_empty() {
        let dest = trace.as_mut().map(|writer| &mut **writer as &mut dyn Write);
        let Ok(header) = PageHeader::parse(input) else {
            return record(work, dest, 8, &[], &[]);
        };
        let Some(page) = input.get(..header.page_len()) else {
            return record(work, dest, 8, &[], &[]);
        };
        let lacing = header.lacing(page);
        rewritten.clear();
        write_page(
            header.flags,
            header.granule,
            header.serial,
            header.sequence,
            lacing,
            &page[header.header_len()..],
            &mut rewritten,
        );
        let fields = [
            u64::from(page[4]),
            u64::from(header.flags),
            header.granule as u64,
            u64::from(header.serial),
            u64::from(header.sequence),
            u64::from(header.crc),
            u64::from(header.crc_ok(page)),
            lacing.len() as u64,
        ];
        record(work, dest, 3, &fields, lacing)?;
        let dest = trace.as_mut().map(|writer| &mut **writer as &mut dyn Write);
        record(work, dest, 1, &[rewritten.len() as u64], &rewritten)?;
        input = &input[header.page_len()..];
    }
    Ok(())
}

pub(super) fn command(command: &str, arguments: &[OsString]) -> Result<(), String> {
    if command.starts_with("audio-") {
        return decode::run(command, arguments);
    }
    let text = arguments
        .iter()
        .map(|arg| arg.to_str().ok_or("arguments must be UTF-8"))
        .collect::<Result<Vec<_>, _>>()?;
    let read = |path| std::fs::read(path).map_err(|error| format!("{path}: {error}"));
    match (command, text.as_slice()) {
        ("packets-stream", ["ogg", path]) => {
            packets::stream(&read(path)?).map_err(|error| error.to_string())
        }
        ("packets-push", [format, path, window, pool]) => {
            let window = window
                .parse::<usize>()
                .ok()
                .filter(|n| (1..=65536).contains(n))
                .ok_or("invalid reader window")?;
            let small = match *pool {
                "small" => true,
                "default" => false,
                _ => return Err("reader pool must be small or default".into()),
            };
            packets::push(format, &read(path)?, window, small).map_err(|error| error.to_string())
        }
        ("seek", [format, path, track, seconds, policy]) => {
            packets::seek(format, &read(path)?, track, seconds, policy)
        }
        _ => Err(format!("usage: {command} operation is unavailable")),
    }
}
