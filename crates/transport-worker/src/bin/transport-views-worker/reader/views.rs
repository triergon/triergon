use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::atomic::AtomicBool;

use triergon_core::container::{
    CodecId, ContainerReader, Event, MediaKind, PacketInfo, RandomAccess, ReaderStep, TrackInfo,
};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputWindow, Rounding, SourceId,
    Step, TimeBase, WorkBudget,
};
use triergon_framing::ac3::{Ac3Family, Ac3FrameHeader, probe_header};
use triergon_framing::mpeg_audio::MpegLayer;
use triergon_mpegts::compat::annex_b::{
    ContinuityEvent, ContinuityReader, ContinuityResidue, VideoEofPolicy,
};
use triergon_mpegts::compat::input::{InputBootstrap, InputStep};
use triergon_mpegts::compat::pes::{DemuxClock, PrivateAudioCarriage, UnframedAudioPolicy};
use triergon_mpegts::compat::{adts, latm};
use triergon_mpegts::pes::{Pes, PesHeader};
use triergon_mpegts::psi::{DescriptorNamespace, DvbNamespaceEvidence};
use triergon_mpegts::reader::{
    AudioEofPolicy, DvbSubtitleTailPolicy, EntityTransition, PacketClock,
};
use triergon_mpegts::reference_trace::input::PositionBasis;
use triergon_mpegts::stream::TraceEvent;
use triergon_mpegts::{
    AccessUnit, Packet, PacketFormat, Reader, ReaderEvent, ReaderLimits, SourceEpochs, Track,
};

use super::{CAPACITY, SOURCE, WINDOW, WORK, capabilities, explain, row};
use crate::protocol::{Operation, Work};

struct Entity {
    bytes: Bytes,
    base: u64,
    transition: EntityTransition,
}

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    args: &[&str],
) -> Result<Operation<'a>, String> {
    let [size, program, rest @ ..] = args else {
        return Err("expected SIZE PROGRAM [--dvb|--atsc] [[--discontinuity] ENTITY]...".into());
    };
    let format = match *size {
        "188" => PacketFormat::Ts188,
        "192" => PacketFormat::M2ts192,
        _ => return Err("expected TS188 or M2TS192".into()),
    };
    let program = program.parse().map_err(explain)?;
    let (descriptor_namespace, mut rest) = match rest {
        ["--dvb", rest @ ..] => (
            DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile),
            rest,
        ),
        ["--atsc", rest @ ..] => (DescriptorNamespace::Atsc, rest),
        _ => (DescriptorNamespace::Unspecified, rest),
    };
    let limits = ReaderLimits {
        format,
        program: Some(program),
        descriptor_namespace,
        audio_eof_policy: AudioEofPolicy::ReportIncomplete,
        unframed_audio_policy: UnframedAudioPolicy::ReportAtEof,
        adts_packet_policy: adts::PacketPolicy::ReportFragments,
        adts_pes_policy: adts::PesPolicy::ReportUnbounded,
        latm_packet_policy: latm::PacketPolicy::PreservePes,
        dvb_subtitle_tail_policy: DvbSubtitleTailPolicy::ReportCompletePrefix,
        video_eof_policy: VideoEofPolicy::ReportUnframed,
        ..ReaderLimits::default()
    };
    if input.len() > CAPACITY / 2 {
        return Err("TS sequence exceeds its input budget".into());
    }
    let pool = Budget::new(CAPACITY).map_err(explain)?;
    let mut entities = vec![Entity {
        bytes: Bytes::copy_from_slice(&pool, input).map_err(explain)?,
        base: 0,
        transition: EntityTransition::Contiguous,
    }];
    let mut base = input.len() as u64;
    while !rest.is_empty() {
        let (transition, path, next) = match rest {
            ["--discontinuity", path, next @ ..] => (EntityTransition::Discontinuity, path, next),
            [path, next @ ..] if !path.starts_with('-') => {
                (EntityTransition::Contiguous, path, next)
            }
            _ => return Err("invalid following entity".into()),
        };
        if entities.len() == 64 {
            return Err("too many entities".into());
        }
        let mut file = std::fs::File::open(path).map_err(explain)?;
        let length = file.metadata().map_err(explain)?.len();
        if length > (CAPACITY as u64 / 2).saturating_sub(base) {
            return Err("TS sequence exceeds its input budget".into());
        }
        let mut storage =
            BudgetVec::try_filled(&pool, 0, usize::try_from(length).map_err(explain)?)
                .map_err(explain)?;
        file.read_exact(storage.as_mut_slice()).map_err(explain)?;
        if file.read(&mut [0]).map_err(explain)? != 0 {
            return Err("TS entity grew beyond its checked length".into());
        }
        let bytes = storage.freeze().map_err(|error| explain(error.error))?;
        entities.push(Entity {
            bytes,
            base,
            transition,
        });
        base += length;
        rest = next;
    }
    let container = prefix == "ts-reader-container-";
    Ok(Operation {
        name: if container {
            "ts-reader-container"
        } else {
            "ts-reader-views"
        },
        run: Box::new(move |work, trace| {
            let caps = capabilities(entities[0].bytes.len(), false);
            let mut output = Output {
                work,
                trace,
                entities: &entities,
                format,
                sync_offset: if format == PacketFormat::M2ts192 {
                    position_offset(&pool, &entities).map_err(explain)?
                } else {
                    0
                },
                clock: DemuxClock::new(program).map_err(explain)?,
                tracks: BTreeMap::new(),
                indices: BTreeMap::new(),
                audio: BTreeMap::new(),
                container,
            };
            let result = if container {
                let reader =
                    ContinuityReader::new(&pool, SOURCE, caps, limits, WINDOW).map_err(explain)?;
                drain(reader, &mut output)
            } else {
                let reader = Reader::new(&pool, SOURCE, caps, limits, WINDOW).map_err(explain)?;
                drain(reader, &mut output)
            };
            match result {
                Ok(()) => output.row(7, &[], &[]),
                Err(error) => {
                    if let Some(trace) = &mut output.trace {
                        writeln!(trace, "# {error}").map_err(explain)?;
                    }
                    output.row(8, &[], &[])
                }
            }
        }),
    })
}

trait ViewReader: ContainerReader<Packet = Packet, Track = Track> {
    fn request(&self) -> triergon_core::Result<InputRequest>;
    fn close(&mut self);
    fn continue_entity(
        &mut self,
        request: InputRequest,
        source: SourceId,
        entity: &Entity,
    ) -> triergon_core::Result<()>;
    fn observation(event: Self::Other) -> ContinuityEvent;
}

impl ViewReader for Reader {
    fn request(&self) -> triergon_core::Result<InputRequest> {
        self.request()
    }
    fn close(&mut self) {
        self.close();
    }
    fn continue_entity(
        &mut self,
        request: InputRequest,
        source: SourceId,
        entity: &Entity,
    ) -> triergon_core::Result<()> {
        self.finish_entity(
            request.source,
            request.token,
            source,
            capabilities(entity.bytes.len(), false),
            entity.transition,
        )
    }
    fn observation(event: ReaderEvent) -> ContinuityEvent {
        ContinuityEvent::Source(event)
    }
}

impl ViewReader for ContinuityReader {
    fn request(&self) -> triergon_core::Result<InputRequest> {
        self.request()
    }
    fn close(&mut self) {
        self.close();
    }
    fn continue_entity(
        &mut self,
        request: InputRequest,
        source: SourceId,
        entity: &Entity,
    ) -> triergon_core::Result<()> {
        self.finish_entity(
            request.source,
            request.token,
            source,
            capabilities(entity.bytes.len(), false),
            entity.transition,
        )
    }
    fn observation(event: ContinuityEvent) -> ContinuityEvent {
        event
    }
}

fn drain<R: ViewReader>(mut reader: R, out: &mut Output<'_, '_>) -> Result<(), String> {
    let cancel = AtomicBool::new(false);
    let mut index = 0;
    let request = reader.request().map_err(explain)?;
    if reader.capabilities() != capabilities(out.entities[0].bytes.len(), false)
        || request.source != SOURCE
        || request.offset != 0
    {
        return Err("reader returned a different initial source view".into());
    }
    loop {
        let step: ReaderStep<R> = reader
            .step(&mut WorkBudget::new(WORK), &cancel)
            .map_err(explain)?;
        match step {
            Step::NeedInput(request) => {
                let bytes = &out.entities[index].bytes;
                let start = usize::try_from(request.offset).map_err(explain)?;
                if start == bytes.len() {
                    if let Some(next) = out.entities.get(index + 1) {
                        index += 1;
                        reader
                            .continue_entity(request, SourceId(1 + index as u64), next)
                            .map_err(explain)?;
                    } else {
                        reader
                            .finish(request.source, request.token)
                            .map_err(explain)?;
                    }
                } else {
                    let data = bytes
                        .slice(start..bytes.len().min(start.saturating_add(request.maximum)))
                        .map_err(explain)?;
                    reader
                        .provide(
                            &InputWindow::new(request.source, request.token, request.offset, data)
                                .map_err(explain)?,
                        )
                        .map_err(explain)?;
                }
            }
            Step::Ready(Event::Packet(packet)) => out.queue_packet(packet, &cancel)?,
            Step::Ready(Event::Other(event)) => match R::observation(event) {
                ContinuityEvent::Source(event) | ContinuityEvent::Pending(event) => {
                    out.source(event, &cancel)?;
                }
                ContinuityEvent::Residue(residue) => out.residue(&residue)?,
            },
            Step::Ready(Event::Seek(_)) => {
                return Err("unexpected seek while reading forward".into());
            }
            Step::Backpressure(pressure) => return Err(explain(Error::Backpressure(pressure))),
            Step::Yield => {}
            Step::End(end) => {
                if end.source != SourceId(1 + index as u64)
                    || end.offset != out.entities[index].bytes.len() as u64
                    || index + 1 != out.entities.len()
                {
                    return Err("reader ended before the complete source sequence".into());
                }
                out.flush_audio(&cancel)?;
                for index in 0..reader.track_count() {
                    out.track(reader.track(index).ok_or("track count exceeds catalogue")?)?;
                }
                reader.close();
                if !matches!(reader.request(), Err(Error::Closed)) {
                    return Err("closed reader still offers input".into());
                }
                return Ok(());
            }
        }
    }
}

struct Output<'work, 'trace> {
    work: &'work mut Work,
    trace: Option<&'trace mut dyn Write>,
    entities: &'work [Entity],
    format: PacketFormat,
    sync_offset: u64,
    clock: DemuxClock,
    tracks: BTreeMap<u64, u64>,
    indices: BTreeMap<u16, u64>,
    audio: BTreeMap<u16, AudioLookahead>,
    container: bool,
}

struct PacketPayload<'a> {
    base: TimeBase,
    duration: i64,
    flags: u64,
    bytes: &'a [u8],
}

impl<'a> PacketPayload<'a> {
    fn untimed(flags: u64, bytes: &'a [u8]) -> Self {
        Self {
            base: TimeBase::HZ_90000,
            duration: 0,
            flags,
            bytes,
        }
    }
}

#[derive(Default)]
struct AudioLookahead {
    ac3: Option<Packet>,
    ac3_duration_known: bool,
    mpeg_audio_seen: bool,
}

fn ac3_header(packet: &Packet) -> Option<Ac3FrameHeader> {
    match packet.access_unit() {
        AccessUnit::Ac3(unit) => Some(unit.facts().header),
        AccessUnit::IncompleteAudio { bytes, .. } => probe_header(bytes.as_ref()).ok(),
        _ => None,
    }
}

fn ac3_duration(packet: &Packet, known: bool) -> Result<i64, String> {
    if !known {
        return Ok(0);
    }
    if let Some(duration) = packet.duration() {
        return Ok(duration);
    }
    let span = ac3_header(packet)
        .map(Ac3FrameHeader::sample_span)
        .transpose()
        .map_err(explain)?;
    span.map_or(Ok(0), |span| {
        span.to_i64_ticks(TimeBase::HZ_90000, Rounding::NearestAway)
            .map_err(explain)
    })
}

fn kind(kind: MediaKind) -> u64 {
    match kind {
        MediaKind::Video => 1,
        MediaKind::Audio => 2,
        MediaKind::Subtitle => 3,
        MediaKind::Data => 4,
        MediaKind::Unknown => 0,
    }
}

fn codec(codec: CodecId) -> u64 {
    match codec {
        CodecId::Mpeg2Video => 1,
        CodecId::H264 => 2,
        CodecId::Hevc => 3,
        CodecId::Aac => 4,
        CodecId::AacLatm => 5,
        CodecId::Ac3 => 6,
        CodecId::Eac3 => 7,
        CodecId::Dts => 8,
        CodecId::MpegAudio => 9,
        CodecId::DvbSubtitle => 10,
        CodecId::DvbTeletext => 11,
        _ => 0,
    }
}

impl Output<'_, '_> {
    fn row(&mut self, kind: u64, words: &[u64], bytes: &[u8]) -> Result<(), String> {
        row(self.work, &mut self.trace, kind, words, bytes)
    }

    fn track(&mut self, track: &Track) -> Result<(), String> {
        let pid = track.id().0;
        let epoch = track.configuration_epoch();
        if self.tracks.insert(pid, epoch) == Some(epoch) || self.container {
            return Ok(());
        }
        let tag = *track
            .codec_tag()
            .first()
            .ok_or("track is missing its transport stream type")?;
        let config = track.configuration();
        self.row(
            2,
            &[
                pid,
                kind(track.kind()),
                codec(track.codec()),
                u64::from(tag),
                epoch,
                config.len() as u64,
            ],
            config,
        )
    }

    fn queue_packet(&mut self, packet: Packet, cancel: &AtomicBool) -> Result<(), String> {
        let pid = packet.track_info().stream().pid;
        let ac3 = matches!(packet.access_unit(), AccessUnit::Ac3(_))
            || matches!(packet.access_unit(), AccessUnit::IncompleteAudio { .. })
                && matches!(packet.track_info().stream().stream_type, 0x06 | 0x81 | 0x87);
        let duration = if ac3 {
            let header = ac3_header(&packet);
            if header.is_some_and(|header| header.family == Ac3Family::Eac3) {
                ac3_duration(&packet, true)?
            } else {
                let audio = self.audio.entry(pid).or_default();
                audio.ac3_duration_known |= audio.ac3.is_some() && header.is_some();
                let Some(previous) = audio.ac3.replace(packet) else {
                    return Ok(());
                };
                let duration = ac3_duration(&previous, audio.ac3_duration_known)?;
                return self.packet(&previous, duration, cancel);
            }
        } else {
            match packet.access_unit() {
                AccessUnit::MpegAudio(frame) => {
                    let first = !std::mem::replace(
                        &mut self.audio.entry(pid).or_default().mpeg_audio_seen,
                        true,
                    );
                    let header = frame.header();
                    if first && header.layer != MpegLayer::III {
                        0
                    } else {
                        i64::from(header.samples_per_frame()) * 90_000
                            / i64::from(header.sample_rate)
                    }
                }
                AccessUnit::Dts(frame) if frame.core_bytes().len() == frame.bytes().len() => {
                    let span = frame.sample_span();
                    span.to_i64_ticks(TimeBase::HZ_90000, Rounding::TowardZero)
                        .map_err(explain)?
                }
                _ => 0,
            }
        };
        self.packet(&packet, duration, cancel)
    }

    fn flush_audio(&mut self, cancel: &AtomicBool) -> Result<(), String> {
        let mut audio: Vec<_> = std::mem::take(&mut self.audio).into_iter().collect();
        audio.sort_by_key(|(pid, _)| self.indices.get(pid).copied());
        for (_, lookahead) in audio {
            if let Some(packet) = lookahead.ac3 {
                let duration = ac3_duration(&packet, lookahead.ac3_duration_known)?;
                self.packet(&packet, duration, cancel)?;
            }
        }
        Ok(())
    }

    fn packet(
        &mut self,
        packet: &Packet,
        duration: i64,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let track = packet.track_info();
        self.track(track)?;
        let source = packet
            .sources()
            .first()
            .ok_or("unit has no PES provenance")?;
        let pes = source
            .pes
            .event()
            .pes_source()
            .ok_or("unit provenance is not a PES")?;
        let clock = packet.clock();
        let positioned = match packet.access_unit() {
            AccessUnit::DvbSubtitle(_) | AccessUnit::DtsExtension(_) => Some(pes),
            AccessUnit::Dts(frame) if frame.core_bytes().len() != frame.bytes().len() => Some(pes),
            AccessUnit::MpegAudio(_) | AccessUnit::Dts(_) => {
                clock.and_then(|clock| clock.pes.event().pes_source())
            }
            _ => (source.payload_range.start == 0).then_some(pes),
        };
        let flags = match packet.access_unit() {
            AccessUnit::Video(unit) => u64::from(unit.random_access),
            AccessUnit::Loas(_) | AccessUnit::DtsExtension(_) => 0,
            AccessUnit::Dts(frame) if frame.core_bytes().len() != frame.bytes().len() => 0,
            _ => u64::from(packet.random_access() != RandomAccess::NotSync),
        };
        self.timed_packet(
            pes,
            positioned,
            clock_header(clock)?,
            packet.epochs(),
            PacketPayload {
                base: packet.time_base(),
                duration,
                flags,
                bytes: packet.payload(),
            },
            cancel,
        )?;
        if !self.container {
            match packet.access_unit() {
                AccessUnit::Ac3(unit) => {
                    for frame in unit.syncframes() {
                        self.row(4, &[6, frame.header.frame_bytes as u64], &[])?;
                    }
                }
                AccessUnit::DvbSubtitle(unit) => {
                    let tail = unit.truncated_tail();
                    self.row(
                        4,
                        &[
                            10,
                            unit.segment_count() as u64,
                            tail.is_some() as u64,
                            tail.map_or(0, |tail| tail.end - tail.start),
                        ],
                        &[],
                    )?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn source(&mut self, event: ReaderEvent, cancel: &AtomicBool) -> Result<(), String> {
        match event {
            ReaderEvent::VideoHeaders(packet)
            | ReaderEvent::IncompleteAudio(packet)
            | ReaderEvent::DvbSubtitlePrefix(packet)
            | ReaderEvent::AdtsFragment(packet)
            | ReaderEvent::RecoveredAdts(packet) => self.queue_packet(packet, cancel),
            ReaderEvent::IncompleteVideo(video) => {
                self.track(video.track_info())?;
                let first = video.sources().first().ok_or("video has no PES")?;
                let pes = first
                    .pes
                    .event()
                    .pes_source()
                    .ok_or("video source is not PES")?;
                let range = video.elementary_range();
                self.timed_packet(
                    pes,
                    (first.payload_range.start == 0).then_some(pes),
                    clock_header(video.clock())?,
                    video.epochs(),
                    PacketPayload::untimed(0, video.bytes().as_ref()),
                    cancel,
                )?;
                if !self.container {
                    self.row(4, &[2, range.start, range.end], &[])?;
                }
                Ok(())
            }
            ReaderEvent::UnframedAudio(audio) => {
                self.track(audio.track_info())?;
                let first = audio.sources().first().ok_or("audio has no PES")?;
                let pes = first
                    .pes
                    .event()
                    .pes_source()
                    .ok_or("audio source is not PES")?;
                let range = audio.elementary_range();
                let carriage = match audio.carriage() {
                    PrivateAudioCarriage::DvbAc3Tag => 0,
                    PrivateAudioCarriage::DvbEac3Tag => 1,
                    PrivateAudioCarriage::PrivateType81Ac3 => 2,
                    PrivateAudioCarriage::PrivateType87Eac3 => 3,
                };
                self.timed_packet(
                    pes,
                    (first.payload_range.start == 0).then_some(pes),
                    clock_header(audio.clock())?,
                    audio.epochs(),
                    PacketPayload::untimed(1, audio.bytes().as_ref()),
                    cancel,
                )?;
                if !self.container {
                    self.row(4, &[7, carriage, range.start, range.end], &[])?;
                }
                Ok(())
            }
            ReaderEvent::LatmPes(value) => {
                self.track(value.track_info())?;
                let pes = value.pes();
                if value.clock_is_quarantined() {
                    return Err("PES clock is quarantined".into());
                }
                let bytes: Vec<u8> = pes.payload().flatten().copied().collect();
                self.timed_packet(
                    pes,
                    Some(pes),
                    Some(&pes.header),
                    pes.epochs,
                    PacketPayload::untimed(0, &bytes),
                    cancel,
                )
            }
            ReaderEvent::Source(source) => {
                if let Some(pes) = source.event().pes_source() {
                    self.clock
                        .project(pes, &mut WorkBudget::new(WORK), cancel)
                        .map_err(explain)?
                        .ok_or("source PES clock yielded")?;
                }
                match source.event() {
                    TraceEvent::ProgramMap {
                        map, active: true, ..
                    } => {
                        for stream in map.streams.as_slice() {
                            let index = self.indices.len() as u64;
                            self.indices.entry(stream.pid).or_insert(index);
                        }
                    }
                    TraceEvent::Discontinuity { .. } => {
                        for audio in self.audio.values_mut() {
                            audio.ac3 = None;
                            audio.ac3_duration_known = false;
                        }
                    }
                    TraceEvent::UnboundedAdtsPes(pes) => {
                        if let Some(trace) = &mut self.trace {
                            writeln!(trace, "# unbounded ADTS PES {pes:?}").map_err(explain)?;
                        }
                    }
                    _ => {}
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn timed_packet(
        &mut self,
        pes: &Pes,
        position: Option<&Pes>,
        header: Option<&PesHeader>,
        epochs: SourceEpochs,
        payload: PacketPayload<'_>,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let view = self
            .clock
            .project_header(pes.pid, epochs, header, &mut WorkBudget::new(WORK), cancel)
            .map_err(explain)?
            .ok_or("clock projection yielded")?;
        let position = if let Some(pes) = position {
            let header = pes
                .header_source_ranges
                .as_slice()
                .first()
                .ok_or("header lacks a physical source range")?;
            self.position(header.entity, header.offset)?
        } else {
            u64::MAX
        };
        self.payload(
            pes.pid,
            (view.pts90, view.dts90),
            position,
            pes.header.stream_id,
            payload,
        )
    }

    fn position(&self, entity: SourceId, offset: u64) -> Result<u64, String> {
        let index = usize::try_from(entity.0.checked_sub(1).ok_or("invalid source ID")?)
            .map_err(explain)?;
        let item = self
            .entities
            .get(index)
            .ok_or("unrecognized packet source")?;
        let size = if self.format == PacketFormat::M2ts192 {
            192
        } else {
            188
        };
        Ok(item.base + offset / size * size + self.sync_offset)
    }

    fn payload(
        &mut self,
        pid: u16,
        (pts, dts): (Option<i128>, Option<i128>),
        position: u64,
        stream_id: u8,
        PacketPayload {
            base,
            duration,
            flags,
            bytes,
        }: PacketPayload<'_>,
    ) -> Result<(), String> {
        let ticks =
            |value: Option<i128>| value.map_or(Ok(i64::MIN), i64::try_from).map_err(explain);
        let next_index = self.indices.len() as u64;
        let index = *self.indices.entry(pid).or_insert(next_index);
        self.row(
            1,
            &[
                index,
                u64::from(pid),
                ticks(pts)? as u64,
                ticks(dts)? as u64,
                duration as u64,
                position,
                flags,
                bytes.len() as u64,
                base.numerator(),
                base.denominator(),
            ],
            bytes,
        )?;
        self.row(3, &[1, 1], &[stream_id])
    }

    fn residue(&mut self, residue: &ContinuityResidue) -> Result<(), String> {
        let first = residue
            .chunks()
            .first()
            .ok_or("continuity residue has no source chunks")?;
        let clock = residue.clock();
        let position = self.position(first.source_origin.source, first.source_origin.offset)?;
        let data: Vec<u8> = residue.payload().flatten().copied().collect();
        self.payload(
            first.pid,
            (clock.pts90, clock.dts90),
            position,
            first.stream_id,
            PacketPayload::untimed(0, &data),
        )?;
        let end = residue.source_end();
        self.row(6, &[end.offset, residue.evidence().len() as u64], &[])?;
        for evidence in residue.evidence() {
            self.row(
                6,
                &[
                    evidence.carrier.source_offset,
                    evidence.carrier.source.len() as u64,
                ],
                evidence.carrier.source.as_ref(),
            )?;
        }
        Ok(())
    }
}

fn clock_header(clock: Option<&PacketClock>) -> Result<Option<&PesHeader>, String> {
    match clock {
        Some(clock) if clock.quarantined => Err("clock is quarantined".into()),
        Some(clock) => clock
            .pes
            .event()
            .pes_source()
            .map(|pes| Some(&pes.header))
            .ok_or_else(|| "clock source is not a PES".into()),
        None => Ok(None),
    }
}

fn position_offset(pool: &Budget, entities: &[Entity]) -> triergon_core::Result<u64> {
    let contiguous = entities
        .iter()
        .take_while(|entity| entity.transition == EntityTransition::Contiguous);
    let len = contiguous
        .clone()
        .map(|entity| entity.bytes.len())
        .sum::<usize>();
    let complete = len <= InputBootstrap::MAX_BORROWED_BYTES;
    let count = if complete {
        len
    } else {
        InputBootstrap::M2TS_PREFIX_BYTES
    };
    let prefix = if count <= entities[0].bytes.len() {
        entities[0].bytes.slice(0..count)?
    } else {
        let mut prefix = BudgetVec::new(pool);
        prefix.try_reserve_exact(count)?;
        for entity in contiguous {
            let take = (count - prefix.len()).min(entity.bytes.len());
            prefix.try_extend_from_slice(&entity.bytes.as_ref()[..take])?;
            if prefix.len() == count {
                break;
            }
        }
        prefix.freeze().map_err(|refused| refused.error)?
    };
    let mut input = InputCursor::new(pool, SOURCE, capabilities(len, false), count)?;
    let request = input.request();
    let window = InputWindow::new(request.source, request.token, 0, prefix)?;
    input.provide(&window)?;
    let mut bootstrap = if complete {
        let mut bootstrap = InputBootstrap::new(&input, &window, PacketFormat::M2ts192)?;
        bootstrap.observe_entity_eof(&input, window.source, &window.token, count as u64)?;
        bootstrap
    } else {
        InputBootstrap::new_m2ts_prefix(&input, &window)?
    };
    loop {
        match bootstrap.poll(&input, &mut WorkBudget::new(WORK), &AtomicBool::new(false))? {
            InputStep::Ready(context) => {
                return Ok(if context.basis() == PositionBasis::SyncStart {
                    4
                } else {
                    0
                });
            }
            InputStep::Yield => {}
            InputStep::NeedEntityEof => {
                return Err(Error::Invalid("input-position probe lost EOF"));
            }
        }
    }
}
