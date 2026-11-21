#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(clippy::print_stdout, clippy::print_stderr, clippy::unwrap_used)]
// Pending: `missing_docs`, `missing_debug_implementations`, `unreachable_pub`, `clippy::expect_used`.

pub mod audio;
#[cfg(feature = "codecs")]
pub mod compat {
    //! Optional decoder policies.
    /// Channel mapping and priming policies for AAC.
    pub mod aac {
        pub use triergon_codec_aac::compat::layout::Policy;
        pub use triergon_codec_aac::compat::priming::Policy as PrimingPolicy;

        /// Create an ADTS decoder with the selected source of priming discard.
        pub fn adts_decoder(policy: PrimingPolicy) -> Box<crate::DynAudioDecoder> {
            use crate::AudioDecoder;
            triergon_codec_aac::Decoder::new()
                .with_priming_policy(policy)
                .boxed()
        }
    }
}
mod discard;
pub mod mix;
pub mod queue;
#[cfg(feature = "codecs")]
mod track;
pub mod video;

pub use audio::{AudioSession, PcmBlock};
pub use discard::Discard;
pub use queue::{FeedError, QueueLimits};
#[cfg(feature = "codecs")]
pub use track::{AacAccessUnitConfig, TrackDecoder};
pub use triergon_codec::{
    AudioCodec, AudioDecoder, DynAudioDecoder, DynVideoDecoder, VideoCodec, VideoDecoder,
};
pub use triergon_core::Error;
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
    ) -> Result<(), Error> {
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
pub fn video_decoder(codec: VideoCodec) -> Box<DynVideoDecoder> {
    match codec {
        VideoCodec::H264 => triergon_codec_h264::Decoder::new().boxed(),
        VideoCodec::Hevc => triergon_codec_hevc::Decoder::new().boxed(),
    }
}

/// The decoder lane for a container track's codec, if this crate decodes it.
pub fn audio_codec(codec: triergon_core::container::CodecId) -> Option<AudioCodec> {
    use triergon_core::container::CodecId;
    Some(match codec {
        CodecId::Ac3 => AudioCodec::Ac3,
        CodecId::Eac3 => AudioCodec::Eac3,
        CodecId::Aac => AudioCodec::Aac,
        CodecId::Dts => AudioCodec::Dts,
        CodecId::Flac => AudioCodec::Flac,
        CodecId::Opus => AudioCodec::Opus,
        CodecId::Vorbis => AudioCodec::Vorbis,
        CodecId::MpegAudio => AudioCodec::MpegAudio,
        _ => return None,
    })
}

/// The canonical configuration [`audio_decoder`] takes, from the configuration bytes a
/// container track carries (`TrackInfo::configuration`): Matroska CodecPrivate, an ISO
/// BMFF `dfLa` or `dOps` payload, or the headers an Ogg reader collects. Empty for codecs
/// that take none.
pub fn audio_configuration(
    codec: AudioCodec,
    track_configuration: &[u8],
    out: &mut triergon_core::BudgetVec<u8>,
) -> triergon_core::Result<()> {
    out.clear();
    match codec {
        AudioCodec::Flac => {
            triergon_framing::flac::canonical_configuration(track_configuration, out)
        }
        AudioCodec::Opus => {
            triergon_framing::opus::canonical_configuration(track_configuration, out)
        }
        AudioCodec::Vorbis => {
            triergon_framing::vorbis::canonical_configuration(track_configuration, out)
        }
        AudioCodec::Ac3 | AudioCodec::Eac3 | AudioCodec::Aac | AudioCodec::Dts => Ok(()),
        AudioCodec::MpegAudio => Ok(()),
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
) -> Result<Box<DynAudioDecoder>, Error> {
    Ok(match codec {
        AudioCodec::Ac3 | AudioCodec::Eac3 => triergon_codec_ac3::Decoder::new().boxed(),
        AudioCodec::Aac => triergon_codec_aac::Decoder::new().boxed(),
        AudioCodec::Dts => triergon_codec_dca::Decoder::new().boxed(),
        AudioCodec::Flac => triergon_codec_flac::Decoder::new(configuration)?.boxed(),
        AudioCodec::Opus => triergon_codec_opus::Decoder::new(configuration)?.boxed(),
        AudioCodec::Vorbis => triergon_codec_vorbis::Decoder::new(configuration)?.boxed(),
        AudioCodec::MpegAudio => triergon_codec_mp3::Decoder::new().boxed(),
    })
}

/// [`audio_decoder`] charging the decoder's storage, its output frame included, to
/// `budget` instead of a pool of the codec's default limit.
#[cfg(feature = "codecs")]
pub fn audio_decoder_in(
    codec: AudioCodec,
    configuration: &[u8],
    budget: &triergon_codec::Budget,
) -> Result<Box<DynAudioDecoder>, Error> {
    Ok(match codec {
        AudioCodec::Ac3 | AudioCodec::Eac3 => triergon_codec_ac3::Decoder::new_in(budget).boxed(),
        AudioCodec::Aac => triergon_codec_aac::Decoder::new_in(budget).boxed(),
        AudioCodec::Dts => triergon_codec_dca::Decoder::new_in(budget).boxed(),
        AudioCodec::Flac => triergon_codec_flac::Decoder::new_in(configuration, budget)?.boxed(),
        AudioCodec::Opus => triergon_codec_opus::Decoder::new_in(configuration, budget)?.boxed(),
        AudioCodec::Vorbis => {
            triergon_codec_vorbis::Decoder::new_in(configuration, budget)?.boxed()
        }
        AudioCodec::MpegAudio => triergon_codec_mp3::Decoder::new_in(budget).boxed(),
    })
}

/// Why a session is not producing output right now. A starved session waits
/// for input; an ended one has drained its decoder after an explicit end of
/// stream; a failed one stays failed until the next load/flush/stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// No session (nothing loaded, or the stream named no codec for this lane).
    Idle,
    /// Accepting input; output follows the clock.
    Running,
    /// Every queued access unit is decoded and every picture/sample delivered.
    Starved,
    /// `end_of_stream` was called and all decoded output has been delivered.
    Ended,
    /// The decoder refused an access unit, with this error.
    Failed(Error),
}
