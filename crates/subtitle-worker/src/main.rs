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
//! Preparation (reading the file, demuxing, extracting caption data, creating the
//! decoder's pool) is outside the timed interval and memory window. The operation
//! creates the decoder, decodes every prepared packet in order, drains it, folds
//! every output subtitle into the canonical [`sink`] form and drops everything.
//! `sub-fold`, `sub-bench` and `sub-memory` print one JSON line with the event
//! count, folded bytes and digest (`triergon-harness-runner`); `sub-trace` prints
//! the header line and one line per event instead.
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
use sink::Sink;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;
use triergon_core::Budget;
use triergon_harness_runner::{Invocation, Mode, Totals, measure};

/// The words after the file: KIND and its arguments.
const USAGE: &str = " KIND [ARG...]";

/// Work units a decoder gets per call, so its yield and resume paths run inside
/// the measurement as a cooperative caller drives them.
pub const WORK_SLICE: u64 = 1 << 16;

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
        match self
            .options
            .iter()
            .find(|(n, _)| !known.contains(&n.as_str()))
        {
            Some((name, _)) => Err(format!("unsupported decoder option {name}").into()),
            None => Ok(()),
        }
    }
}

/// Prepared input for one kind: everything the timed operation reads.
pub trait Prepared {
    /// One operation: decoder creation, every packet, the drain and teardown.
    fn run(&self, sink: &mut Sink<'_>) -> Result<()>;
    /// The pool the prepared input and the decoder are charged to; an operation
    /// leaves it as it found it.
    fn pool(&self) -> &Budget;
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

fn main() -> ExitCode {
    let words: Vec<String> = std::env::args().skip(1).collect();
    match run(&words) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("subtitle-worker: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(words: &[String]) -> Result<()> {
    if let Some(("sub-input", rest)) = words.split_first().map(|(m, r)| (m.as_str(), r)) {
        let [file, arguments @ ..] = rest else {
            return Err(format!("usage: subtitle-worker sub-input FILE{USAGE}").into());
        };
        let prepared = prepare(Path::new(file), &Args::parse(arguments)?)?;
        let track = prepared.input().ok_or("no prepared packet view")?;
        let mut out = io::stdout().lock();
        container::write_input(&mut out, track)?;
        out.flush()?;
        return Ok(());
    }
    let invocation = Invocation::parse(words, "sub-", USAGE, 1..=usize::MAX, true)?;
    let prepared = prepare(
        Path::new(invocation.path),
        &Args::parse(invocation.arguments)?,
    )?;
    if invocation.mode == Mode::Trace {
        let mut out = io::stdout().lock();
        prepared.run(&mut Sink::new(Some(&mut out)))?;
        out.flush()?;
        return Ok(());
    }
    measure(
        &invocation,
        "subtitle-decode",
        Some(prepared.pool()),
        || {
            let mut sink = Sink::new(None);
            prepared.run(&mut sink)?;
            Ok(Totals {
                packets: sink.work.events,
                bytes: sink.work.bytes,
                digest: sink.work.digest,
                refusal: None,
            })
        },
    )
}
