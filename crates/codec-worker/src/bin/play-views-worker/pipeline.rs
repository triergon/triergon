use std::{
    fs::File,
    io::{Cursor, Read},
    path::Path,
    sync::atomic::AtomicBool,
};
use triergon::{
    AnyReader, Backpressure, Blocking, Budget, BudgetVec, Bytes, CodecId, ContainerReader, Error,
    PacketInfo, Result, Rounding, SourceCapabilities, SourceId, TimeBase, TrackInfo, WorkBudget,
    access::{AccessUnitLimits, AccessUnits},
    framing::BuildStep,
    isobmff::config::ConfigurationLimits,
    matroska,
    playback::{
        ByteSource, Engine, Format, Output, Player, PlayerPolicy, Progress, Selection, Source,
        mp4::{Mp4IndexJob, Mp4Open, OpenLimits, OpenProgress, PlanLimits},
        probe::{Probe, ProbeProgress, SelectedInput},
    },
    render::ass::{Fonts, Frame, Options as ScriptOptions, Renderer, Script, Settings, Size},
    session::{
        AudioSession, Canvas, Discard, PcmBlock, Published, QueueLimits, SessionState,
        VideoSession, audio_codec, audio_configuration, audio_decoder_in, video_codec,
        video_decoder_in,
    },
};

pub(super) type PlayResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const WINDOW: usize = 64 * 1024;
const WIDTH: usize = 640;
const HEIGHT: usize = 360;

fn read_file(pool: &Budget, path: &Path) -> Result<Bytes> {
    let mut file = File::open(path)?;
    let len = usize::try_from(file.metadata()?.len()).map_err(|_| Error::Overflow)?;
    let mut bytes = BudgetVec::try_filled(pool, 0, len)?;
    file.read_exact(&mut bytes)?;
    bytes.freeze().map_err(|refused| refused.error)
}

pub(super) type TransportOpen =
    fn(&Budget, SourceId, SourceCapabilities, Format) -> Result<AnyReader>;

#[derive(Clone, Copy)]
pub(super) struct Options {
    pub(super) canvas: Canvas,
    pub(super) video: Option<usize>,
    pub(super) audio: Option<usize>,
    pub(super) transport: TransportOpen,
}

pub(super) trait Observer {
    fn packet(&mut self, index: usize, packet: &impl PacketInfo) -> Result<()>;
    fn picture(&mut self, picture: &Published) -> Result<()>;
    fn pcm(&mut self, block: &PcmBlock) -> Result<()>;
    fn subtitle(&mut self, now_ms: i64, frame: Frame<'_>) -> Result<()>;
}

fn play_source<S: Source>(
    selected: SelectedInput<S>,
    pool: &Budget,
    cancel: &AtomicBool,
) -> Result<Player<S>> {
    let (engine, source) = match selected.format {
        Format::Matroska => {
            let reader = matroska::Reader::new(
                pool,
                selected.input.source(),
                selected.input.capabilities(),
                matroska::Limits::default(),
                WINDOW,
            )?;
            (Engine::matroska(reader), selected.source)
        }
        Format::Mp4 => {
            let mut open = Mp4Open::new(selected, OpenLimits::default())?;
            while !matches!(open.advance(&mut work(), cancel)?, OpenProgress::Ready) {}
            let opened = open.finish()?;
            let mut index = Mp4IndexJob::new(&opened.movie, PlanLimits::default())?;
            while !index.poll(&mut work(), cancel)? {}
            let mut plan = index.finish()?;
            while !plan.identify_tracks(ConfigurationLimits::default(), &mut work())? {}
            (Engine::mp4(opened.input, plan, 1 << 20)?, opened.source)
        }
        Format::Ts(_) => return Err(Error::Invalid("transport needs an access-unit reader")),
    };
    Player::new_direct(engine, source, Selection::All, PlayerPolicy::default())
}

fn work() -> WorkBudget {
    WorkBudget::new(1 << 20)
}

fn ready<T>(mut step: impl FnMut(&mut WorkBudget) -> Result<BuildStep<T>>) -> Result<T> {
    loop {
        match step(&mut work())? {
            BuildStep::Ready(value) => return Ok(value),
            BuildStep::Yield => {}
            BuildStep::Complete => return Err(Error::Invalid("framing produced no access unit")),
        }
    }
}

fn next_output(player: &mut Player<impl Source>, cancel: &AtomicBool) -> Result<Option<Output>> {
    let mut output = None;
    loop {
        match player.read_output(&mut output, &mut work(), cancel)? {
            Progress::Queued => return Ok(output),
            Progress::Ended => return Ok(None),
            Progress::Yield | Progress::NeedInput(_) => {}
            Progress::Backpressure {
                requested,
                available,
            } => {
                return Err(Backpressure {
                    requested,
                    available,
                }
                .into());
            }
            _ => return Err(Error::Invalid("unexpected state of local file playback")),
        }
    }
}

struct Decode<'a, O> {
    pool: &'a Budget,
    opts: Options,
    format: Format,
    video: VideoSession,
    audio: AudioSession,
    observer: &'a mut O,
    framer: Option<Framers>,
}

struct Framers {
    video: (usize, AccessUnits),
    audio: Option<(usize, AccessUnits)>,
}

impl<O: Observer> Decode<'_, O> {
    fn start<T>(
        &mut self,
        tracks: impl Iterator<Item = (usize, T)> + Clone,
        info: impl Fn(&T) -> &dyn TrackInfo,
        cancel: &AtomicBool,
    ) -> Result<Framers> {
        let find = |wanted: Option<usize>| {
            tracks
                .clone()
                .filter(move |(index, _)| wanted.is_none_or(|i| *index == i))
        };
        let (vi, vt, vc) = find(self.opts.video)
            .find_map(|(i, track)| video_codec(info(&track).codec()).map(|codec| (i, track, codec)))
            .ok_or(Error::Unsupported("no decodable video track"))?;
        self.video.load(Some(video_decoder_in(vc, self.pool)));
        let frame = |track: &dyn TrackInfo| {
            let mut setup = AccessUnits::for_track(self.pool, track, AccessUnitLimits::default())?;
            ready(|work| setup.step(work, cancel))
        };
        let audio = if let Some((ai, at, ac)) = find(self.opts.audio)
            .find_map(|(i, track)| audio_codec(info(&track).codec()).map(|codec| (i, track, codec)))
        {
            let mut config = BudgetVec::new(self.pool);
            audio_configuration(ac, info(&at).configuration(), &mut config)?;
            self.audio
                .load(Some(audio_decoder_in(ac, &config, self.pool)?));
            Some((ai, frame(info(&at))?))
        } else if self.opts.audio.is_some() {
            return Err(Error::Unsupported("no decodable selected audio track"));
        } else {
            None
        };
        Ok(Framers {
            video: (vi, frame(info(&vt))?),
            audio,
        })
    }

    fn packet<T>(
        &mut self,
        tracks: impl Iterator<Item = (usize, T)> + Clone,
        info: impl Fn(&T) -> &dyn TrackInfo,
        packet: &impl PacketInfo,
        cancel: &AtomicBool,
    ) -> PlayResult<()> {
        let (index, track) = tracks
            .clone()
            .find(|(_, track)| info(track).id() == packet.track())
            .ok_or(Error::Invalid("packet track is absent"))?;
        if self.framer.is_none() {
            self.framer = Some(self.start(tracks, &info, cancel)?);
        }
        self.observer.packet(index, packet)?;
        let Some(framers) = &mut self.framer else {
            return Err(Error::Closed.into());
        };
        let video = index == framers.video.0;
        let units = if video {
            &mut framers.video.1
        } else if let Some((audio_index, units)) = &mut framers.audio
            && *audio_index == index
        {
            units
        } else {
            return Ok(());
        };
        let mut job = units.packet(packet)?;
        let unit = ready(|work| job.step(work, cancel))?;
        let pts = unit
            .pts()
            .map(|pts| {
                unit.time_base()
                    .rescale(pts, TimeBase::NANOSECONDS, Rounding::Floor)
            })
            .transpose()?;
        if video {
            self.video.feed(unit.payload(), pts)?;
            self.step_video()?;
        } else {
            self.audio
                .feed_discarding(unit.payload(), pts, Discard::of(&unit))?;
            let duration = if self.format == Format::Mp4 && info(&track).codec() == CodecId::Aac {
                unit.duration().map(|ticks| (unit.time_base(), ticks))
            } else {
                None
            };
            self.pull_audio(duration)?;
        }
        Ok(())
    }

    fn step_video(&mut self) -> Result<()> {
        while self.video.step(None) {
            if let Some(picture) = self.video.latest() {
                self.observer.picture(picture)?;
            }
        }
        Ok(())
    }

    fn pull_audio(&mut self, duration: Option<(TimeBase, i64)>) -> Result<()> {
        let mut block = PcmBlock::default();
        self.audio.pull(None, 0, &mut block);
        if let Some((base, ticks)) = duration
            && !block.samples.is_empty()
        {
            let sample_base = TimeBase::new(1, u64::from(block.sample_rate))?;
            let frames = base.rescale(ticks, sample_base, Rounding::Floor)?;
            let frames = usize::try_from(frames.max(0)).unwrap_or(usize::MAX);
            block.samples.truncate(frames.saturating_mul(2));
        }
        if !block.samples.is_empty() {
            self.observer.pcm(&block)?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        if self.framer.is_none() {
            return Err(Error::Invalid("no media packets"));
        }
        self.video.end_of_stream();
        self.step_video()?;
        self.audio.end_of_stream();
        while self.audio.state() == SessionState::Running {
            self.pull_audio(None)?;
        }
        for state in [self.video.state(), self.audio.state()] {
            if let SessionState::Failed(error) = state {
                return Err(error);
            }
        }
        Ok(())
    }
}

pub(super) fn play_with(
    pool: &Budget,
    media: Bytes,
    opts: Options,
    observer: &mut impl Observer,
) -> PlayResult<()> {
    let caps = SourceCapabilities {
        length: Some(media.len() as u64),
        random_access: true,
    };
    let id = SourceId(1);
    let cancel = AtomicBool::new(false);
    let source = ByteSource::new(id, media.clone(), WINDOW)?;
    let mut probe = Probe::new(pool, id, caps, WINDOW, source)?;
    while !matches!(
        probe.advance(&mut work(), &cancel)?,
        ProbeProgress::Selected(_)
    ) {}
    let selected = probe.finish()?;
    let mut decode = Decode {
        pool,
        format: selected.format,
        video: VideoSession::with_canvas(QueueLimits::VIDEO, opts.canvas),
        audio: AudioSession::new(QueueLimits::AUDIO),
        opts,
        observer,
        framer: None,
    };
    if matches!(selected.format, Format::Ts(_)) {
        let format = selected.format;
        drop(selected);
        let reader = (opts.transport)(pool, id, caps, format)?;
        let mut driver = Blocking::seekable(reader, Cursor::new(media), pool);
        while let Some(packet) = driver.next_packet()? {
            let reader = driver.reader();
            let tracks =
                (0..reader.track_count()).filter_map(|i| reader.track(i).map(|track| (i, track)));
            decode.packet(tracks, |track| *track, &packet, &cancel)?;
        }
    } else {
        let mut player = play_source(selected, pool, &cancel)?;
        while let Some(output) = next_output(&mut player, &cancel)? {
            if let Some(packet) = output.packet() {
                let engine = player.engine().ok_or(Error::Closed)?;
                let tracks = (0..engine.track_count())
                    .filter_map(|i| engine.track(i).map(|track| (i, track)));
                decode.packet(tracks, |track| track, &packet, &cancel)?;
            }
        }
    }
    decode.finish()?;
    Ok(())
}

pub(super) fn play_with_subtitles(
    pool: &Budget,
    media: Bytes,
    opts: Options,
    subtitles: &Path,
    times_ms: &[i64],
    observer: &mut impl Observer,
) -> PlayResult<()> {
    play_with(pool, media, opts, observer)?;
    let options = ScriptOptions {
        extract_fonts: true,
        ..ScriptOptions::default()
    };
    let mut script = Script::parse(pool, &read_file(pool, subtitles)?, &options)?;
    let settings = Settings::new(Size::new(WIDTH as u32, HEIGHT as u32));
    let mut renderer = Renderer::new(pool, &settings, &Fonts::default())?;
    for &now_ms in times_ms {
        observer.subtitle(now_ms, renderer.render(&mut script, now_ms)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Output {
        pictures: usize,
        audio_samples: usize,
    }

    impl Observer for Output {
        fn packet(&mut self, _: usize, _: &impl PacketInfo) -> Result<()> {
            Ok(())
        }

        fn subtitle(&mut self, _: i64, _: Frame<'_>) -> Result<()> {
            Ok(())
        }

        fn picture(&mut self, _: &Published) -> Result<()> {
            self.pictures += 1;
            Ok(())
        }

        fn pcm(&mut self, block: &PcmBlock) -> Result<()> {
            self.audio_samples += block.samples.len() / 2;
            Ok(())
        }
    }

    fn play_mp4(input: &[u8]) -> PlayResult<Output> {
        let pool = Budget::new(128 << 20)?;
        let mut output = Output::default();
        assert_eq!(
            triergon::probe(input),
            triergon::Probed::Format(triergon::SourceFormat::Isobmff)
        );
        play_with(
            &pool,
            Bytes::copy_from_slice(&pool, input)?,
            Options {
                canvas: Canvas::new(64, 36).ok_or(Error::Invalid("example canvas"))?,
                video: Some(0),
                audio: Some(1),
                transport: |_, _, _, _| Err(Error::Invalid("expected MP4")),
            },
            &mut output,
        )?;
        Ok(output)
    }

    #[test]
    fn mp4_playback_loads_the_video_and_audio_sample_descriptions() -> PlayResult<()> {
        let input =
            include_bytes!("../../../../playback/tests/fixtures/mp4/encoded-hevc-aac-front.mp4");
        let output = play_mp4(input)?;
        assert_eq!(output.pictures, 2);
        assert_eq!(output.audio_samples, 2048);
        Ok(())
    }

    #[test]
    fn mp4_playback_stops_aac_pcm_at_the_final_sample_boundary() -> PlayResult<()> {
        let input = include_bytes!(
            "../../../../playback/tests/fixtures/mp4/avc-aac-quicktime-reordered.mov"
        );
        let output = play_mp4(input)?;
        assert_eq!(output.pictures, 100);
        assert_eq!(output.audio_samples, 173 * 1024 + 272);
        Ok(())
    }
}
