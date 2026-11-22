use playback_worker::runner::{Invocation, MAX_INPUT, Mode, Result, Totals, measure};
use std::io::{self, Write};
use std::sync::atomic::AtomicBool;
use triergon_core::container::{PacketInfo, RandomAccess};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputWindow, SourceCapabilities,
    SourceId, Time, WorkBudget,
};
use triergon_framing::aac::inspect_adts_header;
use triergon_harness_protocol::digest::{fold, fold_bytes, signed};
use triergon_hls::aac::{HlsAac, HlsAacFrame};
use triergon_hls::avc::{AvcTsAccessUnit, AvcTsLimits, AvcTsReader, AvcTsStep};
use triergon_hls::clock::{HlsAacClock, HlsAacPtsOrigin, resolve_hls_aac_clocks};
use triergon_hls::progressive::{
    TsProgressiveLimits, TsProgressiveReader, TsProgressiveStep, TsProgressiveUnit,
};
use triergon_hls::segment::{
    HlsAudioCodec, HlsFeedUnit, HlsMediaProfile, HlsMediaSegment, HlsMediaSegmentReader,
    HlsSegment, HlsSegmentAudio, HlsSegmentLimits, HlsSegmentProgress, HlsSegmentReader,
    HlsSegmentTracks, HlsVideoCodec,
};
use triergon_mpegts::{AssemblyStep, ParseLimits, compat::pes::RegisteredCarriage};
use triergon_playback::{
    Source, SourceReply,
    compat::pes::Carriage,
    pes_feed::{FeedPacket, FeedStep, TsEngine},
};

const SOURCE: SourceId = SourceId(1);
const MAXIMUM_UNITS: usize = 8192;

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Packets,
    Clocks,
    Compact,
    Push,
}

impl View {
    fn operation(self) -> &'static str {
        match self {
            Self::Packets => "hls-unit-packets",
            Self::Clocks => "hls-aac-clocks",
            Self::Compact => "hls-compact-push",
            Self::Push => "hls-push-input",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reader {
    Segment,
    Progressive,
    Media,
}

#[derive(Clone, Copy)]
struct Case {
    view: View,
    reader: Reader,
    profile: HlsMediaProfile,
    window: usize,
}

impl Case {
    fn parse(args: &[String]) -> Result<Self> {
        let [view, reader, video, audio, window] = args else {
            return Err("expected VIEW READER VIDEO AUDIO WINDOW".into());
        };
        let view = match view.as_str() {
            "packets" => View::Packets,
            "clocks" => View::Clocks,
            "compact" => View::Compact,
            "push" => View::Push,
            _ => return Err("unknown HLS view".into()),
        };
        let reader = match reader.as_str() {
            "segment" => Reader::Segment,
            "progressive" => Reader::Progressive,
            "media" => Reader::Media,
            _ => return Err("unknown HLS reader".into()),
        };
        let video = match video.as_str() {
            "avc" => HlsVideoCodec::Avc,
            "hevc" => HlsVideoCodec::Hevc,
            _ => return Err("unknown HLS video codec".into()),
        };
        let audio = match audio.as_str() {
            "aac" => HlsAudioCodec::Aac,
            "ac3" => HlsAudioCodec::Ac3,
            "eac3" => HlsAudioCodec::Eac3,
            _ => return Err("unknown HLS audio codec".into()),
        };
        let window = window.parse::<usize>()?;
        let aac = audio == HlsAudioCodec::Aac;
        if !matches!(window, 1 | 188 | 4096)
            || (reader != Reader::Media && (video != HlsVideoCodec::Avc || !aac))
            || (view == View::Packets && reader == Reader::Media)
            || (view == View::Clocks && !aac)
            || (view == View::Compact && reader == Reader::Media)
            || (view == View::Push && reader == Reader::Progressive)
        {
            return Err("unsupported HLS view, reader, codecs or window".into());
        }
        Ok(Self {
            view,
            reader,
            profile: HlsMediaProfile {
                video,
                audio: Some(audio),
            },
            window,
        })
    }
}

pub(super) fn run(args: &[String]) -> Result<()> {
    let invocation = Invocation::parse(
        args,
        "hls-views-",
        " VIEW READER VIDEO AUDIO WINDOW",
        5..=5,
        true,
    )?;
    let case = Case::parse(invocation.arguments)?;
    let pool = Budget::new(256 << 20)?;
    let raw =
        Bytes::try_from_vec(&pool, invocation.input(MAX_INPUT)?).map_err(|error| error.error)?;
    let trace = invocation.mode == Mode::Trace;
    measure(&invocation, case.view.operation(), Some(&pool), || {
        let cancel = AtomicBool::new(false);
        if raw.is_empty() || !raw.len().is_multiple_of(188) {
            return refusal(trace);
        }
        let input = InputCursor::new(
            &pool,
            SOURCE,
            SourceCapabilities {
                length: (case.reader != Reader::Progressive).then_some(raw.len() as u64),
                random_access: false,
            },
            case.window,
        )?;
        let read = ReadBody {
            raw: &raw,
            cancel: &cancel,
        };
        let outcome = match (case.view, case.reader) {
            (View::Compact, _) => compact(input, &read, trace),
            (_, Reader::Progressive) => progressive(input, &read, case.view, trace),
            (_, Reader::Segment) => segment(input, &read, case.view, trace),
            (_, Reader::Media) => media(input, &read, case, trace),
        };
        match outcome {
            Err(error)
                if error.downcast_ref::<Error>().is_some_and(|e| {
                    matches!(
                        e,
                        Error::Invalid(_) | Error::Unsupported(_) | Error::Truncated
                    )
                }) =>
            {
                if trace {
                    eprintln!("{error}");
                }
                refusal(trace)
            }
            result => result,
        }
    })
}

fn refusal(trace: bool) -> Result<Totals> {
    let mut output = Records::new(trace);
    output.record(8, &[], std::iter::empty())?;
    Ok(output.total)
}

struct Records<'a> {
    total: Totals,
    trace: Option<io::StdoutLock<'a>>,
}

impl Records<'_> {
    fn new(trace: bool) -> Self {
        Self {
            total: Totals::default(),
            trace: trace.then(|| io::stdout().lock()),
        }
    }

    fn record<'a>(
        &mut self,
        kind: u64,
        fields: &[u64],
        payload: impl Iterator<Item = &'a [u8]>,
    ) -> Result<()> {
        fold(&mut self.total.digest, kind);
        for &value in fields {
            fold(&mut self.total.digest, value);
        }
        if let Some(out) = &mut self.trace {
            write!(out, "{kind}")?;
            for value in fields {
                write!(out, " {value}")?;
            }
        }
        let mut payload = payload.peekable();
        if payload.peek().is_some()
            && let Some(out) = &mut self.trace
        {
            write!(out, " ")?;
        }
        for bytes in payload {
            self.total.bytes += bytes.len() as u64;
            fold_bytes(&mut self.total.digest, bytes);
            if let Some(out) = &mut self.trace {
                for byte in bytes {
                    write!(out, "{byte:02x}")?;
                }
            }
        }
        if let Some(out) = &mut self.trace {
            writeln!(out)?;
        }
        self.total.packets += 1;
        Ok(())
    }

    fn end(mut self) -> Result<Totals> {
        self.record(7, &[], std::iter::empty())?;
        Ok(self.total)
    }

    fn packet(&mut self, lane: u64, packet: &impl PacketInfo) -> Result<()> {
        let time_base = packet.time_base();
        let [pts, pts_value] = optional(packet.pts());
        let [dts, dts_value] = optional(packet.dts());
        let [duration, duration_value] = optional(packet.duration());
        let bytes = packet.payload();
        self.record(
            1,
            &[
                lane,
                packet.track().0,
                time_base.numerator(),
                time_base.denominator(),
                pts,
                pts_value,
                dts,
                dts_value,
                duration,
                duration_value,
                match packet.random_access() {
                    RandomAccess::Unknown => 0,
                    RandomAccess::Sync => 1,
                    RandomAccess::NotSync => 2,
                },
                bytes.len() as u64,
            ],
            std::iter::once(bytes),
        )
    }

    fn aac_clock(&mut self, frame: &HlsAacFrame, clock: &HlsAacClock) -> Result<()> {
        let [pn, pd] = ratio(clock.pts)?;
        let [dn, dd] = ratio(clock.dts)?;
        let origin = match clock.origin {
            HlsAacPtsOrigin::Pes => 0,
            HlsAacPtsOrigin::BeforeFirstPes => 1,
            HlsAacPtsOrigin::AfterPes => 2,
            HlsAacPtsOrigin::SegmentLocal => 3,
        };
        self.record(
            3,
            &[
                pn,
                pd,
                dn,
                dd,
                origin,
                signed(clock.demux90.pts_and_dts),
                signed(clock.demux90.duration),
                frame.bytes.len() as u64,
            ],
            std::iter::once(frame.bytes.as_ref()),
        )
    }

    fn video<'a>(
        &mut self,
        clock: Option<triergon_hls::avc::AvcTsClock>,
        random_access: bool,
        raster: (u32, u32),
        size: usize,
        spans: impl Iterator<Item = &'a [u8]>,
    ) -> Result<()> {
        let [pts, pts_value] = optional(clock.and_then(|c| c.pts90));
        let [dts, dts_value] = optional(clock.and_then(|c| c.dts90));
        self.record(
            1,
            &[
                0,
                pts,
                pts_value,
                dts,
                dts_value,
                u64::from(random_access),
                u64::from(raster.0),
                u64::from(raster.1),
                size as u64,
            ],
            spans,
        )
    }

    fn audio_ns(&mut self, pts: i64, duration: i64, bytes: &[u8]) -> Result<()> {
        self.record(
            1,
            &[1, signed(pts), signed(duration), bytes.len() as u64],
            std::iter::once(bytes),
        )
    }

    fn tails(&mut self, video: i64, audio: Option<i64>) -> Result<()> {
        let [present, value] = optional(audio);
        self.record(3, &[signed(video), present, value], std::iter::empty())
    }
}

fn optional(value: Option<i64>) -> [u64; 2] {
    value.map_or([0, 0], |v| [1, signed(v)])
}

fn ratio(value: Time) -> Result<[u64; 2]> {
    Ok([
        signed(value.numerator().try_into()?),
        value.denominator().try_into()?,
    ])
}

struct Pending;

impl Source for Pending {
    fn poll(
        &mut self,
        _: &InputRequest,
        _: &mut WorkBudget,
        _: &AtomicBool,
    ) -> triergon_core::Result<SourceReply> {
        Ok(SourceReply::Pending)
    }

    fn invalidate(&mut self) {}
    fn abort(&mut self) {}
    fn close(&mut self) {}
}

struct ReadBody<'a> {
    raw: &'a Bytes,
    cancel: &'a AtomicBool,
}

impl ReadBody<'_> {
    fn answer(
        &self,
        request: &InputRequest,
        provide: impl FnOnce(&SourceReply) -> triergon_core::Result<bool>,
    ) -> Result<()> {
        let InputRequest {
            source,
            token,
            offset,
            maximum,
        } = request.clone();
        let start = usize::try_from(offset)?;
        let reply = if start >= self.raw.len() {
            SourceReply::End { source, token }
        } else {
            let end = start.saturating_add(maximum).min(self.raw.len());
            SourceReply::Data(InputWindow::new(
                source,
                token,
                offset,
                self.raw.slice(start..end)?,
            )?)
        };
        if !provide(&reply)? {
            return Err("reader refused its current request".into());
        }
        Ok(())
    }
}

fn segment(input: InputCursor, read: &ReadBody<'_>, view: View, trace: bool) -> Result<Totals> {
    let mut reader = HlsSegmentReader::new(
        input,
        Pending,
        HlsSegmentTracks {
            video_pid: None,
            audio: HlsSegmentAudio::Required,
        },
        HlsSegmentLimits::default(),
    )?;
    let segment = loop {
        match reader.advance(&mut WorkBudget::new(1 << 18), read.cancel)? {
            HlsSegmentProgress::Yield => {}
            HlsSegmentProgress::NeedInput(request) => {
                read.answer(&request, |r| reader.provide(r))?;
            }
            HlsSegmentProgress::Ready => {
                break reader.take(read.cancel)?.ok_or("missing segment")?;
            }
            HlsSegmentProgress::Backpressure { .. } => return Err("segment exhausted pool".into()),
        }
    };
    segment_records(&segment, view, trace)
}

fn segment_records(segment: &HlsSegment, view: View, trace: bool) -> Result<Totals> {
    let mut output = Records::new(trace);
    for video_pass in [true, false] {
        for unit in segment.access_units() {
            if video_pass != matches!(unit, HlsFeedUnit::Video(_)) {
                continue;
            }
            match (view, unit) {
                (View::Packets, unit) => {
                    output.packet(u64::from(!video_pass), &segment.packet(&unit)?)?;
                }
                (View::Clocks | View::Push, HlsFeedUnit::Audio(frame, clock)) => {
                    output.aac_clock(frame, clock)?;
                }
                (View::Push, HlsFeedUnit::Video(unit)) => output.video(
                    unit.clock,
                    unit.idr,
                    (unit.raster.width, unit.raster.height),
                    unit.feed_spans().map(<[u8]>::len).sum(),
                    unit.feed_spans(),
                )?,
                _ => {}
            }
        }
    }
    if view == View::Push {
        output.tails(segment.video_tail90(), segment.audio_tail90())?;
    }
    output.end()
}

fn media(input: InputCursor, read: &ReadBody<'_>, case: Case, trace: bool) -> Result<Totals> {
    let pool = input.budget().clone();
    let mut reader =
        HlsMediaSegmentReader::new(input, Pending, case.profile, HlsSegmentLimits::default())?;
    let segment = loop {
        match reader.advance(&mut WorkBudget::new(1 << 18), read.cancel)? {
            HlsSegmentProgress::Yield => {}
            HlsSegmentProgress::NeedInput(request) => {
                read.answer(&request, |r| reader.provide(r))?;
            }
            HlsSegmentProgress::Ready => {
                break reader.take(read.cancel)?.ok_or("missing media segment")?;
            }
            HlsSegmentProgress::Backpressure { .. } => {
                return Err("media segment exhausted pool".into());
            }
        }
    };
    if case.view == View::Push {
        return media_records(&segment, trace);
    }
    let mut frames = BudgetVec::new(&pool);
    for unit in segment.access_units() {
        if let HlsFeedUnit::Audio(frame, _) = unit {
            frames
                .try_push(HlsAacFrame {
                    bytes: frame.bytes.clone(),
                    header: inspect_adts_header(&frame.bytes)?,
                    pts: frame.pts,
                    dts: frame.dts,
                    duration: frame.duration,
                })
                .map_err(|error| error.error)?;
        }
    }
    clock_records(&pool, read.cancel, &frames, trace)
}

fn media_records(segment: &HlsMediaSegment, trace: bool) -> Result<Totals> {
    let raster = segment.initial_raster();
    let mut output = Records::new(trace);
    for video_pass in [true, false] {
        for unit in segment.access_units() {
            match unit {
                HlsFeedUnit::Video(unit) if video_pass => {
                    output.video(
                        unit.clock(),
                        unit.random_access(),
                        (raster.width, raster.height),
                        unit.feed_spans().map(<[u8]>::len).sum(),
                        unit.feed_spans(),
                    )?;
                }
                HlsFeedUnit::Audio(unit, clock) if !video_pass => {
                    output.audio_ns(clock.pts_ns, clock.duration_ns, &unit.bytes)?;
                }
                _ => {}
            }
        }
    }
    output.tails(segment.video_tail90(), segment.audio_tail_ns())?;
    output.end()
}

fn clock_records(
    pool: &Budget,
    cancel: &AtomicBool,
    frames: &[HlsAacFrame],
    trace: bool,
) -> Result<Totals> {
    let clocks = resolve_hls_aac_clocks(pool, frames, MAXIMUM_UNITS, cancel)?;
    let mut output = Records::new(trace);
    for (frame, clock) in frames.iter().zip(clocks.as_slice()) {
        output.aac_clock(frame, clock)?;
    }
    output.end()
}

fn progressive(input: InputCursor, read: &ReadBody<'_>, view: View, trace: bool) -> Result<Totals> {
    let pool = input.budget().clone();
    let mut reader = TsProgressiveReader::new(input, Pending, TsProgressiveLimits::default())?;
    let (mut video, mut audio) = (BudgetVec::new(&pool), BudgetVec::new(&pool));
    loop {
        match reader.advance(&mut WorkBudget::new(1 << 18), read.cancel)? {
            TsProgressiveStep::Yield => {}
            TsProgressiveStep::NeedInput(request) => {
                read.answer(&request, |r| reader.provide(r))?;
            }
            TsProgressiveStep::Unit(unit @ TsProgressiveUnit::Video(_)) => {
                video.try_push(unit).map_err(|error| error.error)?;
            }
            TsProgressiveStep::Unit(unit @ TsProgressiveUnit::Audio { .. }) => {
                audio.try_push(unit).map_err(|error| error.error)?;
            }
            TsProgressiveStep::End => break,
            TsProgressiveStep::Backpressure { .. } => {
                return Err("progressive reader exhausted pool".into());
            }
        }
    }
    if view == View::Clocks {
        let mut frames = BudgetVec::new(&pool);
        for unit in audio {
            if let TsProgressiveUnit::Audio { frame, .. } = unit {
                frames.try_push(frame).map_err(|error| error.error)?;
            }
        }
        return clock_records(&pool, read.cancel, &frames, trace);
    }
    let mut output = Records::new(trace);
    for (lane, units) in [(0, video), (1, audio)] {
        for unit in units.as_slice() {
            output.packet(lane, &reader.packet(unit)?)?;
        }
    }
    output.end()
}

fn compact(input: InputCursor, read: &ReadBody<'_>, trace: bool) -> Result<Totals> {
    let pool = input.budget().clone();
    let mut engine = TsEngine::progressive(input, ParseLimits::default(), true)?;
    let (mut avc, mut aac) = (None, None);
    let (mut pictures, mut frames) = (BudgetVec::new(&pool), BudgetVec::new(&pool));
    loop {
        let mut work = WorkBudget::new(1 << 18);
        match engine.poll_progressive_payload(&mut work, read.cancel)? {
            FeedStep::Configuration(config) => match config.carriage {
                Carriage::Registered(RegisteredCarriage::Avc) => {
                    avc = Some((
                        config.track,
                        AvcTsReader::new(&pool, config, AvcTsLimits::default())?,
                    ));
                }
                Carriage::Registered(RegisteredCarriage::AacAdts) => {
                    aac = Some((config.track, HlsAac::new(&pool, config, 8191)?));
                }
                _ => return Err(Error::Unsupported("standalone carriage").into()),
            },
            FeedStep::Packet {
                packet: FeedPacket::Compact(packet),
                clock,
            } => {
                let track = triergon_playback::TrackKey::Ts {
                    program: packet.program,
                    pid: packet.pid,
                };
                let mut packet = Some(packet);
                if let Some((key, reader)) = avc.as_mut()
                    && *key == track
                    && reader.push_compact(&mut packet, &clock)?
                {
                    drain_avc(reader, read.cancel, &mut pictures, false)?;
                } else if let Some((key, reader)) = aac.as_mut()
                    && *key == track
                    && reader.push_compact(&mut packet, &clock)?
                {
                    drain_aac(reader, read.cancel, &mut frames, false)?;
                } else {
                    return Err("no standalone lane consumed the compact PES".into());
                }
            }
            FeedStep::Refusal { error, .. } => return Err(error.into()),
            FeedStep::NeedInput(request) => read.answer(&request, |r| engine.provide(r))?,
            FeedStep::Yield | FeedStep::Event(_) | FeedStep::EntityEnd(_) => {}
            FeedStep::End => break,
            _ => return Err("unexpected compact TS feed step".into()),
        }
    }
    let (_, mut avc) = avc.ok_or(Error::Invalid("no AVC carriage"))?;
    let (_, mut aac) = aac.ok_or(Error::Invalid("no AAC carriage"))?;
    avc.finish();
    aac.finish();
    drain_avc(&mut avc, read.cancel, &mut pictures, true)?;
    drain_aac(&mut aac, read.cancel, &mut frames, true)?;
    if pictures.is_empty() || frames.is_empty() {
        return Err(Error::Invalid("empty standalone lane").into());
    }
    let mut output = Records::new(trace);
    for unit in pictures.as_slice() {
        output.video(
            unit.clock,
            unit.idr,
            (unit.raster.width, unit.raster.height),
            unit.source_bytes().len(),
            std::iter::once(unit.source_bytes().as_ref()),
        )?;
    }
    for frame in frames.as_slice() {
        let [dn, dd] = ratio(frame.duration)?;
        let bytes = frame.bytes.as_ref();
        output.record(1, &[1, dn, dd, bytes.len() as u64], std::iter::once(bytes))?;
    }
    output.end()
}

fn drain_avc(
    reader: &mut AvcTsReader,
    cancel: &AtomicBool,
    units: &mut BudgetVec<AvcTsAccessUnit>,
    end: bool,
) -> Result<()> {
    loop {
        match reader.step(&mut WorkBudget::new(1 << 18), cancel)? {
            AvcTsStep::Ready(unit) => units.try_push(unit).map_err(|error| error.error)?,
            AvcTsStep::Yield => {}
            AvcTsStep::NeedInput if !end => return Ok(()),
            AvcTsStep::End if end => return Ok(()),
            _ => return Err("unexpected standalone AVC completion".into()),
        }
    }
}

fn drain_aac(
    reader: &mut HlsAac,
    cancel: &AtomicBool,
    units: &mut BudgetVec<HlsAacFrame>,
    end: bool,
) -> Result<()> {
    loop {
        match reader.poll(&mut WorkBudget::new(1 << 18), cancel)? {
            AssemblyStep::Output(unit) => units.try_push(unit).map_err(|error| error.error)?,
            AssemblyStep::Yield => {}
            AssemblyStep::NeedPayload if !end => return Ok(()),
            AssemblyStep::Finished if end => return Ok(()),
            _ => return Err("unexpected standalone AAC completion".into()),
        }
    }
}
