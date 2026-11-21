use std::{fs::File, io::Read, path::Path, sync::atomic::AtomicBool};
use triergon::{
    Backpressure, Budget, BudgetVec, Bytes, Error, PacketInfo, Result, Rounding,
    SourceCapabilities, SourceId, TimeBase, TrackId, TrackInfo, WorkBudget,
    bridge::access::{AccessUnitLimits, AccessUnits},
    framing::BuildStep,
    matroska,
    playback::{
        ByteSource, Engine, Format, Output, Player, PlayerPolicy, Progress, Selection, Source,
        probe::{Probe, ProbeProgress},
    },
    render::ass::{Fonts, Options, Renderer, Script, Settings, Size},
    session::{
        AudioSession, Canvas, Discard, PcmBlock, QueueLimits, SessionState, VideoSession,
        audio_codec, audio_configuration, audio_decoder_in, video_codec, video_decoder_in,
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

fn open(pool: &Budget, file: &Path, cancel: &AtomicBool) -> Result<Player<impl Source + use<>>> {
    let bytes = read_file(pool, file)?;
    let caps = SourceCapabilities {
        length: Some(bytes.len() as u64),
        random_access: true,
    };
    let id = SourceId(1);
    let mut probe = Probe::new(pool, id, caps, WINDOW, ByteSource::new(id, bytes, WINDOW)?)?;
    loop {
        match probe.advance(&mut work(), cancel)? {
            ProbeProgress::Selected(Format::Matroska) => break,
            ProbeProgress::Selected(_) => return Err(Error::Unsupported("expected Matroska")),
            ProbeProgress::Yield | ProbeProgress::NeedInput(_) => {}
        }
    }
    let selected = probe.finish()?;
    let reader = matroska::Reader::new(
        pool,
        selected.input.source(),
        selected.input.capabilities(),
        matroska::Limits::default(),
        WINDOW,
    )?;
    Player::new_direct(
        Engine::matroska(reader),
        selected.source,
        Selection::All,
        PlayerPolicy::default(),
    )
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

fn start_tracks(
    pool: &Budget,
    engine: &Engine,
    video: &mut VideoSession,
    audio: &mut AudioSession,
    cancel: &AtomicBool,
) -> Result<[(TrackId, AccessUnits); 2]> {
    let tracks = || (0..engine.track_count()).filter_map(|i| engine.track(i));
    let (video_track, video_format) = tracks()
        .find_map(|track| video_codec(track.codec()).map(|codec| (track, codec)))
        .ok_or(Error::Unsupported("no decodable video track"))?;
    let (audio_track, audio_format) = tracks()
        .find_map(|track| audio_codec(track.codec()).map(|codec| (track, codec)))
        .ok_or(Error::Unsupported("no decodable audio track"))?;
    let mut config = BudgetVec::new(pool);
    audio_configuration(audio_format, audio_track.configuration(), &mut config)?;
    audio.load(Some(audio_decoder_in(audio_format, &config, pool)?));
    video.load(video_decoder_in(video_format, pool));
    let frame = |track: &dyn TrackInfo| {
        let mut setup = AccessUnits::for_track(pool, track, AccessUnitLimits::default())?;
        ready(|work| setup.step(work, cancel)).map(|units| (track.id(), units))
    };
    Ok([frame(&video_track)?, frame(&audio_track)?])
}

fn pull_audio(audio: &mut AudioSession, played: &mut Playback) {
    let mut pcm = PcmBlock::default();
    audio.pull(None, 0, &mut pcm);
    if !pcm.samples.is_empty() {
        played.audio_samples += pcm.samples.len() / 2;
        played.sample_rate = pcm.sample_rate;
    }
}

pub(super) fn play(media: &Path, subtitles: &Path, now_ms: i64) -> ExampleResult<Playback> {
    let pool = Budget::new(128 << 20)?;
    let cancel = AtomicBool::new(false);
    let mut player = open(&pool, media, &cancel)?;
    let canvas = Canvas::new(WIDTH, HEIGHT).ok_or(Error::Invalid("example canvas"))?;
    let mut video = VideoSession::with_canvas(QueueLimits::VIDEO, canvas);
    let mut audio = AudioSession::new(QueueLimits::AUDIO);
    let mut tracks = None;
    let mut played = Playback::default();

    while let Some(output) = next_output(&mut player, &cancel)? {
        let Some(packet) = output.packet() else {
            continue;
        };
        let [(video_id, video_units), (audio_id, audio_units)] = if let Some(tracks) = &mut tracks {
            tracks
        } else {
            let engine = player.engine().ok_or(Error::Closed)?;
            tracks.insert(start_tracks(
                &pool, engine, &mut video, &mut audio, &cancel,
            )?)
        };
        let framing = if packet.track() == *video_id {
            video_units
        } else if packet.track() == *audio_id {
            audio_units
        } else {
            continue;
        };
        let mut job = framing.packet(&packet)?;
        let unit = ready(|work| job.step(work, &cancel))?;
        let pts = unit
            .pts()
            .map(|pts| {
                unit.time_base()
                    .rescale(pts, TimeBase::NANOSECONDS, Rounding::Floor)
            })
            .transpose()?;
        if unit.track() == *video_id {
            video.feed(unit.payload(), pts)?;
            while video.step(None) {
                played.video_frames += 1;
            }
        } else {
            audio.feed_discarding(unit.payload(), pts, Discard::of(&unit))?;
            pull_audio(&mut audio, &mut played);
        }
    }
    if tracks.is_none() {
        return Err(Error::Invalid("no Matroska packets").into());
    }
    video.end_of_stream();
    while video.step(None) {
        played.video_frames += 1;
    }
    audio.end_of_stream();
    while audio.state() == SessionState::Running {
        pull_audio(&mut audio, &mut played);
    }
    for state in [video.state(), audio.state()] {
        if let SessionState::Failed(error) = state {
            return Err(error.into());
        }
    }
    played.rgba_bytes = video.latest().map_or(0, |picture| picture.rgba.len());

    let options = Options {
        extract_fonts: true,
        ..Options::default()
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
