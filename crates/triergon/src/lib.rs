#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_docs,
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]

#[cfg(any(
    feature = "matroska",
    feature = "isobmff",
    feature = "mpegts",
    feature = "ogg",
    feature = "elementary"
))]
mod any;
mod probe;

#[cfg(any(
    feature = "matroska",
    feature = "isobmff",
    feature = "mpegts",
    feature = "ogg",
    feature = "elementary"
))]
pub use any::{AnyOther, AnyPacket, AnyReader, Replayed, open_blocking, open_sequential};
pub use probe::{Probed, SourceFormat, probe, probe_whole};
pub use triergon_core::blocking::Blocking;
pub use triergon_core::container::{
    CodecId, ColourDescription, ColourRange, ContainerReader, ContentLight, Event,
    MasteringDisplay, MediaKind, Origin, PacketInfo, RandomAccess, ReaderStep, SeekMethod,
    SeekPolicy, SeekReceipt, SeekRequest, Sourced, TrackId, TrackInfo, Trim,
};
pub use triergon_core::{
    Backpressure, Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputToken,
    InputWindow, Result, Rounding, SourceCapabilities, SourceFailure, SourceId, SourceLocation,
    Step, Time, TimeBase, WorkBudget,
};

#[cfg(feature = "playback")]
/// Adapt container packets to decoder access units.
pub use triergon_bridge as bridge;
#[cfg(feature = "elementary")]
/// Readers for native FLAC, MPEG audio, ADTS and AC-3 streams.
pub use triergon_elementary as elementary;
#[cfg(feature = "framing")]
/// Codec configuration readers and packet framing adapters.
pub use triergon_framing as framing;

/// Run a framing job to its next output, supplying a fresh work budget on each
/// yield. Returns `None` when the job completes without another output. The job's
/// errors and cancellation are returned unchanged.
#[cfg(feature = "framing")]
pub fn run_build<T>(
    mut step: impl FnMut(&mut WorkBudget) -> Result<framing::BuildStep<T>>,
) -> Result<Option<T>> {
    loop {
        match step(&mut WorkBudget::new(1 << 20))? {
            framing::BuildStep::Ready(value) => return Ok(Some(value)),
            framing::BuildStep::Yield => {}
            framing::BuildStep::Complete => return Ok(None),
        }
    }
}

#[cfg(feature = "playback")]
/// Read segment feeds and progressive MPEG transport.
pub use triergon_hls as hls;
#[cfg(feature = "isobmff")]
/// Readers for ISO BMFF sources, including fragmented MP4.
pub use triergon_isobmff as isobmff;
#[cfg(feature = "matroska")]
/// Readers for Matroska and WebM sources.
pub use triergon_matroska as matroska;
#[cfg(feature = "mpegts")]
/// Readers for TS188 and M2TS192 transport streams.
pub use triergon_mpegts as mpegts;
#[cfg(feature = "ogg")]
/// Readers for Ogg pages, logical streams and chained links.
pub use triergon_ogg as ogg;
#[cfg(feature = "playback")]
/// Container playback engines, sources and packet delivery.
pub use triergon_playback as playback;
#[cfg(feature = "playback")]
/// Probe and frame raw Annex B video.
pub use triergon_raw_video as raw_video;
#[cfg(feature = "session")]
/// Bounded decode queues and clocked audio and video output.
pub use triergon_session as session;
#[cfg(feature = "subtitles")]
/// Text cue extraction and bitmap subtitle decoding.
pub use triergon_subtitles as subtitles;

/// Decoder traits and decoded-picture and PCM vocabulary.
#[cfg(feature = "session")]
pub mod codec {
    pub use triergon_codec::{
        AudioDecoder, AudioFrame, ChannelLayout, Colour, DecodeError, DownmixLevels,
        DynAudioDecoder, DynVideoDecoder, PictureView, Plane, SampleFormat, Speaker,
        SurroundEncoding, VideoDecoder,
    };
    /// Pixel formats, colour conversion and letterboxing.
    pub use triergon_pixel as pixel;
    pub use triergon_session::{AudioCodec, VideoCodec};

    #[cfg(feature = "codecs")]
    /// Decode AAC audio.
    pub use triergon_codec_aac as aac;
    #[cfg(feature = "codecs")]
    /// Decode AC-3 and E-AC-3 audio.
    pub use triergon_codec_ac3 as ac3;
    #[cfg(feature = "codecs")]
    /// Decode DTS audio.
    pub use triergon_codec_dca as dca;
    #[cfg(feature = "codecs")]
    /// Decode FLAC audio.
    pub use triergon_codec_flac as flac;
    #[cfg(feature = "codecs")]
    /// Decode H.264 video.
    pub use triergon_codec_h264 as h264;
    #[cfg(feature = "codecs")]
    /// Decode HEVC video.
    pub use triergon_codec_hevc as hevc;
    #[cfg(feature = "codecs")]
    /// Decode MPEG audio.
    pub use triergon_codec_mp3 as mp3;
    #[cfg(feature = "codecs")]
    /// Decode Opus audio.
    pub use triergon_codec_opus as opus;
    #[cfg(feature = "codecs")]
    /// Decode Vorbis audio.
    pub use triergon_codec_vorbis as vorbis;
}

/// Render ASS and SSA subtitle scripts and packet streams.
#[cfg(feature = "render")]
pub mod render {
    /// ASS and SSA scripts, styles, fonts and positioned alpha images.
    pub use triergon_ass as ass;
    /// OpenType and TrueType faces, outlines and metrics.
    pub use triergon_font as font;
    /// Outline rasterising, blur and bitmap composition.
    pub use triergon_raster as raster;
    /// OpenType shaping and bidirectional text.
    pub use triergon_shape as shape;
}
