//! AVC access units for independent MPEG-TS segments with in-band AUDs.
//! H.222.0 §2.14.1 supplies the delimiter contract. Annex B and the RBSP
//! prefixes used for active SPS/PPS resolution follow H.264 §7.3 and Annex B.

pub(crate) mod syntax;

use crate::{
    TrackKey, TsNativeCarriage, TsNativeConfiguration,
    ts_payload::{PesFacts, PesInput},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, BytesView, Error, Result, SourceId, WorkBudget};
use triergon_framing::{Codec, NalHeader};
use triergon_mpegts::compat::pes::{
    CompactDemuxPacket, DemuxPacket, DemuxPayloadKind, DemuxTimeView, PayloadCursor,
    RegisteredCarriage,
};

pub use syntax::AvcRaster;

const SEI: u8 = 6;
const AUD: u8 = 9;
const PREFIX: &[u8] = &[0, 0, 0, 1];
const LOOKAHEAD: usize = PREFIX.len() + 1;

#[derive(Clone, Copy, Debug)]
pub struct AvcTsLimits {
    pub maximum_access_unit_bytes: usize,
    pub maximum_parameter_set_bytes: usize,
}

impl Default for AvcTsLimits {
    fn default() -> Self {
        Self {
            maximum_access_unit_bytes: 4 * 1024 * 1024,
            maximum_parameter_set_bytes: 4096,
        }
    }
}

#[derive(Debug)]
pub struct AvcTsAccessUnit {
    source: Bytes,
    sps: Option<Bytes>,
    pps: Option<Bytes>,
    pub clock: Option<AvcTsClock>,
    pub raster: AvcRaster,
    pub idr: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcTsClock {
    pub source_pts_raw33: Option<u64>,
    pub source_dts_raw33: Option<u64>,
    pub pts90: Option<i64>,
    pub dts90: Option<i64>,
}

impl TryFrom<DemuxTimeView> for AvcTsClock {
    type Error = Error;

    fn try_from(clock: DemuxTimeView) -> Result<Self> {
        Self::from_demux(&clock)
    }
}

impl AvcTsClock {
    fn from_demux(clock: &DemuxTimeView) -> Result<Self> {
        let narrow = |value: Option<i128>| {
            value
                .map(i64::try_from)
                .transpose()
                .map_err(|_| Error::Overflow)
        };
        Ok(Self {
            source_pts_raw33: clock.source_pts_raw33,
            source_dts_raw33: clock.source_dts_raw33,
            pts90: narrow(clock.pts90)?,
            dts90: narrow(clock.dts90)?,
        })
    }
}

impl AvcTsAccessUnit {
    pub fn source_bytes(&self) -> &Bytes {
        &self.source
    }

    /// Annex B with the referenced SPS/PPS supplied on IDR if absent in-band.
    /// The original access unit follows any inserted sets byte for byte.
    pub fn feed_spans(&self) -> impl Iterator<Item = &[u8]> {
        self.sps
            .iter()
            .chain(self.pps.iter())
            .flat_map(|set| [PREFIX, set.as_ref()])
            .chain(std::iter::once(self.source.as_ref()))
    }

    /// A contiguous queue payload. Share the original access unit unless an
    /// IDR needs cached parameter sets added in front of it.
    pub fn contiguous_feed(&self, pool: &Budget) -> Result<Bytes> {
        if !self.source.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        if self.sps.is_none() && self.pps.is_none() {
            return Ok(self.source.clone());
        }
        let total = self.feed_spans().try_fold(0_usize, |size, span| {
            size.checked_add(span.len()).ok_or(Error::Overflow)
        })?;
        let mut output = BudgetVec::new(pool);
        output.try_reserve_exact(total)?;
        for span in self.feed_spans() {
            output.try_extend_from_slice(span)?;
        }
        output.freeze().map_err(|error| error.error)
    }
}

#[derive(Debug)]
pub enum AvcTsStep {
    NeedInput,
    Yield,
    Ready(AvcTsAccessUnit),
    End,
}

/// Whether a PES belongs to an HLS lane's track and configuration epoch.
pub(crate) fn lane_track(track: TrackKey, configuration_epoch: u64, packet: &PesFacts<'_>) -> bool {
    track
        == (TrackKey::Ts {
            program: packet.program,
            pid: packet.pid,
        })
        && packet.source_epochs.configuration == configuration_epoch
}

/// Whether the demux clock view describes this PES and its header's raw stamps.
pub(crate) fn clock_describes(
    clock: &DemuxTimeView,
    packet: &PesFacts<'_>,
    pts_raw33: Option<u64>,
    dts_raw33: Option<u64>,
) -> bool {
    clock.program == packet.program
        && clock.pid == packet.pid
        && clock.profile == packet.profile
        && clock.source_epochs == packet.source_epochs
        && clock.source_pts_raw33 == pts_raw33
        && clock.source_dts_raw33 == dts_raw33
}

/// The 90 kHz time whose ticks match a PES's raw 33-bit stamp. A PTS-only
/// clock projection can mirror PTS into DTS; an absent stamp stays absent.
pub(crate) fn pes_time(
    raw: Option<u64>,
    ticks: Option<i128>,
    mismatch: &'static str,
) -> Result<Option<triergon_core::Time>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let ticks = ticks
        .filter(|value| value.rem_euclid(1 << 33) == i128::from(raw))
        .ok_or(Error::Invalid(mismatch))?;
    Ok(Some(triergon_core::Time::new(ticks, 90_000)?))
}

#[derive(Clone, Copy)]
struct Anchor {
    serial: u64,
    clock: AvcTsClock,
}

struct Chunk {
    // Direct syntax fixtures do not construct a transport packet.
    #[cfg(test)]
    bytes: Option<Bytes>,
    offset: usize,
}

struct PendingPes {
    spans: PayloadCursor,
    remaining: usize,
    may_reserve: bool,
}

impl Chunk {
    fn view<'a>(&'a self, spans: Option<&'a PayloadCursor>) -> Option<BytesView<'a>> {
        #[cfg(test)]
        if let Some(bytes) = &self.bytes {
            return Some(bytes.view(0..bytes.len()).expect("complete direct chunk"));
        }
        spans.and_then(PayloadCursor::current_bytes)
    }

    fn retire(&self, pending: &mut Option<PendingPes>) {
        #[cfg(test)]
        if self.bytes.is_some() {
            return;
        }
        pending
            .as_mut()
            .expect("pending PES chunk")
            .spans
            .retire_current();
    }
}

#[derive(Clone, Copy)]
struct Prefix {
    start: usize,
    anchor: Option<Anchor>,
}

#[derive(Clone, Copy)]
struct Nal {
    prefix: Prefix,
    header: usize,
    kind: u8,
}

#[derive(Clone, Copy)]
struct Picture {
    pps: u8,
    idr: bool,
}

struct Unit {
    clock: Option<AvcTsClock>,
    picture: Option<Picture>,
}

struct ParameterSet<T> {
    id: u8,
    seen_in_unit: u64,
    bytes: Bytes,
    value: T,
}

fn remember<T>(
    sets: &mut BudgetVec<ParameterSet<T>>,
    seen_in_unit: u64,
    raw: &[u8],
    parse: impl FnOnce(&[u8]) -> Result<(u8, T)>,
) -> Result<()> {
    // Repeated in-band sets keep the same parsed value and storage. Check one
    // cached set before parsing; other inputs follow the ordinary ID lookup.
    if let Some(set) = sets.as_mut_slice().last_mut()
        && set.bytes.as_ref() == raw
    {
        set.seen_in_unit = seen_in_unit;
        return Ok(());
    }
    let (id, value) = parse(&raw[1..])?;
    let existing = sets.as_slice().iter().position(|set| set.id == id);
    if let Some(index) = existing
        && sets.as_slice()[index].bytes.as_ref() == raw
    {
        sets.as_mut_slice()[index].seen_in_unit = seen_in_unit;
        return Ok(());
    }
    let bytes = Bytes::copy_from_slice(sets.budget(), raw)?;
    let set = ParameterSet {
        id,
        seen_in_unit,
        bytes,
        value,
    };
    if let Some(index) = existing {
        sets.as_mut_slice()[index] = set;
    } else {
        sets.try_push(set).map_err(|error| error.error)?;
    }
    Ok(())
}

/// One selected AVC track and segment. PES and carrier boundaries can occur
/// anywhere in an access unit. Create a fresh reader at a segment or track reset.
pub struct AvcTsReader {
    pool: Budget,
    limits: AvcTsLimits,
    config: TsNativeConfiguration,
    source_clock_epoch: Option<(SourceId, u64)>,
    pending_pes: Option<PendingPes>,
    input: Option<Chunk>,
    pes_anchors: [Option<Anchor>; 4],
    pes_anchor: Option<u8>,
    pes_serial: u64,
    stamped_serial: u64,
    zero_anchors: [Option<u8>; 3],
    next_zero_anchor: usize,
    zeroes: usize,
    prefix: Option<Prefix>,
    next_nal: Option<Nal>,
    nal: Option<Nal>,
    nal_data_end: usize,
    unit: Option<Unit>,
    unit_serial: u64,
    bytes: BudgetVec<u8>,
    sps: BudgetVec<ParameterSet<AvcRaster>>,
    pps: BudgetVec<ParameterSet<u8>>,
    allow_startup_sei: bool,
    ended: bool,
    failure: Option<Error>,
}

impl AvcTsReader {
    pub fn new(pool: &Budget, config: TsNativeConfiguration, limits: AvcTsLimits) -> Result<Self> {
        if config.carriage != TsNativeCarriage::Registered(RegisteredCarriage::Avc) {
            return Err(Error::Unsupported("HLS AVC needs selected AVC carriage"));
        }
        if !matches!(config.track, TrackKey::Ts { program, pid } if program != 0 && (1..0x1fff).contains(&pid))
            || limits.maximum_access_unit_bytes == 0
            || limits.maximum_parameter_set_bytes == 0
        {
            return Err(Error::Invalid("AVC TS reader policy"));
        }
        limits
            .maximum_access_unit_bytes
            .checked_add(LOOKAHEAD)
            .ok_or(Error::Overflow)?;
        Ok(Self {
            pool: pool.clone(),
            limits,
            config,
            source_clock_epoch: None,
            pending_pes: None,
            input: None,
            pes_anchors: [None; 4],
            pes_anchor: None,
            pes_serial: 0,
            stamped_serial: 0,
            zero_anchors: [None; 3],
            next_zero_anchor: 0,
            zeroes: 0,
            prefix: None,
            next_nal: None,
            nal: None,
            nal_data_end: 0,
            unit: None,
            unit_serial: 0,
            bytes: BudgetVec::new(pool),
            sps: BudgetVec::new(pool),
            pps: BudgetVec::new(pool),
            allow_startup_sei: false,
            ended: false,
            failure: None,
        })
    }

    pub(crate) fn new_progressive(
        pool: &Budget,
        config: TsNativeConfiguration,
        limits: AvcTsLimits,
    ) -> Result<Self> {
        let mut reader = Self::new(pool, config, limits)?;
        reader.allow_startup_sei = true;
        Ok(reader)
    }

    /// Moves a PES only on acceptance. Drain to NeedInput before offering another.
    pub fn push(
        &mut self,
        mailbox: &mut Option<DemuxPacket>,
        clock: DemuxTimeView,
    ) -> Result<bool> {
        self.push_packet(mailbox, &clock)
    }

    /// Accept the same PES facts through borrowed logical carrier ranges.
    /// The mailbox is moved only after the ordinary admission checks succeed.
    pub fn push_compact(
        &mut self,
        mailbox: &mut Option<CompactDemuxPacket>,
        clock: &DemuxTimeView,
    ) -> Result<bool> {
        self.push_packet(mailbox, clock)
    }

    pub(crate) fn push_packet<P: PesInput>(
        &mut self,
        mailbox: &mut Option<P>,
        clock: &DemuxTimeView,
    ) -> Result<bool> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.ended {
            return Err(Error::Invalid("AVC TS input after EOF"));
        }
        if self.input.is_some() || self.pending_pes.is_some() {
            return Ok(false);
        }
        let Some(packet) = mailbox.as_ref() else {
            return Ok(false);
        };
        let packet = packet.facts();
        if !lane_track(self.config.track, self.config.configuration_epoch, &packet)
            || packet.source_header.stream_id & 0xf0 != 0xe0
        {
            return Err(Error::Invalid("AVC TS PES track or configuration"));
        }
        if packet.flags & 2 != 0
            || packet.payload_kind == DemuxPayloadKind::DeclaredIncomplete
            || packet.empty
            || packet.payload_len == 0
        {
            return Err(Error::Invalid("AVC TS corrupt or empty PES"));
        }
        if !packet.payload_budget.same_pool(&self.pool) {
            return Err(Error::ForeignPool);
        }
        let origin = (packet.source_origin.source, packet.source_epochs.clock);
        if self
            .source_clock_epoch
            .is_some_and(|current| current != origin)
        {
            return Err(Error::Invalid("AVC TS PES crosses source or clock epoch"));
        }
        let header = packet.time_header();
        let (pts, dts) = (
            header.and_then(|h| h.pts_raw33),
            header.and_then(|h| h.dts_raw33),
        );
        if !clock_describes(clock, &packet, pts, dts) {
            return Err(Error::Invalid("AVC TS clock does not describe this PES"));
        }
        if header.is_some() {
            self.remember_clock(AvcTsClock::from_demux(clock)?)?;
        }
        self.source_clock_epoch = Some(origin);
        let remaining = packet.payload_len;
        self.pending_pes = mailbox.take().map(|packet| PendingPes {
            remaining,
            spans: packet.into_payload().into_cursor(),
            may_reserve: true,
        });
        Ok(true)
    }

    fn remember_clock(&mut self, clock: AvcTsClock) -> Result<()> {
        let serial = self.pes_serial.checked_add(1).ok_or(Error::Overflow)?;
        // Three prefix zeros and the following byte can each start a PES.
        let index = (serial % 4) as u8;
        self.pes_anchors[usize::from(index)] = Some(Anchor { serial, clock });
        self.pes_anchor = Some(index);
        self.pes_serial = serial;
        Ok(())
    }

    /// Seal only after definitive segment EOF. Already provided bytes drain.
    pub fn finish(&mut self) {
        self.ended = true;
    }

    pub fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<AvcTsStep> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let step = self.advance(work, cancel);
        if let Err(error) = step
            && !matches!(error, Error::Allocation | Error::Backpressure { .. })
        {
            self.failure = Some(error);
            self.pending_pes = None;
            self.input = None;
            self.bytes = BudgetVec::new(&self.pool);
            self.sps = BudgetVec::new(&self.pool);
            self.pps = BudgetVec::new(&self.pool);
        }
        step
    }

    fn advance(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<AvcTsStep> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if let Some(next) = self.next_nal {
                if !work.try_charge(1) {
                    return Ok(AvcTsStep::Yield);
                }
                if let Some(nal) = self.nal {
                    self.end_nal(nal)?;
                    self.nal = None;
                }
                if next.kind == AUD {
                    if self.unit.is_some() {
                        let result = self.emit(next.prefix.start)?;
                        let header = next.header - next.prefix.start;
                        self.begin(Nal {
                            header,
                            prefix: Prefix {
                                start: 0,
                                ..next.prefix
                            },
                            ..next
                        })?;
                        return Ok(AvcTsStep::Ready(result));
                    }
                    self.begin(next)?;
                } else {
                    if self.unit.is_none()
                        && !(self.allow_startup_sei
                            && self.unit_serial == 0
                            && next.kind == SEI
                            && next.prefix.anchor.is_some())
                    {
                        return Err(Error::Unsupported(
                            "AVC TS access unit has no initial delimiter",
                        ));
                    }
                    self.begin_nal(next);
                }
                continue;
            }
            if self.input.is_none()
                && let Some(pending) = &mut self.pending_pes
            {
                if pending.spans.remaining() != 0 {
                    if !work.try_charge(1) {
                        return Ok(AvcTsStep::Yield);
                    }
                    let bytes = pending
                        .spans
                        .next_bytes()
                        .ok_or(Error::Invalid("AVC TS missing PES span"))?;
                    if !bytes.belongs_to(&self.pool) {
                        return Err(Error::ForeignPool);
                    }
                    pending.remaining = pending
                        .remaining
                        .checked_sub(bytes.len())
                        .ok_or(Error::Invalid("AVC TS PES payload extent"))?;
                    self.input = Some(Chunk {
                        #[cfg(test)]
                        bytes: None,
                        offset: 0,
                    });
                } else {
                    if pending.remaining != 0 {
                        return Err(Error::Invalid("AVC TS PES payload extent"));
                    }
                    self.pending_pes = None;
                }
            }
            if let Some(chunk) = &mut self.input {
                let (span, remaining, may_reserve) = match self.pending_pes.as_mut() {
                    Some(PendingPes {
                        spans,
                        remaining,
                        may_reserve,
                    }) => (chunk.view(Some(spans)), *remaining, Some(may_reserve)),
                    None => (chunk.view(None), 0, None),
                };
                let span = span.ok_or(Error::Invalid("AVC TS missing PES span"))?;
                let span_len = span.len();
                if chunk.offset == span_len {
                    chunk.retire(&mut self.pending_pes);
                    self.input = None;
                    continue;
                }
                let available = &span.as_ref()[chunk.offset..];
                let count = available.len().min(work.remaining().min(4096) as usize);
                if count == 0 {
                    return Ok(AvcTsStep::Yield);
                }
                let first = available[0];
                // Within a NAL, isolated zeros followed by data cannot
                // begin a start code. Leave possible split prefixes for accept.
                let copy =
                    if first != 0 && self.prefix.is_none() && self.zeroes < 2 && self.nal.is_some()
                    {
                        nal_body_prefix_len(&available[..count])
                    } else {
                        1
                    };
                let limit = self.limits.maximum_access_unit_bytes + LOOKAHEAD;
                append(&mut self.bytes, &available[..copy], limit, |len, growth| {
                    // Once a slice has started, parameter set processing for
                    // this picture is complete.
                    self.nal.as_ref().filter(|nal| matches!(nal.kind, 1 | 5))?;
                    let may_reserve = may_reserve.filter(|reserve| **reserve)?;
                    let extent = len
                        .checked_add(available.len())?
                        .checked_add(remaining)?
                        .checked_add(LOOKAHEAD)?;
                    // A PES can contain multiple pictures. Limit the hint
                    // relative to ordinary growth and try it once per PES.
                    if extent <= growth || extent > limit.min(growth.saturating_mul(4)) {
                        return None;
                    }
                    *may_reserve = false;
                    Some(extent)
                })?;
                chunk.offset += copy;
                if chunk.offset == span_len {
                    chunk.retire(&mut self.pending_pes);
                    self.input = None;
                }
                work.try_charge(copy as u64);
                if copy == 1 {
                    self.accept(first)?;
                } else {
                    self.zeroes = 0;
                    self.nal_data_end = self.bytes.len();
                }
                continue;
            }
            if !self.ended {
                return Ok(AvcTsStep::NeedInput);
            }
            if self.prefix.is_some() || (self.unit.is_none() && !self.bytes.is_empty()) {
                return Err(Error::Truncated);
            }
            if self.unit.is_none() {
                return Ok(AvcTsStep::End);
            }
            if !work.try_charge(1) {
                return Ok(AvcTsStep::Yield);
            }
            let end = self.bytes.len();
            if let Some(nal) = self.nal {
                self.end_nal(nal)?;
                self.nal = None;
            }
            return self.emit(end).map(AvcTsStep::Ready);
        }
    }

    fn accept(&mut self, byte: u8) -> Result<()> {
        if let Some(prefix) = self.prefix.take() {
            let kind = NalHeader::parse(Codec::Avc, &[byte])?.nal_unit_type();
            self.next_nal = Some(Nal {
                prefix,
                header: self.bytes.len() - 1,
                kind,
            });
        } else if byte == 0 {
            self.zero_anchors[self.next_zero_anchor] = self.pes_anchor;
            self.next_zero_anchor = (self.next_zero_anchor + 1) % self.zero_anchors.len();
            self.zeroes = (self.zeroes + 1).min(3);
            return Ok(());
        } else if byte == 1 && self.zeroes >= 2 {
            self.prefix = Some(Prefix {
                start: self.bytes.len() - self.zeroes - 1,
                anchor: self.zero_anchors[(self.next_zero_anchor + self.zero_anchors.len()
                    - self.zeroes)
                    % self.zero_anchors.len()]
                .and_then(|index| self.pes_anchors[usize::from(index)]),
            });
        } else if self.nal.is_none() {
            return Err(Error::Invalid("bytes before AVC Annex B start code"));
        } else {
            self.nal_data_end = self.bytes.len();
        }
        self.zeroes = 0;
        Ok(())
    }

    fn begin(&mut self, delimiter: Nal) -> Result<()> {
        self.unit_serial = self.unit_serial.checked_add(1).ok_or(Error::Overflow)?;
        let clock = delimiter
            .prefix
            .anchor
            .filter(|anchor| anchor.serial > self.stamped_serial)
            .map(|anchor| {
                self.stamped_serial = anchor.serial;
                anchor.clock
            });
        self.unit = Some(Unit {
            clock,
            picture: None,
        });
        self.begin_nal(delimiter);
        Ok(())
    }

    fn begin_nal(&mut self, nal: Nal) {
        self.nal_data_end = nal.header + 1;
        self.nal = Some(nal);
        self.next_nal = None;
    }

    fn end_nal(&mut self, nal: Nal) -> Result<()> {
        let raw = &self.bytes.as_slice()[nal.header..self.nal_data_end];
        let payload = &raw[1..];
        let Some(unit) = self.unit.as_mut() else {
            // Keep the progressive muxer's leading SEI with the first
            // delimited picture only when both begin in the same PES.
            if let (Some(anchor), Some(delimiter)) = (nal.prefix.anchor, self.next_nal)
                && delimiter.kind == AUD
                && delimiter
                    .prefix
                    .anchor
                    .is_some_and(|next| next.serial == anchor.serial)
                && !payload.is_empty()
            {
                return Ok(());
            }
            return Err(Error::Invalid(
                "AVC TS startup SEI without a delimiter in its PES",
            ));
        };
        match nal.kind {
            AUD if matches!(payload, [value] if value & 0x1f == 0x10) => {}
            AUD => return Err(Error::Invalid("AVC access unit delimiter RBSP")),
            1 | 5 => {
                let (first_mb, pps) = syntax::slice_header_prefix(payload)?;
                let idr = nal.kind == 5;
                if let Some(picture) = unit.picture {
                    if picture.pps != pps || picture.idr != idr || first_mb == 0 {
                        return Err(Error::Unsupported(
                            "AVC TS picture or redundant slices without a new delimiter",
                        ));
                    }
                } else {
                    if first_mb != 0 {
                        return Err(Error::Unsupported(
                            "AVC TS picture starts outside macroblock zero",
                        ));
                    }
                    unit.picture = Some(Picture { pps, idr });
                }
            }
            7 | 8 => {
                if unit.picture.is_some() {
                    return Err(Error::Invalid("AVC parameter set after picture slices"));
                }
                let length = raw.len();
                if length > self.limits.maximum_parameter_set_bytes {
                    return Err(Error::Limit {
                        requested: length,
                        maximum: self.limits.maximum_parameter_set_bytes,
                    });
                }
                if nal.kind == 7 {
                    remember(&mut self.sps, self.unit_serial, raw, syntax::sps)?;
                } else {
                    remember(&mut self.pps, self.unit_serial, raw, syntax::pps)?;
                }
            }
            SEI | 10 | 11 | 12 => {}
            _ => return Err(Error::Unsupported("AVC TS NAL type")),
        }
        Ok(())
    }

    fn emit(&mut self, end: usize) -> Result<AvcTsAccessUnit> {
        if end > self.limits.maximum_access_unit_bytes {
            return Err(Error::Limit {
                requested: end,
                maximum: self.limits.maximum_access_unit_bytes,
            });
        }
        let unit = self
            .unit
            .as_ref()
            .ok_or(Error::Invalid("missing AVC access unit"))?;
        let picture = unit.picture.ok_or(Error::Truncated)?;
        if self.unit_serial == 1 && !picture.idr {
            return Err(Error::Unsupported(
                "independent AVC segment must start with IDR",
            ));
        }
        let pps = self
            .pps
            .as_slice()
            .iter()
            .find(|set| set.id == picture.pps)
            .ok_or(Error::Invalid("AVC slice references unknown PPS"))?;
        let sps = self
            .sps
            .as_slice()
            .iter()
            .find(|set| set.id == pps.value)
            .ok_or(Error::Invalid("AVC PPS references unknown SPS"))?;
        let sps_feed =
            (picture.idr && sps.seen_in_unit != self.unit_serial).then(|| sps.bytes.clone());
        let pps_feed =
            (picture.idr && pps.seen_in_unit != self.unit_serial).then(|| pps.bytes.clone());
        let feed_bytes = sps_feed
            .iter()
            .chain(pps_feed.iter())
            .try_fold(end, |total, set| {
                total
                    .checked_add(PREFIX.len())
                    .and_then(|total| total.checked_add(set.len()))
                    .ok_or(Error::Overflow)
            })?;
        if feed_bytes > self.limits.maximum_access_unit_bytes {
            return Err(Error::Limit {
                requested: feed_bytes,
                maximum: self.limits.maximum_access_unit_bytes,
            });
        }
        let clock = unit.clock;
        let raster = sps.value;
        let mut next = BudgetVec::new(&self.pool);
        next.try_extend_from_slice(&self.bytes.as_slice()[end..])?;
        let builder = std::mem::replace(&mut self.bytes, next);
        let source = match builder.freeze() {
            Ok(bytes) => bytes.into_slice(0..end)?,
            Err(error) => {
                self.bytes = error.buffer;
                return Err(error.error);
            }
        };
        self.unit = None;
        Ok(AvcTsAccessUnit {
            source,
            sps: sps_feed,
            pps: pps_feed,
            clock,
            raster,
            idr: picture.idr,
        })
    }
}

fn adjacent_zero_masks(chunk: &[u8; 16], next: u8) -> (u64, u64) {
    let (words, _) = chunk.as_chunks::<8>();
    let first = u64::from_le_bytes(words[0]);
    let second = u64::from_le_bytes(words[1]);
    let first_pairs = first | (first >> 8) | (second << 56);
    let second_pairs = second | (second >> 8) | (u64::from(next) << 56);
    let zeroes = |pairs: u64| {
        // Borrow can mark a later byte. The first mark starts a zero pair.
        pairs.wrapping_sub(0x0101_0101_0101_0101) & !pairs & 0x8080_8080_8080_8080
    };
    (zeroes(first_pairs), zeroes(second_pairs))
}

fn first_zero_pair(first: u64, second: u64) -> usize {
    if first != 0 {
        first.trailing_zeros() as usize / 8
    } else {
        8 + second.trailing_zeros() as usize / 8
    }
}

fn nal_body_prefix_len(bytes: &[u8]) -> usize {
    let mut rest = bytes;
    while let Some((chunk, tail)) = rest.split_first_chunk::<16>()
        && let Some(&next) = tail.first()
    {
        let (first, second) = adjacent_zero_masks(chunk, next);
        if first | second != 0 {
            return bytes.len() - rest.len() + first_zero_pair(first, second);
        }
        rest = tail;
    }
    if let Some((head, tail)) = bytes.split_last_chunk::<16>() {
        // The overlap has already been checked. Treat a trailing zero as a
        // possible prefix for the next input span.
        let (first, second) = adjacent_zero_masks(tail, 0);
        return head.len() + first_zero_pair(first, second);
    }
    bytes
        .windows(2)
        .position(|pair| pair == [0, 0])
        .unwrap_or_else(|| bytes.len() - usize::from(bytes.last() == Some(&0)))
}

fn append(
    buffer: &mut BudgetVec<u8>,
    bytes: &[u8],
    limit: usize,
    reserve_hint: impl FnOnce(usize, usize) -> Option<usize>,
) -> Result<()> {
    let end = buffer
        .len()
        .checked_add(bytes.len())
        .ok_or(Error::Overflow)?;
    if end > limit {
        return Err(Error::Limit {
            requested: end,
            maximum: limit,
        });
    }
    if end > buffer.capacity() {
        let target = buffer
            .capacity()
            .saturating_mul(2)
            .max(64)
            .max(end)
            .min(limit);
        let reserved = reserve_hint(buffer.len(), target).is_some_and(|extent| {
            // Leave room for publication and subsequent input. A refused
            // hint leaves the builder ready for its ordinary growth.
            let headroom = buffer
                .budget()
                .available()
                .saturating_sub(Bytes::owner_overhead_bytes() + LOOKAHEAD);
            extent <= headroom / 2 && buffer.try_reserve_exact(extent - buffer.len()).is_ok()
        });
        if !reserved && let Err(error) = buffer.try_reserve_exact(target - buffer.len()) {
            if target == end
                || !matches!(
                    error,
                    Error::Backpressure { .. } | Error::Limit { .. } | Error::Allocation
                )
            {
                return Err(error);
            }
            buffer.try_reserve_exact(end - buffer.len())?;
        }
    }
    buffer.try_extend_from_slice(bytes)
}

#[cfg(test)]
mod tests;
