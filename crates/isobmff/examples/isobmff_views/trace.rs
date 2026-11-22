use std::{ffi::OsString, io::Write, ops::Range, sync::atomic::AtomicBool};

use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, Result, Rounding, SourceCapabilities, SourceId,
    TimeBase, WorkBudget,
};
use triergon_harness_protocol::digest::signed;
use triergon_isobmff::{
    BuildStatus, CursorStatus, Limits, Next,
    boxes::{BoxHeader, BoxView, HeaderRead},
    compat::mov::{
        packets::{DemuxProfile, DemuxTrack, PacketCursor, PacketLimits},
        recovery::{
            DescriptionContext, DescriptionRead, DescriptionRecoveryBuilder,
            DescriptionRecoveryProfile, DescriptionRecoveryTable, RecoveryTrackHeader,
            RunRecoveryProfile,
        },
    },
    index::{SegmentIndex, TrackFragmentRandomAccess},
    movie::EntryLayout,
    open::{MovieOpen, OpenLimits, OpenStep},
    reference_trace::mov::{
        diagnostics::{DiagnosticKind, PrivateDiagnosticJob},
        fragment_clock::{FragmentClockProfile, check_signed_decode_runs},
        operation::{InitialOperationJob, RejectedPacketSeekProfile},
        receiver::{ReceiverClockProfile, ReceiverPacketTimes},
        sample_entry::{
            PrivateCatalogueProfile, PrivateConfigurationJob, SampleEntryHeaderJob,
            SampleEntryHeaderProfile,
        },
        transport::{
            RootWalkAccess, RootWalkTransportBuilder, RootWalkTransportModel,
            RootWalkTransportProfile,
        },
    },
    table::SampleTableBuilder,
};

use super::{
    protocol::{Operation, Work},
    segment_index::parsed_box,
};

const EOF: i64 = -541_478_725;
const INVALID_DATA: i64 = -1_094_995_529;
const MAX_PACKET: usize = 16 << 20;

#[derive(Clone, Copy)]
enum Model {
    Receiver,
    Diagnostics,
    Configuration,
    Header,
    Initial,
    Transport(Option<u64>),
}

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> std::result::Result<Operation<'a>, String> {
    let args = arguments
        .iter()
        .map(|arg| arg.to_str().ok_or("non-UTF-8 MOV trace argument"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if prefix == "index-landings-" {
        return landing(input, &args);
    }
    let [model, window, quantum, extra @ ..] = args.as_slice() else {
        return Err("MOV trace needs model, read maximum and work quantum".into());
    };
    let model = match (*model, extra) {
        ("receiver", []) => Model::Receiver,
        ("diagnostics", []) => Model::Diagnostics,
        ("configuration", []) => Model::Configuration,
        ("header", []) => Model::Header,
        ("initial", []) => Model::Initial,
        ("transport", ["entry"]) => Model::Transport(None),
        ("transport", [anchor]) => {
            Model::Transport(Some(anchor.parse().map_err(|_| "invalid rewind offset")?))
        }
        _ => return Err("unknown MOV trace model".into()),
    };
    let window = window
        .parse::<usize>()
        .ok()
        .filter(|v| (1..=65536).contains(v))
        .ok_or("invalid read maximum")?;
    let quantum = quantum
        .parse::<u64>()
        .ok()
        .filter(|v| *v != 0)
        .ok_or("invalid work quantum")?;
    Ok(Operation {
        name: "mov-reference-trace",
        run: Box::new(move |work, trace| {
            let mut out = Output { work, trace };
            let result = match model {
                Model::Receiver => receiver(input, window, quantum, &mut out),
                _ => RawMovie::new(input, quantum)
                    .and_then(|movie| movie.run(model, window, quantum, &mut out)),
            };
            complete(result, &mut out)
        }),
    })
}

fn complete(result: Result<()>, out: &mut Output<'_, '_>) -> std::result::Result<(), String> {
    match result {
        Ok(()) => Ok(()),
        Err(
            error @ (Error::Invalid(_)
            | Error::Unsupported(_)
            | Error::Truncated
            | Error::Overflow
            | Error::Limit { .. }),
        ) => {
            if out.trace.is_some() {
                eprintln!("{error:?}");
            }
            out.row(8, &[], &[]).map_err(|e| e.to_string())
        }
        Err(error) => Err(error.to_string()),
    }
}

struct Output<'a, 'b> {
    work: &'a mut Work,
    trace: Option<&'b mut dyn Write>,
}

impl Output<'_, '_> {
    fn row(&mut self, kind: u64, words: &[u64], payload: &[u8]) -> Result<()> {
        self.work.record(kind, words, payload);
        if let Some(out) = self.trace.as_deref_mut() {
            let mut render = || -> std::io::Result<()> {
                write!(out, "{kind}")?;
                for word in words {
                    write!(out, " {word}")?;
                }
                if !payload.is_empty() {
                    write!(out, " hex=")?;
                    for byte in payload {
                        write!(out, "{byte:02x}")?;
                    }
                }
                writeln!(out)
            };
            render().map_err(|_| Error::Transport)?;
        }
        Ok(())
    }

    fn result(&mut self, code: i64) -> Result<()> {
        self.row(
            if code >= 0 || code == EOF { 7 } else { 8 },
            &[signed(code)],
            &[],
        )
    }

    fn bytes(&mut self, payload: &[u8]) -> Result<()> {
        self.row(2, &[payload.len() as u64], payload)
    }
}

fn landing<'a>(input: &'a [u8], args: &[&str]) -> std::result::Result<Operation<'a>, String> {
    let [
        kind @ ("sidx" | "tfra"),
        start,
        len,
        track,
        quantum,
        ranges @ ..,
    ] = args
    else {
        return Err("index landing arguments incomplete".into());
    };
    if ranges.is_empty() || ranges.len() % 3 != 0 {
        return Err("index landing needs fragment ranges and seek times".into());
    }
    let start: usize = start.parse().map_err(|_| "invalid index offset")?;
    let len: usize = len.parse().map_err(|_| "invalid index length")?;
    let end = start.checked_add(len).ok_or("index range overflow")?;
    let track: u32 = track.parse().map_err(|_| "invalid track")?;
    let quantum: u64 = quantum
        .parse::<u64>()
        .ok()
        .filter(|q| *q != 0)
        .ok_or("invalid quantum")?;
    let is_segment = *kind == "sidx";
    Ok(Operation {
        name: "isobmff-index-landings",
        run: Box::new(move |work, trace| {
            let mut out = Output { work, trace };
            let pool = Budget::new(16 << 20).map_err(|e| e.to_string())?;
            let result = parsed_box(input, start..end, &pool).and_then(|raw| {
                if is_segment {
                    let index = SegmentIndex::parse(raw, Limits::default())?;
                    let base = TimeBase::new(1, index.timescale().into())?;
                    let mut cursor = index.cursor();
                    loop {
                        match cursor.poll(&mut WorkBudget::new(quantum))? {
                            Next::Item(row) => {
                                let ticks = row
                                    .earliest_presentation
                                    .to_i64_ticks(base, Rounding::TowardZero)?;
                                out.row(
                                    3,
                                    &[
                                        index.reference_id().into(),
                                        row.range.start,
                                        row.range.end,
                                        signed(ticks),
                                        u64::from(!row.references_index),
                                    ],
                                    &[],
                                )?;
                            }
                            Next::Yield => (),
                            Next::End => break,
                        }
                    }
                } else {
                    let index = TrackFragmentRandomAccess::parse(raw, Limits::default())?;
                    let mut cursor = index.cursor();
                    loop {
                        match cursor.poll(&mut WorkBudget::new(quantum))? {
                            Next::Item(row) => out.row(
                                3,
                                &[index.track_id().into(), row.moof_offset, row.time_ticks],
                                &[],
                            )?,
                            Next::Yield => (),
                            Next::End => break,
                        }
                    }
                }
                if track == 0 {
                    return Err(Error::Invalid("zero landing track"));
                }
                out.row(7, &[], &[])
            });
            complete(result, &mut out)
        }),
    })
}

fn child(parent: &BoxView, kind: &[u8; 4], quantum: u64) -> Result<BoxView> {
    let mut children = parent.children(0, Limits::default())?;
    loop {
        match children.poll(&mut WorkBudget::new(quantum))? {
            Next::Item(raw) if raw.header().kind == *kind => return Ok(raw),
            Next::Item(_) | Next::Yield => (),
            Next::End => return Err(Error::Invalid("MOV model child absent")),
        }
    }
}

fn input_cursor(pool: &Budget, size: u64, window: usize) -> Result<InputCursor> {
    InputCursor::new(
        pool,
        SourceId(1),
        SourceCapabilities {
            length: Some(size),
            random_access: true,
        },
        window,
    )
}

struct RawMovie {
    pool: Budget,
    roots: Vec<BoxHeader>,
    ftyp: BoxView,
    movie: BoxView,
    trak: BoxView,
    mdia: BoxView,
    table: BoxView,
    media: Vec<Range<u64>>,
    size: u64,
}

impl RawMovie {
    fn new(input: &[u8], quantum: u64) -> Result<Self> {
        let pool = Budget::new(32 << 20)?;
        let size = input.len() as u64;
        let mut roots = Vec::new();
        let mut media = Vec::new();
        let mut ftyp = None;
        let mut movie = None;
        let mut at = 0;
        while at < input.len() {
            let HeaderRead::Ready(header) =
                BoxHeader::parse(&input[at..], at as u64, Some(size), true)?
            else {
                return Err(Error::Truncated);
            };
            let end = usize::try_from(header.end()?.ok_or(Error::Truncated)?)
                .map_err(|_| Error::Overflow)?;
            match &header.kind {
                b"ftyp" => ftyp = Some(parsed_box(input, at..end, &pool)?),
                b"moov" => movie = Some(parsed_box(input, at..end, &pool)?),
                b"mdat" => media.push(header.payload_range()?),
                _ => (),
            }
            roots.push(header);
            at = end;
        }
        let ftyp = ftyp.ok_or(Error::Invalid("MOV model ftyp absent"))?;
        let movie = movie.ok_or(Error::Invalid("MOV model moov absent"))?;
        let trak = child(&movie, b"trak", quantum)?;
        let mdia = child(&trak, b"mdia", quantum)?;
        let table = child(&child(&mdia, b"minf", quantum)?, b"stbl", quantum)?;
        Ok(Self {
            pool,
            roots,
            ftyp,
            movie,
            trak,
            mdia,
            table,
            media,
            size,
        })
    }

    fn run(
        &self,
        model: Model,
        window: usize,
        quantum: u64,
        out: &mut Output<'_, '_>,
    ) -> Result<()> {
        let cancel = AtomicBool::new(false);
        let limits = Limits::default();
        if matches!(model, Model::Initial | Model::Transport(_)) {
            let mut job = DescriptionRecoveryBuilder::new(
                self.table.clone(),
                self.pool.clone(),
                limits,
                DescriptionRecoveryProfile,
            )?;
            while job.poll(&mut WorkBudget::new(quantum), &cancel)? == BuildStatus::Yield {}
            let table = job.finish()?;
            let transport = self.transport(&table, window, quantum, &cancel, model)?;
            return if matches!(model, Model::Transport(_)) {
                transport_accesses(&transport, window, out)
            } else {
                self.initial(&table, &transport, window, quantum, &cancel, out)
            };
        }
        let mut job = SampleTableBuilder::new(self.table.clone(), self.pool.clone(), limits)?;
        while job.poll(&mut WorkBudget::new(quantum))? == BuildStatus::Yield {}
        let table = job.finish()?;
        let [description] = table.descriptions() else {
            return Err(Error::Unsupported("MOV model requires one description"));
        };
        match model {
            Model::Diagnostics => {
                let mut job = PrivateDiagnosticJob::new(&table, &self.pool, limits)?;
                while job.poll(&mut WorkBudget::new(quantum), &cancel)? == BuildStatus::Yield {}
                let result = job.try_finish(&cancel)?;
                for entry in result.events.as_slice() {
                    let level = match entry.kind {
                        DiagnosticKind::IgnoredAdditionalConfiguration => 24,
                        DiagnosticKind::InvalidDtsRate(_)
                        | DiagnosticKind::EmptyVpConfiguration => 16,
                        _ => 24,
                    };
                    let text = entry.kind.to_string();
                    out.row(3, &[level, text.len() as u64], text.as_bytes())?;
                }
                out.result(result.demux_open_result.map_or(0, i64::from))
            }
            Model::Header => {
                let mut header = SampleEntryHeaderJob::new(
                    description,
                    EntryLayout::Visual,
                    limits,
                    SampleEntryHeaderProfile,
                )?;
                while header.poll(&mut WorkBudget::new(quantum), &cancel)? == BuildStatus::Yield {}
                let result = header.try_finish(&cancel)?;
                let code = result.demux_open_result().map_or(0, i64::from);
                if code == 0 {
                    let source = result.installed_configuration_source.as_ref();
                    if source.is_some_and(|raw| raw.header().kind == *b"dOps") {
                        self.configuration(description, quantum, &cancel, out)?;
                        return Ok(());
                    }
                    out.bytes(source.map_or(&[], BoxView::payload))?;
                }
                out.result(code)
            }
            Model::Configuration => self.configuration(description, quantum, &cancel, out),
            _ => unreachable!("model already dispatched"),
        }
    }

    fn configuration(
        &self,
        description: &triergon_isobmff::movie::SampleDescription,
        quantum: u64,
        cancel: &AtomicBool,
        out: &mut Output<'_, '_>,
    ) -> Result<()> {
        let mut job = PrivateConfigurationJob::new(
            description,
            &self.pool,
            MAX_PACKET,
            Limits::default(),
            PrivateCatalogueProfile,
        )?;
        while job.poll(&mut WorkBudget::new(quantum), cancel)? == BuildStatus::Yield {}
        let result = job.try_finish(cancel)?;
        let code = result.source().demux_open_result().map_or(0, i64::from);
        if code == 0 {
            out.bytes(result.demux_bytes().map_or(&[], AsRef::as_ref))?;
        }
        out.result(code)
    }

    fn transport(
        &self,
        table: &DescriptionRecoveryTable,
        window: usize,
        quantum: u64,
        cancel: &AtomicBool,
        model: Model,
    ) -> Result<RootWalkTransportModel> {
        let mut builder = RootWalkTransportBuilder::new(
            self.ftyp.clone(),
            self.movie.clone(),
            &self.roots,
            table,
            &self.pool,
            DescriptionContext {
                physical_length: self.size,
                custom_read_maximum: window,
            },
            RootWalkTransportProfile,
        )?;
        if let Model::Transport(rewind) = model {
            builder = builder.with_fixed_rewind(rewind)?;
        }
        while builder.poll(&mut WorkBudget::new(quantum), cancel)? == BuildStatus::Yield {}
        builder.finish()
    }

    fn initial(
        &self,
        table: &DescriptionRecoveryTable,
        transport: &RootWalkTransportModel,
        window: usize,
        quantum: u64,
        cancel: &AtomicBool,
        out: &mut Output<'_, '_>,
    ) -> Result<()> {
        let track = RecoveryTrackHeader::parse(
            &child(&self.trak, b"tkhd", quantum)?,
            &child(&self.mdia, b"mdhd", quantum)?,
            &child(&self.mdia, b"hdlr", quantum)?,
            RunRecoveryProfile,
        )?;
        let mut input = input_cursor(&self.pool, self.size, window)?;
        let limits = PacketLimits::default();
        let mut job = InitialOperationJob::new(
            table,
            &track,
            &self.media,
            transport,
            &input,
            limits,
            MAX_PACKET,
            RejectedPacketSeekProfile,
        )?;
        while job.poll(&mut input, &mut WorkBudget::new(quantum), cancel)? == BuildStatus::Yield {}
        let first = job.try_finish(&mut input, cancel)?;
        if let Some(rejected) = first.rejected_seek() {
            return out.result(rejected.demux_read_result().into());
        }
        // The rejection job leaves available packets and forward-discard EOF to
        // the root-walk packet cursor. It does not synthesize a return code for them.
        let mut cursor = table.root_walk_packet_cursor(
            &self.pool,
            &track,
            &self.media,
            limits,
            transport,
            RootWalkTransportProfile,
        )?;
        loop {
            match cursor.poll(&mut WorkBudget::new(quantum), cancel)? {
                CursorStatus::Yield => (),
                CursorStatus::End => return out.result(EOF),
                CursorStatus::Ready => {
                    let event = cursor.take_event()?;
                    match event.read {
                        DescriptionRead::Available => return out.result(0),
                        DescriptionRead::ModeledInitialEof => return out.result(EOF),
                        DescriptionRead::NoPacketForAbsentCatalogue => (),
                        _ => return Err(Error::Unsupported("unmodeled first packet read")),
                    }
                }
            }
        }
    }
}

fn transport_accesses(
    model: &RootWalkTransportModel,
    window: usize,
    out: &mut Output<'_, '_>,
) -> Result<()> {
    const BUFFER: u64 = 32768;
    let quantum = (window as u64).min(BUFFER);
    let mut at = 0;
    for &access in model
        .open_accesses()
        .ok_or(Error::Invalid("transport geometry absent"))?
    {
        match access {
            RootWalkAccess::ReadUntil(end) => {
                while at < end {
                    let size = quantum.min(end - at);
                    out.row(1, &[at, BUFFER, size], &[])?;
                    at += size;
                }
            }
            RootWalkAccess::Seek(offset) => {
                out.row(3, &[0, offset, offset], &[])?;
                at = offset;
            }
        }
    }
    out.row(7, &[model.modeled_open_state().logical_position], &[])
}

fn receiver(input: &[u8], window: usize, quantum: u64, out: &mut Output<'_, '_>) -> Result<()> {
    let pool = Budget::new(32 << 20)?;
    let cancel = AtomicBool::new(false);
    let mut reader = input_cursor(&pool, input.len() as u64, window)?;
    let mut open = MovieOpen::new_recovery(&reader, OpenLimits::default())?;
    let movie = loop {
        match open.advance(&mut reader, &mut WorkBudget::new(quantum), &cancel)? {
            OpenStep::Ready => break open.finish(&reader)?,
            OpenStep::Root(_) | OpenStep::Yield => (),
            OpenStep::NeedInput(req) => {
                let offset = usize::try_from(req.offset).map_err(|_| Error::Overflow)?;
                if let Some(rest) = input.get(offset..).filter(|rest| !rest.is_empty()) {
                    let bytes =
                        Bytes::copy_from_slice(&pool, &rest[..rest.len().min(req.maximum)])?;
                    reader.provide_owned(InputWindow::new(
                        req.source, req.token, req.offset, bytes,
                    )?)?;
                } else {
                    reader.finish(req.source, req.token)?;
                }
            }
        }
    };
    for track in movie.tracks() {
        match check_signed_decode_runs(track.fragments(), FragmentClockProfile) {
            Ok(()) => (),
            Err(Error::Invalid(_)) => return out.result(INVALID_DATA),
            Err(error) => return Err(error),
        }
    }
    let movie_base = movie.header().time_base()?;
    let tracks = movie
        .tracks()
        .iter()
        .map(|track| {
            Ok(DemuxTrack {
                track_id: track.header().track_id,
                table: track.table(),
                fragments: track.fragments(),
                media_base: track.header().time_base()?,
                edits: track.edits().map(|edits| (edits, movie_base)),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut cursor = PacketCursor::new_complete_source(
        &pool,
        &tracks,
        movie.media(),
        PacketLimits::default(),
        DemuxProfile::NoParseNoFillin,
    )?;
    loop {
        match cursor.poll(&mut WorkBudget::new(quantum), &cancel)? {
            CursorStatus::Yield => (),
            CursorStatus::End => return out.result(EOF),
            CursorStatus::Ready => {
                let row = cursor.take_occurrence()?;
                let times = ReceiverPacketTimes::from_demux(&row.demux, ReceiverClockProfile)?;
                out.row(
                    1,
                    &[
                        row.track_id.into(),
                        signed(times.pts.public_raw_ticks()),
                        signed(times.dts.public_raw_ticks()),
                    ],
                    &[],
                )?;
            }
        }
    }
}
