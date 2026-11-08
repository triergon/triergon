#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]
// Pending: `missing_docs`.

mod ac3;
mod adts;
mod blocking;
mod flac;
mod input;
mod mpeg;
mod reader;
mod stream;
mod syntax;
mod tags;

pub use ac3::Ac3;
pub use adts::Adts;
pub use blocking::BlockingReader;
pub use flac::{Flac, FlacBlockKind, FlacMetadata, FlacSeekPoint};
pub use mpeg::{Gapless, InfoFrame, MpegAudio};
pub use reader::{Format, Reader};
pub use stream::{Limits, Packet, ReaderEvent, SeekDetail, TRACK, Track};
pub use syntax::SeekBasis;
pub use tags::{Tag, TagKind};

/// Native FLAC.
pub type FlacReader = Reader<Flac>;
/// MPEG-1/2/2.5 audio, Layer I, II and III.
pub type MpegAudioReader = Reader<MpegAudio>;
/// AAC in ADTS.
pub type AdtsReader = Reader<Adts>;
/// AC-3 and E-AC-3.
pub type Ac3Reader = Reader<Ac3>;
