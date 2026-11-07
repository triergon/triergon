//! Subtitle decode measurement worker for the harness: the subtitle operations of
//! the subtitles build of `harness/reference.c` (`harness/reference_subtitles.h`),
//! through `triergon-subtitles` and the Triergon container readers.
//!
//! usage: subtitle-worker sub-trace FILE KIND [ARG...] | sub-fold FILE KIND [ARG...]
//! | sub-bench FILE ITERATIONS KIND [ARG...] | sub-memory FILE KIND [ARG...]
//! | sub-input FILE KIND [ARG...]
//!
//! KIND is `pgs`, `dvb`, `dvd`, `teletext` or `cc`. ARG is `stream=N` (the Nth
//! stream of that kind, from 0), `opt:NAME=VALUE` (an option of the reference
//! decoder, applied with the same meaning) or, for `cc`, `source=video` (A/53 from
//! the video stream, the default) or `source=packets` (608 byte pairs, such as an
//! SCC file).
//!
//! Preparation (reading the file, demuxing, extracting caption data) is outside
//! the timed interval and memory window. The operation creates the decoder,
//! decodes every prepared packet in order, drains it, folds every output subtitle
//! into the canonical [`sink`] form and drops everything. `sub-fold`, `sub-bench`
//! and `sub-memory` print one JSON line with the event count, folded bytes and
//! digest; `sub-trace` prints the header line and one line per event instead.
//! `sub-input` prints the prepared packets as the reference's `sub-input` does:
//! a header line with the time base and configuration, then one line per packet.
#![forbid(unsafe_code)]

mod cc;
mod container;
mod dvb;
mod dvd;
mod pgs;
mod sink;
mod teletext;

use container::Result;
use sink::{Sink, Work};
use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Pgs,
    Dvb,
    Dvd,
    Teletext,
    Cc,
}

/// The operation's arguments after the file.
#[derive(Clone, Debug)]
pub struct Args {
    pub kind: Kind,
    pub stream: usize,
    /// Reference decoder options, in the order given.
    pub options: Vec<(String, String)>,
    /// For `cc`: extract A/53 from the video stream rather than read 608 packets.
    pub from_video: bool,
}

impl Args {
    fn parse(words: &[String]) -> Result<Self> {
        let (kind, rest) = words.split_first().ok_or("missing KIND")?;
        let kind = match kind.as_str() {
            "pgs" => Kind::Pgs,
            "dvb" => Kind::Dvb,
            "dvd" => Kind::Dvd,
            "teletext" => Kind::Teletext,
            "cc" => Kind::Cc,
            other => return Err(format!("unknown kind {other}").into()),
        };
        let mut args = Args {
            kind,
            stream: 0,
            options: Vec::new(),
            from_video: kind == Kind::Cc,
        };
        for word in rest {
            if let Some(n) = word.strip_prefix("stream=") {
                args.stream = n.parse()?;
            } else if word == "source=video" {
                args.from_video = true;
            } else if word == "source=packets" {
                args.from_video = false;
            } else if let Some((name, value)) =
                word.strip_prefix("opt:").and_then(|o| o.split_once('='))
            {
                args.options.push((name.to_owned(), value.to_owned()));
            } else {
                return Err(format!("unknown argument {word}").into());
            }
        }
        Ok(args)
    }

    /// The value of decoder option `name`, if given.
    pub fn option(&self, name: &str) -> Option<&str> {
        self.options
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// Refuse options the candidate does not implement, as the reference refuses
    /// options its decoder does not have.
    pub fn only(&self, known: &[&str]) -> Result<()> {
        match self.options.iter().find(|(n, _)| !known.contains(&n.as_str())) {
            Some((name, _)) => Err(format!("unsupported decoder option {name}").into()),
            None => Ok(()),
        }
    }
}

/// Prepared input for one kind: everything the timed operation reads.
pub trait Prepared {
    /// One operation: decoder creation, every packet, the drain and teardown.
    fn run(&self, sink: &mut Sink<'_>) -> Result<()>;
    /// The prepared packets, for `sub-input`.
    fn input(&self) -> Option<&container::Track> {
        None
    }
}

fn prepare(file: &Path, args: &Args) -> Result<Box<dyn Prepared>> {
    match args.kind {
        Kind::Pgs => pgs::prepare(file, args),
        Kind::Dvb => dvb::prepare(file, args),
        Kind::Dvd => dvd::prepare(file, args),
        Kind::Teletext => teletext::prepare(file, args),
        Kind::Cc => cc::prepare(file, args),
    }
}

fn mix(total: &mut Work, once: Work) {
    total.events += once.events;
    total.bytes += once.bytes;
    triergon_harness_protocol::digest::fold(&mut total.digest, once.digest);
}

fn main() {
    let words: Vec<String> = std::env::args().skip(1).collect();
    match run(&words) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("subtitle-worker: {error}");
            std::process::exit(1);
        }
    }
}

fn run(words: &[String]) -> Result<i32> {
    let Some((mode, rest)) = words.split_first() else {
        return Ok(2);
    };
    let bench = mode == "sub-bench";
    let memory = mode == "sub-memory";
    let trace = mode == "sub-trace";
    if !(bench || memory || trace || mode == "sub-fold" || mode == "sub-input") {
        return Ok(2);
    }
    let first = if bench { 2 } else { 1 };
    if rest.len() <= first {
        return Ok(2);
    }
    let iterations: u64 = if bench { rest[1].parse()? } else { 1 };
    if iterations == 0 || iterations > 1_000_000 {
        return Ok(2);
    }
    let args = Args::parse(&rest[first..])?;
    let prepared = prepare(Path::new(&rest[0]), &args)?;
    let stdout = io::stdout();
    if mode == "sub-input" {
        let track = prepared.input().ok_or("no prepared packet view")?;
        container::write_input(&mut stdout.lock(), track)?;
        return Ok(0);
    }
    if memory {
        stdout.lock().write_all(b"TRIERGON_BEGIN_V1\n")?;
        stdout.lock().flush()?;
    }
    let mut total = Work::default();
    let start = Instant::now();
    let mut lock = stdout.lock();
    for _ in 0..iterations {
        let mut sink = if trace {
            Sink::new(Some(&mut lock))
        } else {
            Sink::new(None)
        };
        prepared.run(&mut sink)?;
        let once = sink.work;
        mix(&mut total, once);
    }
    let elapsed = if bench { start.elapsed().as_nanos() } else { 0 };
    if memory {
        lock.write_all(b"TRIERGON_END_V1\n")?;
    }
    if !trace {
        writeln!(
            lock,
            "{{\"schema\":1,\"operation\":\"subtitle-decode\",\"iterations\":{iterations},\"packets\":{},\"bytes\":{},\"digest\":{},\"elapsed_ns\":{elapsed},\"eof\":true}}",
            total.events, total.bytes, total.digest
        )?;
    }
    lock.flush()?;
    Ok(0)
}
