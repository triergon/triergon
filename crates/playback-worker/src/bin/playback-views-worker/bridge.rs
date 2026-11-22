//! Access-unit framing and Matroska discovery through the public bridges.
use playback_worker::runner::{Invocation, MAX_INPUT, Mode, Result, Totals, measure};
use std::sync::atomic::AtomicBool;
use triergon_bridge::{
    aac::AacBridge,
    access::{AccessUnitLimits, AccessUnits},
    av1::{Av1Bridge, Av1BridgeSetup, Av1Input},
    bitmap::{PgsBridge, PgsStep},
    matroska::{
        MatroskaBitmapTrack, MatroskaCompactDiscovery, MatroskaDiscovery, MatroskaDiscoveryLimits,
        MatroskaDiscoveryStep, MatroskaFeedLimits, MatroskaFeedSetup, MatroskaHevcDolbyLimits,
        MatroskaHevcDolbySetup, MatroskaReplayPrefix, MatroskaTrackFeed, MatroskaVideoSeekGate,
    },
};
use triergon_core::container::{CodecId, MediaKind, PacketInfo, RandomAccess, TrackId, TrackInfo};
use triergon_core::{
    Budget, Bytes, Error, InputCursor, Rounding, SourceCapabilities, SourceId, Time, WorkBudget,
};
use triergon_framing::{
    BuildStep,
    aac::{AacAscParser, AacConfigLimits, AacSource},
    av1::{Av1ConfigLimits, Av1ObuLimits, Av1TemporalUnitLimits},
};
use triergon_harness_protocol::{
    digest::{fold, fold_bytes, signed},
    hex,
};
use triergon_isobmff::{
    BuildStatus, Next,
    config::{AacConfigurationJob, ConfigurationLimits, EntryConfiguration},
};
use triergon_playback::{
    ByteSource, Engine, Event, Format, Output, Player, PlayerPolicy, Progress, Selection, Source,
    SourceOwner, Track, TrackKey,
    mp4::{
        Mp4Configuration, Mp4Event, Mp4IndexJob, Mp4Open, Mp4Packet, OpenLimits, OpenProgress,
        PlanLimits,
    },
    probe::SelectedInput,
};

use triergon_matroska::{Configuration, Event as MkvEvent, TypedPacket};

const SOURCE: SourceId = SourceId(1);
const WINDOW: usize = 32 * 1024;
const GRANT: u64 = 65_536;
const MAX_POLLS: usize = 1_000_000;
static CANCELLATION: AtomicBool = AtomicBool::new(false);
const CANCEL: &AtomicBool = &CANCELLATION;
const ASC_LIMITS: AacConfigLimits = AacConfigLimits {
    maximum_record_bytes: 1 << 20,
    maximum_pce_elements: 64,
    maximum_comment_bytes: 255,
};
const AV1_CONFIG: Av1ConfigLimits = Av1ConfigLimits {
    maximum_record_bytes: 1 << 20,
    obu: Av1ObuLimits {
        maximum_obu_bytes: 1 << 20,
        maximum_obus: 1024,
        maximum_input_window: WINDOW,
    },
};
const AV1_PACKET: Av1TemporalUnitLimits = Av1TemporalUnitLimits {
    maximum_sample_bytes: 1 << 20,
    maximum_output_bytes: 2 << 20,
    maximum_obus: 1024,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Container {
    Mp4,
    Matroska,
}

#[derive(Clone, Copy)]
enum Operation {
    Access,
    Aac,
    Av1,
    Discovery(Discovery),
}

#[derive(Clone, Copy)]
enum Discovery {
    Full,
    Compact,
    Seek,
}

#[derive(Clone, Copy)]
struct Framing {
    container: Container,
    stream: usize,
    codec: CodecId,
    record: bool,
}

struct Records {
    trace: bool,
    digest: u64,
    count: u64,
    bytes: u64,
}

impl Records {
    fn record(&mut self, kind: u64, words: &[u64], payload: Option<&[u8]>) {
        fold(&mut self.digest, kind);
        for &word in words {
            fold(&mut self.digest, word);
        }
        if let Some(bytes) = payload {
            fold(&mut self.digest, bytes.len() as u64);
            fold_bytes(&mut self.digest, bytes);
            self.bytes += bytes.len() as u64;
        }
        self.count += 1;
        if self.trace {
            print!("{kind}");
            for word in words {
                print!(" {word}");
            }
            if let Some(bytes) = payload {
                print!(" {}", bytes.len());
                if !bytes.is_empty() {
                    print!(" {}", hex(bytes));
                }
            }
            println!();
        }
    }

    fn finish(self) -> Totals {
        Totals {
            packets: self.count,
            bytes: self.bytes,
            digest: self.digest,
            refusal: None,
        }
    }
}

pub(super) fn run(args: &[String]) -> Result<()> {
    let command = args.first().ok_or("missing bridge operation")?;
    let (prefix, name) = if command.starts_with("access-units-") {
        ("access-units-", "bridge-access-units")
    } else if command.starts_with("mp4-bridges-") {
        ("mp4-bridges-", "bridge-mp4-packets")
    } else {
        ("mkv-discovery-", "bridge-matroska-discovery")
    };
    let invocation = Invocation::parse(
        args,
        prefix,
        " CONTAINER STREAM CODEC [declared]",
        1..=4,
        true,
    )?;
    let (operation, framing) = if prefix == "mkv-discovery-" {
        let mode = match invocation.arguments {
            [mode] if mode == "full" => Discovery::Full,
            [mode] if mode == "compact" => Discovery::Compact,
            [mode] if mode == "seek" => Discovery::Seek,
            _ => return Err("expected a Matroska discovery mode (full, compact or seek)".into()),
        };
        (Operation::Discovery(mode), None)
    } else {
        let [container, stream, codec, tail @ ..] = invocation.arguments else {
            return Err("expected CONTAINER STREAM CODEC [declared]".into());
        };
        let container = match container.as_str() {
            "mp4" => Container::Mp4,
            "mkv" => Container::Matroska,
            _ => return Err("expected container mp4 or mkv".into()),
        };
        let codec = match codec.as_str() {
            "h264" => CodecId::H264,
            "hevc" => CodecId::Hevc,
            "aac" => CodecId::Aac,
            "av1" => CodecId::Av1,
            _ => return Err("unknown bridge codec".into()),
        };
        let record = match tail {
            [] => false,
            [kind]
                if kind == "declared"
                    && container == Container::Matroska
                    && codec == CodecId::Aac =>
            {
                true
            }
            _ => return Err("declared ASC is only supported for the AAC Matroska fixtures".into()),
        };
        let operation = match (prefix, container, codec, record) {
            ("access-units-", _, _, _) => Operation::Access,
            ("mp4-bridges-", Container::Mp4, CodecId::Aac, false) => Operation::Aac,
            ("mp4-bridges-", _, CodecId::Av1, false) => Operation::Av1,
            _ => return Err("unsupported packet bridge".into()),
        };
        (
            operation,
            Some(Framing {
                container,
                stream: stream.parse()?,
                codec,
                record,
            }),
        )
    };
    let raw = invocation.input(MAX_INPUT)?;
    let pool = Budget::new(128 << 20)?;
    let data = Bytes::copy_from_slice(&pool, &raw)?;
    measure(&invocation, name, None, || {
        let mut rows = Records {
            trace: invocation.mode == Mode::Trace,
            digest: 0,
            count: 0,
            bytes: 0,
        };
        let result = match operation {
            Operation::Discovery(mode) => discover(&pool, &data, mode, &mut rows),
            _ => frame(
                &pool,
                &data,
                operation,
                framing.ok_or("missing stream choice")?,
                &mut rows,
            ),
        };
        match result {
            Ok(()) => rows.record(7, &[], None),
            Err(error)
                if error.downcast_ref::<Error>().is_some_and(|error| {
                    matches!(
                        error,
                        Error::Invalid(_)
                            | Error::Unsupported(_)
                            | Error::Truncated
                            | Error::Limit { .. }
                            | Error::Policy(_)
                    )
                }) =>
            {
                if rows.trace {
                    eprintln!("{error}");
                }
                rows.record(8, &[], None);
            }
            Err(error) => return Err(error),
        }
        Ok(rows.finish())
    })
}

fn work() -> WorkBudget {
    WorkBudget::new(GRANT)
}

fn ready<T>(
    mut step: impl FnMut(&mut WorkBudget) -> triergon_core::Result<BuildStep<T>>,
) -> Result<T> {
    for _ in 0..MAX_POLLS {
        match step(&mut work())? {
            BuildStep::Ready(value) => return Ok(value),
            BuildStep::Complete => return Err("one-shot bridge finished without its result".into()),
            BuildStep::Yield => {}
        }
    }
    Err("bridge exceeded its poll bound".into())
}

fn capabilities(data: &Bytes) -> SourceCapabilities {
    SourceCapabilities {
        length: Some(data.len() as u64),
        random_access: true,
    }
}

fn source(data: &Bytes) -> Result<SourceOwner<ByteSource>> {
    Ok(SourceOwner::new(ByteSource::new(
        SOURCE,
        data.clone(),
        WINDOW,
    )?))
}

fn matroska(pool: &Budget, data: &Bytes) -> Result<Engine> {
    Ok(Engine::matroska(triergon_matroska::Reader::new(
        pool,
        SOURCE,
        capabilities(data),
        triergon_matroska::Limits::default(),
        WINDOW,
    )?))
}

fn player(
    pool: &Budget,
    data: &Bytes,
    container: Container,
    identify: bool,
) -> Result<Player<ByteSource>> {
    let source = source(data)?;
    let (engine, source) = match container {
        Container::Matroska => (matroska(pool, data)?, source),
        Container::Mp4 => {
            let mut opening = Mp4Open::new(
                SelectedInput {
                    input: InputCursor::new(pool, SOURCE, capabilities(data), WINDOW)?,
                    source,
                    format: Format::Mp4,
                },
                OpenLimits::default(),
            )?;
            while !matches!(opening.advance(&mut work(), CANCEL)?, OpenProgress::Ready) {}
            let opened = opening.finish()?;
            let mut index = Mp4IndexJob::new(&opened.movie, PlanLimits::default())?;
            while !index.poll(&mut work(), CANCEL)? {}
            let mut plan = index.finish()?;
            if identify {
                while !plan.identify_tracks(ConfigurationLimits::default(), &mut work())? {}
            }
            (Engine::mp4(opened.input, plan, 1 << 20)?, opened.source)
        }
    };
    Ok(Player::new(
        engine,
        source,
        Selection::All,
        PlayerPolicy::default(),
    )?)
}

fn outputs<S: Source>(
    player: &mut Player<S>,
    mut consume: impl FnMut(Output) -> Result<()>,
) -> Result<()> {
    let mut out = None;
    for _ in 0..MAX_POLLS {
        let progress = player.read_output(&mut out, &mut work(), CANCEL)?;
        if let Some(value) = out.take() {
            consume(value)?;
        }
        match progress {
            Progress::Ended => return Ok(()),
            Progress::Yield | Progress::Queued | Progress::NeedInput(_) => {}
            other => return Err(format!("unexpected bridge player progress {other:?}").into()),
        }
    }
    Err("bridge player exceeded its poll bound".into())
}

fn track<S: Source>(player: &Player<S>, index: usize) -> Result<Track<'_>> {
    player
        .engine()
        .and_then(|engine| engine.track(index))
        .ok_or_else(|| Error::Invalid("selected stream is absent").into())
}

fn clock(ticks: Option<i64>, base: triergon_core::TimeBase) -> Result<Option<i64>> {
    Ok(ticks
        .map(|tick| {
            base.rescale(
                tick,
                triergon_core::TimeBase::new(1, 1_000_000_000)?,
                Rounding::NearestAway,
            )
        })
        .transpose()?)
}

fn push_optional(words: &mut Vec<u64>, time: Option<i64>) {
    words.push(u64::from(time.is_some()));
    if let Some(time) = time {
        words.push(signed(time));
    }
}

fn packet_row(
    rows: &mut Records,
    stream: usize,
    packet: &impl PacketInfo,
    changed: bool,
) -> Result<()> {
    let mut words = Vec::with_capacity(7);
    words.push(stream as u64);
    push_optional(&mut words, clock(packet.pts(), packet.time_base())?);
    push_optional(&mut words, clock(packet.dts(), packet.time_base())?);
    words.push(u64::from(packet.random_access() == RandomAccess::Sync));
    words.push(u64::from(changed));
    rows.record(1, &words, Some(packet.payload()));
    Ok(())
}

struct AccessBinding {
    id: TrackId,
    revision: Option<u64>,
    session: AccessUnits,
}

fn access(pool: &Budget, track: Track<'_>, config: Framing) -> Result<AccessBinding> {
    if track.codec() != config.codec {
        return Err(Error::Invalid("selected stream codec differs").into());
    }
    let limits = AccessUnitLimits::default();
    let mut setup = if config.record {
        // The declared configuration of the authored 48 kHz stereo legacy files.
        AccessUnits::with_record(pool, &track, &[0x11, 0x90], limits)?
    } else {
        AccessUnits::for_track(pool, &track, limits)?
    };
    Ok(AccessBinding {
        id: track.id(),
        revision: None,
        session: ready(|w| setup.step(w, CANCEL))?,
    })
}

struct DescriptionTrack<'a> {
    configuration: &'a Mp4Configuration,
    entry: EntryConfiguration,
}

impl TrackInfo for DescriptionTrack<'_> {
    fn id(&self) -> TrackId {
        TrackId(u64::from(self.configuration.track.track_id))
    }

    fn kind(&self) -> MediaKind {
        triergon_isobmff::media_kind(&self.configuration.track.handler)
    }

    fn codec_tag(&self) -> &[u8] {
        &self.configuration.description.format
    }

    fn codec(&self) -> CodecId {
        self.entry.codec()
    }

    fn configuration(&self) -> &[u8] {
        self.entry.bytes(&self.configuration.description)
    }
}

fn access_description(
    pool: &Budget,
    configuration: &Mp4Configuration,
    codec: CodecId,
) -> Result<AccessUnits> {
    let mut entry = EntryConfiguration::new(
        configuration.track.handler,
        &configuration.description,
        ConfigurationLimits::default(),
        triergon_isobmff::Limits::default(),
    );
    while entry.poll(&mut work())? == BuildStatus::Yield {}
    let track = DescriptionTrack {
        configuration,
        entry,
    };
    if track.codec() != codec {
        return Err(Error::Invalid("sample description changed codec").into());
    }
    let mut setup = AccessUnits::for_track(pool, &track, AccessUnitLimits::default())?;
    ready(|w| setup.step(w, CANCEL))
}

fn aac(pool: &Budget, packet: &Mp4Packet) -> Result<AacBridge> {
    let config = &packet.configuration;
    let mut find = AacConfigurationJob::new(&config.description, ConfigurationLimits::default())?;
    let record = loop {
        match find.poll(&mut work(), CANCEL)? {
            Next::Item(value) => break value.bytes,
            Next::Yield => {}
            Next::End => return Err(Error::Invalid("sample entry has no AAC record").into()),
        }
    };
    let mut parse = AacAscParser::new(
        pool,
        AacSource {
            source: SOURCE,
            generation: config.revision,
            offset: 0,
        },
        record,
        ASC_LIMITS,
    )?;
    Ok(AacBridge::new(
        pool,
        SOURCE,
        config.track.track_id,
        config.revision,
        ready(|w| parse.step(w, CANCEL))?,
        8191,
    )?)
}

fn frame(
    pool: &Budget,
    data: &Bytes,
    operation: Operation,
    cfg: Framing,
    rows: &mut Records,
) -> Result<()> {
    let mut player = player(
        pool,
        data,
        cfg.container,
        matches!(operation, Operation::Access),
    )?;
    let mut binding = if cfg.container == Container::Mp4 && matches!(operation, Operation::Access) {
        Some(access(pool, track(&player, cfg.stream)?, cfg)?)
    } else {
        None
    };
    let mp4_id = if cfg.container == Container::Mp4 {
        Some(track(&player, cfg.stream)?.id())
    } else {
        None
    };
    let mut aac_bridge = None;
    let mut av1_bridge = None;
    outputs(&mut player, |output| {
        match output.event() {
            Event::Matroska(triergon_matroska::Event::Configuration(configuration))
                if matches!(operation, Operation::Access) =>
            {
                let track = configuration
                    .tracks()
                    .get(cfg.stream)
                    .ok_or(Error::Invalid("selected Matroska stream is absent"))?;
                binding = Some(access(pool, Track::Matroska(track), cfg)?);
            }
            _ => {}
        }
        let Some(packet) = output.packet() else {
            return Ok(());
        };
        let id = binding.as_ref().map(|b| b.id).or(mp4_id);
        let selected = if let Some(id) = id {
            packet.track() == id
        } else {
            matches!(output.event(), Event::Matroska(triergon_matroska::Event::Packet(p)) if p.packet.track == cfg.stream)
        };
        if !selected {
            return Ok(());
        }
        match operation {
            Operation::Access => {
                let binding = binding
                    .as_mut()
                    .ok_or("access units lack a track binding")?;
                let revision = match output.event() {
                    Event::Mp4(Mp4Event::Packet(p)) => p.configuration.revision,
                    Event::Matroska(triergon_matroska::Event::Packet(p)) => {
                        p.configuration.revision()
                    }
                    _ => return Err(Error::Truncated.into()),
                };
                if let Event::Mp4(Mp4Event::Packet(source)) = output.event()
                    && binding
                        .revision
                        .is_some_and(|previous| previous != revision)
                {
                    binding.session = access_description(pool, &source.configuration, cfg.codec)?;
                }
                let changed = binding.revision.replace(revision) != Some(revision);
                let mut job = binding.session.packet(&packet)?;
                let unit = ready(|w| job.step(w, CANCEL))?;
                if unit.track() != binding.id {
                    return Err("access unit did not retain its source track".into());
                }
                packet_row(rows, cfg.stream, &unit, changed)?;
            }
            Operation::Aac => {
                let Event::Mp4(Mp4Event::Packet(packet)) = output.event() else {
                    return Err(Error::Truncated.into());
                };
                let revision = packet.configuration.revision;
                if aac_bridge
                    .as_ref()
                    .is_none_or(|(bound, _)| *bound != revision)
                {
                    aac_bridge = Some((revision, aac(pool, packet)?));
                }
                let bridge = &aac_bridge.as_ref().ok_or("missing AAC bridge")?.1;
                let mut job = bridge.packet(packet.clone())?;
                let unit = ready(|w| job.step(w, CANCEL))?;
                rows.record(1, &[cfg.stream as u64], Some(unit.adts.bytes()));
            }
            Operation::Av1 => {
                let (input, revision) = match output.event() {
                    Event::Mp4(Mp4Event::Packet(p)) => (Av1Input::Mp4(p), p.configuration.revision),
                    Event::Matroska(triergon_matroska::Event::Packet(p)) => {
                        (Av1Input::Matroska(p), p.configuration.revision())
                    }
                    _ => return Err(Error::Truncated.into()),
                };
                av1(pool, input, revision, &mut av1_bridge, rows)?;
            }
            Operation::Discovery(_) => return Err("discovery dispatched as framing".into()),
        }
        Ok(())
    })?;
    if rows.count == 0 {
        return Err(Error::Invalid("selected stream has no complete packets").into());
    }
    Ok(())
}

fn av1(
    pool: &Budget,
    input: Av1Input<'_>,
    revision: u64,
    binding: &mut Option<(u64, Av1Bridge)>,
    rows: &mut Records,
) -> Result<()> {
    if binding.as_ref().is_none_or(|(bound, _)| *bound != revision) {
        let mut setup = match input {
            Av1Input::Mp4(packet) => Av1BridgeSetup::for_mp4(
                pool,
                SOURCE,
                &packet.configuration,
                AV1_CONFIG,
                AV1_PACKET,
            )?,
            Av1Input::Matroska(packet) => Av1BridgeSetup::for_matroska(
                pool,
                SOURCE,
                &packet.configuration,
                packet.packet.track,
                AV1_CONFIG,
                AV1_PACKET,
            )?,
        };
        *binding = Some((revision, ready(|w| setup.step(w, CANCEL))?));
    }
    let mut job = binding
        .as_ref()
        .ok_or("missing AV1 bridge")?
        .1
        .packet(input)?;
    let temporal = ready(|w| job.step(w, CANCEL))?;
    let mut bytes = Vec::with_capacity(temporal.byte_len());
    for segment in temporal.segments() {
        bytes.extend_from_slice(segment);
    }
    rows.record(1, &[], Some(&bytes));
    Ok(())
}

const AVC_AAC: MatroskaFeedLimits = MatroskaFeedLimits {
    avc_config: triergon_framing::avc::AvcConfigLimits {
        maximum_record_bytes: 8192,
        maximum_nals: 64,
        maximum_nal_bytes: 4096,
    },
    avc_packet: triergon_framing::avc::AvcAnnexBLimits {
        maximum_packet_bytes: 32768,
        maximum_output_bytes: 49152,
        maximum_nals: 64,
    },
    aac_config: ASC_LIMITS,
    aac_output_bytes: 8191,
};

const HEVC_DOLBY: MatroskaHevcDolbyLimits = MatroskaHevcDolbyLimits {
    hevc_config: triergon_framing::hevc::HvcConfigLimits {
        maximum_record_bytes: 8192,
        maximum_arrays: 16,
        maximum_nals: 64,
        maximum_nal_bytes: 4096,
    },
    hevc_packet: triergon_framing::hevc::HevcAnnexBLimits {
        maximum_packet_bytes: 32768,
        maximum_output_bytes: 49152,
        maximum_nals: 64,
    },
    dolby_packet: triergon_bridge::matroska::MatroskaDolbyLimits {
        maximum_packet_bytes: 16384,
        maximum_syncframes: 8,
    },
};

fn track_code(codec: CodecId) -> u64 {
    match codec {
        CodecId::H264 => 1,
        CodecId::Hevc => 2,
        CodecId::Aac => 3,
        CodecId::Ac3 => 4,
        CodecId::Eac3 => 5,
        CodecId::Pgs => 6,
        CodecId::DvbSubtitle => 7,
        CodecId::Av1 => 8,
        CodecId::VobSub => 9,
        CodecId::Vp9 => 10,
        _ => 0,
    }
}

fn discovered_fields(
    configuration: &Configuration,
    canvases: &[[u64; 2]],
    asc: &[Option<[u8; 2]>],
    rows: &mut Records,
) {
    for (index, track) in configuration.tracks().iter().enumerate() {
        let codec = track.codec();
        let dimensions = canvases
            .get(index)
            .copied()
            .unwrap_or([track.width, track.height]);
        let extra = asc
            .get(index)
            .and_then(Option::as_ref)
            .map_or_else(|| track.configuration(), <[u8; 2]>::as_slice);
        rows.record(
            5,
            &[
                index as u64,
                track_code(codec),
                dimensions[0],
                dimensions[1],
            ],
            Some(extra),
        );
    }
}

fn replay_row(rows: &mut Records, packet: &TypedPacket, payload: &[u8]) {
    let mut words = Vec::with_capacity(5);
    words.push(packet.packet.track as u64);
    push_optional(&mut words, packet.packet.pts_ns);
    push_optional(&mut words, packet.packet.dts_ns);
    words.push(u64::from(packet.packet.keyframe));
    rows.record(1, &words, Some(payload));
}

fn output_packet(output: &Output) -> Option<&TypedPacket> {
    match output.event() {
        Event::Matroska(MkvEvent::Packet(packet)) => Some(packet),
        _ => None,
    }
}

#[expect(
    clippy::default_trait_access,
    reason = "The bridge accepts display policy types from the subtitle crate."
)]
fn canvas(pool: &Budget, index: usize, number: u64, prefix: &[Output]) -> Result<[u16; 2]> {
    let configuration = prefix
        .iter()
        .find(|output| matches!(output.event(), Event::Matroska(MkvEvent::Configuration(_))))
        .ok_or(Error::Truncated)?;
    let track = MatroskaBitmapTrack::new(configuration, number)?;
    let mut pgs = PgsBridge::new(pool, track.scope(), Default::default(), Default::default())?;
    for (sequence, output) in prefix.iter().enumerate() {
        let Some(packet) = output_packet(output) else {
            continue;
        };
        if packet.packet.track != index {
            continue;
        }
        let input = track
            .packet(output, sequence as u64)?
            .ok_or("PGS prefix lost a selected packet")?;
        pgs.provide(input).map_err(|error| error.error)?;
        loop {
            match pgs.step(&mut work(), CANCEL)? {
                PgsStep::Output(picture) => {
                    let [width, height] = picture.decoded.context_canvas;
                    if width > 0 && height > 0 {
                        return Ok([width, height]);
                    }
                }
                PgsStep::NeedPacket | PgsStep::Finished => break,
                PgsStep::Yield => {}
            }
        }
    }
    Ok([0, 0])
}

fn configuration<'a>(mut events: impl Iterator<Item = &'a MkvEvent>) -> Result<Configuration> {
    events
        .find_map(|event| match event {
            MkvEvent::Configuration(config) => Some(config.clone()),
            _ => None,
        })
        .ok_or_else(|| Error::Truncated.into())
}

fn feed(pool: &Budget, configuration: &Configuration, index: usize) -> Result<MatroskaTrackFeed> {
    let mut setup = MatroskaFeedSetup::new(pool, SOURCE, configuration, index, AVC_AAC)?;
    ready(|w| setup.step(w, CANCEL))
}

fn trace_cutoff(
    rows: &Records,
    cutoff: triergon_bridge::matroska::MatroskaDiscoveryCutoff,
    outputs: usize,
) {
    if rows.trace {
        eprintln!("discovery cutoff {cutoff:?}, {outputs} buffered outputs");
    }
}

fn compact(pool: &Budget, data: &Bytes, rows: &mut Records) -> Result<()> {
    let mut discovery = MatroskaCompactDiscovery::for_av_feed(
        matroska(pool, data)?,
        source(data)?,
        Selection::All,
        PlayerPolicy::default(),
        MatroskaDiscoveryLimits::default(),
    )?;
    let mut configured = false;
    let mut finished = false;
    for _ in 0..MAX_POLLS {
        if matches!(
            discovery.poll(&mut WorkBudget::new(128), CANCEL)?,
            MatroskaDiscoveryStep::Ready(_)
        ) {
            finished = true;
            break;
        }
        if !configured {
            let config = discovery.buffered_events().find_map(|event| match event {
                MkvEvent::Configuration(config) => Some(config.clone()),
                _ => None,
            });
            if let Some(config) = config {
                for (index, track) in config.tracks().iter().enumerate() {
                    if track.codec() == CodecId::H264 {
                        feed(pool, &config, index)?;
                        discovery.confirm_stream_ready(track.number, config.revision())?;
                    }
                }
                configured = true;
            }
        }
    }
    if !finished {
        return Err("compact discovery exceeded its poll bound".into());
    }
    let mut discovered = discovery.finish()?;
    let prefix = &discovered.prefix;
    trace_cutoff(rows, discovered.cutoff, prefix.events().len());
    let config = configuration(prefix.events())?;
    let mut feeds = Vec::new();
    let mut asc = vec![None; config.tracks().len()];
    for (index, track) in config.tracks().iter().enumerate() {
        feeds.push(if track.codec() == CodecId::Aac {
            Some(feed(pool, &config, index)?)
        } else {
            None
        });
    }
    // Reconstruct the two ASC bytes from the ADTS header the feed actually emits.
    for (ordinal, event) in prefix.events().enumerate() {
        if let MkvEvent::Packet(packet) = event {
            let index = packet.packet.track;
            let track = &config.tracks()[index];
            if track.legacy_aac_profile().is_some()
                && track.configuration().is_empty()
                && asc[index].is_none()
                && let Some(feed) = &feeds[index]
            {
                let mut job = prefix
                    .feed_packet(ordinal, feed)?
                    .ok_or("legacy AAC packet not replayed")?;
                let packet = ready(|w| job.step(w, CANCEL))?;
                let header = packet
                    .payload
                    .segments()
                    .next()
                    .ok_or("missing ADTS header")?;
                let object_type = (header[2] >> 6) + 1;
                let frequency_index = (header[2] >> 2) & 15;
                let channels = ((header[2] & 1) << 2) | (header[3] >> 6);
                asc[index] = Some([
                    (object_type << 3) | (frequency_index >> 1),
                    (frequency_index << 7) | (channels << 3),
                ]);
            }
        }
    }
    discovered_fields(&config, &[], &asc, rows);
    compact_replay(prefix, &feeds, rows)?;
    outputs(&mut discovered.player, |output| {
        if let Some(packet) = output_packet(&output) {
            replay_row(rows, packet, &packet.packet.data);
        }
        Ok(())
    })
}

fn compact_replay(
    prefix: &MatroskaReplayPrefix,
    feeds: &[Option<MatroskaTrackFeed>],
    rows: &mut Records,
) -> Result<()> {
    for (ordinal, event) in prefix.events().enumerate() {
        if let MkvEvent::Packet(packet) = event {
            if let Some(feed) = feeds.get(packet.packet.track).and_then(Option::as_ref) {
                let mut job = prefix
                    .feed_packet(ordinal, feed)?
                    .ok_or("selected AAC packet not replayed")?;
                let framed = ready(|w| job.step(w, CANCEL))?;
                let mut spans = framed.payload.segments();
                let header = spans.next().ok_or("missing ADTS header")?;
                let payload = spans.next().ok_or("missing ADTS payload")?;
                if header.len() != 7 || spans.next().is_some() {
                    return Err("invalid AAC feed segments".into());
                }
                replay_row(rows, framed.source, payload);
            } else {
                replay_row(rows, packet, &packet.packet.data);
            }
        }
    }
    Ok(())
}

fn discover(pool: &Budget, data: &Bytes, mode: Discovery, rows: &mut Records) -> Result<()> {
    if matches!(mode, Discovery::Compact) {
        return compact(pool, data, rows);
    }
    let mut discovery = MatroskaDiscovery::new(
        matroska(pool, data)?,
        source(data)?,
        Selection::All,
        PlayerPolicy::default(),
        MatroskaDiscoveryLimits::default(),
    )?;
    while !matches!(
        discovery.poll(&mut work(), CANCEL)?,
        MatroskaDiscoveryStep::Ready(_)
    ) {}
    let mut discovered = discovery.finish()?;
    let prefix = discovered.prefix.as_slice();
    trace_cutoff(rows, discovered.cutoff, prefix.len());
    let config = configuration(prefix.iter().filter_map(|output| match output.event() {
        Event::Matroska(event) => Some(event),
        _ => None,
    }))?;
    let configuration_output = prefix
        .iter()
        .find(|output| matches!(output.event(), Event::Matroska(MkvEvent::Configuration(_))))
        .ok_or(Error::Truncated)?;
    let mut dimensions = Vec::with_capacity(config.tracks().len());
    for (index, track) in config.tracks().iter().enumerate() {
        dimensions.push(match track.codec() {
            CodecId::Pgs => canvas(pool, index, track.number, prefix)?.map(u64::from),
            CodecId::VobSub => {
                let track = MatroskaBitmapTrack::new(configuration_output, track.number)?;
                track
                    .vobsub_settings()?
                    .canvas
                    .unwrap_or([0, 0])
                    .map(u64::from)
            }
            _ => [track.width, track.height],
        });
    }
    discovered_fields(&config, &dimensions, &[], rows);
    if matches!(mode, Discovery::Seek) {
        return seek(&mut discovered.player, &config, rows);
    }
    let mut dolby = Vec::new();
    for (index, track) in config.tracks().iter().enumerate() {
        dolby.push(if matches!(track.codec(), CodecId::Ac3 | CodecId::Eac3) {
            let mut setup = MatroskaHevcDolbySetup::new(pool, SOURCE, &config, index, HEVC_DOLBY)?;
            Some(ready(|w| setup.step(w, CANCEL))?)
        } else {
            None
        });
    }
    let mut consume = |output: &Output| {
        if let Some(packet) = output_packet(output) {
            let index = packet.packet.track;
            if let Some(feed) = dolby.get(index).and_then(Option::as_ref) {
                let mut job = feed
                    .packet(output)?
                    .ok_or("missing selected Dolby packet")?;
                let packet = ready(|w| job.step(w, CANCEL))?;
                let mut words = vec![index as u64];
                push_optional(&mut words, packet.demux_duration_ns()?.map(|ns| ns as i64));
                rows.record(3, &words, None);
            }
            replay_row(rows, packet, &packet.packet.data);
        }
        Ok(())
    };
    for output in prefix {
        consume(output)?;
    }
    outputs(&mut discovered.player, |output| consume(&output))
}

fn seek<S: Source>(
    player: &mut Player<S>,
    configuration: &Configuration,
    rows: &mut Records,
) -> Result<()> {
    let track = configuration
        .tracks()
        .iter()
        .find(|track| track.codec() == CodecId::Hevc)
        .ok_or(Error::Invalid("missing video for discovery seek"))?;
    while !player.seek_and_select(
        TrackKey::Matroska(track.number),
        Time::new(700, 1000)?,
        Selection::All,
        &mut work(),
        CANCEL,
    )? {}
    let mut intent = None;
    let mut gate = None;
    outputs(player, |output| {
        match output.event() {
            Event::Seek(_) => intent = Some(output),
            Event::Matroska(MkvEvent::Seek(_)) => {
                let intent = intent.as_ref().ok_or("seek receipt lacks intent")?;
                gate = Some(MatroskaVideoSeekGate::new(intent, &output)?);
            }
            _ => {
                if let Some(gate) = &mut gate
                    && gate.admits(&output)?
                {
                    let packet =
                        output_packet(&output).ok_or("admitted seek output is not a packet")?;
                    replay_row(rows, packet, &packet.packet.data);
                }
            }
        }
        Ok(())
    })?;
    rows.record(
        3,
        &[u64::from(gate.is_some_and(|gate| gate.landed()))],
        None,
    );
    Ok(())
}
