//! Packet outcomes of the public player, delivery and demux entry points.

#[cfg(not(test))]
pub(super) use worker::run;
#[cfg(test)]
pub(super) fn run(_: &[String]) -> playback_worker::runner::Result<()> {
    Err("player views require the playback packet fold".into())
}

#[cfg(not(test))]
mod worker {
    use super::super::packet_fold::{Packet, Work};
    use playback_worker::runner::{Invocation, MAX_INPUT, Mode, Result, Totals, measure};
    use std::{path::Path, sync::atomic::AtomicBool};
    use triergon_core::{
        Budget, BudgetVec, Bytes, Error, InputCursor, Rounding, SourceCapabilities, SourceId, Time,
        TimeBase, WorkBudget,
        container::{MediaKind, PacketInfo, RandomAccess, TrackInfo},
    };
    use triergon_harness_protocol::digest::{fold, signed};
    use triergon_isobmff::{
        CursorStatus,
        compat::mov::{
            edits::{EncodedEditContext, EncodedEditJob, EncodedEditLimits, EncodedEditPlan},
            encoded::{EncodedSelection, EncodedStreamJob},
        },
    };
    use triergon_mpegts::{
        PacketFormat, ParseLimits,
        clock::SourceTimePolicy,
        compat::pes::{PrivateAudioCarriage, RegisteredCarriage},
        psi::{DescriptorNamespace, DvbNamespaceEvidence},
    };
    use triergon_playback::{
        ByteSource, ClockChoice, Delivery, Engine, Event, Format, Output, Player, PlayerPolicy,
        Progress, Selection, SourceOwner, TrackKey,
        compat::{
            mov,
            pes::{
                self, Carriage, PidSelection, PrivateAudioSelection,
                startup::{Startup, StartupStep},
            },
        },
        mp4::{
            MetadataReadScope, Movie, MovieSeekOutcome, Mp4Catalogue, Mp4CatalogueMode,
            Mp4DemuxSeekRequest, Mp4Event, Mp4IndexJob, Mp4Open, Mp4RangeOpen, Mp4SyncSeekRequest,
            OpenLimits, OpenProgress, OpenedMp4, PlanLimits, ProgressiveLimits,
        },
        probe::SelectedInput,
        pump::{Feed, FeedToken, Pump, PumpStep, Sink, Submission},
    };

    const WINDOW: usize = 32768;
    const MAX_PACKET: usize = 1 << 20;
    const SOURCE: SourceId = SourceId(1);
    const NEXT: SourceId = SourceId(2);
    const NAMESPACE: DescriptorNamespace =
        DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile);

    #[derive(Clone, Copy)]
    struct Geometry {
        seekable: bool,
        known: bool,
        window: usize,
        chunk: usize,
        grant: u64,
    }

    impl Geometry {
        const FILE: Self = Self {
            seekable: true,
            known: true,
            window: WINDOW,
            chunk: WINDOW,
            grant: 65536,
        };
        const PIPE: Self = Self {
            seekable: false,
            ..Self::FILE
        };

        fn work(self) -> WorkBudget {
            WorkBudget::new(self.grant)
        }
    }

    #[derive(Clone, Copy)]
    enum Pathway {
        Direct,
        Fifo,
        Ready,
        Pump,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Index {
        Source,
        Demux,
        AdaptiveDemux,
        Encoded,
        ForwardEncoded,
        Recovery,
    }

    struct Rows {
        work: Work,
        trace: bool,
        details: bool,
        source_clock: bool,
        landing: Option<u32>,
        planned_position: Option<u64>,
    }

    impl Rows {
        fn field(&mut self, kind: u64, value: u64) {
            fold(&mut self.work.digest, kind);
            fold(&mut self.work.digest, value);
            self.work.packets += 1;
            if self.trace {
                println!("{kind} {value}");
            }
        }

        fn packet<'a>(&mut self, row: Packet<impl IntoIterator<Item = &'a [u8]>>) -> Result<()> {
            if self.trace {
                let time = |value: Option<i64>| value.map_or_else(|| "-".into(), |n| n.to_string());
                println!(
                    "{} {} {} {} {} {}",
                    row.track,
                    time(row.pts),
                    time(row.dts),
                    row.duration,
                    row.flags,
                    row.size
                );
            }
            self.work.packet(row)
        }

        fn output(&mut self, output: &Output) -> Result<()> {
            match output.event() {
                Event::Mp4(Mp4Event::Packet(packet)) => {
                    let sample = &packet.source.sample;
                    let payload = packet.source.payload.as_ref();
                    let track = packet.configuration.track.track_id;
                    if self.landing == Some(track) {
                        let position = self.planned_position.take().unwrap_or(sample.range.start);
                        self.field(4, position);
                        self.landing = None;
                    }
                    if let Some(native) = &packet.occurrence {
                        self.mov_packet(native, payload, packet.encoded_skip_samples)?;
                    } else if self.source_clock {
                        self.packet(Packet {
                            track: u64::from(track),
                            pts: sample.cts_ticks.map(i64::try_from).transpose()?,
                            dts: sample.dts_ticks.map(i64::try_from).transpose()?,
                            duration: u64::from(sample.duration_ticks),
                            flags: u64::from(sample.is_container_sync()),
                            size: payload.len(),
                            payload: [payload],
                        })?;
                        if self.details {
                            self.location(
                                sample.range.start,
                                packet.configuration.track.base.require()?,
                                None,
                            );
                        }
                    }
                }
                Event::Mp4(Mp4Event::ShortPacket(packet)) => {
                    self.mov_packet(
                        &packet.occurrence,
                        &packet.prefix.payload,
                        packet.encoded_skip_samples,
                    )?;
                }
                Event::Mp4(Mp4Event::EmptyPacket(packet)) => {
                    self.packet(Packet {
                        track: u64::from(packet.boundary.track_id),
                        pts: Some(packet.pts),
                        dts: Some(packet.dts),
                        duration: packet.duration.max(0) as u64,
                        flags: u64::from(packet.flags),
                        size: 0,
                        payload: [],
                    })?;
                    if self.details {
                        self.location(packet.position, packet.time_base, None);
                    }
                }
                Event::Matroska(_) => {
                    if let Some(packet) = output.packet() {
                        self.packet(Packet {
                            track: packet.track().0,
                            pts: packet.pts(),
                            dts: packet.dts(),
                            duration: packet.duration().unwrap_or(0).max(0) as u64,
                            flags: u64::from(packet.random_access() == RandomAccess::Sync),
                            size: packet.payload().len(),
                            payload: [packet.payload()],
                        })?;
                    }
                }
                Event::DemuxPes { packet, clock } => {
                    self.packet(Packet {
                        track: u64::from(packet.pid),
                        pts: clock.pts90.map(i64::try_from).transpose()?,
                        dts: clock.dts90.map(i64::try_from).transpose()?,
                        duration: packet.duration90_raw.max(0) as u64,
                        flags: u64::from(packet.flags),
                        size: packet.payload_len,
                        payload: packet.payload(),
                    })?;
                }
                Event::PesRefusal { error, .. } => return Err((*error).into()),
                _ => {}
            }
            Ok(())
        }

        fn mov_packet(
            &mut self,
            native: &triergon_isobmff::compat::mov::packets::PacketOccurrence,
            payload: &[u8],
            skip: Option<[u8; 10]>,
        ) -> Result<()> {
            let demux = &native.demux;
            self.packet(Packet {
                track: u64::from(native.track_id),
                pts: demux.pts,
                dts: demux.dts,
                duration: demux.duration.max(0) as u64,
                flags: u64::from(demux.flags),
                size: payload.len(),
                payload: [payload],
            })?;
            if self.details {
                self.location(
                    demux.source_range.start,
                    native.media_clock.demux_base,
                    skip,
                );
            }
            Ok(())
        }

        fn location(&mut self, offset: u64, base: TimeBase, skip: Option<[u8; 10]>) {
            self.work.location(offset, base);
            self.work.skip_samples(skip);
            if self.trace {
                print!(
                    "source {offset} {}/{} ",
                    base.numerator(),
                    base.denominator()
                );
                if let Some([a, b, c, d, e, f, g, h, i, j]) = skip {
                    println!(
                        "{} {} {i} {j}",
                        u32::from_le_bytes([a, b, c, d]),
                        u32::from_le_bytes([e, f, g, h])
                    );
                } else {
                    println!("-");
                }
            }
        }

        fn totals(self) -> Totals {
            Totals {
                packets: self.work.packets,
                bytes: self.work.bytes,
                digest: self.work.digest,
                refusal: None,
            }
        }
    }

    fn selected_input(
        pool: &Budget,
        raw: &Bytes,
        id: SourceId,
        geometry: Geometry,
        format: Format,
    ) -> triergon_core::Result<SelectedInput<ByteSource>> {
        let capabilities = capabilities(raw, geometry);
        Ok(SelectedInput {
            input: InputCursor::new(pool, id, capabilities, geometry.window)?,
            source: SourceOwner::new(ByteSource::new(id, raw.clone(), geometry.chunk)?),
            format,
        })
    }

    fn capabilities(raw: &Bytes, geometry: Geometry) -> SourceCapabilities {
        SourceCapabilities {
            length: geometry.known.then_some(raw.len() as u64),
            random_access: geometry.seekable,
        }
    }

    fn open_mp4(
        pool: &Budget,
        raw: &Bytes,
        id: SourceId,
        geometry: Geometry,
        index: Index,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<OpenedMp4<ByteSource>> {
        let selected = selected_input(pool, raw, id, geometry, Format::Mp4)?;
        let limits = OpenLimits::default();
        let mut open = if index == Index::Recovery {
            Mp4Open::new_recovery(selected, limits)?
        } else if index == Index::Source && !geometry.seekable {
            Mp4Open::new_progressive(selected, limits)?
        } else {
            Mp4Open::new(selected, limits)?
        };
        while !matches!(
            open.advance(&mut geometry.work(), cancel)?,
            OpenProgress::Ready
        ) {}
        open.finish()
    }

    fn edits(
        pool: &Budget,
        opened: &mut OpenedMp4<ByteSource>,
        geometry: Geometry,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<BudgetVec<Option<EncodedEditPlan>>> {
        let mut edits = BudgetVec::new(pool);
        edits.try_reserve_exact(opened.movie.tracks().len())?;
        for track in opened.movie.tracks() {
            let table = track.table();
            let kind = triergon_isobmff::media_kind(&track.header().handler);
            let plan = if let Some(edit) = track.edits().filter(|_| {
                track.fragments().is_empty()
                    && !matches!(kind, MediaKind::Data | MediaKind::Subtitle)
            }) {
                let [description] = table.descriptions() else {
                    return Err(Error::Unsupported(
                        "encoded edit requires one sample description",
                    ));
                };
                let header = track.header();
                let mut config = EncodedStreamJob::new_range_playback(
                    header,
                    table,
                    &opened.input,
                    EncodedSelection {
                        major_brand: opened.movie.major_brand(),
                        description: description.index,
                        limits: triergon_isobmff::config::ConfigurationLimits::default(),
                    },
                )?;
                while config.poll(&mut opened.input, &mut geometry.work(), cancel)?
                    != CursorStatus::Ready
                {}
                let configs = [config.take(&mut opened.input, cancel)?];
                let mut job = EncodedEditJob::new_range_playback(
                    EncodedEditContext {
                        table,
                        edits: edit,
                        stream: &configs[0],
                        media_base: header.time_base()?,
                        movie_base: opened.movie.header().time_base()?,
                    },
                    &configs,
                    &opened.input,
                    EncodedEditLimits::default(),
                )?;
                while job.poll(&mut opened.input, &mut geometry.work(), cancel)?
                    != CursorStatus::Ready
                {}
                Some(job.take(&mut opened.input, cancel)?)
            } else {
                None
            };
            edits.try_push(plan).map_err(|err| err.error)?;
        }
        Ok(edits)
    }

    fn mp4_plan(
        opened: &OpenedMp4<ByteSource>,
        geometry: Geometry,
        index: Index,
        plans: &[Option<EncodedEditPlan>],
        cancel: &AtomicBool,
    ) -> triergon_core::Result<(triergon_playback::mp4::PreparedMp4, Mp4CatalogueMode)> {
        let limits = PlanLimits::default();
        let front = matches!(
            opened.movie.read_scope(),
            MetadataReadScope::FrontMetadataBeforeMdat(_)
        );
        let mut job = match index {
            Index::Source => Mp4IndexJob::new(&opened.movie, limits)?,
            Index::Encoded => mov::index_encoded_range(&opened.movie, plans, limits)?,
            Index::ForwardEncoded if !plans.is_empty() => {
                mov::forward_encoded_edits(&opened.movie, plans, limits)?
            }
            Index::AdaptiveDemux if !plans.is_empty() => {
                mov::index_encoded_range(&opened.movie, plans, limits)?
            }
            Index::Demux | Index::AdaptiveDemux | Index::ForwardEncoded | Index::Recovery
                if front =>
            {
                mov::index_front(&opened.movie, limits)?
            }
            Index::Demux | Index::AdaptiveDemux | Index::ForwardEncoded | Index::Recovery => {
                mov::index(&opened.movie, limits)?
            }
        };
        while !job.poll(&mut geometry.work(), cancel)? {}
        let mode = if !geometry.seekable && index == Index::Source {
            Mp4CatalogueMode::Progressive(ProgressiveLimits::default())
        } else if front {
            Mp4CatalogueMode::Front(ProgressiveLimits::default())
        } else {
            Mp4CatalogueMode::Indexed
        };
        Ok((job.finish()?, mode))
    }

    fn mp4_engine(
        opened: OpenedMp4<ByteSource>,
        plan: triergon_playback::mp4::PreparedMp4,
        mode: Mp4CatalogueMode,
    ) -> triergon_core::Result<(Engine, SourceOwner<ByteSource>, Movie)> {
        let engine = match mode {
            Mp4CatalogueMode::Indexed => Engine::mp4(opened.input, plan, MAX_PACKET)?,
            Mp4CatalogueMode::Front(limits) => {
                mov::open_front(opened.input, plan, MAX_PACKET, limits)?
            }
            Mp4CatalogueMode::Progressive(limits) => {
                Engine::mp4_progressive(opened.input, plan, MAX_PACKET, limits)?
            }
        };
        Ok((engine, opened.source, opened.movie))
    }

    fn new_player(
        engine: Engine,
        source: SourceOwner<ByteSource>,
        selection: Selection,
        pathway: Pathway,
    ) -> triergon_core::Result<Player<ByteSource>> {
        let policy = PlayerPolicy::default();
        match pathway {
            Pathway::Direct => Player::new_direct(engine, source, selection, policy),
            _ => Player::new(engine, source, selection, policy),
        }
    }

    fn next_output(
        player: &mut Player<ByteSource>,
        pathway: Pathway,
        geometry: Geometry,
        cancel: &AtomicBool,
    ) -> Result<Option<Output>> {
        loop {
            let mut slot = None;
            let mut work = geometry.work();
            let progress = match pathway {
                Pathway::Direct => player.read_output(&mut slot, &mut work, cancel)?,
                Pathway::Fifo => {
                    let progress = player.advance(&mut work, cancel)?;
                    slot = player.pop();
                    progress
                }
                Pathway::Ready => {
                    let progress = player.advance_ready(&mut work, cancel)?;
                    slot = player.pop();
                    progress
                }
                Pathway::Pump => return Err("pump output is delivered by its sink".into()),
            };
            if let Some(output) = slot {
                if player.may_deliver(&output) && output.delivery() != Delivery::Unselected {
                    return Ok(Some(output));
                }
                continue;
            }
            match progress {
                Progress::Ended => return Ok(None),
                Progress::Queued | Progress::NeedInput(_) | Progress::Yield => {}
                _ => return Err(format!("unexpected player progress {progress:?}").into()),
            }
        }
    }

    fn drain(
        player: &mut Player<ByteSource>,
        pathway: Pathway,
        geometry: Geometry,
        cancel: &AtomicBool,
        rows: &mut Rows,
    ) -> Result<()> {
        if matches!(pathway, Pathway::Pump) {
            return Err("pump requires ownership of the player".into());
        }
        while let Some(output) = next_output(player, pathway, geometry, cancel)? {
            rows.output(&output)?;
        }
        Ok(())
    }

    struct Recorder<'a> {
        rows: &'a mut Rows,
        completed: Option<FeedToken>,
        failure: Option<Box<dyn std::error::Error>>,
        track: Option<TrackKey>,
    }

    impl Sink for Recorder<'_> {
        fn submit(&mut self, feed: Feed) -> Submission {
            if (self.track.is_none() || feed.output().event().track() == self.track)
                && let Err(error) = self.rows.output(feed.output())
            {
                self.failure = Some(error);
                return Submission::Failed {
                    feed,
                    error: Error::Invalid("packet fold"),
                };
            }
            let token = feed.token().clone();
            self.completed = Some(token.clone());
            Submission::Accepted(token)
        }
        fn completion(&mut self) -> Option<FeedToken> {
            self.completed.take()
        }
        fn pause(&mut self, _: bool) -> triergon_core::Result<()> {
            Ok(())
        }
        fn reset(&mut self, _: &triergon_playback::SeekIntent) -> triergon_core::Result<()> {
            Ok(())
        }
        fn stop(&mut self) {}
    }

    fn deliver(
        mut player: Player<ByteSource>,
        pathway: Pathway,
        geometry: Geometry,
        cancel: &AtomicBool,
        track: Option<TrackKey>,
        rows: &mut Rows,
    ) -> Result<()> {
        if matches!(pathway, Pathway::Pump) {
            let mut pump = Pump::new(
                player,
                Recorder {
                    rows,
                    completed: None,
                    failure: None,
                    track,
                },
                1,
            )?;
            loop {
                let step = pump.step(&mut geometry.work(), cancel);
                if let Some(error) = pump.sink_mut().and_then(|sink| sink.failure.take()) {
                    return Err(error);
                }
                if step? == PumpStep::Ended {
                    return Ok(());
                }
            }
        }
        while let Some(output) = next_output(&mut player, pathway, geometry, cancel)? {
            if track.is_none() || output.event().track() == track {
                rows.output(&output)?;
            }
        }
        Ok(())
    }

    fn matroska(
        pool: &Budget,
        bytes: &Bytes,
        geometry: Geometry,
    ) -> triergon_core::Result<(Engine, SourceOwner<ByteSource>)> {
        let reader = triergon_matroska::Reader::new(
            pool,
            SOURCE,
            capabilities(bytes, geometry),
            triergon_matroska::Limits::default(),
            geometry.window,
        )?;
        let source = SourceOwner::new(ByteSource::new(SOURCE, bytes.clone(), geometry.chunk)?);
        Ok((Engine::matroska(reader), source))
    }

    fn first_track(player: &Player<ByteSource>, kind: MediaKind) -> Option<TrackKey> {
        let engine = player.engine()?;
        (0..engine.track_count())
            .filter_map(|i| engine.track(i))
            .find(|track| track.kind() == kind)
            .map(|track| track.key())
    }

    fn delivery_matroska(
        pool: &Budget,
        bytes: &Bytes,
        pathway: Pathway,
        capsule: (u8, i64),
        cancel: &AtomicBool,
        rows: &mut Rows,
    ) -> Result<()> {
        let geometry = Geometry {
            window: 4096,
            chunk: 4096,
            ..Geometry::FILE
        };
        let (engine, source) = matroska(pool, bytes, geometry)?;
        let mut player = new_player(engine, source, Selection::All, pathway)?;
        let seed_path = if matches!(pathway, Pathway::Pump) {
            Pathway::Fifo
        } else {
            pathway
        };
        let mut video = None;
        loop {
            let Some(output) = next_output(&mut player, seed_path, geometry, cancel)? else {
                break;
            };
            video = video.or_else(|| first_track(&player, MediaKind::Video));
            if let Some(track) = video {
                if capsule.0 == 0 {
                    if output.event().track() == Some(track) && output.packet().is_some() {
                        rows.output(&output)?;
                    }
                    break;
                }
                if capsule.0 == 1
                    && output.event().track() == Some(track)
                    && output.packet().is_some()
                {
                    break;
                }
            }
        }
        let video = video.ok_or(Error::Invalid("Matroska has no video track"))?;
        if capsule.0 != 0 {
            let target = Time::new(i128::from(capsule.1), 1000)?;
            while !player.seek_and_select(
                video,
                target,
                Selection::All,
                &mut geometry.work(),
                cancel,
            )? {}
        }
        deliver(player, pathway, geometry, cancel, Some(video), rows)
    }

    fn uses_encoded_range(movie: &Movie) -> bool {
        movie.trex().is_empty()
            && movie
                .tracks()
                .iter()
                .any(|track| track.edits().is_some() && track.table().has_composition_offsets())
    }

    fn mp4_view(
        pool: &Budget,
        raw: &Bytes,
        index: Index,
        geometry: Geometry,
        pathway: Pathway,
        cancel: &AtomicBool,
        rows: &mut Rows,
    ) -> Result<()> {
        let mut opened = open_mp4(pool, raw, SOURCE, geometry, index, cancel)?;
        let needs_edits = matches!(index, Index::Encoded | Index::ForwardEncoded)
            && opened.movie.trex().is_empty()
            || (index == Index::AdaptiveDemux && uses_encoded_range(&opened.movie));
        let plans = if needs_edits {
            edits(pool, &mut opened, geometry, cancel)?
        } else {
            BudgetVec::new(pool)
        };
        let (plan, mode) = mp4_plan(&opened, geometry, index, plans.as_slice(), cancel)?;
        let (engine, source, _) = mp4_engine(opened, plan, mode)?;
        let player = new_player(engine, source, Selection::All, pathway)?;
        deliver(player, pathway, geometry, cancel, None, rows)
    }

    fn catalogue(
        pool: &Budget,
        raw: &Bytes,
        next: &Bytes,
        count: u64,
        explicit: bool,
        cancel: &AtomicBool,
        rows: &mut Rows,
    ) -> Result<()> {
        let g = Geometry::FILE;
        let opened = open_mp4(pool, raw, SOURCE, g, Index::Demux, cancel)?;
        let (plan, mode) = mp4_plan(&opened, g, Index::Demux, &[], cancel)?;
        let (engine, source, _) = mp4_engine(opened, plan, mode)?;
        let mut player = new_player(engine, source, Selection::All, Pathway::Direct)?;
        prefix(&mut player, count, g, cancel, rows)?;
        let opened = open_mp4(pool, next, NEXT, g, Index::Demux, cancel)?;
        let (plan, mode) = mp4_plan(&opened, g, Index::Demux, &[], cancel)?;
        let mut next = Some(Mp4Catalogue::new(opened, plan, MAX_PACKET, mode)?);
        let retired = if explicit {
            let prepared = player
                .prepare_mp4_catalogue(&mut next, Selection::All, &mut g.work(), cancel)?
                .ok_or("catalogue preparation yielded with work available")?;
            Some(prepared.commit()?)
        } else {
            player.replace_mp4_catalogue(&mut next, Selection::All, &mut g.work(), cancel)?
        };
        drop(retired.ok_or("catalogue replacement yielded with work available")?);
        rows.field(3, count);
        drain(&mut player, Pathway::Direct, g, cancel, rows)
    }

    fn prefix(
        player: &mut Player<ByteSource>,
        count: u64,
        g: Geometry,
        cancel: &AtomicBool,
        rows: &mut Rows,
    ) -> Result<()> {
        let start = rows.work.packets;
        while rows.work.packets - start < count {
            let output =
                next_output(player, Pathway::Direct, g, cancel)?.ok_or(Error::Truncated)?;
            rows.output(&output)?;
        }
        Ok(())
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Seek {
        Sync,
        Demux,
        ProgressiveSync,
        ProgressiveDemux,
        Movie,
        Open,
        Discard,
        OpusRoll,
    }

    fn first_packet(player: &mut Player<ByteSource>, cancel: &AtomicBool) -> Result<()> {
        while let Some(output) = next_output(player, Pathway::Direct, Geometry::FILE, cancel)? {
            if output.packet().is_some() {
                return Ok(());
            }
        }
        Err(Error::Truncated.into())
    }

    fn seek(
        pool: &Budget,
        raw: &Bytes,
        mode: Seek,
        video: u32,
        target: i64,
        cancel: &AtomicBool,
        rows: &mut Rows,
    ) -> Result<()> {
        if matches!(mode, Seek::Open | Seek::Discard | Seek::OpusRoll) {
            return seek_range(pool, raw, mode, video, target, cancel, rows);
        }
        let g = Geometry::FILE;
        let progressive = matches!(mode, Seek::ProgressiveSync | Seek::ProgressiveDemux);
        let source = matches!(mode, Seek::Sync | Seek::ProgressiveSync);
        let index = if source {
            Index::Source
        } else {
            Index::AdaptiveDemux
        };
        let front = if progressive { Geometry::PIPE } else { g };
        let mut opened = open_mp4(pool, raw, SOURCE, front, index, cancel)?;
        let edit_plans = if !source && !progressive && uses_encoded_range(&opened.movie) {
            Some(edits(pool, &mut opened, front, cancel)?)
        } else {
            None
        };
        let plans = edit_plans.as_ref().map_or(&[][..], BudgetVec::as_slice);
        let (plan, catalogue_mode) = mp4_plan(&opened, front, index, plans, cancel)?;
        let base = plan.track(video)?.snapshot.base.require()?;
        let (engine, owner, movie) = mp4_engine(opened, plan, catalogue_mode)?;
        let mut player = new_player(engine, owner, Selection::All, Pathway::Direct)?;
        if !source {
            player.set_clock_view(ClockChoice::Demux)?;
        }
        first_packet(&mut player, cancel)?;
        let time = Time::from_ticks(i128::from(target), base)?;
        let sync = Mp4SyncSeekRequest {
            video,
            source_time: time,
            audio_preroll: Time::new(0, 1)?,
            maximum_scanned_rows: 2_000_000,
        };
        let demux = Mp4DemuxSeekRequest {
            video,
            timestamp: target,
            limits: triergon_isobmff::compat::mov::seek::SeekLimits::default(),
        };
        if progressive {
            let opened = open_mp4(pool, raw, SOURCE, g, index, cancel)?;
            let (plan, layout) = mp4_plan(&opened, g, index, &[], cancel)?;
            let (candidate, movie) =
                Mp4Catalogue::new_with_movie(opened, plan, MAX_PACKET, layout)?;
            let mut next = Some(candidate);
            let prepared = next.as_ref().ok_or("seek candidate missing")?;
            let retired = if source {
                let mut job =
                    player.prepare_mp4_progressive_sync_seek(prepared, sync, Selection::All)?;
                while !job.poll(&mut g.work(), cancel)? {}
                let command = job.finish()?;
                player.apply_mp4_progressive_sync_seek(
                    &mut next,
                    &command,
                    &mut g.work(),
                    cancel,
                )?
            } else {
                let mut job = player.prepare_mp4_progressive_demux_seek(
                    prepared,
                    &movie,
                    demux,
                    Selection::All,
                )?;
                while !job.poll(&mut g.work(), cancel)? {}
                let command = job.finish()?;
                rows.planned_position = Some(u64::try_from(command.receipt().selected.position)?);
                player.apply_mp4_progressive_demux_seek(
                    &mut next,
                    &command,
                    &mut g.work(),
                    cancel,
                )?
            };
            drop(retired.ok_or("progressive seek yielded with work available")?);
        } else if source {
            let mut job = player.prepare_mp4_sync_seek(sync, Selection::All)?;
            while !job.poll(&mut g.work(), cancel)? {}
            let command = job.finish()?;
            while !player.apply_mp4_sync_seek(&command, &mut g.work(), cancel)? {}
        } else if mode == Seek::Movie {
            let plan = player
                .engine()
                .and_then(Engine::mp4_plan)
                .ok_or("missing movie plan")?;
            let mut job = plan.movie_seek(video, time, 128)?;
            while !job.poll(&mut g.work(), cancel)? {}
            let command = job.finish()?;
            loop {
                match player.apply_movie_seek(&command, Selection::All, &mut g.work(), cancel)? {
                    MovieSeekOutcome::Applied(_) => break,
                    MovieSeekOutcome::Yield => {}
                    MovieSeekOutcome::Empty(_) => {
                        rows.field(3, 0);
                        return Ok(());
                    }
                    MovieSeekOutcome::Outside { .. } => {
                        rows.field(3, 1);
                        return Ok(());
                    }
                }
            }
        } else {
            let mut job = player.prepare_mp4_demux_seek(&movie, demux, Selection::All)?;
            while !job.poll(&mut g.work(), cancel)? {}
            let command = job.finish()?;
            rows.planned_position = Some(u64::try_from(command.receipt().selected.position)?);
            while !player.apply_mp4_demux_seek(&command, &mut g.work(), cancel)? {}
        }
        rows.source_clock = source;
        rows.landing = Some(video);
        drain(&mut player, Pathway::Direct, g, cancel, rows)
    }

    fn seek_range(
        pool: &Budget,
        raw: &Bytes,
        mode: Seek,
        video: u32,
        target: i64,
        cancel: &AtomicBool,
        rows: &mut Rows,
    ) -> Result<()> {
        let g = Geometry::FILE;
        let mut opened = open_mp4(pool, raw, SOURCE, g, Index::Demux, cancel)?;
        let plans = edits(pool, &mut opened, g, cancel)?;
        let mut job =
            Mp4RangeOpen::for_media(&opened.movie, Some(plans.as_slice()), PlanLimits::default())?;
        while !job.poll(&mut g.work(), cancel)? {}
        let mut player = job.finish(
            opened.input,
            opened.source,
            MAX_PACKET,
            Selection::All,
            PlayerPolicy::default(),
        )?;
        rows.field(3, signed(player.duration_us()));
        let mut slot = None;
        if mode != Seek::Open {
            loop {
                let progress = player.read_output(&mut slot, &mut g.work(), cancel)?;
                if slot.take().is_some_and(|o| o.packet().is_some()) {
                    break;
                }
                if matches!(progress, Progress::Ended) {
                    return Err(Error::Truncated.into());
                }
            }
            if mode == Seek::Discard {
                let released = player.discard_seek_checkpoints();
                if !released {
                    return Err("checkpoint-discard control had no checkpoints to release".into());
                }
            }
            let request = Mp4DemuxSeekRequest {
                video,
                timestamp: target,
                limits: triergon_isobmff::compat::mov::seek::SeekLimits::default(),
            };
            let mut job = if mode == Seek::OpusRoll {
                let audio = opened
                    .movie
                    .tracks()
                    .iter()
                    .find(|track| track.header().handler == *b"soun")
                    .ok_or(Error::Invalid("expected Opus track"))?
                    .header()
                    .track_id;
                player.prepare_seek_with_opus_recovery(request, audio)?
            } else {
                player.prepare_seek(request)?
            };
            while !job.poll(&mut g.work(), cancel)? {}
            let mut command = job.finish()?;
            if mode == Seek::OpusRoll {
                let recovery = command
                    .receipt()
                    .opus_recovery
                    .ok_or("missing Opus roll receipt")?;
                let sample_clock = TimeBase::new(1, 48_000)?;
                let samples = recovery
                    .preroll
                    .to_i64_ticks(sample_clock, Rounding::TowardZero)?;
                if Time::from_ticks(i128::from(samples), sample_clock)? != recovery.preroll {
                    return Err("Opus preroll is off the sample clock".into());
                }
                rows.field(5, recovery.sync_sample);
                rows.field(5, recovery.first_sample);
                rows.field(5, signed(samples));
            }
            rows.planned_position = Some(u64::try_from(command.receipt().selected.position)?);
            while !player.apply_seek(&mut command, &mut g.work(), cancel)? {}
            rows.landing = Some(video);
        }
        loop {
            let progress = player.read_output(&mut slot, &mut g.work(), cancel)?;
            if let Some(output) = slot.take() {
                rows.output(&output)?;
            }
            match progress {
                Progress::Ended => return Ok(()),
                Progress::Queued | Progress::NeedInput(_) | Progress::Yield => {}
                _ => return Err(format!("unexpected range progress {progress:?}").into()),
            }
        }
    }

    #[derive(Clone, Copy)]
    enum Container {
        Mk,
        Mp4,
        Ts,
    }

    impl Container {
        fn track(self, number: u64) -> Result<TrackKey> {
            Ok(match self {
                Self::Mk => TrackKey::Matroska(number),
                Self::Mp4 => TrackKey::Mp4(number.try_into()?),
                Self::Ts => TrackKey::Ts {
                    program: 1,
                    pid: number.try_into()?,
                },
            })
        }

        fn selection(self, argument: &str) -> Result<Selection> {
            let [mut video, mut audio, mut subtitle] = [None; 3];
            for field in argument.split(',') {
                let (kind, number) = field.split_once('=').ok_or("expected KIND=TRACK")?;
                let slot = match kind {
                    "video" => &mut video,
                    "audio" => &mut audio,
                    "subtitle" => &mut subtitle,
                    _ => return Err("unknown selection kind".into()),
                };
                if slot.replace(self.track(number.parse()?)?).is_some() {
                    return Err("duplicate selection kind".into());
                }
            }
            Ok(Selection::Tracks {
                video,
                audio,
                subtitle,
            })
        }
    }

    #[derive(Clone, Copy)]
    struct Switch {
        count: u64,
        timestamp: i64,
        video: TrackKey,
        selection: Selection,
    }

    fn select(
        pool: &Budget,
        raw: &Bytes,
        container: Container,
        selection: Selection,
        switch: Option<Switch>,
        cancel: &AtomicBool,
        rows: &mut Rows,
    ) -> Result<()> {
        let g = Geometry::FILE;
        let (engine, owner) = match container {
            Container::Mk => matroska(pool, raw, g)?,
            Container::Mp4 => {
                let index = Index::AdaptiveDemux;
                let mut opened = open_mp4(pool, raw, SOURCE, g, index, cancel)?;
                let edit_plans = if uses_encoded_range(&opened.movie) {
                    Some(edits(pool, &mut opened, g, cancel)?)
                } else {
                    None
                };
                let plans = edit_plans.as_ref().map_or(&[][..], BudgetVec::as_slice);
                let (plan, mode) = mp4_plan(&opened, g, index, plans, cancel)?;
                let (engine, owner, _) = mp4_engine(opened, plan, mode)?;
                (engine, owner)
            }
            Container::Ts => {
                let selected =
                    selected_input(pool, raw, SOURCE, g, Format::Ts(PacketFormat::Ts188))?;
                let engine = pes::open_segment(
                    selected.input,
                    ParseLimits::default(),
                    SourceTimePolicy::Unanchored,
                    NAMESPACE,
                )?;
                (engine, selected.source)
            }
        };
        let mut player = new_player(engine, owner, selection, Pathway::Direct)?;
        if !matches!(container, Container::Mk) {
            player.set_clock_view(ClockChoice::Demux)?;
        }
        if let Some(switch) = switch {
            prefix(&mut player, switch.count, g, cancel, rows)?;
            let base = match switch.video {
                TrackKey::Mp4(id) => player
                    .engine()
                    .and_then(Engine::mp4_plan)
                    .ok_or("missing MP4 plan")?
                    .track(id)?
                    .snapshot
                    .base
                    .require()?,
                TrackKey::Matroska(_) => TimeBase::new(1, 1000)?,
                TrackKey::Ts { .. } => {
                    return Err("TS track switches are not in this operation".into());
                }
            };
            let target = Time::from_ticks(i128::from(switch.timestamp), base)?;
            while !player.seek_and_select(
                switch.video,
                target,
                switch.selection,
                &mut g.work(),
                cancel,
            )? {}
        }
        drain(&mut player, Pathway::Direct, g, cancel, rows)
    }

    #[derive(Clone, Copy)]
    enum PesOpen {
        Segment,
        PrivateAudio,
        Startup,
    }

    fn ts_selection(argument: &str) -> Result<(PacketFormat, Vec<PidSelection>)> {
        let (format, selections) = argument
            .split_once(':')
            .ok_or("expected FORMAT:PID=CARRIAGE")?;
        let format = match format {
            "ts188" => PacketFormat::Ts188,
            "m2ts192" => PacketFormat::M2ts192,
            _ => return Err("unknown TS format".into()),
        };
        let selections = selections
            .split(',')
            .map(|pair| {
                use Carriage::{PrivateAudio, Registered};
                use PrivateAudioCarriage::{
                    DvbAc3Tag, DvbEac3Tag, PrivateType81Ac3, PrivateType87Eac3,
                };
                use RegisteredCarriage::{AacAdts, AacLatm, Avc, Hevc};
                let (pid, carriage) = pair.split_once('=').ok_or("expected PID=CARRIAGE")?;
                let carriage = match carriage {
                    "avc" => Registered(Avc),
                    "hevc" => Registered(Hevc),
                    "aac" => Registered(AacAdts),
                    "latm" => Registered(AacLatm),
                    "audio" => Carriage::AudioFromPmt,
                    "dvb-ac3" => PrivateAudio(DvbAc3Tag),
                    "dvb-eac3" => PrivateAudio(DvbEac3Tag),
                    "ac3" => PrivateAudio(PrivateType81Ac3),
                    "eac3" => PrivateAudio(PrivateType87Eac3),
                    "dvbsub" => Carriage::DvbSubtitles,
                    _ => return Err("unknown TS carriage".into()),
                };
                Ok(PidSelection {
                    pid: pid.parse()?,
                    carriage,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((format, selections))
    }

    fn ts_open(
        pool: &Budget,
        raw: &Bytes,
        mode: PesOpen,
        format: PacketFormat,
        pids: &[PidSelection],
        cancel: &AtomicBool,
        rows: &mut Rows,
    ) -> Result<()> {
        let mut geometry = if format == PacketFormat::Ts188 {
            Geometry::FILE
        } else {
            Geometry::PIPE
        };
        if matches!(mode, PesOpen::Startup) {
            geometry.chunk = geometry.chunk.max(raw.len());
            geometry.window = geometry.chunk;
        }
        let selected = selected_input(pool, raw, SOURCE, geometry, Format::Ts(format))?;
        let limits = ParseLimits::default();
        let policy = SourceTimePolicy::Unanchored;
        let mut player = match (mode, pids) {
            (
                PesOpen::Startup,
                [
                    PidSelection {
                        pid,
                        carriage: Carriage::Registered(carriage),
                    },
                ],
            ) => {
                let mut startup = Startup::new(selected)?;
                while startup.advance(&mut geometry.work(), cancel)? != StartupStep::Ready {}
                startup.finish()?.into_player(
                    1,
                    limits,
                    policy,
                    *pid,
                    *carriage,
                    Selection::All,
                    PlayerPolicy::default(),
                )?
            }
            (
                PesOpen::PrivateAudio,
                [
                    PidSelection {
                        pid,
                        carriage: Carriage::PrivateAudio(carriage),
                    },
                ],
            ) if format == PacketFormat::Ts188 => {
                let engine = pes::open_private_audio(
                    selected.input,
                    1,
                    limits,
                    policy,
                    PrivateAudioSelection {
                        pid: *pid,
                        carriage: *carriage,
                    },
                )?;
                new_player(engine, selected.source, Selection::All, Pathway::Direct)?
            }
            (PesOpen::Segment, _) if format == PacketFormat::Ts188 => {
                let engine = pes::open_segment(selected.input, limits, policy, NAMESPACE)?;
                new_player(engine, selected.source, Selection::All, Pathway::Direct)?
            }
            _ => return Err("PES opener cannot express the requested carriage selection".into()),
        };
        player.set_clock_view(ClockChoice::Demux)?;
        while let Some(output) = next_output(&mut player, Pathway::Direct, geometry, cancel)? {
            if let Some(TrackKey::Ts { pid, .. }) = output.event().track()
                && pids.iter().any(|selection| selection.pid == pid)
            {
                rows.output(&output)?;
            }
        }
        Ok(())
    }

    enum Operation {
        Seek(Seek, u32, i64),
        Catalogue(Bytes, u64, bool),
        DeliveryMp4(Pathway, Index, Geometry),
        DeliveryMatroska(Pathway, u8, i64),
        Select(Container, Selection, Option<Switch>),
        Index(Index, Geometry),
        PesOpen(PesOpen, PacketFormat, Vec<PidSelection>),
    }

    fn layout(argument: &str) -> Result<(Index, Geometry)> {
        Ok(match argument {
            "regular" => (Index::Demux, Geometry::FILE),
            "progressive" => (Index::Demux, Geometry::PIPE),
            "progressive-edited" => (Index::ForwardEncoded, Geometry::PIPE),
            "progressive-edited-recovery" => (Index::Recovery, Geometry::PIPE),
            _ => return Err("unknown MP4 layout".into()),
        })
    }

    fn path(argument: &str) -> Result<Pathway> {
        Ok(match argument {
            "direct" => Pathway::Direct,
            "fifo" => Pathway::Fifo,
            "ready" => Pathway::Ready,
            "pump" => Pathway::Pump,
            _ => return Err("unknown delivery pathway".into()),
        })
    }

    fn load(pool: &Budget, data: Vec<u8>) -> Result<Bytes> {
        Ok(Bytes::try_from_vec(pool, data).map_err(|error| error.error)?)
    }

    fn prepare(
        pool: &Budget,
        prefix: &str,
        args: &[String],
        bytes: &mut Bytes,
    ) -> Result<Operation> {
        Ok(match (prefix, args) {
            ("mp4-seek-", [mode, video, target]) => {
                let mode = match mode.as_str() {
                    "open" => Seek::Open,
                    "sync" => Seek::Sync,
                    "demux" => Seek::Demux,
                    "progressive-sync" => Seek::ProgressiveSync,
                    "progressive-demux" => Seek::ProgressiveDemux,
                    "movie" => Seek::Movie,
                    "discard" => Seek::Discard,
                    "opus-roll" => Seek::OpusRoll,
                    _ => return Err("unknown seek mode".into()),
                };
                Operation::Seek(mode, video.parse()?, target.parse()?)
            }
            ("catalogue-", [mode, count, dir, file]) => {
                let file = Path::new(dir).join(file);
                if std::fs::metadata(&file)?.len() > MAX_INPUT {
                    return Err("replacement file exceeds input limit".into());
                }
                let explicit = match mode.as_str() {
                    "prepare" => true,
                    "replace" => false,
                    _ => return Err("unknown catalogue operation".into()),
                };
                Operation::Catalogue(load(pool, std::fs::read(file)?)?, count.parse()?, explicit)
            }
            ("delivery-", [pathway, argument]) if argument == "matroska" => {
                let [tag @ .., operation, a, b, c, d, e, f, g, h] = &bytes[..17.min(bytes.len())]
                else {
                    return Err("incomplete Matroska capsule".into());
                };
                if tag != b"SMVCL001" || *operation > 2 {
                    return Err("invalid Matroska capsule".into());
                }
                let (operation, target) = (
                    *operation,
                    i64::from_be_bytes([*a, *b, *c, *d, *e, *f, *g, *h]),
                );
                *bytes = bytes.slice(17..bytes.len())?;
                Operation::DeliveryMatroska(path(pathway)?, operation, target)
            }
            ("delivery-", [pathway, argument]) => {
                let (index, geom) = layout(argument)?;
                let index = if geom.seekable {
                    Index::AdaptiveDemux
                } else {
                    index
                };
                Operation::DeliveryMp4(path(pathway)?, index, geom)
            }
            ("select-", [container, tracks, rest @ ..]) => {
                let container = match container.as_str() {
                    "matroska" => Container::Mk,
                    "mp4" => Container::Mp4,
                    "ts188" => Container::Ts,
                    _ => return Err("unknown selection container".into()),
                };
                let switch = match rest {
                    [] => None,
                    [count, video, target, after] => Some(Switch {
                        count: count.parse()?,
                        timestamp: target.parse()?,
                        video: container.track(video.parse()?)?,
                        selection: container.selection(after)?,
                    }),
                    _ => return Err("expected COUNT VIDEO TARGET TRACKS".into()),
                };
                Operation::Select(container, container.selection(tracks)?, switch)
            }
            ("mp4-index-", [mode, argument]) => {
                let (index, geometry) = layout(argument)?;
                let index = match mode.as_str() {
                    "index" => index,
                    "index-encoded-range" if geometry.seekable => Index::Encoded,
                    _ => return Err("unknown MP4 index operation or geometry".into()),
                };
                Operation::Index(index, geometry)
            }
            ("mp4-index-", [mode, transport, length, chunk, grant]) if mode == "terminal" => {
                let seekable = match transport.as_str() {
                    "file" => true,
                    "pipe" => false,
                    _ => return Err("expected file or pipe".into()),
                };
                let known = match length.as_str() {
                    "known" => true,
                    "unknown" => false,
                    _ => return Err("expected known or unknown length".into()),
                };
                let geometry = Geometry {
                    seekable,
                    known,
                    window: 4096,
                    chunk: chunk.parse()?,
                    grant: grant.parse()?,
                };
                if geometry.chunk == 0 || geometry.grant == 0 {
                    return Err("terminal quantum must be positive".into());
                }
                Operation::Index(Index::Recovery, geometry)
            }
            ("ts-pes-open-", [mode, argument]) => {
                let mode = match mode.as_str() {
                    "segment" => PesOpen::Segment,
                    "private-audio" => PesOpen::PrivateAudio,
                    "startup" => PesOpen::Startup,
                    _ => return Err("unknown PES opener".into()),
                };
                let (format, selections) = ts_selection(argument)?;
                Operation::PesOpen(mode, format, selections)
            }
            _ => return Err("invalid player view arguments".into()),
        })
    }

    pub(in super::super) fn run(args: &[String]) -> Result<()> {
        let (prefix, name) = [
            ("mp4-seek-", "playback-mp4-seek"),
            ("catalogue-", "playback-catalogue-replacement"),
            ("delivery-", "playback-delivery"),
            ("select-", "playback-track-selection"),
            ("mp4-index-", "playback-mov-index"),
            ("ts-pes-open-", "playback-ts-open"),
        ]
        .into_iter()
        .find(|(prefix, _)| args[0].starts_with(prefix))
        .ok_or("unknown player view")?;
        let invocation = Invocation::parse(args, prefix, " ARG...", 2..=6, true)?;
        let pool = Budget::new(256 << 20)?;
        let mut bytes = load(&pool, invocation.input(MAX_INPUT)?)?;
        let op = prepare(&pool, prefix, invocation.arguments, &mut bytes)?;
        let cancel = AtomicBool::new(false);
        let trace = invocation.mode == Mode::Trace;
        let details = matches!(op, Operation::Seek(..))
            || matches!(op, Operation::Index(index, geometry) if index == Index::ForwardEncoded || index == Index::Recovery || geometry.window == 4096)
            || matches!(
                op,
                Operation::DeliveryMp4(_, Index::ForwardEncoded | Index::Recovery, _)
            );
        measure(&invocation, name, Some(&pool), || {
            let mut rows = Rows {
                work: Work::default(),
                trace,
                details,
                source_clock: false,
                landing: None,
                planned_position: None,
            };
            let result = match &op {
                Operation::Seek(mode, track, target) => {
                    seek(&pool, &bytes, *mode, *track, *target, &cancel, &mut rows)
                }
                Operation::Catalogue(next, count, explicit) => {
                    catalogue(&pool, &bytes, next, *count, *explicit, &cancel, &mut rows)
                }
                Operation::DeliveryMp4(path, index, geometry) => {
                    mp4_view(&pool, &bytes, *index, *geometry, *path, &cancel, &mut rows)
                }
                Operation::DeliveryMatroska(path, operation, target) => delivery_matroska(
                    &pool,
                    &bytes,
                    *path,
                    (*operation, *target),
                    &cancel,
                    &mut rows,
                ),
                Operation::Select(container, selection, switch) => select(
                    &pool, &bytes, *container, *selection, *switch, &cancel, &mut rows,
                ),
                Operation::Index(index, geometry) => mp4_view(
                    &pool,
                    &bytes,
                    *index,
                    *geometry,
                    Pathway::Direct,
                    &cancel,
                    &mut rows,
                ),
                Operation::PesOpen(mode, format, pids) => {
                    ts_open(&pool, &bytes, *mode, *format, pids, &cancel, &mut rows)
                }
            };
            match result {
                Ok(()) => rows.field(7, 0),
                Err(error)
                    if error.downcast_ref::<Error>().is_some_and(|error| {
                        matches!(
                            error,
                            Error::Invalid(_)
                                | Error::Unsupported(_)
                                | Error::Limit { .. }
                                | Error::Truncated
                                | Error::Policy(_)
                        )
                    }) =>
                {
                    if trace {
                        eprintln!("player refusal: {error}");
                    }
                    rows.field(8, 0);
                }
                Err(error) => return Err(error),
            }
            Ok(rows.totals())
        })
    }
}
