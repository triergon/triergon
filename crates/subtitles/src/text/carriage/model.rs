use crate::compat::document::Syntax;
use crate::text::Input;
use triergon_core::{BudgetVec, Bytes, SourceId};

/// Explicit native caller setup, separate from the original exact source Time.
/// Ratios remain unreduced. Unknown and caller-known zero remain distinct here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallerClock {
    pub numerator: i32,
    pub denominator: i32,
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub duration: Option<i64>,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub payload_bytes: usize,
    pub config_bytes: usize,
    pub tag_bytes: usize,
    pub label_bytes: usize,
    pub addition_bytes: usize,
    pub line_bytes: usize,
    pub packets: usize,
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
            line_bytes: 64 * 1024,
            packets: 4096,
            total_packet_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub struct SideData {
    pub side_data_type: i32,
    pub bytes: Bytes,
}

#[derive(Debug)]
pub struct Metadata {
    pub key: Bytes,
    pub value: Bytes,
}

/// Public-API carriage view of the reference. Raw source codec
/// tag/charset/addition facts remain in Snapshot.input, never guessed into these
/// fields. The first native sample operation supplies no side-data/metadata.
#[derive(Debug)]
pub struct Stream {
    pub index: u32,
    pub codec_type: i32,
    pub codec_id: i32,
    pub codec_name: &'static str,
    pub codec_tag: u32,
    pub time_base: [i32; 2],
    pub start_raw: i64,
    pub duration_raw: i64,
    pub disposition: i32,
    pub extradata: Option<Bytes>,
    pub coded_side_data: BudgetVec<SideData>,
    pub metadata: BudgetVec<Metadata>,
}

#[derive(Debug)]
pub struct Packet {
    pub ordinal: u32,
    pub stream: i32,
    pub data: Bytes,
    pub position_raw: i64,
    pub flags: i32,
    pub buffer_is_null: bool,
    pub pts_raw: i64,
    pub dts_raw: i64,
    pub duration_raw: i64,
    pub packet_time_base: [i32; 2],
    pub stream_time_base: [i32; 2],
    pub side_data: BudgetVec<SideData>,
}

/// A whole SubRip or WebVTT document for the document operation: its bytes,
/// the syntax its caller declares and its source facts.
#[derive(Clone, Debug)]
pub struct DocumentInput {
    pub syntax: Syntax,
    pub payload: Bytes,
    pub source_id: SourceId,
    pub generation: u64,
    pub sequence: u64,
    pub configuration_epoch: u64,
}

/// What an operation reads: one demuxed sample or one whole document.
#[derive(Clone, Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "The input moves through the session inline, as Input did, without a new allocation."
)]
pub enum Source {
    Sample(Input),
    Document(DocumentInput),
}

#[derive(Debug)]
pub struct Snapshot {
    pub input: Source,
    pub operation: Operation,
    pub format: Option<FormatFacts>,
    pub stream: Option<Stream>,
    pub packets: BudgetVec<Packet>,
    pub progress: BudgetVec<Progress>,
    pub error_code: Option<i32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discovery {
    Header,
    StreamInfo,
}

#[derive(Clone, Debug)]
pub enum Operation {
    Sample(CallerClock),
    /// The original public AVFormat call also supplies a filename. It can
    /// influence automatic probing and is an explicit charged caller fact.
    Document {
        discovery: Discovery,
        file_name: Bytes,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatFacts {
    pub name: &'static str,
    pub probe_size: i64,
    pub max_analyze_duration: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressKind {
    FormatOpen,
    Discovery,
    ReadFrame,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub kind: ProgressKind,
    pub ordinal: Option<u32>,
    pub returned: Option<i32>,
}

// Inline values are bounded separately from the charged immutable allocations.
// These are physical-size ceilings, not measured heap/RSS claims.
const _: () = assert!(size_of::<Snapshot>() <= 2048);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    NeedInput,
    Yield,
    Ready,
    NeedOutput,
    End,
}
