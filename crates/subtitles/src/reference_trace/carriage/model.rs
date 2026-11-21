use crate::compat::document::Syntax;
use crate::text::Input;
use triergon_core::{BudgetVec, Bytes, SourceId};

/// The clock a caller gives a sample's packet, as FFmpeg's packet fields hold
/// it: an unreduced time base and raw ticks. It must state the same times as
/// the sample's [`Timing`](crate::text::Timing); unknown and zero stay apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallerClock {
    /// Time base numerator; not negative.
    pub numerator: i32,
    /// Time base denominator; positive.
    pub denominator: i32,
    /// Presentation time in ticks, if known.
    pub pts: Option<i64>,
    /// Decode time in ticks, if known.
    pub dts: Option<i64>,
    /// Duration in ticks, if known.
    pub duration: Option<i64>,
}

/// Bounds of one session input; a larger one is refused with
/// [`Error::Limit`](triergon_core::Error::Limit).
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Bytes of a sample payload or a document.
    pub payload_bytes: usize,
    /// Bytes of a sample's codec configuration.
    pub config_bytes: usize,
    /// Bytes of a sample's codec tag.
    pub tag_bytes: usize,
    /// Bytes of a sample's charset label or a document's file name.
    pub label_bytes: usize,
    /// Bytes of a sample's block addition.
    pub addition_bytes: usize,
    /// Packets one input becomes.
    pub packets: usize,
    /// Bytes of all packets together, and of a document's UTF-8 conversion.
    pub total_packet_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            payload_bytes: 16 * 1024 * 1024,
            config_bytes: 8 * 1024 * 1024,
            tag_bytes: 8 * 1024 * 1024,
            label_bytes: 8 * 1024 * 1024,
            addition_bytes: 8 * 1024 * 1024,
            packets: 4096,
            total_packet_bytes: 32 * 1024 * 1024,
        }
    }
}

/// One side data entry of a stream or packet.
#[derive(Debug)]
pub struct SideData {
    /// FFmpeg's side data type number: 16 for a WebVTT identifier, 17 for
    /// WebVTT settings.
    pub side_data_type: i32,
    /// The entry's bytes.
    pub bytes: Bytes,
}

/// One metadata entry of a stream.
#[derive(Debug)]
pub struct Metadata {
    /// The key.
    pub key: Bytes,
    /// The value.
    pub value: Bytes,
}

/// The stream FFmpeg reports for an input. The source's own codec tag,
/// charset and addition stay in [`Snapshot::input`] and are never mapped into
/// these fields.
#[derive(Debug)]
pub struct Stream {
    /// Stream index; always 0.
    pub index: u32,
    /// Media type number; 3 for subtitles.
    pub codec_type: i32,
    /// FFmpeg's codec identifier number.
    pub codec_id: i32,
    /// FFmpeg's codec name: `subrip`, `webvtt`, `mov_text`, `text` or `ass`.
    pub codec_name: &'static str,
    /// Codec tag; always 0.
    pub codec_tag: u32,
    /// Time base as numerator and denominator.
    pub time_base: [i32; 2],
    /// Start time in ticks; `i64::MIN` for none.
    pub start_raw: i64,
    /// Duration in ticks; `i64::MIN` for none.
    pub duration_raw: i64,
    /// Disposition flags; always 0.
    pub disposition: i32,
    /// The codec configuration, if the input has one.
    pub extradata: Option<Bytes>,
    /// Side data of the stream's codec parameters; always empty.
    pub coded_side_data: BudgetVec<SideData>,
    /// Stream metadata; always empty.
    pub metadata: BudgetVec<Metadata>,
}

/// One packet FFmpeg reads for an input.
#[derive(Debug)]
pub struct Packet {
    /// The packet's place among the input's packets, from 0.
    pub ordinal: u32,
    /// Stream index; always 0.
    pub stream: i32,
    /// The packet's bytes.
    pub data: Bytes,
    /// Byte position in the source; -1 for a sample.
    pub position_raw: i64,
    /// Packet flags: 1 (key) for a document cue, 0 for a sample.
    pub flags: i32,
    /// Whether the packet has no buffer; always false.
    pub buffer_is_null: bool,
    /// Presentation time in ticks; `i64::MIN` for none.
    pub pts_raw: i64,
    /// Decode time in ticks; `i64::MIN` for none.
    pub dts_raw: i64,
    /// Duration in ticks; 0 for none, negative values kept.
    pub duration_raw: i64,
    /// The packet's own time base; `[0, 1]` for a document cue.
    pub packet_time_base: [i32; 2],
    /// The stream's time base.
    pub stream_time_base: [i32; 2],
    /// The packet's side data, in order.
    pub side_data: BudgetVec<SideData>,
}

/// A whole SubRip or WebVTT document: its bytes, the syntax its caller
/// declares and its source facts. The document is read as the file name of
/// [`Operation::Document`] and its first line select, as FFmpeg probes it.
#[derive(Clone, Debug)]
pub struct DocumentInput {
    /// The syntax the caller declares.
    pub syntax: Syntax,
    /// The document's bytes.
    pub payload: Bytes,
    /// The source the document comes from.
    pub source_id: SourceId,
    /// The caller's input generation.
    pub generation: u64,
    /// The caller's input sequence number.
    pub sequence: u64,
    /// The caller's configuration epoch.
    pub configuration_epoch: u64,
}

/// What an operation reads: one demuxed sample or one whole document.
#[derive(Clone, Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "The input moves through the session inline, without an allocation."
)]
pub enum Source {
    /// One demuxed text sample.
    Sample(Input),
    /// One whole document.
    Document(DocumentInput),
}

/// The outcome of one input, returned unchanged with every packet FFmpeg
/// reports for it.
#[derive(Debug)]
pub struct Snapshot {
    /// The input, unchanged.
    pub input: Source,
    /// The operation the caller asked for, unchanged.
    pub operation: Operation,
    /// The format a document opened as; `None` for a sample or a document
    /// that failed to open.
    pub format: Option<FormatFacts>,
    /// The stream; `None` for a document that failed to open.
    pub stream: Option<Stream>,
    /// The packets, in the order FFmpeg reads them.
    pub packets: BudgetVec<Packet>,
    /// The calls FFmpeg's API made for a document and what each returned;
    /// empty for a sample.
    pub progress: BudgetVec<Progress>,
    /// The error code opening a document returned, if it failed.
    pub error_code: Option<i32>,
}

/// How far a document operation reads its stream before reading packets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discovery {
    /// Only the format header.
    Header,
    /// The header and FFmpeg's stream information pass.
    StreamInfo,
}

/// The operation applied to an input.
#[derive(Clone, Debug)]
pub enum Operation {
    /// A sample as one packet with the caller's clock.
    Sample(CallerClock),
    /// A document opened from a file name, which selects the syntax as
    /// FFmpeg's probing does: a name ending in `.vtt` reads as WebVTT.
    Document {
        /// How far the stream is read before its packets.
        discovery: Discovery,
        /// The file name given when opening.
        file_name: Bytes,
    },
}

/// The format a document opened as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatFacts {
    /// FFmpeg's format name: `srt` or `webvtt`.
    pub name: &'static str,
    /// Probe size in bytes.
    pub probe_size: i64,
    /// Maximum analysis duration; 0.
    pub max_analyze_duration: i64,
}

/// Which call of FFmpeg's API a [`Progress`] entry records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressKind {
    /// Opening the input.
    FormatOpen,
    /// Reading stream information.
    Discovery,
    /// Reading a packet.
    ReadFrame,
}

/// One call of FFmpeg's API and what it returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    /// Which call.
    pub kind: ProgressKind,
    /// For a packet read, the packet's ordinal.
    pub ordinal: Option<u32>,
    /// The return code, if the call was made.
    pub returned: Option<i32>,
}

// Inline values are bounded separately from the charged immutable allocations.
const _: () = assert!(size_of::<Snapshot>() <= 2048);

/// What a [`Session::poll`](super::Session::poll) did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// No input is pending; provide one.
    NeedInput,
    /// The work budget ran out; poll again.
    Yield,
    /// A snapshot is ready; take it.
    Ready,
    /// The snapshot has not been taken.
    NeedOutput,
    /// Input has finished and every snapshot was taken.
    End,
}
