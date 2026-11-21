//! A bounded forward-only AVC/AAC TS188 session with two retained units per stream.
#![forbid(unsafe_code)]

use crate::runner::{Invocation, MAX_INPUT, Mode, Result, Totals, measure};
use std::cell::Cell;
use std::error::Error;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, InputCursor, InputRequest, InputWindow, SourceCapabilities, SourceId, Time,
    WorkBudget,
};
use triergon_harness_protocol::digest::fold_bytes;
use triergon_hls::progressive::{
    TsProgressiveLimits, TsProgressiveReader, TsProgressiveStep, TsProgressiveUnit,
};
use triergon_playback::{Source, SourceReply};

pub const PREFIX: &str = "progressive-";
const OP: &str = "ProgressiveAvcAacTsOwnedFeedV1";
const WINDOW: usize = 4096;
const DEPTH: usize = 2;
const LIVE_BYTES: usize = 256 * 1024;
const ID: SourceId = SourceId(1);

#[derive(Default)]
struct Lifetime {
    aborts: Cell<usize>,
    closes: Cell<usize>,
}

struct Forward(Rc<Lifetime>);
impl Source for Forward {
    fn poll(
        &mut self,
        _: &InputRequest,
        _: &mut WorkBudget,
        _: &AtomicBool,
    ) -> triergon_core::Result<SourceReply> {
        // The caller reads the next bounded window only in response to NeedInput.
        Ok(SourceReply::Pending)
    }

    fn invalidate(&mut self) {
        unreachable!("forward body cannot be invalidated for a seek");
    }

    fn abort(&mut self) {
        self.0.aborts.set(self.0.aborts.get() + 1);
    }

    fn close(&mut self) {
        self.0.closes.set(self.0.closes.get() + 1);
    }
}

#[derive(Default)]
struct InputFacts {
    bytes: u64,
    reads: u64,
    max_read: usize,
    eof: bool,
}

struct Held {
    unit: TsProgressiveUnit,
    acquired_at_read: u64,
}

#[derive(Default)]
struct RetainedStream {
    queue: [Option<Held>; DEPTH],
    head: usize,
    high: usize,
    count: u64,
    bytes: u64,
    digest: u64,
    first: Option<u64>,
    across_read: u64,
    retired_before_eof: u64,
}

fn mix(value: &mut u64, word: u64) {
    *value = value.wrapping_mul(31).wrapping_add(word);
}

fn pair(time: Option<Time>) -> Result<[i64; 2]> {
    let Some(time) = time else {
        return Ok([0, 0]);
    };
    Ok([
        i64::try_from(time.numerator()).map_err(boxed)?,
        i64::try_from(time.denominator()).map_err(boxed)?,
    ])
}

impl RetainedStream {
    #[inline]
    fn fields(
        &mut self,
        tag: char,
        fields: &[i64],
        out: &mut Option<&mut dyn Write>,
    ) -> Result<()> {
        if let Some(out) = out.as_deref_mut() {
            return self.trace_fields(out, tag, fields);
        }
        mix(&mut self.digest, u64::from(tag as u32));
        for &field in fields {
            mix(&mut self.digest, field as u64);
        }
        Ok(())
    }

    /// Folds and writes a unit's fields to the trace.
    #[cold]
    fn trace_fields(&mut self, out: &mut dyn Write, tag: char, fields: &[i64]) -> Result<()> {
        mix(&mut self.digest, u64::from(tag as u32));
        write!(out, "{tag}")?;
        for &field in fields {
            mix(&mut self.digest, field as u64);
            write!(out, " {field}")?;
        }
        write!(out, " ")?;
        Ok(())
    }

    #[inline]
    fn payload(&mut self, data: &[u8], out: &mut Option<&mut dyn Write>) -> Result<()> {
        match out.as_deref_mut() {
            Some(out) => self.trace_payload(out, data)?,
            None => fold_bytes(&mut self.digest, data),
        }
        self.bytes += data.len() as u64;
        Ok(())
    }

    /// Folds and writes a unit's payload to the trace in hex.
    #[cold]
    fn trace_payload(&mut self, out: &mut dyn Write, data: &[u8]) -> Result<()> {
        for &byte in data {
            mix(&mut self.digest, u64::from(byte));
            write!(out, "{byte:02x}")?;
        }
        Ok(())
    }

    fn retire(
        &mut self,
        held: Held,
        input: &InputFacts,
        out: &mut Option<&mut dyn Write>,
    ) -> Result<TsProgressiveUnit> {
        self.across_read += u64::from(input.reads > held.acquired_at_read);
        self.retired_before_eof += u64::from(!input.eof);
        match &held.unit {
            TsProgressiveUnit::Video(video) => {
                let clock = video.clock.ok_or_else(|| failure("missing AVC clock"))?;
                let size = video.feed_spans().map(<[u8]>::len).sum::<usize>();
                self.fields(
                    'v',
                    &[
                        clock.pts90.ok_or_else(|| failure("missing AVC PTS"))?,
                        clock.dts90.ok_or_else(|| failure("missing AVC DTS"))?,
                        i64::try_from(
                            clock
                                .source_pts_raw33
                                .ok_or_else(|| failure("missing AVC source PTS"))?,
                        )
                        .map_err(boxed)?,
                        clock
                            .source_dts_raw33
                            .map(i64::try_from)
                            .transpose()
                            .map_err(boxed)?
                            .unwrap_or(-1),
                        i64::from(video.idr),
                        i64::from(video.raster.width),
                        i64::from(video.raster.height),
                        i64::try_from(size).map_err(boxed)?,
                    ],
                    out,
                )?;
                for span in video.feed_spans() {
                    self.payload(span, out)?;
                }
            }
            TsProgressiveUnit::Audio {
                frame,
                presentation,
            } => {
                let rate = frame
                    .header
                    .coded_frequency_hz()
                    .ok_or_else(|| failure("missing AAC rate"))?;
                let channels = frame
                    .header
                    .coded_channel_count()
                    .ok_or_else(|| failure("missing AAC channels"))?;
                let [pn, pd] = pair(frame.pts)?;
                let [dn, dd] = pair(frame.dts)?;
                let [tn, td] = pair(Some(*presentation))?;
                let [sn, sd] = pair(Some(frame.duration))?;
                self.fields(
                    'a',
                    &[
                        i64::from(rate),
                        i64::from(channels),
                        1024,
                        pn,
                        pd,
                        dn,
                        dd,
                        tn,
                        td,
                        sn,
                        sd,
                        i64::try_from(frame.bytes.len()).map_err(boxed)?,
                    ],
                    out,
                )?;
                self.payload(frame.bytes.as_ref(), out)?;
            }
        }
        if let Some(out) = out.as_deref_mut() {
            end_trace_line(out)?;
        }
        Ok(held.unit)
    }

    /// Holds `unit` and returns the oldest held unit, retired, when the queue
    /// was full.
    fn enqueue(
        &mut self,
        unit: TsProgressiveUnit,
        input: &InputFacts,
        out: &mut Option<&mut dyn Write>,
    ) -> Result<Option<TsProgressiveUnit>> {
        self.first.get_or_insert(input.bytes);
        let retired = match self.queue[self.head].take() {
            Some(oldest) => Some(self.retire(oldest, input, out)?),
            None => None,
        };
        self.queue[self.head] = Some(Held {
            unit,
            acquired_at_read: input.reads,
        });
        self.head = (self.head + 1) % DEPTH;
        self.high = self
            .high
            .max(self.queue.iter().filter(|slot| slot.is_some()).count());
        self.count += 1;
        Ok(retired)
    }

    fn drain(&mut self, input: &InputFacts, out: &mut Option<&mut dyn Write>) -> Result<()> {
        for _ in 0..DEPTH {
            if let Some(held) = self.queue[self.head].take() {
                self.retire(held, input, out)?;
            }
            self.head = (self.head + 1) % DEPTH;
        }
        Ok(())
    }
}

/// Writes which track a unit came out on to the trace.
#[cold]
fn trace_output(out: &mut dyn Write, stream: usize) -> Result<()> {
    writeln!(out, "o {}", ['v', 'a'][stream])?;
    Ok(())
}

/// Ends a line of the trace.
#[cold]
fn end_trace_line(out: &mut dyn Write) -> Result<()> {
    writeln!(out)?;
    Ok(())
}

/// Describes a playback failure as the operation reports it.
#[cold]
fn playback_error(error: triergon_playback::Error) -> Box<dyn Error> {
    format!("{error:?}").into()
}

/// The error of a check the operation failed. Errors are built out of line, so
/// that the session loop holds only the code it runs.
#[cold]
fn failure(message: &'static str) -> Box<dyn Error> {
    message.into()
}

/// A failure of a library call, built out of line as [`failure`] is.
#[cold]
fn boxed(error: impl Error + 'static) -> Box<dyn Error> {
    Box::new(error)
}

/// Prints the facts of both retained streams to stderr, after a traced session.
#[cold]
fn report_streams(input: &InputFacts, [video, audio]: &[RetainedStream; 2]) {
    eprintln!(
        "STREAM {{\"input_bytes\":{},\"reads\":{},\"max_read\":{},\"first\":[{},{}],\"high\":[{},{}],\"across_read\":[{},{}],\"retired_before_eof\":[{},{}],\"retired\":true}}",
        input.bytes,
        input.reads,
        input.max_read,
        video.first.unwrap(),
        audio.first.unwrap(),
        video.high,
        audio.high,
        video.across_read,
        audio.across_read,
        video.retired_before_eof,
        audio.retired_before_eof,
    );
}

#[derive(Default)]
struct Work {
    packets: u64,
    bytes: u64,
    digest: u64,
}

fn operation(
    path: &Path,
    has_dts: bool,
    mut out: Option<&mut dyn Write>,
    require_streaming: bool,
) -> Result<Work> {
    let mut file = File::open(path)?;
    let facts = file.metadata()?;
    let size = facts.len();
    if !facts.is_file() || size == 0 || size > MAX_INPUT || !size.is_multiple_of(188) {
        return Err(failure("expected bounded TS188 body"));
    }
    let pool = Budget::new(LIVE_BYTES)?;
    let lifetime = Rc::new(Lifetime::default());
    let cursor = InputCursor::new(
        &pool,
        ID,
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        WINDOW,
    )?;
    let mut reader = TsProgressiveReader::new(
        cursor,
        Forward(lifetime.clone()),
        TsProgressiveLimits::default(),
    )
    .map_err(playback_error)?;
    let cancel = AtomicBool::new(false);
    let mut input = InputFacts::default();
    let mut streams: [RetainedStream; 2] = Default::default();
    let mut scratch = [0; WINDOW];
    loop {
        match reader
            .advance(&mut WorkBudget::new(65536), &cancel)
            .map_err(playback_error)?
        {
            TsProgressiveStep::NeedInput(r) => {
                if input.eof || r.offset != input.bytes || r.source != ID || r.maximum == 0 {
                    return Err(failure("unexpected forward source request"));
                }
                let count = file
                    .read(&mut scratch[..r.maximum.min(WINDOW)])
                    .map_err(boxed)?;
                let reply = if count == 0 {
                    if input.bytes != size {
                        return Err(failure("early physical EOF"));
                    }
                    input.eof = true;
                    SourceReply::End {
                        source: r.source,
                        token: r.token,
                    }
                } else {
                    input.bytes += count as u64;
                    input.reads += 1;
                    input.max_read = input.max_read.max(count);
                    if input.bytes > size {
                        return Err(failure("body grew"));
                    }
                    SourceReply::Data(
                        InputWindow::new(
                            r.source,
                            r.token,
                            r.offset,
                            Bytes::copy_from_slice(&pool, &scratch[..count]).map_err(boxed)?,
                        )
                        .map_err(boxed)?,
                    )
                };
                if !reader.provide(&reply).map_err(playback_error)? {
                    return Err(failure("current reply was refused"));
                }
            }
            TsProgressiveStep::Unit(unit) => {
                if let TsProgressiveUnit::Video(ref unit) = unit
                    && unit
                        .clock
                        .and_then(|clock| clock.source_dts_raw33)
                        .is_some()
                        != has_dts
                {
                    return Err(failure(
                        "AVC source DTS shape differs from fixture contract",
                    ));
                }
                if let TsProgressiveUnit::Audio { ref frame, .. } = unit
                    && frame.dts.is_some()
                {
                    return Err(failure(
                        "AAC source DTS differs from PTS-only fixture contract",
                    ));
                }
                let stream = usize::from(matches!(unit, TsProgressiveUnit::Audio { .. }));
                if let Some(out) = out.as_deref_mut() {
                    trace_output(out, stream)?;
                }
                if let Some(retired) = streams[stream].enqueue(unit, &input, &mut out)? {
                    reader.reuse_unit(retired);
                }
            }
            TsProgressiveStep::Yield => {}
            TsProgressiveStep::End => break,
            TsProgressiveStep::Backpressure { .. } => {
                return Err(failure("progressive pool backpressure"));
            }
        }
    }
    drop(reader);
    if !input.eof || lifetime.aborts.get() != 1 || lifetime.closes.get() != 1 {
        return Err(failure("source completion or retirement did not match"));
    }
    let mut work = Work::default();
    for stream in &mut streams {
        stream.drain(&input, &mut out)?;
        if stream.high != DEPTH
            || (require_streaming
                && (stream.first.is_none_or(|first| first >= size)
                    || stream.across_read == 0
                    || stream.retired_before_eof == 0))
        {
            return Err(failure("progressive owned retention proof missing"));
        }
        work.packets += stream.count;
        work.bytes += stream.bytes;
        mix(&mut work.digest, stream.count);
        mix(&mut work.digest, stream.digest);
    }
    if pool.used() != Budget::control_storage_bytes() {
        return Err(failure("progressive pool retained charged storage"));
    }
    if out.is_some() {
        report_streams(&input, &streams);
    }
    Ok(work)
}

/// `progressive-fold FILE CLOCK PROGRESS`, `progressive-bench FILE 50 CLOCK PROGRESS`,
/// `progressive-memory FILE CLOCK PROGRESS` or `progressive-trace FILE CLOCK PROGRESS`
/// (`fold` with every retired unit printed, and the stream facts on stderr); CLOCK
/// is `pts-only` or `pts-dts`, PROGRESS `before-eof` or `at-eof`. Every operation
/// reads the file from disk in bounded windows.
pub(super) fn run(args: &[String]) -> Result<()> {
    let invocation = Invocation::parse(args, PREFIX, " CLOCK PROGRESS", 2..=2, true)?;
    let [clock, progress] = invocation.arguments else {
        return Err(failure("expected CLOCK and PROGRESS"));
    };
    let has_dts = match clock.as_str() {
        "pts-dts" => true,
        "pts-only" => false,
        _ => return Err(failure("expected pts-only or pts-dts video clock")),
    };
    let require_streaming = match progress.as_str() {
        "before-eof" => true,
        "at-eof" => false,
        _ => {
            return Err(failure(
                "expected before-eof or at-eof progress requirement",
            ));
        }
    };
    if invocation.mode == Mode::Bench && invocation.iterations != 50 {
        return Err(failure("progressive-bench runs 50 sessions"));
    }
    let path = Path::new(invocation.path);
    let trace = invocation.mode == Mode::Trace;
    // Every operation reads the file into its own pool and checks that pool itself.
    measure(&invocation, OP, None, || {
        let work = if trace {
            let mut out = io::BufWriter::new(io::stdout().lock());
            let work = operation(path, has_dts, Some(&mut out), require_streaming)?;
            out.flush()?;
            work
        } else {
            operation(path, has_dts, None, require_streaming)?
        };
        Ok(Totals {
            packets: work.packets,
            bytes: work.bytes,
            digest: work.digest,
            refusal: None,
        })
    })
}

#[cfg(test)]
#[path = "progressive_tests.rs"]
mod tests;
