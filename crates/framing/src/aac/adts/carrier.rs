//! An ADTS stream read as contiguous units that keep the bytes outside complete
//! frames. A unit is source bytes with a diagnostic; it does not vouch for audio.
use super::{AacAdtsFrame, AacAdtsHeader, AacAdtsStreamLimits, AacAdtsStreamReader};
use crate::aac::AacSource;
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputRequest, InputToken, InputWindow, Result, SourceCapabilities,
    SourceFailure, SourceId, Step, WorkBudget,
};

/// What ends a unit of an [`AacAdtsCarrierReader`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AacAdtsCarrierBoundary {
    /// The `aac_frame_length` of a header with `layer` 0 in the unit.
    DeclaredEnvelope {
        /// Bytes of the unit before that header: 0 when the unit starts with it,
        /// more when it was found by scanning and the bytes before it are kept in
        /// the same unit.
        header_offset: usize,
    },
    /// The end of the source, reported with `finish`: the unit is what was left,
    /// short of a complete header or frame, or without any header.
    ExplicitEofFragment,
    /// A header found by scanning: the unit is the bytes before it. Only a reader
    /// made by [`AacAdtsCarrierReader::for_parser_packets`] ends units this way.
    FollowingEnvelope,
    /// The `aac_frame_length` of the header the unit starts with, whose `layer` is
    /// not 0. Only a reader made by [`AacAdtsCarrierReader::for_parser_packets`]
    /// ends units this way.
    ParserLengthCandidate,
}
/// One unit of an ADTS stream read by [`AacAdtsCarrierReader`]: a frame, or bytes
/// that are not a complete frame, with what ends the unit and why it is not a
/// frame.
///
/// The units of a stream are contiguous: together they hold every byte read, in
/// order, each unit in an allocation of its own.
#[derive(Clone, Debug)]
pub struct AacAdtsCarrier {
    pub(crate) raw: Bytes,
    pub(crate) source: AacSource,
    pub(crate) header: Option<AacAdtsHeader>,
    pub(crate) boundary: AacAdtsCarrierBoundary,
    pub(crate) selected_error: Option<Error>,
    pub(crate) inspection_end: u64,
}
impl AacAdtsCarrier {
    /// The unit as an [`AacAdtsFrame`] that shares its bytes, when it is a complete
    /// AAC LC frame of one raw data block that starts with its header: the boundary
    /// is `DeclaredEnvelope { header_offset: 0 }`, there is no
    /// [`selected_error`](Self::selected_error) and the unit is as long as the
    /// header declares. Any other unit is returned as it was.
    pub fn into_selected_frame(self) -> std::result::Result<AacAdtsFrame, Self> {
        let Some(header) = self.header else {
            return Err(self);
        };
        if self.boundary != (AacAdtsCarrierBoundary::DeclaredEnvelope { header_offset: 0 })
            || self.selected_error.is_some()
            || self.raw.len() != usize::from(header.frame_length())
        {
            return Err(self);
        }
        AacAdtsFrame::from_envelope(self.raw.clone(), self.source, header).map_err(|_| self)
    }

    /// The unit's bytes.
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    /// Where the unit starts in its source.
    pub const fn source(&self) -> AacSource {
        self.source
    }
    /// The header the unit's extent comes from: the one that declares its frame
    /// length, or, for a unit the end of the source cut short, the one of the
    /// unfinished frame. `None` when the unit has none: bytes before a following
    /// header, or a remainder at the end of the source in which no header was found.
    pub const fn header(&self) -> Option<AacAdtsHeader> {
        self.header
    }
    /// What ends the unit.
    pub const fn boundary(&self) -> AacAdtsCarrierBoundary {
        self.boundary
    }
    /// Source offset just past the last byte the reader had read when it produced
    /// the unit. It is the unit's end, except for a unit that a following header
    /// ends: the seven bytes of that header were read to find the boundary.
    pub const fn inspection_end(&self) -> u64 {
        self.inspection_end
    }
    /// Why the bytes at this position are not a complete AAC LC frame of one raw
    /// data block, judged on everything read up to
    /// [`inspection_end`](Self::inspection_end): [`Error::Truncated`] when the source
    /// ended first, [`Error::Invalid`] for a header or frame length that breaks the
    /// syntax or for bytes without a header, [`Error::Unsupported`] for another
    /// profile or several raw data blocks. `None` only for a unit that
    /// [`into_selected_frame`](Self::into_selected_frame) converts.
    ///
    /// The diagnostic stands beside the unit's bytes: carrying them neither makes
    /// them a frame nor removes the error [`AacAdtsStreamReader`] reports for the
    /// same bytes.
    pub const fn selected_error(&self) -> Option<Error> {
        self.selected_error
    }
}

/// Bounds of an [`AacAdtsCarrierReader`]. None may be zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacAdtsCarrierLimits {
    /// Most bytes one unit may hold. A unit that its header declares longer, or
    /// more bytes than this with neither a header nor the end of the source, is
    /// [`Error::Limit`].
    pub maximum_packet_bytes: usize,
    /// Most units the reader produces between resets; a further one is
    /// [`Error::Limit`].
    pub maximum_packets: usize,
    /// Most bytes one input window may carry.
    pub maximum_input_window: usize,
}

/// Reads an ADTS stream as contiguous [`AacAdtsCarrier`] units, from input of any
/// chunking, without failing on bytes that are not frames.
///
/// A unit that starts with a plausible header (syncword `0xfff`, layer 0, a
/// sampling frequency index that names a frequency, a frame length of at least 7)
/// extends as far as that header declares, whatever its profile and block count.
/// From anything else the reader scans byte by byte for such a header. Bytes that
/// the end of the source leaves over form a last unit. Nothing is decoded and no
/// CRC is verified; each unit says why it is not a frame.
#[derive(Debug)]
pub struct AacAdtsCarrierReader {
    pub(crate) inner: AacAdtsStreamReader,
}
impl AacAdtsCarrierReader {
    /// A reader of `source` whose units follow the packets of FFmpeg's AAC parser.
    /// Bytes before a header found by scanning become a unit of their own
    /// ([`AacAdtsCarrierBoundary::FollowingEnvelope`]) once the seven bytes of
    /// that header are read; a reader made by [`new`](Self::new) keeps them in one
    /// unit with the header's frame. A header whose `layer` is not 0 also
    /// delimits a unit ([`AacAdtsCarrierBoundary::ParserLengthCandidate`]): the
    /// unit keeps [`Error::Invalid`] as its diagnostic, and its syntax is not
    /// validated.
    ///
    /// Fails when a limit is zero.
    pub fn for_parser_packets(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: AacAdtsCarrierLimits,
    ) -> Result<Self> {
        Ok(Self {
            inner: AacAdtsStreamReader::new_parser_packets(
                pool,
                source,
                capabilities,
                AacAdtsStreamLimits {
                    maximum_frame_bytes: limits.maximum_packet_bytes,
                    maximum_frames: limits.maximum_packets,
                    maximum_input_window: limits.maximum_input_window,
                },
            )?,
        })
    }
    /// A reader of `source` from offset zero, with windows charged to `pool`. Bytes
    /// before a header found by scanning stay in one unit with that header's frame
    /// ([`AacAdtsCarrierBoundary::DeclaredEnvelope`] with a nonzero
    /// `header_offset`).
    ///
    /// Fails when a limit is zero.
    pub fn new(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: AacAdtsCarrierLimits,
    ) -> Result<Self> {
        Ok(Self {
            inner: AacAdtsStreamReader::new_carrier(
                pool,
                source,
                capabilities,
                AacAdtsStreamLimits {
                    maximum_frame_bytes: limits.maximum_packet_bytes,
                    maximum_frames: limits.maximum_packets,
                    maximum_input_window: limits.maximum_input_window,
                },
            )?,
        })
    }
    /// Capacity of the mutable frame buffer, excluding buffered source bytes.
    #[doc(hidden)]
    pub fn input_wait_buffer_capacity(&self) -> usize {
        self.inner.input_wait_buffer_capacity()
    }

    /// The request for the bytes the reader needs next, at
    /// [`position`](Self::position).
    pub fn request(&self) -> InputRequest {
        self.inner.request()
    }
    /// The token an answer to the current request must carry.
    pub fn token(&self) -> InputToken {
        self.inner.token()
    }
    /// Source offset of the next byte the reader reads.
    pub fn position(&self) -> u64 {
        self.inner.position()
    }
    /// Units produced since the reader was made or last reset.
    pub fn completed_packets(&self) -> usize {
        self.inner.completed_frames()
    }
    /// Answers the current request with `window`, as
    /// [`InputCursor::provide`](triergon_core::InputCursor::provide) does: a refused
    /// window changes nothing. A failed reader returns its error.
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.inner.provide(window)
    }
    /// [`provide`](Self::provide) for a window the caller gives away; a refused window
    /// is dropped.
    pub fn provide_owned(&mut self, window: InputWindow) -> Result<()> {
        self.inner.provide_owned(window)
    }
    /// [`provide`](Self::provide) for at most
    /// [`InputCursor::INLINE_CAPACITY`](triergon_core::InputCursor::INLINE_CAPACITY)
    /// bytes, copied into the reader without an allocation.
    pub fn provide_inline(
        &mut self,
        source: SourceId,
        token: InputToken,
        offset: u64,
        bytes: &[u8],
    ) -> Result<()> {
        self.inner.provide_inline(source, token, offset, bytes)
    }
    /// Answers the current request with the end of the source, as
    /// [`InputCursor::finish`](triergon_core::InputCursor::finish) does: a source that
    /// ends before its known length fails the reader with [`Error::Truncated`], and the
    /// bytes left become no unit.
    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        self.inner.finish(source, token)
    }
    /// Answers the current request with a failure, as
    /// [`InputCursor::fail`](triergon_core::InputCursor::fail) does: every later call
    /// returns [`Error::Transport`] or [`Error::Cancelled`], as `failure` says. An
    /// unfinished unit is dropped.
    pub fn fail(
        &mut self,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        self.inner.fail(source, token, failure)
    }
    /// Starts reading the same source again at `offset`, under another token
    /// generation, and returns the request for the first bytes. Buffered bytes, an
    /// unfinished unit, the end of the source, a failure and the unit count are
    /// cleared together; a source length learned from [`finish`](Self::finish) is
    /// kept.
    ///
    /// Fails when `offset` is not zero and the source has no random access, or `offset`
    /// lies past the known length. The reader stays as it was.
    pub fn reset(&mut self, offset: u64) -> Result<InputRequest> {
        self.inner.reset(offset)
    }
    /// Reads for as long as `work` lasts. [`Step::Ready`] carries the next unit, and
    /// [`Step::End`] reports the end of the source once every byte has been produced
    /// in a unit.
    ///
    /// Fails when a unit exceeds `maximum_packet_bytes` or the units exceed
    /// `maximum_packets`, when `cancel` is set, or when the source failed or ended
    /// before its known length. The reader then returns the same error from every later
    /// call until [`reset`](Self::reset). [`Error::Backpressure`] and
    /// [`Error::Allocation`] keep its state, to be stepped again.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<AacAdtsCarrier>> {
        self.inner.step_carrier(work, cancel)
    }
}
