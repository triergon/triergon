//! Normal DTS14 exact-word frame acquisition with independently checked extents.
//! ETSI TS102114 V1.2.1 5.3 low14 storage and V1.6.1 5.3 normal per-frame cadence.
//! Non-byte-aligned tails and unresolved FSIZE domains remain Unsupported.
//! This is an opaque encoded-frame envelope, not audio/CRC validation.
use crate::{Dts14SyncLimits, Dts14SyncReader, DtsWordOrder};
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, InputRequest, InputToken, InputWindow, Result, SourceCapabilities, SourceFailure,
    SourceId, Step, WorkBudget,
};

pub use crate::Dts14SyncFrame as Dts14Frame;

/// One existing cursor/framing state machine; no new arena, queue or copy.
/// The legacy Dts14SyncReader constructor retains its fixed-interval policy.
/// Every boundary remains a real following normal sync or explicit entity end,
/// with complete low14 payload exactly matching the declared byte count.
pub struct Dts14FrameReader {
    inner: Dts14SyncReader,
}
impl Dts14FrameReader {
    pub fn new(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        order: DtsWordOrder,
        limits: Dts14SyncLimits,
    ) -> Result<Self> {
        Ok(Self {
            inner: Dts14SyncReader::per_frame(pool, source, capabilities, order, limits)?,
        })
    }
    pub fn generation(&self) -> u64 {
        self.inner.generation()
    }
    pub fn position(&self) -> u64 {
        self.inner.position()
    }
    pub fn request(&self) -> InputRequest {
        self.inner.request()
    }
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.inner.provide(window)
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
    pub fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step<Dts14Frame>> {
        self.inner.step(work, cancel)
    }
}
