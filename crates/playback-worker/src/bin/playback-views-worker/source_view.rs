//! The source clock and neutral track and packet views of a direct player.

#[cfg(not(test))]
pub(super) use worker::run;

#[cfg(test)]
pub(super) fn run(_args: &[String]) -> playback_worker::runner::Result<()> {
    Err("source-view requires the shared playback packet worker".into())
}

#[cfg(not(test))]
mod worker {
    use playback_worker::runner::{Invocation, MAX_INPUT, Mode, Result, Totals, measure};
    use std::{collections::BTreeMap, path::Path, sync::atomic::AtomicBool};
    use triergon_core::{
        Budget, Bytes, Error, InputCursor, SourceCapabilities, SourceId, Step, TimeBase,
        WorkBudget,
        container::{CodecId, MediaKind, PacketInfo, RandomAccess, TrackInfo, Trim},
    };
    use triergon_framing::nal::{Codec, RawAnnexBLimits};
    use triergon_harness_protocol::digest::{fold, fold_bytes, signed};
    use triergon_isobmff::{config::ConfigurationLimits, table::TimingRunPolicy};
    use triergon_mpegts::{
        PacketFormat, ParseLimits, clock::SourceTimePolicy, psi::DescriptorNamespace,
        stream::TraceEvent,
    };
    use triergon_playback::{
        ByteSource, ClockChoice, DemuxTimes, Engine, Event, Format, Packet, Player, PlayerPolicy,
        Progress, Selection, SourceOwner, Track, TrackKey,
        mp4::{Mp4IndexJob, Mp4Open, OpenLimits, OpenProgress, PlanLimits, ProgressiveLimits},
        probe::SelectedInput,
    };

    use triergon_raw_video::{
        RawFeedClock, RawVideoConfiguration, RawVideoProbe, RawVideoProbeLimits, RawVideoProbeStep,
        hevc_startup::{self, StartupNal},
    };

    use super::super::packet_fold::{Packet as PacketRow, Work};

    const WINDOW: usize = 32768;
    const SOURCE: SourceId = SourceId(1);
    const NEXT: SourceId = SourceId(2);
    const ENGINE_VIEW: Error = Error::Invalid("neutral engine view disagrees with its source");
    const PACKET_VIEW: Error = Error::Invalid("neutral packet clock disagrees with its source");

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Layout {
        Matroska,
        Mp4,
        Progressive,
        TimingPolicy,
        Ts(PacketFormat),
        Entities,
        Raw(Codec),
    }

    impl Layout {
        fn parse(name: &str) -> Result<Self> {
            Ok(match name {
                "matroska" => Self::Matroska,
                "mp4" => Self::Mp4,
                "mp4-progressive" => Self::Progressive,
                "mp4-timing-policy" => Self::TimingPolicy,
                "ts188" => Self::Ts(PacketFormat::Ts188),
                "m2ts192" => Self::Ts(PacketFormat::M2ts192),
                "ts-entities" => Self::Entities,
                "raw-avc" => Self::Raw(Codec::Avc),
                "raw-hevc" => Self::Raw(Codec::Hevc),
                _ => return Err("unknown source-view opener".into()),
            })
        }

        fn seekable(self) -> bool {
            matches!(self, Self::Matroska | Self::Mp4 | Self::TimingPolicy)
        }
    }

    pub(in super::super) fn run(args: &[String]) -> Result<()> {
        let invocation = Invocation::parse(
            args,
            "source-view-",
            " OPENER [clock | configuration | ENTITY-DIR NEXT-ENTITY]",
            1..=3,
            true,
        )?;
        let layout = Layout::parse(&invocation.arguments[0])?;
        let pool = Budget::new(256 << 20)?;
        let data = load(&pool, invocation.input(MAX_INPUT)?)?;
        let mut pids = Vec::new();
        let raw_config = matches!((layout, &invocation.arguments[1..]), (Layout::Raw(_), [mode])
            if mode == "configuration");
        let (clock_only, next) = match (layout, &invocation.arguments[1..]) {
            (Layout::Entities, [dir, file]) => {
                let file = Path::new(dir).join(file);
                if std::fs::metadata(&file)?.len() > MAX_INPUT {
                    return Err("entity exceeds input limit".into());
                }
                (false, Some(load(&pool, std::fs::read(file)?)?))
            }
            (Layout::Mp4 | Layout::Progressive | Layout::TimingPolicy, [mode])
                if mode == "clock" =>
            {
                (true, None)
            }
            (Layout::Ts(_), [selection]) => {
                for pid in selection.split(',') {
                    let pid = pid.parse::<u16>()?;
                    if !(1..=0x1ffe).contains(&pid) || pids.len() == 64 {
                        return Err("invalid source-view PID selection".into());
                    }
                    pids.push(pid);
                }
                (false, None)
            }
            (Layout::Raw(_), [_]) if raw_config => (false, None),
            (Layout::Entities, _) | (_, [_, ..]) => {
                return Err("invalid source-view arguments".into());
            }
            (_, []) => (false, None),
        };
        let trace = invocation.mode == Mode::Trace;
        let name = if clock_only {
            "mp4-source-clock"
        } else if raw_config {
            "raw-video-configuration"
        } else {
            "playback-source-view"
        };
        measure(&invocation, name, Some(&pool), || {
            let mut views = Views::new(trace, clock_only, &pids);
            let result = match layout {
                Layout::Raw(codec) => raw_views(&pool, &data, codec, &mut views, raw_config),
                _ => play(&pool, &data, next.as_ref(), layout, &mut views),
            };
            match result {
                Ok(()) => views.terminal(7),
                Err(error @ (ENGINE_VIEW | PACKET_VIEW)) => return Err(error.into()),
                Err(
                    error @ (Error::Invalid(_)
                    | Error::Unsupported(_)
                    | Error::Truncated
                    | Error::Limit { .. }
                    | Error::Policy(_)),
                ) => {
                    if trace {
                        eprintln!("source refusal: {error}");
                    }
                    if clock_only {
                        views.clocks.clear();
                    }
                    views.terminal(8);
                }
                Err(error) => return Err(error.into()),
            }
            Ok(views.finish())
        })
    }

    fn load(pool: &Budget, bytes: Vec<u8>) -> Result<Bytes> {
        Ok(Bytes::try_from_vec(pool, bytes).map_err(|error| error.error)?)
    }

    fn caps(data: &Bytes, random_access: bool) -> SourceCapabilities {
        SourceCapabilities {
            length: Some(data.len() as u64),
            random_access,
        }
    }

    fn play(
        pool: &Budget,
        data: &Bytes,
        next: Option<&Bytes>,
        layout: Layout,
        views: &mut Views<'_>,
    ) -> triergon_core::Result<()> {
        let cancel = AtomicBool::new(false);
        let work = || WorkBudget::new(1 << 16);
        let capabilities = caps(data, layout.seekable());
        let source = SourceOwner::new(ByteSource::new(SOURCE, data.clone(), WINDOW)?);
        let (engine, source) = match layout {
            Layout::Matroska => (
                Engine::matroska(triergon_matroska::Reader::new(
                    pool,
                    SOURCE,
                    capabilities,
                    triergon_matroska::Limits::default(),
                    WINDOW,
                )?),
                source,
            ),
            Layout::Ts(format) => (
                Engine::ts(
                    InputCursor::new(pool, SOURCE, capabilities, WINDOW)?,
                    format,
                    1,
                    ParseLimits::default(),
                    SourceTimePolicy::RelativeEpochZero,
                    DescriptorNamespace::default(),
                )?,
                source,
            ),
            Layout::Entities => (
                Engine::ts_entities(
                    InputCursor::new(pool, SOURCE, capabilities, WINDOW)?,
                    PacketFormat::Ts188,
                    1,
                    ParseLimits::default(),
                    SourceTimePolicy::RelativeEpochZero,
                    DescriptorNamespace::default(),
                )?,
                source,
            ),
            Layout::Raw(_) => return Err(Error::Invalid("raw input has no container engine")),
            Layout::Mp4 | Layout::Progressive | Layout::TimingPolicy => {
                let selected = SelectedInput {
                    input: InputCursor::new(pool, SOURCE, capabilities, WINDOW)?,
                    source,
                    format: Format::Mp4,
                };
                let limits = OpenLimits::default();
                let mut opener = match layout {
                    Layout::Progressive => Mp4Open::new_progressive(selected, limits)?,
                    Layout::TimingPolicy => Mp4Open::with_timing_policy(
                        selected,
                        limits,
                        TimingRunPolicy::BoundedPrefix,
                    )?,
                    _ => Mp4Open::new(selected, limits)?,
                };
                while !matches!(opener.advance(&mut work(), &cancel)?, OpenProgress::Ready) {}
                let opened = opener.finish()?;
                let limits = PlanLimits::default();
                let mut index = if layout == Layout::TimingPolicy {
                    triergon_playback::compat::mov::index(&opened.movie, limits)?
                } else {
                    Mp4IndexJob::new(&opened.movie, limits)?
                };
                while !index.poll(&mut work(), &cancel)? {}
                let mut plan = index.finish()?;
                while !plan.identify_tracks(ConfigurationLimits::default(), &mut work())? {}
                let engine = if layout == Layout::Progressive {
                    Engine::mp4_progressive(
                        opened.input,
                        plan,
                        1 << 20,
                        ProgressiveLimits::default(),
                    )?
                } else {
                    Engine::mp4(opened.input, plan, 1 << 20)?
                };
                (engine, opened.source)
            }
        };
        let has_mp4 = engine.mp4_plan().is_some();
        let has_matroska = engine.matroska_reader().is_some();
        if has_mp4
            != matches!(
                layout,
                Layout::Mp4 | Layout::Progressive | Layout::TimingPolicy
            )
            || has_matroska != (layout == Layout::Matroska)
        {
            return Err(ENGINE_VIEW);
        }
        let mut player =
            Player::new_direct(engine, source, Selection::All, PlayerPolicy::default())?;
        player.set_clock_view(ClockChoice::Source)?;
        if views.trace {
            eprintln!("source views: MP4 plan={has_mp4}, Matroska reader={has_matroska}");
        }
        let mut slot = None;
        let mut next = next;
        let mut catalogue = false;
        loop {
            let progress = player.read_output(&mut slot, &mut work(), &cancel)?;
            if !catalogue
                && let Some(engine) = player.engine()
                && engine.track_count() != 0
            {
                for index in 0..engine.track_count() {
                    let track = engine.track(index).ok_or(ENGINE_VIEW)?;
                    check_track(engine, index, track)?;
                    if views.selected(track.id().0) {
                        views.track(&track);
                    }
                }
                catalogue = true;
            }
            if let Some(output) = slot.take() {
                if let Some(packet) = output.packet() {
                    views.packet(&packet)?;
                } else if let Event::Ts(event) = output.event()
                    && let TraceEvent::Pes(pes) = &event.source
                    && views.selected(u64::from(pes.pid))
                {
                    let ticks = |time: Option<triergon_mpegts::clock::SourceTimestamp>| {
                        time.and_then(|time| time.ticks90)
                            .and_then(|value| i64::try_from(value).ok())
                    };
                    views.record_packet(PacketRow {
                        track: u64::from(pes.pid),
                        pts: ticks(event.source_pts),
                        dts: ticks(event.source_dts),
                        duration: 0,
                        flags: 0,
                        size: pes.payload_len,
                        payload: pes.payload(),
                    })?;
                }
            }
            match progress {
                Progress::Ended => return Ok(()),
                Progress::AwaitEntity => {
                    if let Some(data) = next.take() {
                        let mut source = Some(ByteSource::new(NEXT, data.clone(), WINDOW)?);
                        player.continue_entity(
                            NEXT,
                            caps(data, false),
                            &mut source,
                            false,
                            &mut work(),
                            &cancel,
                        )?;
                    } else {
                        player.finish_logical_source(&mut work(), &cancel)?;
                    }
                }
                Progress::Queued | Progress::NeedInput(_) | Progress::Yield => {}
                _ => return Err(Error::Invalid("unexpected direct player progress")),
            }
        }
    }

    fn check_track(engine: &Engine, index: usize, track: Track<'_>) -> triergon_core::Result<()> {
        let expected = match track {
            Track::Matroska(track) => {
                let reader = engine.matroska_reader().ok_or(ENGINE_VIEW)?;
                let native = reader.tracks().get(index).ok_or(ENGINE_VIEW)?;
                if !std::ptr::eq(native, track) {
                    return Err(ENGINE_VIEW);
                }
                TrackKey::Matroska(native.number)
            }
            Track::Mp4(track) => {
                let plan = engine.mp4_plan().ok_or(ENGINE_VIEW)?;
                let native = plan.tracks().get(index).ok_or(ENGINE_VIEW)?;
                if !std::ptr::eq(native, track) {
                    return Err(ENGINE_VIEW);
                }
                TrackKey::Mp4(native.snapshot.track_id)
            }
            Track::Ts { program, stream } => TrackKey::Ts {
                program,
                pid: stream.pid(),
            },
        };
        let id = match expected {
            TrackKey::Matroska(id) => id,
            TrackKey::Mp4(id) => u64::from(id),
            TrackKey::Ts { pid, .. } => u64::from(pid),
        };
        if expected != track.key() || id != track.id().0 {
            return Err(ENGINE_VIEW);
        }
        Ok(())
    }

    fn encoded_trim(bytes: Option<[u8; 10]>) -> Trim {
        let [a, b, c, d, e, f, g, h, _, _] = bytes.unwrap_or([0; 10]);
        let start = u32::from_le_bytes([a, b, c, d]);
        let end = u32::from_le_bytes([e, f, g, h]);
        Trim {
            start: (start != 0).then_some(i64::from(start)),
            end: (end != 0).then_some(i64::from(end)),
        }
    }

    fn check_demux_times(packet: &Packet<'_>) -> triergon_core::Result<Option<DemuxTimes>> {
        let checked = packet.demux_times();
        let expected = match packet {
            Packet::Matroska(packet) => Some(DemuxTimes {
                time_base: packet.time_base(),
                pts: packet.pts(),
                dts: packet.dts(),
                duration: packet.duration(),
                trim: packet.trim(),
            }),
            Packet::Mp4(packet) => packet.occurrence.as_ref().map(|occurrence| DemuxTimes {
                time_base: occurrence.media_clock.demux_base,
                pts: occurrence.demux.pts,
                dts: occurrence.demux.dts,
                duration: Some(occurrence.demux.duration),
                trim: encoded_trim(packet.encoded_skip_samples),
            }),
            Packet::Mp4Short(packet) => Some(DemuxTimes {
                time_base: packet.occurrence.media_clock.demux_base,
                pts: packet.occurrence.demux.pts,
                dts: packet.occurrence.demux.dts,
                duration: Some(packet.occurrence.demux.duration),
                trim: encoded_trim(packet.encoded_skip_samples),
            }),
        };
        if checked != expected {
            return Err(PACKET_VIEW);
        }
        Ok(checked)
    }

    fn raw_views(
        pool: &Budget,
        data: &Bytes,
        codec: Codec,
        views: &mut Views<'_>,
        raw_config: bool,
    ) -> triergon_core::Result<()> {
        let cancel = AtomicBool::new(false);
        let work = || WorkBudget::new(1 << 16);
        let mut probe = RawVideoProbe::new(
            pool,
            SOURCE,
            caps(data, false),
            ByteSource::new(SOURCE, data.clone(), WINDOW)?,
            RawVideoProbeLimits::default(),
            RawAnnexBLimits::default(),
        )?;
        loop {
            if let RawVideoProbeStep::Detected(detected) = probe.advance(&mut work(), &cancel)? {
                if detected != codec {
                    return Err(Error::Invalid("unexpected Annex B codec"));
                }
                break;
            }
        }
        let mut feed = probe.finish()?.into_feed(4096, RawFeedClock::Unspecified)?;
        let mut first = true;
        loop {
            match feed.poll(&mut work(), &cancel)? {
                Step::Ready(packet) => {
                    if raw_config {
                        if packet.configuration_changed {
                            config_record(&packet, views)?;
                            first = false;
                        }
                        continue;
                    }
                    if first {
                        let rate = match &packet.configuration {
                            Some(config) => frame_rate(config, views.trace)?,
                            None => None,
                        };
                        let (num, den) =
                            rate.map_or((0, 1), |rate| (rate.numerator(), rate.denominator()));
                        views.field(&[4, num, den]);
                        first = false;
                    }
                    views.field(&[3, signed(packet.queue_timestamp_ns())]);
                }
                Step::End(_) if raw_config && first => {
                    return Err(Error::Invalid("no active raw configuration"));
                }
                Step::End(_) => return Ok(()),
                Step::NeedInput(_) | Step::Yield => {}
            }
        }
    }

    fn config_record(
        packet: &triergon_raw_video::RawVideoFeedPacket,
        views: &mut Views<'_>,
    ) -> triergon_core::Result<()> {
        let config = packet
            .configuration
            .as_ref()
            .ok_or(Error::Invalid("configuration change has no value"))?;
        let revision = packet
            .configuration_revision
            .ok_or(Error::Invalid("configuration change has no revision"))?;
        let (codec, timing) = match config.codec {
            Codec::Avc => (
                0,
                config.avc_timing.map(|timing| {
                    [
                        1,
                        u64::from(timing.num_units_in_tick),
                        u64::from(timing.time_scale),
                        u64::from(timing.fixed_frame_rate_flag),
                        0,
                    ]
                }),
            ),
            Codec::Hevc => (
                1,
                config.hevc_timing()?.map(|timing| {
                    [
                        1,
                        u64::from(timing.num_units_in_tick),
                        u64::from(timing.time_scale),
                        u64::from(timing.poc_proportional_to_timing_flag),
                        timing
                            .num_ticks_poc_diff_one_minus1
                            .map_or(0, |ticks| u64::from(ticks) + 1),
                    ]
                }),
            ),
        };
        let [present, units, scale, flag, ticks] = timing.unwrap_or([0; 5]);
        views.field(&[
            4,
            codec,
            revision,
            u64::from(config.width),
            u64::from(config.height),
            present,
            units,
            scale,
            flag,
            ticks,
        ]);
        for (slot, nal) in config.parameter_sets().enumerate() {
            views.field(&[5, slot as u64]);
            views.string(nal);
            if views.trace {
                println!("{}", triergon_harness_protocol::hex(nal));
            }
        }
        if config.codec == Codec::Hevc {
            let chain = hevc_startup::startup_chain(config.parameter_sets().map(AsRef::as_ref))?;
            let mut complete = false;
            for nal in packet.unit.nals() {
                let header = nal.facts().header;
                if let Some((slot, payload)) = hevc_startup::parameter_set_payload(nal.bytes())
                    && [chain.vps, chain.sps, chain.pps][slot] != payload
                {
                    return Err(Error::Invalid("startup parameter-set payload changed"));
                }
                if !matches!(
                    header,
                    triergon_framing::nal::NalHeader::Hevc {
                        nal_unit_type: 0..=31,
                        ..
                    }
                ) {
                    continue;
                }
                if let StartupNal::Picture {
                    first: true,
                    random_access,
                    pps,
                } = hevc_startup::startup_nal(nal.bytes(), header)?
                {
                    if pps != chain.pps_id {
                        return Err(Error::Invalid("startup slice selects another PPS"));
                    }
                    views.field(&[
                        6,
                        u64::from(pps),
                        u64::from(chain.reorder),
                        u64::from(random_access),
                    ]);
                    complete = true;
                    break;
                }
            }
            if !complete {
                return Err(Error::Invalid(
                    "configuration change has no HEVC base picture",
                ));
            }
        }
        Ok(())
    }

    fn frame_rate(
        config: &RawVideoConfiguration,
        trace: bool,
    ) -> triergon_core::Result<Option<TimeBase>> {
        let ratio = match config.codec {
            Codec::Avc => {
                if trace {
                    eprintln!("AVC source VUI: {:?}", config.avc_timing);
                }
                config.avc_timing.map(|timing| {
                    (
                        u64::from(timing.time_scale),
                        2 * u64::from(timing.num_units_in_tick),
                    )
                })
            }
            Codec::Hevc => {
                let timing = config.hevc_timing()?;
                if trace {
                    eprintln!("HEVC source VUI: {timing:?}");
                }
                timing.map(|value| {
                    let ticks = value
                        .num_ticks_poc_diff_one_minus1
                        .map_or(1, |n| u64::from(n) + 1);
                    (
                        u64::from(value.time_scale),
                        ticks * u64::from(value.num_units_in_tick),
                    )
                })
            }
        };
        ratio.map(|(num, den)| TimeBase::new(num, den)).transpose()
    }

    type ClockRow = (u64, u64, Option<i64>, Option<i64>, Option<i64>);

    struct Views<'a> {
        work: Work,
        trace: bool,
        pids: &'a [u16],
        clock_only: bool,
        end: u64,
        clocks: BTreeMap<u64, Vec<ClockRow>>,
    }

    impl<'a> Views<'a> {
        fn new(trace: bool, clock_only: bool, pids: &'a [u16]) -> Self {
            Self {
                work: Work::default(),
                trace,
                pids,
                clock_only,
                end: 0,
                clocks: BTreeMap::new(),
            }
        }

        fn selected(&self, track: u64) -> bool {
            self.pids.is_empty() || u16::try_from(track).is_ok_and(|pid| self.pids.contains(&pid))
        }

        fn field(&mut self, words: &[u64]) {
            for &word in words {
                self.word(word);
            }
            self.work.packets += 1;
            if self.trace {
                println!(
                    "{}",
                    words
                        .iter()
                        .map(u64::to_string)
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
        }

        fn word(&mut self, word: u64) {
            fold(&mut self.work.digest, word);
        }

        fn string(&mut self, value: &[u8]) {
            self.word(value.len() as u64);
            if !value.is_empty() {
                fold_bytes(&mut self.work.digest, value);
            }
            self.work.bytes += value.len() as u64;
        }

        fn track(&mut self, track: &Track<'_>) {
            if self.clock_only {
                return;
            }
            let kind = match track.kind() {
                MediaKind::Unknown => 0,
                MediaKind::Video => 1,
                MediaKind::Audio => 2,
                MediaKind::Subtitle => 3,
                MediaKind::Data => 4,
            };
            let codec = codec_name(track.codec());
            let flag = |value: Option<bool>| value.map_or(0, |on| 1 + u64::from(on));
            let default = flag(track.default_track());
            let forced = flag(track.forced());
            let language = track.language();
            for word in [2, track.id().0, kind] {
                self.word(word);
            }
            self.string(codec.as_bytes());
            self.string(track.codec_tag());
            self.string(track.configuration());
            self.word(u64::from(language.is_some()));
            if let Some(language) = language {
                self.string(language.as_bytes());
            }
            self.word(default);
            self.word(forced);
            self.work.packets += 1;
            if self.trace {
                println!(
                    "2 {} {kind} {codec} {} {} {:?} {default} {forced}",
                    track.id().0,
                    hex(track.codec_tag()),
                    hex(track.configuration()),
                    language,
                );
                eprintln!("track {} colour: {:?}", track.id().0, track.colour());
            }
            if let Track::Ts { stream, .. } = track {
                for descriptor in stream.descriptors().filter_map(std::result::Result::ok) {
                    let Ok(languages) = descriptor.languages() else {
                        continue;
                    };
                    for language in languages {
                        self.field(&[
                            4,
                            track.id().0,
                            u64::from(language.language_code[0]),
                            u64::from(language.language_code[1]),
                            u64::from(language.language_code[2]),
                            u64::from(language.audio_type),
                        ]);
                    }
                }
            }
        }

        fn packet(&mut self, packet: &Packet<'_>) -> triergon_core::Result<()> {
            let base = packet.time_base();
            let demux = check_demux_times(packet)?;
            if self.clock_only {
                self.clocks.entry(packet.track().0).or_default().push((
                    base.numerator(),
                    base.denominator(),
                    packet.pts(),
                    packet.dts(),
                    packet.duration(),
                ));
                return Ok(());
            }
            if self.trace {
                eprintln!(
                    "packet {} clock={}/{} pts={:?} demux={demux:?} trim={:?}",
                    packet.track().0,
                    base.numerator(),
                    base.denominator(),
                    packet.pts_time()?,
                    packet.trim(),
                );
            }
            self.record_packet(PacketRow {
                track: packet.track().0,
                pts: packet.pts(),
                dts: packet.dts(),
                duration: packet
                    .duration()
                    .and_then(|value| u64::try_from(value).ok())
                    .unwrap_or(0),
                flags: u64::from(packet.random_access() == RandomAccess::Sync),
                size: packet.payload().len(),
                payload: [packet.payload()],
            })
        }

        fn record_packet<'payload>(
            &mut self,
            packet: PacketRow<impl IntoIterator<Item = &'payload [u8]>>,
        ) -> triergon_core::Result<()> {
            if self.trace {
                println!(
                    "1 {} {} {} {} {} {}",
                    packet.track,
                    time(packet.pts),
                    time(packet.dts),
                    packet.duration,
                    packet.flags,
                    packet.size,
                );
            }
            self.word(1);
            self.work
                .packet(packet)
                .map_err(|_| Error::Invalid("packet payload size"))
        }

        fn terminal(&mut self, kind: u64) {
            self.end = kind;
            if !self.clock_only {
                self.word(kind);
                self.work.packets += 1;
                if self.trace {
                    println!("{kind}");
                }
            }
        }

        fn finish(mut self) -> Totals {
            if self.clock_only {
                for (id, rows) in std::mem::take(&mut self.clocks) {
                    for (num, den, pts, dts, duration) in rows {
                        let row = format!(
                            "3 {id} {num} {den} {} {} {}",
                            time(pts),
                            time(dts),
                            time(duration)
                        );
                        if self.trace {
                            println!("{row}");
                        }
                        self.string(row.as_bytes());
                        self.work.packets += 1;
                    }
                }
                self.word(self.end);
                self.work.packets += 1;
                if self.trace {
                    println!("{}", self.end);
                }
            }
            Totals {
                packets: self.work.packets,
                bytes: self.work.bytes,
                digest: self.work.digest,
                refusal: None,
            }
        }
    }

    fn time(value: Option<i64>) -> String {
        value.map_or_else(|| "-".into(), |ticks| ticks.to_string())
    }

    fn hex(bytes: &[u8]) -> String {
        if bytes.is_empty() {
            return "-".into();
        }
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn codec_name(codec: CodecId) -> &'static str {
        match codec {
            CodecId::Mpeg2Video => "mpeg2video",
            CodecId::H264 => "h264",
            CodecId::Hevc => "hevc",
            CodecId::Av1 => "av1",
            CodecId::Vp9 => "vp9",
            CodecId::Theora => "theora",
            CodecId::RawVideo => "rawvideo",
            CodecId::Aac => "aac",
            CodecId::AacLatm => "aac_latm",
            CodecId::Ac3 => "ac3",
            CodecId::Eac3 => "eac3",
            CodecId::Dts => "dts",
            CodecId::Vorbis => "vorbis",
            CodecId::Opus => "opus",
            CodecId::Flac => "flac",
            CodecId::Speex => "speex",
            CodecId::MpegAudio => "mpeg_audio",
            CodecId::Pcm => "pcm",
            CodecId::Utf8Text => "subrip",
            CodecId::Ass => "ass",
            CodecId::Ssa => "ssa",
            CodecId::WebVtt => "webvtt",
            CodecId::Tx3g => "mov_text",
            CodecId::VobSub => "dvd_subtitle",
            CodecId::DvbSubtitle => "dvb_subtitle",
            CodecId::DvbTeletext => "dvb_teletext",
            CodecId::Eia608 => "eia_608",
            CodecId::Pgs => "hdmv_pgs_subtitle",
            _ => "other",
        }
    }
}
