mod annex_b;
mod length;

pub(crate) use annex_b::AnnexBNalReader;
use length::LengthNalReader;

use crate::nal::{Codec, NalHeader};
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputRequest, InputToken, InputWindow, Result, SourceCapabilities,
    SourceFailure, SourceId, Step, WorkBudget,
};

/// Width of an explicitly declared unsigned big-endian length field.
/// All widths 1..=4 are supported by this wire contract. Accepting width 3 here
/// does not assert that a particular ISO sample-entry/configuration permits it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LengthSize(u8);

impl LengthSize {
    /// A length field of `bytes` bytes. Fails with [`Error::Invalid`] unless `bytes` is
    /// 1 to 4.
    pub fn new(bytes: u8) -> Result<Self> {
        if !(1..=4).contains(&bytes) {
            return Err(Error::Invalid("NAL length width must be 1 through 4"));
        }
        Ok(Self(bytes))
    }

    /// The width a two-bit `lengthSizeMinusOne` field codes.
    pub(crate) const fn from_minus_one(bits: u8) -> Self {
        Self((bits & 3) + 1)
    }

    /// The width in bytes, 1 to 4.
    pub const fn bytes(self) -> u8 {
        self.0
    }
}

/// How the NAL units of a stream or sample are delimited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// Each NAL unit follows a big-endian length field of this width, as in the
    /// samples of ISO/IEC 14496-15.
    LengthPrefixed(LengthSize),
    /// Each NAL unit follows a `00 00 01` or `00 00 00 01` start code (H.264
    /// and H.265 Annex B).
    AnnexB,
}

/// Bounds of a [`NalReader`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReaderLimits {
    /// The most bytes one NAL unit may hold. A [`NalReader`] reserves this much
    /// for each Annex B unit. Must not be zero.
    pub maximum_nal_bytes: usize,
    /// The most zero bytes accepted before a start code, beyond the code's own,
    /// and at the end of an Annex B source. Not used with length prefixes.
    pub maximum_padding_bytes: usize,
    /// The most NAL units read from the source. Must not be zero.
    pub maximum_nal_units: usize,
    /// The most bytes one input window may carry: [`InputRequest::maximum`] is
    /// never above it. Must not be zero.
    pub maximum_input_window: usize,
}

impl ReaderLimits {
    pub(crate) fn validate(self) -> Result<()> {
        if self.maximum_nal_bytes == 0
            || self.maximum_nal_units == 0
            || self.maximum_input_window == 0
        {
            return Err(Error::Invalid("zero NAL reader policy"));
        }
        self.maximum_padding_bytes
            .checked_add(3)
            .ok_or(Error::Overflow)?;
        Ok(())
    }
}

/// The start code before an Annex B NAL unit, as it lies in the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnnexBPrefix {
    /// Source offset of the start code's first byte, its optional `zero_byte`
    /// included and leading padding excluded.
    pub offset: u64,
    /// Zero bytes before the start code of the first NAL unit read. Zero for every
    /// later unit: the zeros before its start code are the `trailing_zero_bytes` of
    /// the unit before it.
    pub leading_zero_bytes: usize,
    /// Length of the start code in bytes: 3, or 4 with its `zero_byte`.
    pub start_code_bytes: u8,
}

/// Where the framing bytes of a NAL unit lie in its source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NalFraming {
    /// A length field before the NAL unit.
    LengthPrefixed {
        /// Source offset of the length field's first byte.
        prefix_offset: u64,
        /// Width of the length field.
        length_size: LengthSize,
        /// The field's value: the NAL unit's length in bytes, never zero.
        declared_length: u32,
    },
    /// A start code before the NAL unit.
    AnnexB(AnnexBPrefix),
}

impl From<AnnexBPrefix> for NalFraming {
    fn from(prefix: AnnexBPrefix) -> Self {
        Self::AnnexB(prefix)
    }
}

/// Where a NAL unit lies in its source, how it is framed there and what its
/// fixed header says. `F` is the framing record: [`NalFraming`] from a
/// [`NalReader`], [`AnnexBPrefix`] in a [`RawAccessUnit`](crate::nal::RawAccessUnit).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NalFacts<F = NalFraming> {
    /// The source the NAL unit was read from.
    pub source: SourceId,
    /// The length field or start code before the NAL unit.
    pub framing: F,
    /// Source offset of the NAL unit's first byte, directly after its length
    /// field or start code.
    pub payload_offset: u64,
    /// Length of the NAL unit in bytes, its header included.
    pub payload_length: usize,
    /// Zero bytes between the end of the NAL unit and the next start code or
    /// the end of the source (`trailing_zero_8bits`). Always zero with length
    /// prefixes.
    pub trailing_zero_bytes: usize,
    /// The NAL unit's fixed header.
    pub header: NalHeader,
}

/// One complete NAL unit and where it lay in its source; `F` as in [`NalFacts`].
#[derive(Clone, Debug)]
pub struct NalUnit<F = NalFraming> {
    bytes: Bytes,
    facts: NalFacts<F>,
}

impl<F: Copy> NalUnit<F> {
    /// The NAL unit from its header on, without length field, start code or
    /// zero padding.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Where the NAL unit lay in its source and what its header says.
    pub const fn facts(&self) -> NalFacts<F> {
        self.facts
    }

    /// The config parser fed this exact immutable source starting at offset
    /// zero. Rebind the validated NAL to that source span so a configuration
    /// does not retain one maximum-sized scratch allocation per parameter set.
    pub(crate) fn configuration_view(mut self, pool: &Budget, source: &Bytes) -> Result<Self> {
        if !self.bytes.belongs_to(pool) || !source.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        let start = usize::try_from(self.facts.payload_offset).map_err(|_| Error::Overflow)?;
        let end = start
            .checked_add(self.facts.payload_length)
            .ok_or(Error::Overflow)?;
        let bytes = source.slice(start..end)?;
        if NalHeader::parse(self.facts.header.codec(), bytes.as_ref())? != self.facts.header {
            return Err(Error::Invalid("configuration source/header mismatch"));
        }
        self.bytes = bytes;
        Ok(self)
    }
}

impl From<NalUnit<AnnexBPrefix>> for NalUnit {
    fn from(unit: NalUnit<AnnexBPrefix>) -> Self {
        let NalFacts {
            source,
            framing,
            payload_offset,
            payload_length,
            trailing_zero_bytes,
            header,
        } = unit.facts;
        Self {
            bytes: unit.bytes,
            facts: NalFacts {
                source,
                framing: framing.into(),
                payload_offset,
                payload_length,
                trailing_zero_bytes,
                header,
            },
        }
    }
}

/// A complete-NAL reader over one explicitly completed source entity. No NAL
/// is emitted until its boundary, size and fixed header have been checked.
///
/// Length-prefixed NALs reserve their declared length once. Annex-B NALs reserve
/// `maximum_nal_bytes` once because their length is not known in advance. This
/// deliberate tradeoff prevents hidden vector-growth copies. Output clones retain that capacity.
#[derive(Debug)]
pub struct NalReader(Framed);

/// One reader per framing; each call dispatches once.
#[derive(Debug)]
enum Framed {
    Length(LengthNalReader),
    AnnexB(AnnexBNalReader),
}

impl NalReader {
    /// A reader at offset zero of `source`. Its input windows and NAL units are charged
    /// to `pool`. Fails with [`Error::Invalid`] when a limit other than
    /// `maximum_padding_bytes` is zero.
    pub fn new(
        pool: &Budget,
        codec: Codec,
        framing: Framing,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: ReaderLimits,
    ) -> Result<Self> {
        Ok(Self(match framing {
            Framing::LengthPrefixed(width) => Framed::Length(LengthNalReader::new(
                pool,
                codec,
                width,
                source,
                capabilities,
                limits,
            )?),
            Framing::AnnexB => Framed::AnnexB(AnnexBNalReader::new(
                pool,
                codec,
                source,
                capabilities,
                limits,
                false,
            )?),
        }))
    }

    /// The request for the bytes the reader takes next, as
    /// [`step`](Self::step) returns it with `NeedInput`.
    pub fn request(&self) -> InputRequest {
        match &self.0 {
            Framed::Length(reader) => reader.request(),
            Framed::AnnexB(reader) => reader.request(),
        }
    }
    /// The token an answer to the current request must carry.
    pub fn token(&self) -> InputToken {
        match &self.0 {
            Framed::Length(reader) => reader.token(),
            Framed::AnnexB(reader) => reader.token(),
        }
    }
    /// Source offset of the next byte the reader takes from its input.
    pub fn position(&self) -> u64 {
        match &self.0 {
            Framed::Length(reader) => reader.position(),
            Framed::AnnexB(reader) => reader.position(),
        }
    }

    /// Answers the current request with `window`, as
    /// [`InputCursor::provide`](triergon_core::InputCursor::provide) does: a refused
    /// window changes nothing. A failed reader returns its error.
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        match &mut self.0 {
            Framed::Length(reader) => reader.provide(window),
            Framed::AnnexB(reader) => reader.provide(window),
        }
    }

    /// [`provide`](Self::provide) for a window the caller gives away; a refused window
    /// is dropped.
    pub fn provide_owned(&mut self, window: InputWindow) -> Result<()> {
        match &mut self.0 {
            Framed::Length(reader) => reader.provide_owned(window),
            Framed::AnnexB(reader) => reader.provide_owned(window),
        }
    }

    /// Answers the current request with the end of the source, as
    /// [`InputCursor::finish`](triergon_core::InputCursor::finish) does: a source that
    /// ends before its known length fails the reader with [`Error::Truncated`].
    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        match &mut self.0 {
            Framed::Length(reader) => reader.finish(source, token),
            Framed::AnnexB(reader) => reader.finish(source, token),
        }
    }

    /// Answers the current request with a failure, as
    /// [`InputCursor::fail`](triergon_core::InputCursor::fail) does: every later call
    /// returns [`Error::Transport`] or [`Error::Cancelled`], as `failure` says.
    pub fn fail(
        &mut self,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        match &mut self.0 {
            Framed::Length(reader) => reader.fail(source, token, failure),
            Framed::AnnexB(reader) => reader.fail(source, token, failure),
        }
    }

    /// Reads towards the next NAL unit. `Ready` carries one complete unit;
    /// `NeedInput` carries the request to answer with
    /// [`provide`](Self::provide), [`finish`](Self::finish) or
    /// [`fail`](Self::fail); `End` follows the last unit of a finished source.
    ///
    /// Work units charge each input-byte visit (including its constant-size
    /// classification/copy), each deferred Annex-B payload-byte append, and
    /// each reserve/freeze attempt. No growth copies occur after reserve.
    /// Backpressure/allocation errors are retryable with the same parser;
    /// all content, transport and cancellation failures are sticky.
    ///
    /// Fails with [`Error::Invalid`] for a length field of zero, anything but zero
    /// bytes before the first start code, a nonzero byte after `00 00 00`, an Annex B
    /// unit of no bytes and a refused NAL header; [`Error::Truncated`] when the source
    /// ends inside a length field, a length-prefixed unit or the zeros before a first
    /// start code, when a length field declares more bytes than a source of known
    /// length has left, and for a unit shorter than its header; [`Error::Limit`] past
    /// `maximum_nal_bytes`, `maximum_padding_bytes` or `maximum_nal_units`;
    /// [`Error::Cancelled`] when `cancel` is set.
    pub fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step<NalUnit>> {
        match &mut self.0 {
            Framed::Length(reader) => reader.step(work, cancel),
            Framed::AnnexB(reader) => Ok(match reader.step(work, cancel)? {
                Step::Ready(unit) => Step::Ready(unit.into()),
                Step::NeedInput(request) => Step::NeedInput(request),
                Step::Yield => Step::Yield,
                Step::Backpressure(never) => match never {},
                Step::End(location) => Step::End(location),
            }),
        }
    }
}
