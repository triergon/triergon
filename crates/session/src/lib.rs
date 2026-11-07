#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(clippy::print_stdout, clippy::print_stderr, clippy::unwrap_used)]
// Pending: `missing_docs`, `missing_debug_implementations`, `unreachable_pub`, `clippy::expect_used`.

pub mod audio;
pub mod mix;
pub mod queue;
pub mod video;

pub use audio::{AudioSession, PcmBlock};
pub use queue::{FeedError, QueueLimits};
pub use triergon_codec::{AudioCodec, AudioDecoder, DecodeFailure, VideoCodec, VideoDecoder};
pub use video::{Published, VideoSession};

/// The codecs of one stream: a video lane and, optionally, an audio lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamSelection {
    pub video: VideoCodec,
    /// `None` plays no audio.
    pub audio: Option<AudioCodec>,
}

#[cfg(feature = "codecs")]
impl StreamSelection {
    /// Start this stream on both sessions with the pure-Rust decoders.
    /// `audio_configuration` is the audio codec's configuration, as [`audio_decoder`]
    /// takes it. A refused configuration loads neither session.
    pub fn load(
        self,
        video: &mut VideoSession,
        audio: &mut AudioSession,
        audio_configuration: &[u8],
    ) -> Result<(), DecodeFailure> {
        let decoder = self
            .audio
            .map(|codec| audio_decoder(codec, audio_configuration))
            .transpose()?;
        video.load(video_decoder(self.video));
        audio.load(decoder);
        Ok(())
    }
}

/// The pure-Rust decoder for `codec`.
#[cfg(feature = "codecs")]
pub fn video_decoder(codec: VideoCodec) -> Box<dyn VideoDecoder> {
    match codec {
        VideoCodec::H264 => Box::new(triergon_codec_h264::Decoder::new()),
        VideoCodec::Hevc => Box::new(triergon_codec_hevc::Decoder::new()),
    }
}

/// The pure-Rust decoder for `codec`, set up from `configuration`: for FLAC the `fLaC`
/// stream header, for Opus the `OpusHead` packet, for Vorbis the three header packets
/// with Xiph lacing (the canonical forms of `triergon-framing`). AC-3, E-AC-3, AAC
/// (ADTS), DTS and MPEG audio take none and ignore it.
#[cfg(feature = "codecs")]
pub fn audio_decoder(
    codec: AudioCodec,
    configuration: &[u8],
) -> Result<Box<dyn AudioDecoder>, DecodeFailure> {
    Ok(match codec {
        AudioCodec::Ac3 | AudioCodec::Eac3 => Box::new(triergon_codec_ac3::Decoder::new()),
        AudioCodec::Aac => Box::new(triergon_codec_aac::Decoder::new()),
        AudioCodec::Dts => Box::new(triergon_codec_dca::Decoder::new()),
        AudioCodec::Flac => Box::new(
            triergon_codec_flac::Decoder::from_configuration(configuration, Default::default())
                .map_err(DecodeFailure::from_error)?,
        ),
        AudioCodec::Opus => Box::new(
            triergon_codec_opus::Decoder::from_configuration(configuration, Default::default())
                .map_err(DecodeFailure::from_error)?,
        ),
        AudioCodec::Vorbis => Box::new(
            triergon_codec_vorbis::Decoder::from_configuration(configuration, Default::default())
                .map_err(DecodeFailure::from_error)?,
        ),
        AudioCodec::MpegAudio => Box::new(
            triergon_codec_mp3::Decoder::from_configuration(configuration, Default::default())
                .map_err(DecodeFailure::from_error)?,
        ),
    })
}

/// Why a session is not producing output right now. A starved session waits
/// for input; an ended one has drained its decoder after an explicit end of
/// stream; a failed one stays failed until the next load/flush/stop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// No session (nothing loaded, or the stream named no codec for this lane).
    Idle,
    /// Accepting input; output follows the clock.
    Running,
    /// Every queued access unit is decoded and every picture/sample delivered.
    Starved,
    /// `end_of_stream` was called and all decoded output has been delivered.
    Ended,
    /// The decoder rejected the stream; the message names the reason.
    Failed(String),
}
