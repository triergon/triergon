//! One-track MOV landings and regular source seeks.
use std::{ffi::OsString, io::Write, sync::atomic::AtomicBool};

use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputRequest, InputWindow, SourceCapabilities, SourceId,
    Step, Time, WorkBudget,
};
use triergon_harness_protocol::digest::{fold_bytes, signed};
use triergon_isobmff::{
    BuildStatus, CursorStatus, Limits, Next, Sample,
    compat::mov::{
        packets::{DemuxProfile, DemuxTrack, PacketCursor, PacketLimits},
        seek::{SeekJob, SeekLimits},
    },
    interleave::SourceTrack,
    open::{Movie, MovieOpen, MovieTrack, OpenLimits, OpenStep},
    payload::SampleReader,
    reference_trace::mov::sample_entry::{RawVisualEntriesJob, SampleEntryHeaderProfile},
    seek::{
        CompositionTarget, SourceCompositionLookup, SourceSeekLimits, SourceSeekPlanner,
        SourceSeekRequest,
    },
    table::TimingRunPolicy,
};

use super::protocol::{Operation, Work};

const MAX_LOOKUPS: usize = 64;
const MAX_SAMPLES: u64 = 1_000_000;
const PROFILE: DemuxProfile = DemuxProfile::NoParseNoFillin;

enum Query {
    Track {
        ticks: i64,
        eof: bool,
        recovery: bool,
    },
    Plan(SourceSeekRequest),
    Lookup(Vec<i64>),
}

pub(super) fn operation<'a>(
    prefix: &str,
    bytes: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    let args = arguments
        .iter()
        .map(|argument| argument.to_str().ok_or("non-UTF-8 seek argument"))
        .collect::<Result<Vec<_>, _>>()?;
    let (name, query, track, window, quantum) = match (prefix, args.as_slice()) {
        ("mov-track-seek-", [track, ticks, eof, window, quantum, policy]) => (
            "mov-track-seek",
            Query::Track {
                ticks: ticks.parse().map_err(|_| "invalid seek time")?,
                eof: match *eof {
                    "0" => false,
                    "1" => true,
                    _ => return Err("invalid EOF flag".into()),
                },
                recovery: match *policy {
                    "strict" => false,
                    "recovery" => true,
                    _ => return Err("invalid table policy".into()),
                },
            },
            track,
            window,
            quantum,
        ),
        ("source-seek-", ["plan", track, target, preroll, window, quantum]) => (
            "source-seek",
            Query::Plan(SourceSeekRequest {
                target_ordinal: target.parse().map_err(|_| "invalid sample ordinal")?,
                preroll_samples: preroll.parse().map_err(|_| "invalid preroll")?,
            }),
            track,
            window,
            quantum,
        ),
        ("source-seek-", ["lookup", track, window, quantum, ticks @ ..])
            if (1..=MAX_LOOKUPS).contains(&ticks.len()) =>
        {
            let ticks = ticks
                .iter()
                .map(|ticks| ticks.parse().map_err(|_| "invalid lookup time"))
                .collect::<Result<Vec<_>, _>>()?;
            ("source-seek", Query::Lookup(ticks), track, window, quantum)
        }
        _ => return Err("invalid seek arguments".into()),
    };
    let positive = |word: &str| {
        word.parse::<i32>()
            .ok()
            .filter(|&value| value > 0)
            .ok_or("invalid track, window or work quantum")
    };
    let source = Source {
        bytes,
        window: positive(window)? as usize,
        quantum: positive(quantum)? as u64,
        cancel: AtomicBool::new(false),
    };
    let track = positive(track)? as u32;
    Ok(Operation {
        name,
        run: Box::new(move |work, trace| {
            // A range is published only if its complete operation succeeds.
            let mut rows = Vec::new();
            let mut out = Output {
                work,
                trace: trace.is_some().then_some(&mut rows as &mut dyn Write),
            };
            let result = source.run(track, &query, &mut out);
            match result {
                Ok(()) => trace.map_or(Ok(()), |trace| {
                    trace.write_all(&rows).map_err(|error| error.to_string())
                }),
                Err(
                    error @ (Error::Invalid(_)
                    | Error::Unsupported(_)
                    | Error::Truncated
                    | Error::Overflow),
                ) => {
                    if trace.is_some() {
                        eprintln!("{error:?}");
                    }
                    *work = Work::default();
                    let mut out = Output { work, trace };
                    out.row(8, &[], &[]).map_err(|error| format!("{error:?}"))
                }
                Err(error) => Err(format!("{error:?}")),
            }
        }),
    })
}

struct Output<'a, 'b> {
    work: &'a mut Work,
    trace: Option<&'b mut dyn Write>,
}

impl Output<'_, '_> {
    fn row(&mut self, kind: u64, words: &[u64], payload: &[u8]) -> triergon_core::Result<()> {
        self.work.record(kind, words, payload);
        if let Some(out) = self.trace.as_deref_mut() {
            let mut render = || -> std::io::Result<()> {
                write!(out, "{kind}")?;
                for word in words {
                    write!(out, " {word}")?;
                }
                if !payload.is_empty() {
                    let mut digest = 0;
                    fold_bytes(&mut digest, payload);
                    write!(out, " bytes={} digest={digest}", payload.len())?;
                }
                writeln!(out)
            };
            render().map_err(|_| Error::Transport)?;
        }
        Ok(())
    }
}

struct Source<'a> {
    bytes: &'a [u8],
    window: usize,
    quantum: u64,
    cancel: AtomicBool,
}

impl Source<'_> {
    fn work(&self) -> WorkBudget {
        WorkBudget::new(self.quantum)
    }

    fn supply(&self, input: &mut InputCursor, request: InputRequest) -> triergon_core::Result<()> {
        let offset = usize::try_from(request.offset).map_err(|_| Error::Overflow)?;
        let Some(rest) = self.bytes.get(offset..).filter(|rest| !rest.is_empty()) else {
            return input.finish(request.source, request.token);
        };
        let bytes = &rest[..rest.len().min(request.maximum).min(self.window)];
        if bytes.len() <= InputCursor::INLINE_CAPACITY {
            input.provide_inline(request.source, request.token, request.offset, bytes)
        } else {
            let bytes = Bytes::copy_from_slice(input.budget(), bytes)?;
            input.provide_owned(InputWindow::new(
                request.source,
                request.token,
                request.offset,
                bytes,
            )?)
        }
    }

    fn open(&self, input: &mut InputCursor, recovery: bool) -> triergon_core::Result<Movie> {
        let mut job = if recovery {
            let mut job = MovieOpen::new_recovery(input, OpenLimits::default())?;
            job.set_timing_policy(TimingRunPolicy::BoundedPrefix);
            job
        } else {
            MovieOpen::new(input, OpenLimits::default())?
        };
        loop {
            match job.advance(input, &mut self.work(), &self.cancel)? {
                OpenStep::Ready => return job.finish(input),
                OpenStep::NeedInput(request) => self.supply(input, request)?,
                OpenStep::Root(_) | OpenStep::Yield => (),
            }
        }
    }

    fn payload(
        &self,
        input: &mut InputCursor,
        movie: &Movie,
        sample: Sample,
    ) -> triergon_core::Result<Bytes> {
        let extent = movie
            .media()
            .iter()
            .find(|extent| extent.start <= sample.range.start && sample.range.end <= extent.end)
            .ok_or(Error::Invalid("sample outside media extents"))?;
        let mut reader = SampleReader::new(input, sample, extent.clone(), 16 << 20)?;
        loop {
            match reader.poll(input, &mut self.work(), &self.cancel)? {
                Step::Ready(packet) => return Ok(packet.payload),
                Step::NeedInput(request) => self.supply(input, request)?,
                Step::Yield => (),
                Step::End(_) => return Err(Error::Truncated),
            }
        }
    }

    fn run(&self, id: u32, query: &Query, out: &mut Output<'_, '_>) -> triergon_core::Result<()> {
        let mut input = InputCursor::new(
            &Budget::new(64 << 20)?,
            SourceId(1),
            SourceCapabilities {
                length: Some(self.bytes.len() as u64),
                random_access: true,
            },
            self.window,
        )?;
        let movie = self.open(
            &mut input,
            matches!(query, Query::Track { recovery: true, .. }),
        )?;
        let track = movie
            .tracks()
            .iter()
            .find(|track| track.header().track_id == id)
            .ok_or(Error::Invalid("seek track absent"))?;
        match query {
            Query::Track { ticks, eof, .. } => {
                for track in movie.tracks() {
                    let mut check = RawVisualEntriesJob::new(
                        track.header(),
                        track.table(),
                        Limits::default(),
                        SampleEntryHeaderProfile,
                    );
                    while check.poll(&mut self.work(), &self.cancel)? == BuildStatus::Yield {}
                }
                if *eof {
                    self.drain(&mut input, &movie)?;
                }
                self.track_seek(&movie, track, *ticks, out)
            }
            Query::Plan(request) => self.plan(&mut input, &movie, track, *request, out),
            Query::Lookup(ticks) => self.lookup(track, ticks, out),
        }
    }

    fn drain(&self, input: &mut InputCursor, movie: &Movie) -> triergon_core::Result<()> {
        let tracks = movie
            .tracks()
            .iter()
            .map(|track| demux_track(movie, track))
            .collect::<triergon_core::Result<Vec<_>>>()?;
        let mut cursor = PacketCursor::new_complete_source(
            movie.pool(),
            &tracks,
            movie.media(),
            PacketLimits::default(),
            PROFILE,
        )?;
        loop {
            match cursor.poll(&mut self.work(), &self.cancel)? {
                CursorStatus::Yield => (),
                CursorStatus::End => return Ok(()),
                CursorStatus::Ready => {
                    let occurrence = cursor.take_occurrence()?;
                    self.payload(input, movie, occurrence.source)?;
                }
            }
        }
    }

    fn track_seek(
        &self,
        movie: &Movie,
        track: &MovieTrack,
        ticks: i64,
        out: &mut Output<'_, '_>,
    ) -> triergon_core::Result<()> {
        let mut seek = SeekJob::new_complete_source(
            movie.pool(),
            demux_track(movie, track)?,
            movie.media(),
            ticks,
            SeekLimits::default(),
            PROFILE,
        )?;
        while seek.poll(&mut self.work(), &self.cancel)? == BuildStatus::Yield {}
        let plan = seek.finish()?;
        let entry = &plan.landing().selected;
        out.row(
            3,
            &[
                1,
                signed(entry.timestamp),
                signed(entry.position),
                u64::from(entry.size),
                signed(i64::from(entry.flags)),
                signed(i64::from(entry.min_distance)),
                entry.ordinal,
            ],
            &[],
        )
    }

    fn plan(
        &self,
        input: &mut InputCursor,
        movie: &Movie,
        track: &MovieTrack,
        request: SourceSeekRequest,
        out: &mut Output<'_, '_>,
    ) -> triergon_core::Result<()> {
        let mut planner = SourceSeekPlanner::new(
            regular_track(track)?,
            request,
            movie.media(),
            SourceSeekLimits::default(),
        )?;
        while planner.poll(&mut self.work(), &self.cancel)? == BuildStatus::Yield {}
        let plan = planner.finish()?;
        let mut cursor = plan.cursor();
        loop {
            match cursor.poll(&mut self.work(), &self.cancel)? {
                Next::Yield => (),
                Next::End => return out.row(7, &[], &[]),
                Next::Item(sample) => {
                    let position = sample.range.start;
                    let ticks = sample
                        .dts_ticks
                        .map(i64::try_from)
                        .transpose()
                        .map_err(|_| Error::Overflow)?;
                    let payload = self.payload(input, movie, sample)?;
                    let words = [
                        position,
                        payload.len() as u64,
                        u64::from(ticks.is_some()),
                        ticks.map_or(0, signed),
                    ];
                    out.row(1, &words[..3 + usize::from(ticks.is_some())], &payload)?;
                }
            }
        }
    }

    fn lookup(
        &self,
        track: &MovieTrack,
        queries: &[i64],
        out: &mut Output<'_, '_>,
    ) -> triergon_core::Result<()> {
        let source = regular_track(track)?;
        let base = track.header().time_base()?;
        for &query in queries {
            let mut lookup = SourceCompositionLookup::new(
                source,
                Time::from_ticks(i128::from(query), base)?,
                base,
                MAX_SAMPLES,
            )?;
            while lookup.poll(&mut self.work(), &self.cancel)? == BuildStatus::Yield {}
            let query = signed(query);
            match lookup.finish()? {
                CompositionTarget::BeforeFirst => out.row(3, &[query, 0], &[])?,
                CompositionTarget::Target(sample) => out.row(
                    3,
                    &[
                        query,
                        1,
                        sample.ordinal,
                        sample.range.start,
                        clock(sample.cts_ticks)?,
                    ],
                    &[],
                )?,
                CompositionTarget::Ambiguous {
                    first_ordinal,
                    second_ordinal,
                    source_time,
                } => {
                    let ticks = source_time.to_ticks(base, triergon_core::Rounding::Floor)?;
                    out.row(
                        3,
                        &[query, 2, first_ordinal, second_ordinal, clock(Some(ticks))?],
                        &[],
                    )?;
                }
                CompositionTarget::UnknownSourceTime { ordinal } => {
                    out.row(3, &[query, 3, ordinal], &[])?;
                }
            }
        }
        Ok(())
    }
}

fn clock(value: Option<i128>) -> triergon_core::Result<u64> {
    let ticks = value.ok_or(Error::Invalid("missing selected composition time"))?;
    Ok(signed(i64::try_from(ticks).map_err(|_| Error::Overflow)?))
}

fn demux_track<'a>(
    movie: &'a Movie,
    track: &'a MovieTrack,
) -> triergon_core::Result<DemuxTrack<'a>> {
    let movie_base = movie.header().time_base()?;
    Ok(DemuxTrack {
        track_id: track.header().track_id,
        table: track.table(),
        fragments: track.fragments(),
        media_base: track.header().time_base()?,
        edits: track.edits().map(|edits| (edits, movie_base)),
    })
}

fn regular_track(track: &MovieTrack) -> triergon_core::Result<SourceTrack<'_>> {
    if !track.fragments().is_empty() {
        return Err(Error::Unsupported(
            "source seek comparison requires regular samples",
        ));
    }
    SourceTrack::new(track.header().track_id, Some(track.table()), &[])
}
