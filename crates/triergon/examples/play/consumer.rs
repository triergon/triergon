use std::{fs::File, io::Read, path::Path, sync::atomic::AtomicBool};
use triergon::{
    AnyReader, Budget, BudgetVec, ContainerReader, Error, PacketInfo, Result, TrackInfo,
    bridge::access::{AccessUnitLimits, AccessUnits},
    open_blocking,
    render::ass::{Fonts, Options, Renderer, Script, Settings, Size},
    run_build,
    session::{
        AudioSession, Canvas, PcmBlock, QueueLimits, SessionState, VideoSession, audio_codec,
        audio_configuration, audio_decoder_in, video_codec, video_decoder_in,
    },
};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;

fn framer(pool: &Budget, track: &dyn TrackInfo, cancel: &AtomicBool) -> Result<AccessUnits> {
    let mut setup = AccessUnits::for_track(pool, track, AccessUnitLimits::default())?;
    run_build(|work| setup.step(work, cancel))?.ok_or(Error::Invalid("missing track framer"))
}

#[derive(Default)]
pub(super) struct Playback {
    pub(super) pictures: usize,
    pub(super) rgba: usize,
    pub(super) samples: usize,
    pub(super) rate: u32,
}

impl Playback {
    fn video(&mut self, video: &mut VideoSession) {
        while video.step(None) {
            if let Some(picture) = video.latest() {
                self.pictures += 1;
                self.rgba = picture.rgba.len();
            }
        }
    }

    fn audio(&mut self, audio: &mut AudioSession) {
        let mut block = PcmBlock::default();
        audio.pull(None, 0, &mut block);
        if !block.samples.is_empty() {
            self.samples += block.samples.len() / 2;
            self.rate = block.sample_rate;
        }
    }
}

fn decode(
    pool: &Budget,
    media: &Path,
) -> std::result::Result<Playback, Box<dyn std::error::Error>> {
    let mut source = open_blocking(File::open(media)?, pool)?;
    if !matches!(source.reader(), AnyReader::Matroska(_)) {
        return Err(Error::Unsupported("unexpected playback container").into());
    }

    let canvas =
        Canvas::new(WIDTH as usize, HEIGHT as usize).ok_or(Error::Invalid("example canvas"))?;
    let mut video = VideoSession::with_canvas(QueueLimits::VIDEO, canvas);
    let mut audio = AudioSession::new(QueueLimits::AUDIO);
    let mut played = Playback::default();
    let cancel = AtomicBool::new(false);
    let mut packet = source
        .next_packet()?
        .ok_or(Error::Invalid("no media packets"))?;
    let reader = source.reader();
    let mut tracks = (0..reader.track_count()).filter_map(|index| reader.track(index));
    let (track, codec) = tracks
        .clone()
        .find_map(|track| video_codec(track.codec()).map(|codec| (track, codec)))
        .ok_or(Error::Unsupported("no decodable video track"))?;
    let video_id = track.id();
    let mut video_units = framer(pool, track, &cancel)?;
    video.load(Some(video_decoder_in(codec, pool)));
    let mut audio_units = if let Some((track, codec)) =
        tracks.find_map(|track| audio_codec(track.codec()).map(|codec| (track, codec)))
    {
        let mut config = BudgetVec::new(pool);
        audio_configuration(codec, track.configuration(), &mut config)?;
        audio.load(Some(audio_decoder_in(codec, &config, pool)?));
        Some((track.id(), framer(pool, track, &cancel)?))
    } else {
        None
    };

    loop {
        let is_video = packet.track() == video_id;
        let units = if is_video {
            Some(&mut video_units)
        } else {
            audio_units
                .as_mut()
                .filter(|(id, _)| *id == packet.track())
                .map(|(_, units)| units)
        };
        if let Some(units) = units {
            let mut job = units.packet(&packet)?;
            let unit = run_build(|work| job.step(work, &cancel))?
                .ok_or(Error::Invalid("missing access unit"))?;
            if is_video {
                video.feed_packet(&unit)?;
                played.video(&mut video);
            } else {
                audio.feed_packet(&unit)?;
                played.audio(&mut audio);
            }
        }
        let Some(next) = source.next_packet()? else {
            break;
        };
        packet = next;
    }

    video.end_of_stream();
    played.video(&mut video);
    audio.end_of_stream();
    while audio.state() == SessionState::Running {
        played.audio(&mut audio);
    }
    for state in [video.state(), audio.state()] {
        if let SessionState::Failed(error) = state {
            return Err(error.into());
        }
    }
    Ok(played)
}

pub(super) fn play(
    media: &Path,
    subtitles: &Path,
    time: i64,
) -> std::result::Result<(Playback, usize), Box<dyn std::error::Error>> {
    let pool = Budget::new(128 << 20)?;
    let played = decode(&pool, media)?;
    let options = Options {
        extract_fonts: true,
        ..Options::default()
    };
    let mut file = File::open(subtitles)?;
    let len = usize::try_from(file.metadata()?.len())?;
    let mut bytes = BudgetVec::try_filled(&pool, 0, len)?;
    file.read_exact(&mut bytes)?;
    let mut script = Script::parse(&pool, &bytes, &options)?;
    drop(bytes);
    let settings = Settings::new(Size::new(WIDTH, HEIGHT));
    let mut renderer = Renderer::new(&pool, &settings, &Fonts::default())?;
    let frame = renderer.render(&mut script, time)?;
    let images = frame
        .images
        .iter()
        .filter(|image| image.rows().flatten().any(|&alpha| alpha != 0))
        .count();
    Ok((played, images))
}
