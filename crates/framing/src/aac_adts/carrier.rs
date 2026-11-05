//! Separately named carrier output; a raw unit is never an audio certificate.
use super::{AacAdtsHeader, AacAdtsStreamLimits, AacAdtsStreamReader};
use crate::AacSource;
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputRequest, InputToken, InputWindow, Result, SourceCapabilities,
    SourceFailure, SourceId, Step, WorkBudget,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AacAdtsCarrierBoundary {
    DeclaredEnvelope { header_offset: usize },
    ExplicitEofFragment,
    FollowingEnvelope,
    ParserLengthCandidate,
}
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
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    pub const fn source(&self) -> AacSource {
        self.source
    }
    pub const fn header(&self) -> Option<AacAdtsHeader> {
        self.header
    }
    pub const fn boundary(&self) -> AacAdtsCarrierBoundary {
        self.boundary
    }
    /// End of bytes actually inspected by this reader. Recovery may inspect a
    /// following header beyond the opaque prefix it publishes.
    pub const fn inspection_end(&self) -> u64 {
        self.inspection_end
    }
    /// Encountered selected-source diagnostic at this stream position, including
    /// real recovery lookahead through inspection_end. It does not certify the
    /// returned raw unit as a complete frame or erase an existing Strict error.
    pub const fn selected_error(&self) -> Option<Error> {
        self.selected_error
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacAdtsCarrierLimits {
    pub maximum_packet_bytes: usize,
    pub maximum_packets: usize,
    pub maximum_input_window: usize,
}

pub struct AacAdtsCarrierReader {
    pub(crate) inner: AacAdtsStreamReader,
}
impl AacAdtsCarrierReader {
    /// Native-parser packet view. Leading bytes are emitted separately when a
    /// complete following parser candidate establishes their physical extent.
    /// The default carrier view instead retains that prefix in one source span.
    /// This compatibility policy may recognize nonzero layer bits, retaining
    /// an explicit invalid-source diagnostic rather than validating the syntax.
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
    pub fn request(&self) -> InputRequest {
        self.inner.request()
    }
    pub fn token(&self) -> InputToken {
        self.inner.token()
    }
    pub fn generation(&self) -> u64 {
        self.inner.generation()
    }
    pub fn position(&self) -> u64 {
        self.inner.position()
    }
    pub fn completed_packets(&self) -> usize {
        self.inner.completed_frames()
    }
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.inner.provide(window)
    }
    pub fn provide_owned(&mut self, window: InputWindow) -> Result<()> {
        self.inner.provide_owned(window)
    }
    pub fn provide_inline(
        &mut self,
        source: SourceId,
        token: InputToken,
        offset: u64,
        bytes: &[u8],
    ) -> Result<()> {
        self.inner.provide_inline(source, token, offset, bytes)
    }
    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        self.inner.finish(source, token)
    }
    pub fn fail(
        &mut self,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        self.inner.fail(source, token, failure)
    }
    pub fn reset(&mut self, offset: u64) -> Result<InputRequest> {
        self.inner.reset(offset)
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<AacAdtsCarrier>> {
        self.inner.step_carrier(work, cancel)
    }
}
