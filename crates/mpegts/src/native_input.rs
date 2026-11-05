//! Pin-bound input startup evidence. This is not transport validation or
//! the normative source position. No native implementation body was consulted.
use crate::{PacketFormat, TransportPacket};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Bytes, Error, InputCursor, InputToken, InputWindow, Result, SourceId, WorkBudget,
};

/// Explicit result of the observed default-callback, finite aligned operation.
/// Other delivery, mixed stride, damaged sync and ambiguous grids are unproven.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativePositionBasis {
    SyncStart,
    CarrierStart,
}

#[derive(Clone, Copy, Debug)]
enum Extent {
    Finite(u64),
    Prefix(Option<u64>),
}
impl Extent {
    fn permits(self, end: u64) -> bool {
        match self {
            Self::Finite(limit) | Self::Prefix(Some(limit)) => end <= limit,
            Self::Prefix(None) => true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct NativeInputContext {
    source: SourceId,
    token: InputToken,
    format: PacketFormat,
    owner: Bytes,
    extent: Extent,
    basis: NativePositionBasis,
    inspected_bytes: usize,
}
impl NativeInputContext {
    pub fn basis(&self) -> NativePositionBasis {
        self.basis
    }
    /// The startup probe establishes a carrier grid, with Source reads still open.
    pub fn is_prefix_v2(&self) -> bool {
        matches!(self.extent, Extent::Prefix(_))
    }
    pub fn inspected_bytes(&self) -> usize {
        self.inspected_bytes
    }
    pub fn backing_bytes(&self) -> &Bytes {
        &self.owner
    }
    /// Sequential consumption retains the request token. A range/reset/entity
    /// transaction invalidates it independently of clock/configuration epochs.
    pub fn validate_cursor(&self, input: &InputCursor) -> Result<()> {
        if input.source() != self.source {
            return Err(Error::SourceMismatch);
        }
        if input.token() != self.token {
            return Err(Error::Invalid("stale native startup request"));
        }
        if !self.owner.belongs_to(input.budget()) {
            return Err(Error::ForeignPool);
        }
        if !self.extent.permits(input.position()) {
            return Err(Error::Invalid("native input beyond observed entity EOF"));
        }
        Ok(())
    }
    pub(crate) fn binding(&self) -> NativeInputBinding {
        NativeInputBinding {
            source: self.source,
            token: self.token.clone(),
            format: self.format,
            extent: self.extent,
            basis: self.basis,
        }
    }
}

/// Sealed scalar facts retained by assembly. No input allocation is kept alive
/// after startup inspection merely to remember its position interpretation.
#[derive(Clone, Debug)]
pub(crate) struct NativeInputBinding {
    source: SourceId,
    token: InputToken,
    format: PacketFormat,
    extent: Extent,
    basis: NativePositionBasis,
}
impl NativeInputBinding {
    pub(crate) fn continued_media_entity_v1(
        &self,
        end: &crate::MediaEntityEndV1,
        input: &InputCursor,
    ) -> Result<Self> {
        if self.source != end.location.source
            || self.token != end.token
            || self.format != end.format
            || !matches!(self.extent, Extent::Finite(bytes) if bytes == end.location.offset)
        {
            return Err(Error::Invalid("native startup entity End mismatch"));
        }
        let bytes = input.capabilities().length.ok_or(Error::Unsupported(
            "input-bound native media continuation requires a finite entity length",
        ))?;
        usize::try_from(bytes).map_err(|_| Error::Overflow)?;
        Ok(Self {
            source: input.source(),
            token: input.token(),
            format: self.format,
            extent: Extent::Finite(bytes),
            basis: self.basis,
        })
    }
    pub(crate) fn validate_cursor(&self, input: &InputCursor) -> Result<()> {
        if input.source() != self.source {
            return Err(Error::SourceMismatch);
        }
        if input.token() != self.token {
            return Err(Error::Invalid("stale native startup request"));
        }
        if !self.extent.permits(input.position()) {
            return Err(Error::Invalid("native input beyond observed entity EOF"));
        }
        Ok(())
    }
    pub(crate) fn position(&self, packet: &TransportPacket) -> Result<i64> {
        if packet.source_id != Some(self.source) {
            return Err(Error::SourceMismatch);
        }
        let end = packet
            .source_offset
            .checked_add(packet.source.len() as u64)
            .ok_or(Error::Overflow)?;
        if !self.extent.permits(end) || packet.source.len() != self.format.bytes() {
            return Err(Error::Invalid("native startup packet range/format"));
        }
        let prefix = if self.basis == NativePositionBasis::SyncStart {
            packet.prefix.as_ref().map_or(0, Bytes::len)
        } else {
            0
        };
        i64::try_from(
            packet
                .source_offset
                .checked_add(prefix as u64)
                .ok_or(Error::Overflow)?,
        )
        .map_err(|_| Error::Overflow)
    }
}

#[derive(Debug)]
pub enum NativeInputStep {
    NeedEntityEof,
    Yield,
    Ready(NativeInputContext),
}

/// One borrowed, already core-validated initial window. No allocation/copy,
/// second cursor, byte consumption, source reset or retained output queue.
/// The prefix profile can use a caller-assembled initial window.
#[derive(Debug)]
pub struct NativeInputBootstrap {
    source: SourceId,
    token: InputToken,
    format: PacketFormat,
    owner: Bytes,
    eof: bool,
    extent: Extent,
    inspected: usize,
    scores188: [u16; 188],
    scores192: [u16; 192],
    scores204: [u16; 204],
    bad_sync: bool,
    ready: Option<NativeInputContext>,
}
impl NativeInputBootstrap {
    pub const MAX_BORROWED_BYTES: usize = 16 * 1024;
    pub const M2TS_PREFIX_BYTES_V2: usize = 8192;

    pub fn new(input: &InputCursor, window: &InputWindow, format: PacketFormat) -> Result<Self> {
        Self::with_extent(
            input,
            window,
            format,
            Extent::Finite(window.bytes().len() as u64),
        )
    }

    /// Inspect a complete initial M2TS probe window already provided to the
    /// actual cursor. The selected grid must be decisive within that window.
    pub fn new_m2ts_prefix_v2(input: &InputCursor, window: &InputWindow) -> Result<Self> {
        if window.bytes().len() != Self::M2TS_PREFIX_BYTES_V2 {
            return Err(Error::Invalid("native M2TS prefix window length"));
        }
        Self::with_extent(
            input,
            window,
            PacketFormat::M2ts192,
            Extent::Prefix(input.capabilities().length),
        )
    }

    fn with_extent(
        input: &InputCursor,
        window: &InputWindow,
        format: PacketFormat,
        extent: Extent,
    ) -> Result<Self> {
        if input.position() != 0 || window.offset != 0 {
            return Err(Error::Invalid("native startup requires initial input"));
        }
        if window.source != input.source() {
            return Err(Error::SourceMismatch);
        }
        if window.token != input.token() {
            return Err(Error::Invalid("stale native startup request"));
        }
        if !window.bytes().belongs_to(input.budget()) {
            return Err(Error::ForeignPool);
        }
        let retained = input
            .retained_window()
            .ok_or(Error::Invalid("native startup window not provided"))?;
        if !retained.shares_storage_with(window.bytes())
            || (retained.len() != window.bytes().len()
                || retained.as_ref().as_ptr() != window.bytes().as_ref().as_ptr())
        {
            return Err(Error::Invalid("native startup window not provided"));
        }
        if window.bytes().len() > Self::MAX_BORROWED_BYTES {
            return Err(Error::Limit {
                requested: window.bytes().len(),
                maximum: Self::MAX_BORROWED_BYTES,
            });
        }
        Ok(Self {
            source: window.source,
            token: window.token.clone(),
            format,
            owner: window.bytes().clone(),
            eof: false,
            extent,
            inspected: 0,
            scores188: [0; 188],
            scores192: [0; 192],
            scores204: [0; 204],
            bad_sync: false,
            ready: None,
        })
    }
    fn validate(&self, input: &InputCursor) -> Result<()> {
        if input.source() != self.source {
            return Err(Error::SourceMismatch);
        }
        if input.token() != self.token {
            return Err(Error::Invalid("stale native startup request"));
        }
        Ok(())
    }
    /// Caller reports an actual positive-size read returning EOF, never a
    /// declared length or zero-size request. Core finish is deferred until the
    /// real prefix has drained, preserving strict-source error/progress order.
    pub fn observe_entity_eof(
        &mut self,
        input: &InputCursor,
        source: SourceId,
        token: &InputToken,
        end: u64,
    ) -> Result<()> {
        self.validate(input)?;
        if source != self.source {
            return Err(Error::SourceMismatch);
        }
        if *token != self.token || !matches!(self.extent, Extent::Finite(bytes) if bytes == end) {
            return Err(Error::Invalid("native startup EOF boundary"));
        }
        if input.capabilities().length.is_some_and(|n| n != end) {
            return Err(Error::Unsupported(
                "native startup inconsistent declared length",
            ));
        }
        self.eof = true;
        Ok(())
    }
    pub fn inspected_bytes(&self) -> usize {
        self.inspected
    }
    pub fn poll(
        &mut self,
        input: &InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<NativeInputStep> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        self.validate(input)?;
        if let Some(context) = &self.ready {
            return Ok(NativeInputStep::Ready(context.clone()));
        }
        let finite = matches!(self.extent, Extent::Finite(_));
        if finite && !self.eof {
            return Ok(NativeInputStep::NeedEntityEof);
        }
        let width = self.format.bytes();
        if finite && (self.owner.is_empty() || self.owner.len() % width != 0) {
            return Err(Error::Unsupported(
                "native startup incomplete aligned finite entity",
            ));
        }
        let sync = width - 188;
        while self.inspected < self.owner.len() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let count = work.take((self.owner.len() - self.inspected).min(64) as u64) as usize;
            if count == 0 {
                return Ok(NativeInputStep::Yield);
            }
            let start = self.inspected;
            let end = start + count;
            let mut next_sync = start + (sync + width - start % width) % width;
            for (offset, &byte) in self.owner.as_ref()[start..end].iter().enumerate() {
                let i = start + offset;
                if i == next_sync {
                    self.bad_sync |= byte != 0x47;
                    next_sync += width;
                }
                // Score only sync bytes with a complete transport header.
                if byte == 0x47 && i + 4 <= self.owner.len() {
                    self.scores188[i % 188] += 1;
                    self.scores192[i % 192] += 1;
                    self.scores204[i % 204] += 1;
                }
            }
            self.inspected = end;
        }
        if self.bad_sync {
            return Err(Error::Unsupported(
                "native startup damaged/mixed physical grid",
            ));
        }
        let s188 = self.scores188.iter().copied().max().unwrap_or(0);
        let s192 = self.scores192.iter().copied().max().unwrap_or(0);
        let s204 = self.scores204.iter().copied().max().unwrap_or(0);
        let basis = if finite {
            match self.format {
                PacketFormat::Ts188 if s192 < 7 && s204 < 7 => NativePositionBasis::CarrierStart,
                PacketFormat::M2ts192 if s188 < 7 && s204 < 7 => {
                    if s192 >= 7 {
                        NativePositionBasis::CarrierStart
                    } else {
                        NativePositionBasis::SyncStart
                    }
                }
                _ => return Err(Error::Unsupported("native startup competing raw sync grid")),
            }
        } else {
            // Require an unambiguous 192-byte prefix. Cap competing markers
            // and reject enough off-grid noise to obscure the selected chain.
            let markers: u32 = self.scores192.iter().map(|&n| u32::from(n)).sum();
            if s192 < 7 || s188 >= 6 || s204 >= 6 || markers > 10 * u32::from(s192) {
                return Err(Error::Unsupported("native M2TS prefix grid is ambiguous"));
            }
            NativePositionBasis::CarrierStart
        };
        let context = NativeInputContext {
            source: self.source,
            token: self.token.clone(),
            format: self.format,
            owner: self.owner.clone(),
            extent: self.extent,
            basis,
            inspected_bytes: self.inspected,
        };
        self.ready = Some(context.clone());
        Ok(NativeInputStep::Ready(context))
    }
}
