use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::sync::atomic::AtomicBool;

use super::reader::Stop;
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputRequest, InputWindow, SourceCapabilities, SourceId,
    Step, Time, WorkBudget,
};
use triergon_harness_protocol::digest::{fold, fold_bytes, signed};
use triergon_harness_runner::{Operation, Outcome};
use triergon_mpegts::compat::input::{InputBootstrap, InputContext, InputStep};
use triergon_mpegts::compat::pes::{
    CompactDemuxPacket, DemuxClock, DemuxObserver, DemuxPacket, DemuxScope, DemuxSpanViews,
    DvbSubtitlesDescriptors, PrivateAudioCarriage, PrivateAudioDescriptors, RegisteredCarriage,
    SourceContext,
};
use triergon_mpegts::pes::{Pes, PesAssembler, PesHeader};
use triergon_mpegts::psi::{DescriptorNamespace, Descriptors, DvbNamespaceEvidence, ProgramMap};
use triergon_mpegts::stream::{CompactTraceEvent, FeedContinuation, TraceEvent, TransportStream};
use triergon_mpegts::transport::{Continuity, ContinuityOutcome, PacketFormat, TransportPacket};
use triergon_mpegts::{AssemblyStep, ParseLimits, SourceEpochs, SourceRange};

mod controls;

const MAX_INPUT: usize = 64 * 1024 * 1024;
const WINDOW: usize = 64 * 1024;
const GRANT: u64 = 4096;
static CANCEL: AtomicBool = AtomicBool::new(false);
const NO_TIME: i64 = i64::MIN;
const STREAM_ID_SIDE_DATA: u64 = 19;

#[derive(Clone, Copy)]
enum PollForm {
    Poll,
    Feed,
    FeedCarriers,
    Compact,
    CompactObserved,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ObserverForm {
    Unregistered,
    InputBound,
    Compact,
}

#[derive(Clone, Copy)]
enum Mode {
    Source(PollForm),
    Assembler(usize),
    Observer(ObserverForm),
}

struct Entity {
    bytes: Bytes,
    base: u64,
    discontinuity: bool,
}

struct Request {
    mode: Mode,
    budget: Budget,
    entities: Vec<Entity>,
    format: PacketFormat,
    program: u16,
    namespace: DescriptorNamespace,
    scope_refusal: bool,
    report_refusal: bool,
}

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    // A registered refusal case asks the worker to name its typed unsupported detail.
    let (report_refusal, arguments) = match arguments.split_last() {
        Some((last, rest)) if last == "--report-refusal" => (true, rest),
        _ => (false, arguments),
    };
    let [size, program, form, tail @ ..] = arguments else {
        return Err(format!(
            "{prefix} requires SIZE PROGRAM FORM [NAMESPACE] [ENTITY...]"
        ));
    };
    let format = match size.to_str() {
        Some("188") => PacketFormat::Ts188,
        Some("192") => PacketFormat::M2ts192,
        _ => return Err("expected packet size 188 or 192".into()),
    };
    let program = program
        .to_str()
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|&value| value != 0)
        .ok_or("expected a nonzero program number")?;
    let (name, mode) = match (prefix, form.to_str().unwrap_or_default()) {
        ("ts-source-", form) => (
            "ts-source-poll",
            Mode::Source(match form {
                "poll" => PollForm::Poll,
                "feed" => PollForm::Feed,
                "feed-carriers" => PollForm::FeedCarriers,
                "compact" => PollForm::Compact,
                "compact-observed" => PollForm::CompactObserved,
                _ => return Err("unknown source poll form".into()),
            }),
        ),
        ("ts-pes-assembler-", form) => (
            "ts-pes-assembler",
            Mode::Assembler(match form {
                "1" => 1,
                "7" => 7,
                "184" => 184,
                "whole" => usize::MAX,
                _ => return Err("unknown PES payload split".into()),
            }),
        ),
        ("ts-observer-", form) => (
            "ts-observer-forms",
            Mode::Observer(match form {
                "unregistered" => ObserverForm::Unregistered,
                "input-bound" => ObserverForm::InputBound,
                "compact" => ObserverForm::Compact,
                _ => return Err("unknown demux observer form".into()),
            }),
        ),
        _ => return Err(format!("unknown source operation {prefix}")),
    };
    let tail = match tail {
        [option, rest @ ..]
            if option == "--source-clocks" && !matches!(mode, Mode::Observer(_)) =>
        {
            rest
        }
        _ => tail,
    };
    let (scope_refusal, tail) = match tail {
        [option, rest @ ..]
            if option == "--scope-refusal"
                && matches!(
                    mode,
                    Mode::Observer(ObserverForm::Unregistered | ObserverForm::Compact)
                ) =>
        {
            (true, rest)
        }
        _ => (false, tail),
    };
    let (namespace, paths) = match tail {
        [option, rest @ ..] if option == "--dvb" => (
            DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile),
            rest,
        ),
        [option, rest @ ..] if option == "--atsc" => (DescriptorNamespace::Atsc, rest),
        _ => (DescriptorNamespace::Unspecified, tail),
    };
    if input.is_empty() || input.len() > MAX_INPUT {
        return Err("invalid primary entity extent".into());
    }
    let budget = Budget::new(MAX_INPUT + 64 * 1024 * 1024).map_err(err)?;
    let mut entities = vec![Entity {
        bytes: Bytes::copy_from_slice(&budget, input).map_err(err)?,
        base: 0,
        discontinuity: false,
    }];
    let mut paths = paths.iter();
    let mut remaining = MAX_INPUT - input.len();
    while let Some(path) = paths.next() {
        let discontinuity = path == "--discontinuity";
        let path = if discontinuity {
            paths.next().ok_or("discontinuity requires an entity")?
        } else {
            path
        };
        if path.to_string_lossy().starts_with('-') || entities.len() == 64 {
            return Err("invalid ordered entity".into());
        }
        let mut bytes = Vec::new();
        File::open(path)
            .and_then(|file| file.take(remaining as u64 + 1).read_to_end(&mut bytes))
            .map_err(|error| format!("{}: {error}", path.to_string_lossy()))?;
        if bytes.is_empty() || bytes.len() > remaining {
            return Err("invalid ordered entity extent".into());
        }
        entities.push(Entity {
            bytes: Bytes::copy_from_slice(&budget, &bytes).map_err(err)?,
            base: (MAX_INPUT - remaining) as u64,
            discontinuity,
        });
        remaining -= bytes.len();
    }
    let request = Request {
        mode,
        budget,
        entities,
        format,
        program,
        namespace,
        scope_refusal,
        report_refusal,
    };
    Ok(Operation {
        name,
        run: Box::new(move |work, trace| {
            let mut output = Output { work, trace };
            match execute(&request, &mut output) {
                Ok(()) => Ok(()),
                Err(Stop::Refused(error)) => output.refusal(error, request.report_refusal),
                Err(Stop::Failed(failure)) => Err(failure),
            }
        }),
    })
}

struct Output<'a, 'b> {
    work: &'a mut Outcome,
    trace: Option<&'b mut dyn Write>,
}

impl Output<'_, '_> {
    fn note(&mut self, line: std::fmt::Arguments<'_>) -> Result<(), String> {
        if let Some(trace) = &mut self.trace {
            writeln!(trace, "{line}").map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// The operation's last record after a library refusal: kind 8 with the error's
    /// text as payload. On request it names the refusal's detail for a registered
    /// refusal.
    fn refusal(&mut self, error: Error, report: bool) -> Result<(), String> {
        let text = error.to_string();
        self.work.record(8, &[], text.as_bytes());
        if report {
            self.work.unsupported = Some(super::reader::refusal_detail(&error));
        }
        self.note(format_args!("refusal {text}"))
    }

    fn packet(
        &mut self,
        fields: [u64; 8],
        stream_id: u8,
        payload: impl Iterator<Item = impl AsRef<[u8]>>,
    ) -> Result<(), String> {
        self.packet_from(fields, stream_id, |body| {
            for part in payload {
                body.push(part.as_ref());
            }
            Ok(())
        })
    }

    fn packet_from(
        &mut self,
        fields: [u64; 8],
        stream_id: u8,
        payload: impl FnOnce(&mut PacketBody<'_>) -> Result<(), String>,
    ) -> Result<(), String> {
        let digest = &mut self.work.digest;
        for value in fields {
            fold(digest, value);
        }
        let mut body = PacketBody {
            digest,
            trace: self.trace.is_some().then_some(0),
            bytes: 0,
        };
        payload(&mut body)?;
        let PacketBody {
            digest,
            trace,
            bytes,
        } = body;
        let payload_digest = trace.unwrap_or_default();
        if bytes != fields[7] {
            return Err("PES payload length disagrees with its spans".into());
        }
        for value in [1, STREAM_ID_SIDE_DATA, 1, u64::from(stream_id)] {
            fold(digest, value);
        }
        self.work.packets += 1;
        self.work.bytes += bytes;
        self.note(format_args!(
            "packet {} {} {} {} {} {} {} {bytes} {payload_digest} side {STREAM_ID_SIDE_DATA} 1 {stream_id}",
            fields[0], fields[1], fields[2] as i64, fields[3] as i64, fields[4] as i64,
            fields[5] as i64, fields[6],
        ))
    }
}

struct PacketBody<'a> {
    digest: &'a mut u64,
    trace: Option<u64>,
    bytes: u64,
}

impl PacketBody<'_> {
    fn push(&mut self, bytes: &[u8]) {
        fold_bytes(self.digest, bytes);
        if let Some(digest) = &mut self.trace {
            fold_bytes(digest, bytes);
        }
        self.bytes += bytes.len() as u64;
    }
}

fn err(error: Error) -> String {
    error.to_string()
}

fn source_index(source: SourceId) -> usize {
    source.0 as usize - 1
}

fn capabilities(length: usize) -> SourceCapabilities {
    SourceCapabilities {
        length: Some(length as u64),
        random_access: false,
    }
}

fn map_step<T, U>(step: Step<T>, map: impl FnOnce(T) -> U) -> Step<U> {
    match step {
        Step::Ready(value) => Step::Ready(map(value)),
        Step::NeedInput(request) => Step::NeedInput(request),
        Step::End(location) => Step::End(location),
        Step::Yield => Step::Yield,
        Step::Backpressure(never) => match never {},
    }
}

fn view_poll(
    form: PollForm,
    stream: &mut TransportStream,
    input: &mut InputCursor,
    request: &Request,
    output: &mut Output<'_, '_>,
) -> Result<Step<Option<TraceEvent>>, Stop> {
    let work = &mut WorkBudget::new(GRANT);
    let compact = match form {
        PollForm::Compact => stream.poll_compact(input, work, &CANCEL),
        PollForm::CompactObserved => stream
            .poll_compact_observed(input, work, &CANCEL)
            .map(|step| map_step(step, triergon_mpegts::stream::ObservedTrace::into_event)),
        _ => {
            let step = match form {
                PollForm::Poll => stream.poll(input, work, &CANCEL),
                PollForm::Feed => stream.poll_feed(input, work, &CANCEL, &mut |_, _, _, _| {
                    Ok(FeedContinuation::Emit)
                }),
                PollForm::FeedCarriers => {
                    stream.poll_feed_carriers(input, work, &CANCEL, &mut |_, _, _, _| {
                        Ok(FeedContinuation::Emit)
                    })
                }
                _ => unreachable!("literal poll forms"),
            };
            return Ok(map_step(step?, Some));
        }
    };
    let step = compact?;
    if let Step::Ready(CompactTraceEvent::NullRun(run)) = &step {
        controls::null_run(request, run)?;
        output.note(format_args!("null-run {run:?}"))?;
    }
    Ok(map_step(step, |event| match event {
        CompactTraceEvent::Event(event) => Some(event),
        CompactTraceEvent::NullRun(_) => None,
    }))
}

fn supply(input: &mut InputCursor, bytes: &Bytes, request: InputRequest) -> Result<(), Error> {
    let offset = usize::try_from(request.offset).map_err(|_| Error::Overflow)?;
    let end = bytes.len().min(offset.saturating_add(request.maximum));
    if offset < end {
        input.provide_owned(InputWindow::new(
            request.source,
            request.token,
            request.offset,
            bytes.slice(offset..end)?,
        )?)
    } else {
        input.finish(request.source, request.token)
    }
}

fn input_context(
    input: &mut InputCursor,
    bytes: &Bytes,
    format: PacketFormat,
) -> Result<InputContext, Stop> {
    let request = input.request();
    let window = InputWindow::new(request.source, request.token, 0, bytes.clone())?;
    input.provide(&window)?;
    let mut bootstrap = InputBootstrap::new(input, &window, format)?;
    bootstrap.observe_entity_eof(input, window.source, &window.token, bytes.len() as u64)?;
    loop {
        match bootstrap.poll(input, &mut WorkBudget::new(GRANT), &CANCEL)? {
            InputStep::Ready(context) => return Ok(context),
            InputStep::Yield => {}
            InputStep::NeedEntityEof => {
                return Err("input bootstrap did not accept the entity end".into());
            }
        }
    }
}

struct Assembly {
    pid: u16,
    continuity: Continuity,
    pes: PesAssembler,
}

impl Assembly {
    fn new(
        pid: u16,
        video: bool,
        input: &InputCursor,
        epochs: SourceEpochs,
    ) -> Result<Self, String> {
        Ok(Self {
            pid,
            continuity: Continuity::new(pid).map_err(err)?,
            pes: PesAssembler::new(
                pid,
                video,
                input.budget().clone(),
                ParseLimits::default(),
                epochs,
            )
            .map_err(err)?,
        })
    }

    fn push(
        &mut self,
        packet: TransportPacket,
        epochs: SourceEpochs,
        quantum: usize,
        request: &Request,
        output: &mut Output<'_, '_>,
    ) -> Result<(), String> {
        let continuity = self.continuity.observe(&packet).map_err(err)?;
        match continuity {
            ContinuityOutcome::First | ContinuityOutcome::Continuous => {}
            ContinuityOutcome::Duplicate | ContinuityOutcome::Null => return Ok(()),
            ContinuityOutcome::ExcessDuplicate | ContinuityOutcome::Damaged => {
                self.pes.discard(epochs);
                return Ok(());
            }
            ContinuityOutcome::Lost { .. } | ContinuityOutcome::SignalledDiscontinuity => {
                self.pes.discard(epochs);
            }
        }
        let Some(bytes) = packet.payload else {
            return Ok(());
        };
        let previous = self.pes.partial_header().to_vec();
        let prefix = incomplete_header(&previous, &bytes, packet.payload_unit_start);
        let entity = packet.source_id.ok_or("carrier without an entity")?;
        self.pes
            .push(
                bytes,
                packet.payload_unit_start,
                entity,
                packet.payload_source_offset,
            )
            .map_err(|refusal| err(refusal.error))?;
        self.drain(request, output, epochs, quantum, prefix.as_deref())
    }

    fn drain(
        &mut self,
        request: &Request,
        output: &mut Output<'_, '_>,
        epochs: SourceEpochs,
        quantum: usize,
        prefix: Option<&[u8]>,
    ) -> Result<(), String> {
        loop {
            if output.trace.is_some() {
                let partial = self.pes.partial_header();
                output.note(format_args!("partial-header {} {partial:02x?}", self.pid))?;
            }
            match self
                .pes
                .poll(&mut WorkBudget::new((quantum as u64).min(GRANT)), &CANCEL)
            {
                Ok(AssemblyStep::Output(pes)) => {
                    source_packet(&pes, request, output)?;
                }
                Ok(AssemblyStep::NeedPayload | AssemblyStep::Finished) => {
                    if prefix.is_some_and(|wire| wire != self.pes.partial_header()) {
                        return Err("partial PES header differs from the carrier bytes".into());
                    }
                    return Ok(());
                }
                Ok(AssemblyStep::Yield) => {}
                Err(error) if matches!(error, Error::Invalid(_) | Error::Truncated) => {
                    output.note(format_args!("assembler-fault {} {error}", self.pid))?;
                    self.pes.discard(epochs);
                    return Ok(());
                }
                Err(error) => return Err(err(error)),
            }
        }
    }
}

/// The cut before a complete MPEG-2 PES header, from H.222.0 §2.4.3.6.
fn incomplete_header(previous: &[u8], payload: &[u8], pusi: bool) -> Option<Vec<u8>> {
    let preceding = if pusi { &[] } else { previous };
    if (!pusi && preceding.is_empty()) || preceding.len() + payload.len() >= 264 {
        return None;
    }
    let prefix: Vec<_> = preceding.iter().chain(payload).copied().collect();
    let start_code = [0, 0, 1];
    if !prefix.starts_with(&start_code[..prefix.len().min(3)]) {
        return None;
    }
    if prefix.len() > 3 && !matches!(prefix[3], 0xbd | 0xc0..=0xef) {
        return None;
    }
    if prefix.len() > 6 && prefix[6] >> 6 != 0b10 {
        return None;
    }
    let header_len = prefix.get(8).map_or(9, |&length| 9 + usize::from(length));
    if let Some(word) = prefix.get(4..6) {
        let packet_len = usize::from(u16::from_be_bytes([word[0], word[1]]));
        if packet_len != 0 && header_len > 6 + packet_len {
            return None;
        }
    }
    (prefix.len() < header_len).then_some(prefix)
}

fn sequence_position(request: &Request, source: SourceId, offset: u64) -> u64 {
    request.entities[source_index(source)].base + offset
}

fn ticks(time: Option<Time>) -> i64 {
    time.map_or(NO_TIME, |time| {
        (time.numerator() * 90_000 / time.denominator() as i128) as i64
    })
}

fn range_bytes(request: &Request, range: SourceRange) -> Option<&[u8]> {
    let bytes = &request.entities.get(source_index(range.entity))?.bytes;
    let start = usize::try_from(range.offset).ok()?;
    bytes.as_ref().get(start..start.checked_add(range.bytes)?)
}

fn check_pes_ranges(pes: &Pes, request: &Request) -> Result<(), String> {
    let mut header = pes.header.as_bytes();
    for &range in pes.header_source_ranges.as_slice() {
        let wire = range_bytes(request, range).ok_or("PES header range lies outside its entity")?;
        header = header
            .strip_prefix(wire)
            .ok_or("PES header range differs from the input bytes")?;
    }
    if !header.is_empty() {
        return Err("PES header ranges do not cover the entire header".into());
    }
    for (range, payload) in pes.source_ranges().zip(pes.payload()) {
        if range_bytes(request, range) != Some(payload) {
            return Err("PES payload range differs from the input bytes".into());
        }
    }
    Ok(())
}

fn source_packet(pes: &Pes, request: &Request, output: &mut Output<'_, '_>) -> Result<(), String> {
    let start = pes
        .header_source_ranges
        .as_slice()
        .first()
        .ok_or("PES has no source header range")?;
    let width = request.format.bytes() as u64;
    let position = sequence_position(request, start.entity, start.offset / width * width);
    check_pes_ranges(pes, request)?;
    let header = &pes.header;
    let pts = ticks(header.raw_pts_time().map_err(err)?);
    let dts = ticks(header.raw_dts_time().map_err(err)?);
    if output.trace.is_some() {
        output.note(format_args!(
            "header {} {} {} {} {:02x?}",
            header.stream_id,
            header.declared_packet_length,
            u8::from(header.data_alignment_indicator),
            u8::from(header.priority),
            header.as_bytes(),
        ))?;
        for range in pes.source_ranges() {
            output.note(format_args!(
                "source-range {} {} {}",
                range.entity.0, range.offset, range.bytes
            ))?;
        }
    }
    output.packet(
        [
            0,
            u64::from(pes.pid),
            signed(pts),
            signed(dts),
            0,
            position,
            0,
            pes.payload_len as u64,
        ],
        header.stream_id,
        pes.payload(),
    )
}

struct Observer {
    demux: DemuxObserver,
    form: ObserverForm,
    position_base: u64,
    carriers: BTreeMap<(u64, u64), (usize, u64, SourceContext)>,
}

impl Observer {
    fn new(
        form: ObserverForm,
        request: &Request,
        map: &ProgramMap,
        input: &InputCursor,
        context: Option<&InputContext>,
        epochs: SourceEpochs,
    ) -> Result<Self, Error> {
        let [stream] = map.streams.as_slice() else {
            return Err(Error::Invalid(
                "demux observer requires one elementary stream",
            ));
        };
        let registered = match stream.stream_type {
            0x1b => Some(RegisteredCarriage::Avc),
            0x24 => Some(RegisteredCarriage::Hevc),
            0x0f => Some(RegisteredCarriage::AacAdts),
            0x11 => Some(RegisteredCarriage::AacLatm),
            _ => None,
        };
        let scope = DemuxScope {
            program: request.program,
            pid: stream.pid,
            stream_id: match registered {
                Some(RegisteredCarriage::AacAdts | RegisteredCarriage::AacLatm) => 0xc0,
                Some(_) => 0xe0,
                None => 0xbd,
            },
            stream_type: stream.stream_type,
            entity: input.source(),
            epochs,
        };
        let budget = input.budget().clone();
        let limits = ParseLimits::default();
        let mut demux = match (form, registered) {
            (ObserverForm::Unregistered, _) => DemuxObserver::new(scope, budget, limits),
            (_, Some(carriage)) => DemuxObserver::new_registered(scope, carriage, budget, limits),
            (_, None) => {
                let program = map.program_descriptors.as_ref();
                let elementary = stream.descriptor_bytes.as_ref();
                let dvb_subtitle = stream.stream_type == 6
                    && matches!(map.descriptor_namespace, DescriptorNamespace::Dvb(_))
                    && Descriptors::new(elementary)
                        .any(|entry| entry.is_ok_and(|entry| entry.tag == 0x59));
                if dvb_subtitle {
                    DemuxObserver::new_dvb_subtitles(
                        scope,
                        map.descriptor_namespace,
                        DvbSubtitlesDescriptors {
                            program,
                            elementary,
                        },
                        budget,
                        limits,
                    )
                } else {
                    let descriptors = PrivateAudioDescriptors {
                        program,
                        elementary,
                    };
                    let carriage = PrivateAudioCarriage::from_observed_pmt(
                        stream.stream_type,
                        descriptors,
                        limits,
                    )?;
                    DemuxObserver::new_private_audio(scope, carriage, descriptors, budget, limits)
                }
            }
        }?;
        if let Some(context) = context {
            demux.bind_input(context, input)?;
        }
        if form == ObserverForm::Compact {
            demux.enable_compact_payloads()?;
        }
        Ok(Self {
            demux,
            form,
            position_base: sequence_position(request, input.source(), 0),
            carriers: BTreeMap::new(),
        })
    }

    fn remember_carrier(
        &mut self,
        packet: &TransportPacket,
        facts: SourceContext,
    ) -> Result<(), String> {
        let entity = packet.source_id.ok_or("observer carrier without source")?;
        self.carriers.insert(
            (entity.0, packet.source_offset),
            (packet.source.len(), packet.payload_source_offset, facts),
        );
        Ok(())
    }

    fn check_span(
        &self,
        request: &Request,
        source: SourceRange,
        transport: SourceRange,
        context: SourceContext,
        bytes: &[u8],
    ) -> Result<(), String> {
        let &(width, payload, offered) = self
            .carriers
            .get(&(transport.entity.0, transport.offset))
            .ok_or("observer span has no offered carrier")?;
        let end = transport.offset + width as u64;
        if source.entity != transport.entity
            || transport.bytes != width
            || source.offset < payload
            || source.offset.saturating_add(source.bytes as u64) > end
            || context != offered
            || range_bytes(request, source) != Some(bytes)
        {
            return Err("observer span differs from its carrier or source context".into());
        }
        Ok(())
    }

    fn drain(
        &mut self,
        input: &InputCursor,
        clock: &mut DemuxClock,
        request: &Request,
        output: &mut Output<'_, '_>,
    ) -> Result<(), Stop> {
        loop {
            let work = &mut WorkBudget::new(GRANT);
            if self.form == ObserverForm::Compact {
                match self
                    .demux
                    .poll_compact_payload_with_input(input, work, &CANCEL)?
                {
                    AssemblyStep::Output(packet) => {
                        compact_packet(packet, self, request, clock, output)?;
                    }
                    AssemblyStep::Yield => {}
                    AssemblyStep::NeedPayload | AssemblyStep::Finished => return Ok(()),
                }
            } else {
                let step = if self.form == ObserverForm::InputBound {
                    self.demux.poll_with_input(input, work, &CANCEL)
                } else {
                    self.demux.poll(work, &CANCEL)
                }?;
                match step {
                    AssemblyStep::Output(packet) => {
                        for span in packet.spans.as_slice() {
                            self.check_span(
                                request,
                                span.source,
                                span.transport,
                                span.context,
                                span.as_ref(),
                            )?;
                        }
                        owned_packet(&packet, self.position_base, clock, output)?;
                    }
                    AssemblyStep::Yield => {}
                    AssemblyStep::NeedPayload | AssemblyStep::Finished => return Ok(()),
                }
            }
        }
    }
}

fn observer_times(clock: &mut DemuxClock, header: Option<&PesHeader>) -> Result<[u64; 2], Stop> {
    let (pts, dts) = clock.place(
        header.and_then(|h| h.pts_raw33),
        header.and_then(|h| h.dts_raw33),
    )?;
    Ok([pts, dts].map(|time| signed(time.map_or(NO_TIME, |value| value as i64))))
}

fn owned_packet(
    packet: &DemuxPacket,
    base: u64,
    clock: &mut DemuxClock,
    output: &mut Output<'_, '_>,
) -> Result<(), Stop> {
    let [pts, dts] = observer_times(clock, packet.time_header())?;
    output.packet(
        [
            0,
            u64::from(packet.pid),
            pts,
            dts,
            signed(packet.duration90_raw),
            (packet.position as u64).wrapping_add(base),
            u64::from(packet.flags),
            packet.payload_len as u64,
        ],
        packet.stream_id,
        packet.payload(),
    )?;
    Ok(())
}

fn compact_packet(
    packet: CompactDemuxPacket,
    observer: &Observer,
    request: &Request,
    clock: &mut DemuxClock,
    output: &mut Output<'_, '_>,
) -> Result<(), Stop> {
    let [pts, dts] = observer_times(clock, packet.time_header())?;
    let fields = [
        0,
        u64::from(packet.pid),
        pts,
        dts,
        signed(packet.duration90_raw),
        (packet.position as u64).wrapping_add(observer.position_base),
        u64::from(packet.flags),
        packet.payload_len as u64,
    ];
    let spans: DemuxSpanViews<'_> = packet.payload.spans();
    let mut expected = Vec::new();
    for span in spans {
        observer.check_span(
            request,
            span.source,
            span.transport,
            span.context,
            span.as_ref(),
        )?;
        expected.push((span.source, span.transport, span.context));
        if output.trace.is_some() {
            output.note(format_args!(
                "demux-span {} {} {} {} {} {} {:?}",
                span.source.entity.0,
                span.source.offset,
                span.source.bytes,
                span.transport.entity.0,
                span.transport.offset,
                span.transport.bytes,
                span.context,
            ))?;
        }
    }
    let mut cursor = packet.payload.into_cursor();
    output.packet_from(fields, packet.stream_id, |body| {
        let mut expected = expected.into_iter();
        while let Some(next) = cursor.next_span() {
            let facts = (next.source, next.transport, next.context);
            let span = cursor
                .current()
                .ok_or("compact cursor lost its current span")?;
            if expected.next() != Some(facts)
                || facts != (span.source, span.transport, span.context)
            {
                return Err("compact cursor changed the borrowed span order or context".into());
            }
            observer.check_span(
                request,
                span.source,
                span.transport,
                span.context,
                span.as_ref(),
            )?;
            body.push(span.as_ref());
            cursor.retire_current();
        }
        if expected.next().is_some() {
            return Err("compact cursor omitted a payload span".into());
        }
        Ok(())
    })?;
    Ok(())
}

fn execute(request: &Request, output: &mut Output<'_, '_>) -> Result<(), Stop> {
    let budget = &request.budget;
    let mut entity_index = 0;
    let mut bytes = &request.entities[0].bytes;
    let mut input = InputCursor::new(budget, SourceId(1), capabilities(bytes.len()), WINDOW)?;
    let limits = ParseLimits::default();
    let mut stream = if request.namespace == DescriptorNamespace::Unspecified {
        TransportStream::new(budget.clone(), request.format, limits, request.program)
    } else {
        TransportStream::new_with_namespace(
            budget.clone(),
            request.format,
            limits,
            request.program,
            request.namespace,
        )
    }?;
    let form = match request.mode {
        Mode::Source(form) => form,
        Mode::Assembler(_) | Mode::Observer(_) => PollForm::Poll,
    };
    let mut context = match request.mode {
        Mode::Observer(ObserverForm::InputBound | ObserverForm::Compact) => {
            Some(input_context(&mut input, bytes, request.format)?)
        }
        _ => None,
    };
    let mut assembly: Option<Assembly> = None;
    let mut observer: Option<Observer> = None;
    let mut clock = DemuxClock::new(request.program)?;
    let mut ordinal = 0;
    let mut controls = controls::SourceControls::default();
    loop {
        match view_poll(form, &mut stream, &mut input, request, output)? {
            Step::Ready(Some(event)) => {
                let epochs = stream.source_epochs();
                if matches!(request.mode, Mode::Source(_)) {
                    controls.check(request, &stream, &event)?;
                }
                match event {
                    TraceEvent::ProgramMap {
                        map, active: true, ..
                    } => {
                        let active = stream
                            .active_program()
                            .ok_or("accepted PMT has no active program")?;
                        for track in active.streams() {
                            output.note(format_args!(
                                "program {} {} {} {} {}",
                                active.program_number(),
                                active.version(),
                                active.pcr_pid().unwrap_or(0x1fff),
                                track.pid(),
                                track.stream_type(),
                            ))?;
                        }
                        if matches!(request.mode, Mode::Assembler(_)) {
                            let [track] = map.streams.as_slice() else {
                                return Err(
                                    "PES assembler comparison requires one elementary stream"
                                        .into(),
                                );
                            };
                            assembly = Some(Assembly::new(
                                track.pid,
                                matches!(track.stream_type, 1 | 2 | 0x1b | 0x24),
                                &input,
                                epochs,
                            )?);
                        }
                        if let Mode::Observer(form) = request.mode {
                            let opened = Observer::new(
                                form,
                                request,
                                &map,
                                &input,
                                context.as_ref(),
                                epochs,
                            );
                            match opened {
                                Err(Error::Unsupported(reason))
                                    if request.scope_refusal
                                        && matches!(
                                            (form, reason),
                                            (
                                                ObserverForm::Unregistered,
                                                "AVC PES requires PMT stream type 0x1b and PES stream ID 0xe0"
                                            ) | (
                                                ObserverForm::Compact,
                                                "compact private PES payload"
                                            )
                                        ) =>
                                {
                                    return Ok(super::packets::record(
                                        output.work,
                                        &mut output.trace,
                                        8,
                                        &[],
                                        &[],
                                    )?);
                                }
                                result => observer = Some(result?),
                            }
                        }
                    }
                    TraceEvent::Pes(pes) if matches!(request.mode, Mode::Source(_)) => {
                        source_packet(&pes, request, output)?;
                    }
                    TraceEvent::Transport(packet) => {
                        if let (Mode::Assembler(quantum), Some(assembly)) =
                            (request.mode, &mut assembly)
                            && packet.pid == assembly.pid
                        {
                            assembly.push(packet, epochs, quantum, request, output)?;
                        } else if let Some(observer) = &mut observer
                            && packet.pid == observer.demux.scope().pid
                        {
                            let facts = SourceContext {
                                source_event_ordinal: ordinal,
                                epochs,
                                byte_run_epoch: None,
                            };
                            observer.remember_carrier(&packet, facts)?;
                            if observer.form == ObserverForm::Unregistered {
                                observer.demux.push(packet, facts)
                            } else {
                                observer.demux.push_with_input(packet, facts, &input)
                            }
                            .map_err(|refused| Stop::Refused(refused.error))?;
                            observer.drain(&input, &mut clock, request, output)?;
                        }
                    }
                    TraceEvent::Diagnostic(fault) => {
                        output.note(format_args!(
                            "source-fault {:?} {:?} {} {} {}",
                            fault.kind,
                            fault.pid,
                            fault.location.source.0,
                            fault.location.offset,
                            fault.error,
                        ))?;
                        if fault.pid.is_none()
                            && let Some(assembly) = &mut assembly
                        {
                            assembly.continuity.reset();
                            assembly.pes.discard(epochs);
                        }
                    }
                    TraceEvent::Timeline {
                        program,
                        clock_epoch,
                    } => {
                        output.note(format_args!("timeline {program} {clock_epoch}"))?;
                        if let Some(assembly) = &mut assembly {
                            assembly.pes.discard(epochs);
                        }
                    }
                    TraceEvent::Finished => {
                        if let (Mode::Assembler(quantum), Some(assembly)) =
                            (request.mode, &mut assembly)
                        {
                            assembly.pes.end_of_source();
                            assembly.drain(request, output, epochs, quantum, None)?;
                        }
                        if let Some(observer) = &mut observer {
                            observer.demux.end_of_source()?;
                            observer.drain(&input, &mut clock, request, output)?;
                        }
                        return Ok(());
                    }
                    _ => {}
                }
                ordinal += 1;
            }
            Step::Ready(None) | Step::Yield => {}
            Step::NeedInput(request) => supply(&mut input, bytes, request)?,
            Step::End(location) => {
                if matches!(request.mode, Mode::Source(_))
                    && (location.source != input.source() || location.offset != bytes.len() as u64)
                {
                    return Err("source poll ended at a different entity boundary".into());
                }
                let end = stream
                    .media_entity_end(&mut input, &mut WorkBudget::new(GRANT), &CANCEL)?
                    .ok_or("source entity end exhausted its work grant")?;
                entity_index += 1;
                let Some(next) = request.entities.get(entity_index) else {
                    stream.end_of_source()?;
                    continue;
                };
                bytes = &next.bytes;
                input.continue_source(
                    SourceId(entity_index as u64 + 1),
                    capabilities(bytes.len()),
                )?;
                if next.discontinuity {
                    stream.discontinuity()?;
                    assembly = None;
                    observer = None;
                    if context.is_some() {
                        context = Some(input_context(&mut input, bytes, request.format)?);
                    }
                } else if let Some(observer) = &mut observer
                    && !observer.demux.continue_media_entity(
                        &end,
                        &mut input,
                        &mut WorkBudget::new(GRANT),
                        &CANCEL,
                    )?
                {
                    return Err("observer entity end exhausted its work grant".into());
                }
            }
            Step::Backpressure(never) => match never {},
        }
    }
}
