use std::{
    fs::File,
    io::{Cursor, Read},
    path::Path,
    sync::atomic::AtomicBool,
};
use triergon::{
    AnyReader, Backpressure, Blocking, Budget, BudgetVec, Bytes, ContainerReader, Error,
    PacketInfo, Result, Rounding, SourceCapabilities, SourceId, TimeBase, TrackInfo, WorkBudget,
    bridge::access::{AccessUnitLimits, AccessUnits},
    framing::BuildStep,
    matroska,
    playback::{
        ByteSource, Engine, Format, Output, Player, PlayerPolicy, Progress, Selection, Source,
        mp4::{Mp4IndexJob, Mp4Open, OpenLimits, OpenProgress, PlanLimits},
        probe::{Probe, ProbeProgress, SelectedInput},
    },
    render::ass::{Fonts, Options as ScriptOptions, Renderer, Script, Settings, Size},
    session::{
        AudioSession, Canvas, Discard, PcmBlock, Published, QueueLimits, SessionState,
        VideoSession, audio_codec, audio_configuration, audio_decoder_in, video_codec,
        video_decoder_in,
    },
};

pub(super) type ExampleResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const WINDOW: usize = 64 * 1024;
const WIDTH: usize = 640;
const HEIGHT: usize = 360;

#[derive(Default)]
pub(super) struct Playback {
    pub(super) video_frames: usize,
    pub(super) rgba_bytes: usize,
    pub(super) audio_samples: usize,
    pub(super) sample_rate: u32,
    pub(super) subtitle_images: usize,
}

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
    pub(super) format: Option<Format>,
    pub(super) video: Option<usize>,
    pub(super) audio: Option<usize>,
    pub(super) transport: Option<TransportOpen>,
}

pub(super) trait Observer {
    fn packet(&mut self, _index: usize, _packet: &impl PacketInfo) -> Result<()> {
        Ok(())
    }

    fn picture(&mut self, _picture: &Published) -> Result<()> {
        Ok(())
    }

    fn pcm(&mut self, _block: &PcmBlock) -> Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct Count(Playback);

impl Observer for Count {
    fn picture(&mut self, picture: &Published) -> Result<()> {
        self.0.video_frames += 1;
        self.0.rgba_bytes = picture.rgba.len();
        Ok(())
    }

    fn pcm(&mut self, block: &PcmBlock) -> Result<()> {
        self.0.audio_samples += block.samples.len() / 2;
        self.0.sample_rate = block.sample_rate;
        Ok(())
    }
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
            (
                Engine::mp4(opened.input, index.finish()?, 1 << 20)?,
                opened.source,
            )
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
        self.video.load(video_decoder_in(vc, self.pool));
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
    ) -> ExampleResult<()> {
        let (index, _) = tracks
            .clone()
            .find(|(_, track)| info(track).id() == packet.track())
            .ok_or(Error::Invalid("packet track is absent"))?;
        if self.framer.is_none() {
            self.framer = Some(self.start(tracks, info, cancel)?);
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
            self.pull_audio()?;
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

    fn pull_audio(&mut self) -> Result<()> {
        let mut block = PcmBlock::default();
        self.audio.pull(None, 0, &mut block);
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
            self.pull_audio()?;
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
) -> ExampleResult<()> {
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
    if opts
        .format
        .is_some_and(|expected| expected != selected.format)
    {
        return Err(Error::Unsupported("unexpected playback container").into());
    }
    let mut decode = Decode {
        pool,
        video: VideoSession::with_canvas(QueueLimits::VIDEO, opts.canvas),
        audio: AudioSession::new(QueueLimits::AUDIO),
        opts,
        observer,
        framer: None,
    };
    if matches!(selected.format, Format::Ts(_)) {
        let format = selected.format;
        drop(selected);
        let reader = if let Some(open) = opts.transport {
            open(pool, id, caps, format)?
        } else {
            AnyReader::open(&media, pool, id, caps)?
        };
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

pub(super) fn play(media: &Path, subtitles: &Path, now_ms: i64) -> ExampleResult<Playback> {
    let pool = Budget::new(128 << 20)?;
    let canvas = Canvas::new(WIDTH, HEIGHT).ok_or(Error::Invalid("example canvas"))?;
    let mut observer = Count::default();
    play_with(
        &pool,
        read_file(&pool, media)?,
        Options {
            canvas,
            format: Some(Format::Matroska),
            video: None,
            audio: None,
            transport: None,
        },
        &mut observer,
    )?;
    let mut played = observer.0;

    let options = ScriptOptions {
        extract_fonts: true,
        ..ScriptOptions::default()
    };
    let mut script = Script::parse(&pool, &read_file(&pool, subtitles)?, &options)?;
    let settings = Settings::new(Size::new(WIDTH as u32, HEIGHT as u32));
    let mut renderer = Renderer::new(&pool, &settings, &Fonts::default())?;
    played.subtitle_images = renderer
        .render(&mut script, now_ms)?
        .images
        .iter()
        .filter(|image| image.rows().flatten().any(|&alpha| alpha != 0))
        .count();
    Ok(played)
}
