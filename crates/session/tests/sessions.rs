//! Lifecycle tests for both session lanes with scripted decoders.
use std::collections::VecDeque;
use triergon_codec::{
    AudioDecoder, AudioFrame, ChannelLayout, Colour, DecodeFailure, DownmixLevels, PictureView,
    Plane, Speaker, VideoDecoder,
};
use triergon_pixel::letterbox::CANVAS_BYTES;
use triergon_session::{
    AudioSession, FeedError, PcmBlock, QueueLimits, SessionState, VideoSession,
};

/// A reordering decoder: access unit `[luma, display_order]`, delay of two.
#[derive(Default)]
struct Script {
    held: Vec<(u8, u8, Option<i64>)>,
    out: VecDeque<(u8, Option<i64>)>,
    flushed: bool,
    picture: (Vec<u8>, Vec<u8>),
}
impl VideoDecoder for Script {
    fn send(&mut self, au: &[u8], pts: Option<i64>) -> Result<(), DecodeFailure> {
        if au[0] == 0xff {
            return Err(DecodeFailure("not an H.264 Annex-B access unit".into()));
        }
        self.held.push((au[0], au[1], pts));
        if self.held.len() > 2 {
            self.held.sort_by_key(|h| h.1);
            let (luma, _, pts) = self.held.remove(0);
            self.out.push_back((luma, pts));
        }
        Ok(())
    }
    fn receive(&mut self) -> Option<PictureView<'_>> {
        let (luma, pts) = self.out.pop_front()?;
        self.picture = (vec![luma; 4 * 2], vec![128_u8; 2]);
        let (y, c) = &self.picture;
        Some(PictureView {
            width: 4,
            height: 2,
            bit_depth: 8,
            planes: [
                Plane::U8 {
                    data: y,
                    stride: 4,
                    offset: 0,
                },
                Plane::U8 {
                    data: c,
                    stride: 2,
                    offset: 0,
                },
                Plane::U8 {
                    data: c,
                    stride: 2,
                    offset: 0,
                },
            ],
            pts,
            sar: (1, 1),
            colour: Colour {
                matrix: 1,
                full_range: true,
            },
        })
    }
    fn flush(&mut self) {
        self.flushed = true;
        self.held.sort_by_key(|h| h.1);
        for (luma, _, pts) in self.held.drain(..) {
            self.out.push_back((luma, pts));
        }
    }
    fn reset(&mut self) {
        self.held.clear();
        self.out.clear();
    }
}

fn centre(rgba: &[u8]) -> u8 {
    rgba[CANVAS_BYTES / 2 + 1920 * 2]
}

#[test]
fn pictures_keep_their_own_timestamps_through_reordering_and_wait_for_the_clock() {
    let mut video = VideoSession::new(QueueLimits::VIDEO);
    assert_eq!(video.state(), SessionState::Idle);
    assert_eq!(video.feed(&[1, 0], Some(0)), Err(FeedError::Idle));
    video.load(Box::<Script>::default());
    // Decode order I0 P3 B1 B2 with display PTS 0, 30, 10, 20.
    for (luma, order, pts) in [(10, 0, 0), (40, 3, 30), (20, 1, 10), (30, 2, 20)] {
        video.feed(&[luma, order], Some(pts)).unwrap();
    }
    assert!(video.step(Some(0)));
    assert_eq!(video.latest().unwrap().pts, Some(0));
    assert!(
        !video.step(Some(5)),
        "the next picture waits for its own PTS"
    );
    assert!(video.step(Some(10)));
    assert_eq!(
        (
            video.latest().unwrap().pts,
            centre(&video.latest().unwrap().rgba)
        ),
        (Some(10), 20)
    );
    video.end_of_stream();
    assert!(video.step(Some(20)));
    assert!(video.step(Some(30)));
    assert_eq!(video.latest().unwrap().pts, Some(30));
    assert!(!video.step(Some(1_000)));
    assert_eq!(
        video.state(),
        SessionState::Ended,
        "decoded EOF, not starvation"
    );
    assert_eq!(
        video.feed(&[1, 4], Some(40)),
        Err(FeedError::Invalid),
        "no input after end of stream"
    );
}

#[test]
fn flush_keeps_the_picture_stop_clears_it_and_generations_advance() {
    let mut video = VideoSession::new(QueueLimits::VIDEO);
    video.load(Box::<Script>::default());
    let first = video.generation();
    for i in 0..4 {
        video.feed(&[i * 10, i], Some(i64::from(i))).unwrap();
    }
    assert!(video.step(None));
    let shown = video.latest().unwrap().clone();
    video.flush();
    assert!(video.generation() > first);
    assert_eq!(
        video.latest().unwrap().sequence,
        shown.sequence,
        "flush keeps the picture on screen"
    );
    assert_eq!(
        video.state(),
        SessionState::Starved,
        "a flushed lane waits for new input"
    );
    video.stop();
    assert!(video.latest().is_none());
    assert_eq!(video.state(), SessionState::Idle);
}

#[test]
fn decoder_failure_is_explicit_and_backpressure_bounds_input() {
    let mut video = VideoSession::new(QueueLimits {
        units: 3,
        bytes: 1 << 20,
        unit_bytes: 64,
    });
    video.load(Box::<Script>::default());
    for i in 0..3 {
        video.feed(&[1, i], None).unwrap();
    }
    assert_eq!(video.feed(&[1, 9], None), Err(FeedError::Backpressure));
    video.flush();
    video.feed(&[0xff, 0], None).unwrap();
    assert!(!video.step(None));
    assert!(matches!(video.state(), SessionState::Failed(m) if m.contains("H.264")));
    assert_eq!(video.feed(&[1, 0], None), Err(FeedError::Idle));
    video.flush();
    video.feed(&[1, 0], None).unwrap();
}

/// Each unit decodes to `[rate_khz, value]` -> `value` samples at value/100.
struct Tones {
    layout: ChannelLayout,
}
impl Tones {
    #[allow(clippy::unnecessary_box_returns)] // The session takes a boxed decoder.
    fn new(speakers: impl IntoIterator<Item = Speaker>) -> Box<Self> {
        Box::new(Tones {
            layout: ChannelLayout::new(speakers).unwrap(),
        })
    }
}
impl AudioDecoder for Tones {
    fn decode(
        &mut self,
        au: &[u8],
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, DecodeFailure> {
        out.clear();
        if au[0] == 0 {
            return Err(DecodeFailure("bad sync".into()));
        }
        out.sample_rate = u32::from(au[0]) * 1000;
        out.layout = self.layout;
        out.samples_per_channel = 4;
        out.planes = vec![vec![f32::from(au[1]) / 100.0; 4]; self.layout.channels()];
        Ok(None)
    }
    fn reset(&mut self) {}
}

#[test]
fn audio_is_paced_by_clock_minus_lead_and_flush_discards_it() {
    let mut audio = AudioSession::new(QueueLimits::AUDIO);
    audio.load(Some(Tones::new([Speaker::FrontLeft, Speaker::FrontRight])));
    for (i, pts) in [0_i64, 100, 200].into_iter().enumerate() {
        audio
            .feed(&[48, 10 + i as u8], Some(pts * 1_000_000))
            .unwrap();
    }
    let lead = triergon_session::audio::DEFAULT_LEAD_NS;
    assert_eq!(lead, 120_000_000);
    let mut block = PcmBlock::default();
    assert_eq!(
        audio.pull(Some(-200_000_000), lead, &mut block),
        0,
        "too early"
    );
    assert_eq!(
        audio.pull(Some(-20_000_000), lead, &mut block),
        2,
        "PTS 0 and 100 ms are within the lead"
    );
    assert_eq!(
        (block.sample_rate, block.pts, block.samples.len()),
        (48_000, Some(0), 16)
    );
    assert_eq!(block.samples[0], (0.10_f32 * 32768.0).floor() as i16);
    // A paused clock delivers nothing more.
    assert_eq!(
        audio.pull(Some(-20_000_000), lead, &mut PcmBlock::default()),
        0
    );
    audio.stop();
    assert_eq!(
        audio.state(),
        SessionState::Starved,
        "flush discarded the queued frame"
    );
    audio.end_of_stream();
    assert_eq!(audio.state(), SessionState::Ended);
}

#[cfg(feature = "codecs")]
#[test]
fn a_selection_starts_the_lanes_it_names() {
    use triergon_session::{AudioCodec, StreamSelection, VideoCodec};

    let mut video = VideoSession::new(QueueLimits::VIDEO);
    let mut audio = AudioSession::new(QueueLimits::AUDIO);
    let selection = StreamSelection {
        video: VideoCodec::Hevc,
        audio: Some(AudioCodec::Dts),
    };
    selection.load(&mut video, &mut audio, &[]).unwrap();
    assert_eq!(video.state(), SessionState::Starved);
    assert_eq!(audio.state(), SessionState::Starved);
    let silent = StreamSelection {
        audio: None,
        ..selection
    };
    silent.load(&mut video, &mut audio, &[]).unwrap();
    assert_eq!(video.state(), SessionState::Starved);
    assert_eq!(audio.state(), SessionState::Idle);
    assert_eq!(audio.feed(&[1], None), Err(FeedError::Idle));
}

#[test]
fn a_sample_rate_change_starts_a_new_block_and_surround_is_downmixed() {
    let mut audio = AudioSession::new(QueueLimits::AUDIO);
    use Speaker::*;
    audio.load(Some(Tones::new([
        FrontLeft,
        FrontRight,
        FrontCenter,
        LowFrequency,
        SideLeft,
        SideRight,
    ])));
    audio.feed(&[48, 50], None).unwrap();
    audio.feed(&[44, 50], None).unwrap();
    let mut first = PcmBlock::default();
    audio.pull(None, 0, &mut first);
    assert_eq!((first.sample_rate, first.samples.len()), (48_000, 8));
    // Normalised 5.1: equal inputs at 0.5 sum to just under 0.5 per side in
    // float32 (LFE omitted); the device conversion rounds toward -inf.
    assert_eq!(first.samples[0], 16383);
    let mut second = PcmBlock::default();
    audio.pull(None, 0, &mut second);
    assert_eq!((second.sample_rate, second.samples.len()), (44_000, 8));
    audio.feed(&[0, 0], None).unwrap();
    audio.pull(None, 0, &mut PcmBlock::default());
    assert!(matches!(audio.state(), SessionState::Failed(_)));
    audio.load(None);
    assert_eq!(
        audio.feed(&[48, 1], None),
        Err(FeedError::Idle),
        "no audio lane for unknown codecs"
    );
}
