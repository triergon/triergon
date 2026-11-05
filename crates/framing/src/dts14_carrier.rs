//! Explicit conventional-carrier packet view; the Strict DTS14 frame API is unchanged.
//! Low14 unpacking follows ETSI TS102114 5.3. Carrier recognition is deliberately
//! narrower than low14 sync recognition. An unrecognized carrier stays opaque;
//! no canonical syntax, audio validity or source boundary is invented for it.
use crate::{
    Dts14BitPayload, Dts14BitUnpacker, DtsCoreHeader, DtsPackingLimits, DtsSourceIdentity,
    DtsWordOrder, probe_dts_core_header,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, WorkBudget,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dts14CarrierLimits {
    pub maximum_packet_bytes: usize,
    pub maximum_packets: usize,
    pub maximum_input_window: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dts14CarrierBoundary {
    FollowingCarrierSync { offset: u64 },
    ExplicitSourceEnd,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dts14CarrierSyntax {
    /// The physical carrier signature is absent. All original bytes survive.
    OpaqueCarrier,
    /// Exact-word geometry and the existing strict header parser succeeded.
    ExactNormal(DtsCoreHeader),
    /// Carrier bytes exist, but the independently checked syntax is unsupported.
    Refused(Error),
}
/// Actual accepted input-window and reader progress, not a packet boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dts14CarrierProvision {
    pub last_window_start: u64,
    pub last_window_end: u64,
    pub consumed_through: u64,
}
#[derive(Clone, Debug)]
pub struct Dts14CarrierPacket {
    raw: Bytes,
    payload: Option<Dts14BitPayload>,
    pub identity: DtsSourceIdentity,
    pub boundary: Dts14CarrierBoundary,
    pub syntax: Dts14CarrierSyntax,
    pub provision: Dts14CarrierProvision,
}
impl Dts14CarrierPacket {
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    pub fn payload(&self) -> Option<&Dts14BitPayload> {
        self.payload.as_ref()
    }
    pub fn canonical_bytes(&self) -> Option<&Bytes> {
        self.payload.as_ref().map(Dts14BitPayload::canonical_bytes)
    }
}
#[derive(Clone, Copy)]
enum Phase {
    Reserve,
    Scan,
    Check,
    Append,
    GrowReserve,
    GrowCopy,
    GrowSwap,
    Freeze,
    Unpack,
    Publish,
}
/// Bounded stream packet acquisition for the conventional 14-bit carrier.
/// An explicit EOF flushes an opaque packet; a transport failure never does.
/// Every publication owns its bytes independently of producer/input lifetime.
/// This separately named API does not change source syntax or Strict framing.
pub struct Dts14CarrierReader {
    pool: Budget,
    cursor: Option<InputCursor>,
    last_request: InputRequest,
    order: DtsWordOrder,
    limits: Dts14CarrierLimits,
    phase: Phase,
    buffer: Option<BudgetVec<u8>>,
    growth: Option<BudgetVec<u8>>,
    growth_at: usize,
    extent: Option<usize>,
    header_probed: bool,
    last_window_start: u64,
    last_window_end: u64,
    raw: Option<Bytes>,
    unpacker: Option<Dts14BitUnpacker>,
    payload: Option<Dts14BitPayload>,
    syntax: Dts14CarrierSyntax,
    look: [u8; 6],
    look_len: usize,
    append_at: usize,
    append_count: usize,
    offset: u64,
    end: Option<SourceLocation>,
    complete: bool,
    packets: usize,
    failure: Option<Error>,
}
impl Dts14CarrierReader {
    pub fn new(
        pool: &Budget,
        source: SourceId,
        caps: SourceCapabilities,
        order: DtsWordOrder,
        limits: Dts14CarrierLimits,
    ) -> Result<Self> {
        if limits.maximum_packet_bytes == 0
            || limits.maximum_packets == 0
            || limits.maximum_input_window == 0
        {
            return Err(Error::Invalid("zero DTS carrier limit"));
        }
        let minimum = limits
            .maximum_packet_bytes
            .min(128)
            .checked_add(Bytes::owner_overhead_bytes())
            .and_then(|n| n.checked_add(Budget::control_storage_bytes()))
            .ok_or(Error::Overflow)?;
        if minimum > pool.limit() {
            return Err(Error::Limit {
                requested: minimum,
                maximum: pool.limit(),
            });
        }
        let cursor = InputCursor::new(pool, source, caps, limits.maximum_input_window)?;
        let last_request = cursor.request();
        Ok(Self {
            pool: pool.clone(),
            cursor: Some(cursor),
            last_request,
            order,
            limits,
            phase: Phase::Reserve,
            buffer: None,
            growth: None,
            growth_at: 0,
            extent: None,
            header_probed: false,
            last_window_start: 0,
            last_window_end: 0,
            raw: None,
            unpacker: None,
            payload: None,
            syntax: Dts14CarrierSyntax::OpaqueCarrier,
            look: [0; 6],
            look_len: 0,
            append_at: 0,
            append_count: 0,
            offset: 0,
            end: None,
            complete: false,
            packets: 0,
            failure: None,
        })
    }
    pub fn request(&self) -> InputRequest {
        self.cursor
            .as_ref()
            .map_or_else(|| self.last_request.clone(), InputCursor::request)
    }
    pub fn position(&self) -> u64 {
        self.cursor
            .as_ref()
            .map_or(self.last_request.offset, InputCursor::position)
    }
    pub fn generation(&self) -> u64 {
        self.request().token.generation()
    }
    fn healthy(&self) -> Result<()> {
        self.failure.map_or(Ok(()), Err)
    }
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.healthy()?;
        self.cursor_mut()?.provide(window)?;
        self.last_window_start = window.offset;
        self.last_window_end = window
            .offset
            .checked_add(u64::try_from(window.bytes().len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        Ok(())
    }
    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        self.healthy()?;
        let r = self.cursor_mut()?.finish(source, token);
        if r == Err(Error::Truncated) {
            self.abort(Error::Truncated);
        }
        r
    }
    pub fn fail(
        &mut self,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        self.healthy()?;
        self.cursor_mut()?.fail(source, token, failure)?;
        self.abort(match failure {
            SourceFailure::Transport => Error::Transport,
            SourceFailure::Cancelled => Error::Cancelled,
        });
        Ok(())
    }
    pub fn reset(&mut self, offset: u64) -> Result<InputRequest> {
        self.healthy()?;
        if offset % 2 != 0 {
            return Err(Error::Unsupported("DTS carrier reset outside word grid"));
        }
        let r = self.cursor_mut()?.reset(offset)?;
        self.clear();
        self.phase = Phase::Reserve;
        self.look_len = 0;
        self.offset = offset;
        self.last_window_start = offset;
        self.last_window_end = offset;
        self.end = None;
        self.complete = false;
        self.packets = 0;
        Ok(r)
    }
    fn clear(&mut self) {
        self.buffer = None;
        self.growth = None;
        self.raw = None;
        self.unpacker = None;
        self.payload = None;
    }
    fn cursor_mut(&mut self) -> Result<&mut InputCursor> {
        self.cursor
            .as_mut()
            .ok_or(Error::Invalid("missing healthy carrier cursor"))
    }
    fn abort(&mut self, e: Error) {
        self.last_request = self.request();
        self.cursor = None;
        self.failure = Some(e);
        self.clear();
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<Dts14CarrierPacket>> {
        self.healthy()?;
        let r = self.advance(work, cancel);
        if let Err(e) = r {
            if !matches!(e, Error::Backpressure { .. } | Error::Allocation) {
                self.abort(e);
            }
        }
        r
    }
    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<Dts14CarrierPacket>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.complete {
                return Ok(Step::End(
                    self.end.ok_or(Error::Invalid("carrier end missing"))?,
                ));
            }
            if work.remaining() == 0 {
                return Ok(Step::Yield);
            }
            match self.phase {
                Phase::Reserve => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let b = self
                        .buffer
                        .get_or_insert_with(|| BudgetVec::new(&self.pool));
                    b.try_reserve_exact(self.limits.maximum_packet_bytes.min(128))?;
                    self.extent = None;
                    self.header_probed = false;
                    self.syntax = Dts14CarrierSyntax::OpaqueCarrier;
                    self.phase = if self.look_len == 0 {
                        Phase::Scan
                    } else {
                        self.append_at = 0;
                        self.append_count = self.look_len;
                        Phase::Append
                    };
                }
                Phase::Scan => {
                    match self
                        .cursor
                        .as_mut()
                        .ok_or(Error::Invalid("missing carrier scan cursor"))?
                        .read_into(&mut self.look[self.look_len..], work, cancel)?
                    {
                        Step::Ready(n) => {
                            if n == 0 {
                                return Err(Error::Invalid("empty carrier read"));
                            }
                            self.look_len += n;
                            if self.look_len == 6 {
                                self.phase = Phase::Check;
                            }
                        }
                        Step::NeedInput(r) => return Ok(Step::NeedInput(r)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(e) => {
                            self.end = Some(e);
                            if self.look_len == 0 {
                                self.phase = Phase::Freeze;
                            } else {
                                self.append_at = 0;
                                self.append_count = self.look_len;
                                self.phase = Phase::Append;
                            }
                        }
                    }
                }
                Phase::Check => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let b = self
                        .buffer
                        .as_ref()
                        .ok_or(Error::Invalid("carrier builder missing"))?;
                    // Reuse the existing fixed120-bit primary header probe.
                    // The physical extent is admitted only for exact-word
                    // geometry; neither a96-byte cutoff nor a signature alone
                    // proves a boundary. This never validates coded audio.
                    if !self.header_probed && b.len() >= 18 {
                        self.header_probed = true;
                        if conventional_sync(b.as_slice(), self.order) {
                            let prefix: [u8; 18] = b.as_slice()[..18]
                                .try_into()
                                .map_err(|_| Error::Truncated)?;
                            let bits = crate::dts14_sync::header_payload(&prefix, self.order)?;
                            if let Ok(header) = probe_dts_core_header(&bits) {
                                if header.frame_bytes % 7 == 0 {
                                    self.extent = Some(
                                        (header.frame_bytes / 7)
                                            .checked_mul(8)
                                            .ok_or(Error::Overflow)?,
                                    );
                                }
                            }
                        }
                    }
                    if b.len() >= 6 && conventional_sync(&self.look, self.order) {
                        if !conventional_sync(b.as_slice(), self.order) {
                            return Err(Error::Unsupported(
                                "mixed opaque/conventional DTS carrier",
                            ));
                        }
                        match self.extent {
                            Some(end) if b.len() == end => {
                                self.phase = Phase::Freeze;
                                continue;
                            }
                            Some(end) if b.len() < end => {}
                            Some(_) => {
                                return Err(Error::Unsupported("late DTS carrier synchronization"));
                            }
                            None if self.header_probed => {
                                return Err(Error::Unsupported("unresolved DTS carrier extent"));
                            }
                            None => {}
                        }
                    }
                    self.append_at = 0;
                    self.append_count = 2;
                    self.phase = Phase::Append;
                }
                Phase::Append => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    if self.append_at < self.append_count {
                        let b = self
                            .buffer
                            .as_mut()
                            .ok_or(Error::Invalid("carrier append missing"))?;
                        if b.len() >= self.limits.maximum_packet_bytes {
                            return Err(Error::Limit {
                                requested: b.len().checked_add(1).ok_or(Error::Overflow)?,
                                maximum: self.limits.maximum_packet_bytes,
                            });
                        }
                        if b.len() == b.capacity() {
                            self.phase = Phase::GrowReserve;
                            continue;
                        }
                        b.try_push(self.look[self.append_at]).map_err(|e| e.error)?;
                        self.append_at += 1;
                    } else {
                        self.look.copy_within(self.append_count..self.look_len, 0);
                        self.look_len -= self.append_count;
                        self.phase = if self.end.is_some() {
                            Phase::Freeze
                        } else {
                            Phase::Scan
                        };
                    }
                }
                Phase::GrowReserve => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let old = self
                        .buffer
                        .as_ref()
                        .ok_or(Error::Invalid("missing carrier growth input"))?;
                    let target = old
                        .capacity()
                        .checked_mul(2)
                        .ok_or(Error::Overflow)?
                        .min(self.limits.maximum_packet_bytes);
                    let minimum = old
                        .capacity()
                        .checked_add(target)
                        .and_then(|n| n.checked_add(Budget::control_storage_bytes()))
                        .ok_or(Error::Overflow)?;
                    if minimum > self.pool.limit() {
                        return Err(Error::Limit {
                            requested: minimum,
                            maximum: self.pool.limit(),
                        });
                    }
                    let next = self
                        .growth
                        .get_or_insert_with(|| BudgetVec::new(&self.pool));
                    // Only an empty allocation grows. No dynamic prefix moves
                    // inside an allocation call or one uncharged operation.
                    next.try_reserve_exact(target)?;
                    self.growth_at = 0;
                    self.phase = Phase::GrowCopy;
                }
                Phase::GrowCopy => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let old = self
                        .buffer
                        .as_ref()
                        .ok_or(Error::Invalid("missing carrier copied input"))?;
                    if self.growth_at == old.len() {
                        self.phase = Phase::GrowSwap;
                    } else {
                        let next = self
                            .growth
                            .as_mut()
                            .ok_or(Error::Invalid("missing carrier growth output"))?;
                        if next.len() == next.capacity() {
                            return Err(Error::Invalid("carrier growth reservation exhausted"));
                        }
                        next.try_push(old.as_slice()[self.growth_at])
                            .map_err(|e| e.error)?;
                        self.growth_at += 1;
                    }
                }
                Phase::GrowSwap => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    self.buffer = self.growth.take();
                    self.phase = Phase::Append;
                }
                Phase::Freeze => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let b = self
                        .buffer
                        .take()
                        .ok_or(Error::Invalid("carrier freeze missing"))?;
                    if b.is_empty() && self.end.is_some() {
                        self.complete = true;
                        continue;
                    }
                    if self.packets >= self.limits.maximum_packets {
                        self.buffer = Some(b);
                        return Err(Error::Limit {
                            requested: self.packets.checked_add(1).ok_or(Error::Overflow)?,
                            maximum: self.limits.maximum_packets,
                        });
                    }
                    match b.freeze() {
                        Ok(raw) => self.raw = Some(raw),
                        Err(e) => {
                            self.buffer = Some(e.buffer);
                            return Err(e.error);
                        }
                    }
                    let raw = self
                        .raw
                        .as_ref()
                        .ok_or(Error::Invalid("carrier raw missing"))?;
                    if raw.len() >= 6 && conventional_sync(raw.as_ref(), self.order) {
                        if raw.len() % 2 != 0 {
                            self.syntax = Dts14CarrierSyntax::Refused(Error::Truncated);
                            self.phase = Phase::Publish;
                        } else {
                            self.phase = Phase::Unpack;
                        }
                    } else {
                        self.phase = Phase::Publish;
                    }
                }
                Phase::Unpack => {
                    if self.unpacker.is_none() {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        self.unpacker = Some(Dts14BitUnpacker::new(
                            &self.pool,
                            self.identity(),
                            self.raw
                                .as_ref()
                                .ok_or(Error::Invalid("carrier unpack missing"))?
                                .clone(),
                            self.order,
                            DtsPackingLimits {
                                maximum_source_bytes: self.limits.maximum_packet_bytes,
                                maximum_output_bytes: self.limits.maximum_packet_bytes,
                            },
                        )?);
                    }
                    match self
                        .unpacker
                        .as_mut()
                        .ok_or(Error::Invalid("carrier unpacker missing"))?
                        .step(work, cancel)?
                    {
                        Step::Ready(p) => {
                            // No discarded tail or silently selected FSIZE domain.
                            let checked = probe_dts_core_header(p.canonical_bytes().as_ref())
                                .and_then(|h| {
                                    if p.residual_bits() != 0
                                        || h.frame_bytes != p.canonical_bytes().len()
                                    {
                                        Err(Error::Unsupported(
                                            "DTS carrier unresolved extent/tail",
                                        ))
                                    } else {
                                        Ok(h)
                                    }
                                });
                            self.syntax = match checked {
                                Ok(h) => Dts14CarrierSyntax::ExactNormal(h),
                                Err(e) => Dts14CarrierSyntax::Refused(e),
                            };
                            self.payload = Some(p);
                            self.unpacker = None;
                            self.phase = Phase::Publish;
                        }
                        Step::Yield => return Ok(Step::Yield),
                        _ => return Err(Error::Invalid("carrier owned unpack progress")),
                    }
                }
                Phase::Publish => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let raw = self
                        .raw
                        .take()
                        .ok_or(Error::Invalid("carrier publication missing"))?;
                    let next = self
                        .offset
                        .checked_add(u64::try_from(raw.len()).map_err(|_| Error::Overflow)?)
                        .ok_or(Error::Overflow)?;
                    let boundary = match self.end {
                        Some(e) => {
                            if e.offset != next {
                                return Err(Error::Invalid("carrier EOF range"));
                            }
                            self.complete = true;
                            Dts14CarrierBoundary::ExplicitSourceEnd
                        }
                        None => Dts14CarrierBoundary::FollowingCarrierSync { offset: next },
                    };
                    let packet = Dts14CarrierPacket {
                        identity: self.identity(),
                        raw,
                        payload: self.payload.take(),
                        boundary,
                        syntax: self.syntax,
                        provision: Dts14CarrierProvision {
                            last_window_start: self.last_window_start,
                            last_window_end: self.last_window_end,
                            consumed_through: self.position(),
                        },
                    };
                    self.offset = next;
                    self.packets = self.packets.checked_add(1).ok_or(Error::Overflow)?;
                    self.phase = Phase::Reserve;
                    return Ok(Step::Ready(packet));
                }
            }
        }
    }
    fn identity(&self) -> DtsSourceIdentity {
        DtsSourceIdentity {
            source: self.request().source,
            generation: self.generation(),
            offset: self.offset,
        }
    }
}
/// Conventional physical representation of the primary extended normal sync.
/// Only the carrier signature is tested here; this is not syntax validation.
fn conventional_sync(b: &[u8], order: DtsWordOrder) -> bool {
    if b.len() < 6 {
        return false;
    }
    let word = |i| match order {
        DtsWordOrder::BigEndian => u16::from_be_bytes([b[i], b[i + 1]]),
        DtsWordOrder::LittleEndian => u16::from_le_bytes([b[i], b[i + 1]]),
    };
    word(0) == 0x1fff && word(2) == 0xe800 && word(4) & 0xfff0 == 0x07f0
}
