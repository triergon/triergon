//! Local text facts. No renderer, ASS translation or charset detection.
use std::ops::Range;
use triergon_core::{BudgetVec, Bytes, Error, Result, SourceId, SourceLocation, Time, TimeBase};

/// How an input's payload carries its text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// A 3GPP timed text (tx3g, mov_text) sample: a big-endian 16-bit text
    /// length, the text, then modifier boxes kept whole and in order.
    Tx3g,
    /// A packet whose whole payload is strict UTF-8 text.
    Utf8Packet,
    /// A WebVTT cue in a Matroska block: the payload is the cue text, and
    /// block addition 1 holds its settings, identifier and comments.
    MatroskaWebVtt,
    /// An ASS/SSA packet, carried with no syntax validation and no character
    /// view.
    OpaqueAss,
}

/// How text bytes are encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// Strict UTF-8 (RFC 3629): overlong forms, surrogates and values above
    /// U+10FFFF are refused.
    Utf8,
    /// UTF-16 (RFC 2781) in the byte order of its leading byte-order mark,
    /// which is required and is not content.
    Utf16Bom,
    /// Big-endian UTF-16; a leading FEFF is content.
    Utf16Be,
    /// Little-endian UTF-16; a leading FEFF is content.
    Utf16Le,
    /// tx3g in-band selection: big-endian UTF-16 after a leading FE FF, which
    /// is not content, and UTF-8 otherwise.
    Tx3gInBand,
    /// Not decoded: the bytes have no character view.
    Opaque,
    /// An explicit multibyte profile; the label that selected it stays with
    /// the caller.
    Multibyte(super::charset::multibyte::MultibyteCharset),
}

/// A container clock and the ticks of one sample in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawClock {
    /// Time base numerator: a tick lasts `numerator / denominator` seconds.
    pub numerator: u64,
    /// Time base denominator.
    pub denominator: u64,
    /// Presentation time in ticks; `None` when unknown.
    pub pts_ticks: Option<i128>,
    /// Decoding time in ticks; `None` when unknown.
    pub dts_ticks: Option<i128>,
    /// Duration in ticks; `None` when unknown, distinct from zero.
    pub duration_ticks: Option<i128>,
}

/// The exact times of an input, with the clock they came from if any.
/// Unknown time, zero duration and negative times stay distinct.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    start: Option<Time>,
    dts: Option<Time>,
    duration: Option<Time>,
    raw_clock: Option<RawClock>,
}

impl Timing {
    /// No start, decoding time, duration or clock.
    pub const fn unknown() -> Self {
        Self {
            start: None,
            dts: None,
            duration: None,
            raw_clock: None,
        }
    }
    /// `start` and `duration` as given, without a decoding time or clock.
    pub const fn exact(start: Option<Time>, duration: Option<Time>) -> Self {
        Self {
            start,
            dts: None,
            duration,
            raw_clock: None,
        }
    }
    /// The times of `clock`'s ticks, keeping `clock`. `Error::Invalid` when
    /// its numerator or denominator is zero; `Error::Overflow` when a time
    /// does not fit.
    pub fn from_ticks(clock: RawClock) -> Result<Self> {
        let base = TimeBase::new(clock.numerator, clock.denominator)?;
        let start = clock
            .pts_ticks
            .map(|ticks| Time::from_ticks(ticks, base))
            .transpose()?;
        let duration = clock
            .duration_ticks
            .map(|ticks| Time::from_ticks(ticks, base))
            .transpose()?;
        let dts = clock
            .dts_ticks
            .map(|ticks| Time::from_ticks(ticks, base))
            .transpose()?;
        Ok(Self {
            start,
            dts,
            duration,
            raw_clock: Some(clock),
        })
    }
    /// Presentation start; `None` when unknown.
    pub const fn start(self) -> Option<Time> {
        self.start
    }
    /// Duration; `None` when unknown.
    pub const fn duration(self) -> Option<Time> {
        self.duration
    }
    /// Decoding time; `None` when unknown.
    pub const fn dts(self) -> Option<Time> {
        self.dts
    }
    /// The clock the times were derived from; `None` for times given exactly.
    pub const fn raw_clock(self) -> Option<RawClock> {
        self.raw_clock
    }
}

/// A Matroska block addition.
#[derive(Clone, Debug)]
pub struct Addition {
    /// The `BlockAddID`. Only ID 1 is read, for [`Format::MatroskaWebVtt`];
    /// others are kept unread.
    pub id: u64,
    /// The `BlockAdditional` bytes.
    pub bytes: Bytes,
}

/// One demuxed sample with the facts the container gives about it.
#[derive(Clone, Debug)]
pub struct Input {
    /// How the payload carries its text.
    pub format: Format,
    /// The declared encoding. [`Format::Utf8Packet`] and
    /// [`Format::MatroskaWebVtt`] require [`Encoding::Utf8`]; tx3g samples
    /// select their encoding in-band.
    pub encoding: Encoding,
    /// The sample bytes; at most [`Limits::max_input_bytes`].
    pub payload: Bytes,
    /// The container's codec identifier, kept unread; at most
    /// [`Limits::max_label_bytes`].
    pub codec_id: Option<Bytes>,
    /// The codec configuration (Matroska `CodecPrivate`), kept unread; at
    /// most [`Limits::max_config_bytes`].
    pub codec_private: Option<Bytes>,
    /// The declared charset label, kept unread; at most
    /// [`Limits::max_label_bytes`].
    pub charset_label: Option<Bytes>,
    /// The block addition; at most [`Limits::max_addition_bytes`].
    pub addition: Option<Addition>,
    /// The sample's times.
    pub timing: Timing,
    /// The source the sample comes from.
    pub source_id: SourceId,
    /// The caller's input generation; an input of another generation than
    /// the operation's is `Error::StaleGeneration`.
    pub generation: u64,
    /// The caller's input sequence number.
    pub sequence: u64,
    /// The caller's configuration epoch.
    pub configuration_epoch: u64,
    /// Where the container holds the sample; `None` when unknown.
    pub container_position: Option<SourceLocation>,
}

/// Resource bounds of one input; a value beyond one is `Error::Limit`.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Payload bytes, and the source bytes of a charset conversion; 4 MiB
    /// by default.
    pub max_input_bytes: usize,
    /// Codec configuration bytes; 1 MiB by default.
    pub max_config_bytes: usize,
    /// Bytes of a codec identifier or charset label; 256 by default.
    pub max_label_bytes: usize,
    /// Block addition bytes; 1 MiB by default.
    pub max_addition_bytes: usize,
    /// tx3g modifier boxes in one sample; 4096 by default.
    pub max_modifiers: usize,
    /// UTF-8 bytes of one cue's decoded text, or of a charset conversion;
    /// 4 MiB by default.
    pub max_decoded_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_input_bytes: 4 * 1024 * 1024,
            max_config_bytes: 1024 * 1024,
            max_label_bytes: 256,
            max_addition_bytes: 1024 * 1024,
            max_modifiers: 4096,
            max_decoded_bytes: 4 * 1024 * 1024,
        }
    }
}

/// Which bytes of an [`Input`] a [`Field`] indexes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldOwner {
    /// [`Input::payload`].
    Payload,
    /// The bytes of [`Input::addition`].
    Addition,
    /// [`Input::codec_private`].
    CodecPrivate,
}

/// A byte range in one of an input's byte owners.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    /// The bytes the range indexes.
    pub owner: FieldOwner,
    /// Byte range in the owner.
    pub range: Range<usize>,
}

impl Field {
    /// The field's bytes in `input`, sharing its storage. `Error::Invalid`
    /// when `input` lacks the owner or the range is past its end.
    pub fn view(&self, input: &Input) -> Result<Bytes> {
        let source = match self.owner {
            FieldOwner::Payload => &input.payload,
            FieldOwner::Addition => {
                &input
                    .addition
                    .as_ref()
                    .ok_or(Error::Invalid("absent text addition"))?
                    .bytes
            }
            FieldOwner::CodecPrivate => input
                .codec_private
                .as_ref()
                .ok_or(Error::Invalid("absent text configuration"))?,
        };
        source.slice(self.range.clone())
    }
}

/// A tx3g modifier box, kept unread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Modifier {
    /// The four-character box type.
    pub kind: [u8; 4],
    /// Byte range of the whole box in the payload, its size and type
    /// included.
    pub range: Range<usize>,
}

/// The text of one input, with the input itself and exact times.
#[derive(Debug)]
pub struct Cue {
    /// The input, unchanged.
    pub input: Input,
    /// Byte range of the whole sample in the payload.
    pub raw_range: Range<usize>,
    /// Byte range of the text in the payload: for tx3g the bytes its length
    /// counts, a byte-order mark included; otherwise the whole payload.
    pub text_range: Range<usize>,
    /// The text as UTF-8, sharing the payload's storage when it is UTF-8.
    /// Strict character decoding only: no NFC, line-ending, markup, entity,
    /// ASS, timestamp or whitespace rewriting. Opaque sources have no view.
    pub decoded_utf8: Option<Bytes>,
    /// The encoding the text was decoded with: tx3g resolves to
    /// [`Encoding::Utf8`] or [`Encoding::Utf16Be`], ASS is
    /// [`Encoding::Opaque`].
    pub effective_encoding: Encoding,
    /// The WebVTT cue identifier, in the addition; `None` when empty or
    /// absent.
    pub identifier: Option<Field>,
    /// The WebVTT cue settings, in the addition; `None` when empty or absent.
    pub settings: Option<Field>,
    /// The WebVTT comments, in the addition; `None` when empty or absent.
    pub comments: Option<Field>,
    /// Presentation start; `None` when unknown.
    pub start: Option<Time>,
    /// Start plus duration; `None` unless both are known.
    pub end: Option<Time>,
    /// Duration; `None` when unknown.
    pub duration: Option<Time>,
    /// The tx3g modifier boxes in sample order; empty for other formats.
    pub modifiers: BudgetVec<Modifier>,
}

// Backing allocations stay charged by Bytes/BudgetVec. This guards only the
// complete inline value, on every target, and requires review if it grows.
const _: () = assert!(size_of::<Cue>() <= 672);
