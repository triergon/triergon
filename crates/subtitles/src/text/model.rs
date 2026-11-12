//! Local text facts. No renderer, ASS translation or charset detection.
use std::ops::Range;
use triergon_core::{BudgetVec, Bytes, Error, Result, SourceId, SourceLocation, Time, TimeBase};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Tx3g,
    Utf8Packet,
    MatroskaWebVtt,
    OpaqueAss,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16Bom,
    Utf16Be,
    Utf16Le,
    Tx3gInBand,
    Opaque,
    /// Explicit version-pinned multibyte decoding; labels remain caller-owned.
    Multibyte(super::charset::multibyte::MultibyteCharset),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawClock {
    pub numerator: u64,
    pub denominator: u64,
    pub pts_ticks: Option<i128>,
    pub dts_ticks: Option<i128>,
    pub duration_ticks: Option<i128>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    start: Option<Time>,
    dts: Option<Time>,
    duration: Option<Time>,
    raw_clock: Option<RawClock>,
}

impl Timing {
    pub const fn unknown() -> Self {
        Self {
            start: None,
            dts: None,
            duration: None,
            raw_clock: None,
        }
    }
    pub const fn exact(start: Option<Time>, duration: Option<Time>) -> Self {
        Self {
            start,
            dts: None,
            duration,
            raw_clock: None,
        }
    }
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
    pub const fn start(self) -> Option<Time> {
        self.start
    }
    pub const fn duration(self) -> Option<Time> {
        self.duration
    }
    pub const fn dts(self) -> Option<Time> {
        self.dts
    }
    pub const fn raw_clock(self) -> Option<RawClock> {
        self.raw_clock
    }
}

#[derive(Clone, Debug)]
pub struct Addition {
    pub id: u64,
    pub bytes: Bytes,
}

#[derive(Clone, Debug)]
pub struct Input {
    pub format: Format,
    pub encoding: Encoding,
    pub payload: Bytes,
    pub codec_id: Option<Bytes>,
    pub codec_private: Option<Bytes>,
    pub charset_label: Option<Bytes>,
    pub addition: Option<Addition>,
    pub timing: Timing,
    pub source_id: SourceId,
    pub generation: u64,
    pub sequence: u64,
    pub configuration_epoch: u64,
    pub container_position: Option<SourceLocation>,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_input_bytes: usize,
    pub max_config_bytes: usize,
    pub max_label_bytes: usize,
    pub max_addition_bytes: usize,
    pub max_modifiers: usize,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldOwner {
    Payload,
    Addition,
    CodecPrivate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub owner: FieldOwner,
    pub range: Range<usize>,
}

impl Field {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Modifier {
    pub kind: [u8; 4],
    /// Entire original modifier box in the sample, including its size/type.
    pub range: Range<usize>,
}

#[derive(Debug)]
pub struct Cue {
    pub input: Input,
    pub raw_range: Range<usize>,
    pub text_range: Range<usize>,
    /// Strict character decoding only: no NFC, line-ending, markup, entity,
    /// ASS, timestamp or whitespace rewriting. Opaque sources have no view.
    pub decoded_utf8: Option<Bytes>,
    pub effective_encoding: Encoding,
    pub identifier: Option<Field>,
    pub settings: Option<Field>,
    pub comments: Option<Field>,
    pub start: Option<Time>,
    pub end: Option<Time>,
    pub duration: Option<Time>,
    pub modifiers: BudgetVec<Modifier>,
}

// Backing allocations stay charged by Bytes/BudgetVec. This guards only the
// complete inline value, on every target, and requires review if it grows.
const _: () = assert!(size_of::<Cue>() <= 672);
