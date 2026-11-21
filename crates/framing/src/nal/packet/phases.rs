//! The packet job state machine: separate preparation and emission scratch, driven by
//! [`NalPacketJob::advance_into`]. Each method is one step of its loop;
//! `Step::Continue` re-enters the loop and `Step::Break` returns.
use super::{
    AvcState, ConfigurationRef, EmissionPhase, Injection, NalPacket, NalPacketJob, NalPacketOutput,
    NalPacketProgress, NalPacketReceiverErrorKind, NalPacketSegment, PacketEmission, PacketInput,
    PacketPhase, PacketPlan, PacketSegmentStorage, PacketState, PlanPhase, copy_annex_b_segment,
    copy_annex_b_whole, empty_packet_segments, freeze_annex_b_output, start_annex_b_output,
};
use crate::nal::header::NalHeaderError;
use crate::{
    avc::{self, AvcConfigArrayKind},
    nal::{Codec, NalHeader},
};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Bytes, Error, Result, SourceId, SourceLocation, WorkBudget};

/// Outcome of one phase step.
type Step = ControlFlow<NalPacketProgress>;
const NEXT: Step = Step::Continue(());
const YIELD: Step = Step::Break(NalPacketProgress::Yield);

pub(super) struct AvcBufferingLookahead {
    offset: usize,
    length: usize,
    prefix_read: usize,
    remaining_nals: usize,
    sps: Option<std::ops::Range<usize>>,
    pps: Option<std::ops::Range<usize>>,
    insertion_index: usize,
}

#[inline]
fn extend_length(mut value: usize, bytes: &[u8]) -> usize {
    // Length fields have at most four bytes. A four-byte chunk therefore
    // starts a fresh field; partial chunks retain the accumulated prefix.
    if let [a, b, c, d] = bytes {
        return u32::from_be_bytes([*a, *b, *c, *d]) as usize;
    }
    for &byte in bytes {
        value = (value << 8) | usize::from(byte);
    }
    value
}

impl<M> NalPacketJob<'_, M> {
    pub(super) fn advance_into(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        ready: &mut Option<NalPacketOutput<M>>,
    ) -> Result<NalPacketProgress> {
        if let PacketPhase::Prepare(plan) = &mut self.phase {
            let packet = self
                .source
                .as_ref()
                .ok_or(Error::Invalid("missing packet source"))?;
            let input = PacketInput {
                bytes: &packet.bytes,
                new_extradata: packet.new_extradata.as_ref(),
            };
            if let Some(status) = plan.advance(&mut self.state, input, work, cancel)? {
                return Ok(status);
            }
            // Preparation scratch contains no allocations.
            // The next emission loop performs the original Reserve cancel check.
            self.phase = PacketPhase::Emit(PacketEmission::new());
        }
        match &mut self.phase {
            PacketPhase::Emit(emission) => {
                emission.advance(&mut self.state, &mut self.source, work, cancel, ready)
            }
            PacketPhase::Prepare(_) => Err(Error::Invalid("packet emission phase missing")),
        }
    }
}

impl PacketPlan {
    fn advance(
        &mut self,
        state: &mut PacketState<'_>,
        input: PacketInput<'_>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<NalPacketProgress>> {
        loop {
            if matches!(self.phase, PlanPhase::Reserve) {
                return Ok(None);
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let step = match self.phase {
                PlanPhase::Verify => self.verify_update(state, input, work)?,
                PlanPhase::Arrays => self.check_arrays(state, work)?,
                PlanPhase::Lookahead => self.lookahead(state, input.bytes, work, cancel)?,
                PlanPhase::AvcBuffering => {
                    self.lookahead_avc_buffering(state, input.bytes, work, cancel)?
                }
                PlanPhase::AvcBufferingInject => {
                    self.inject_for_avc_buffering(state, input, work)?
                }
                PlanPhase::Scan => self.scan_nal(state, input, work, cancel)?,
                PlanPhase::Inject => self.inject_configuration(state, input, work, cancel)?,
                PlanPhase::Nal => self.append_nal(state, input, work)?,
                PlanPhase::Reserve => return Ok(None),
            };
            if let Step::Break(status) = step {
                return Ok(Some(status));
            }
        }
    }

    #[inline]
    fn verify_update(
        &mut self,
        state: &mut PacketState<'_>,
        input: PacketInput<'_>,
        work: &mut WorkBudget,
    ) -> Result<Step> {
        if let Some(update) = &state.session.pending_update {
            let extra = input
                .new_extradata
                .ok_or(Error::Invalid("missing source update"))?;
            let raw = update.bytes();
            if extra.len() != raw.len() {
                return Err(Error::Invalid("configuration update bytes differ"));
            }
            if self.offset < raw.len() {
                let count = (raw.len() - self.offset)
                    .min(4096)
                    .min(usize::try_from(work.remaining()).unwrap_or(usize::MAX));
                if count == 0 {
                    return Ok(YIELD);
                }
                // Parsed side data usually retains the same immutable input span.
                if !std::ptr::eq(extra.as_ref(), raw.as_ref())
                    && extra[self.offset..self.offset + count]
                        != raw[self.offset..self.offset + count]
                {
                    return Err(Error::Invalid("configuration update bytes differ"));
                }
                work.take(count as u64);
                self.offset += count;
                if self.offset < raw.len() {
                    return Ok(NEXT);
                }
            }
        }
        state.update_verified = true;
        self.offset = 0;
        self.phase = PlanPhase::Scan;
        Ok(NEXT)
    }

    #[inline]
    fn check_arrays(&mut self, state: &mut PacketState<'_>, work: &mut WorkBudget) -> Result<Step> {
        match state
            .session
            .configuration
            .admit_hevc_arrays(&mut self.offset, work)
        {
            Ok(true) => {}
            Ok(false) => return Ok(YIELD),
            Err(_) => return Err(Error::Unsupported("pinned HEVC decoder array type")),
        }
        self.offset = 0;
        self.phase = if state.length_width.is_some() {
            PlanPhase::Lookahead
        } else {
            PlanPhase::Scan
        };
        Ok(NEXT)
    }

    #[inline]
    fn read_length(
        &mut self,
        input: &[u8],
        width: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        refusal: &mut Option<NalPacketReceiverErrorKind>,
    ) -> Result<bool> {
        if width == 4
            && self.prefix_read == 0
            && work.remaining() >= 4
            && let Some(tail) = input.get(self.offset..)
            && let Some(bytes) = tail.first_chunk()
        {
            work.take(4);
            self.length = u32::from_be_bytes(*bytes) as usize;
            self.offset += 4;
            self.prefix_read = 4;
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            return Ok(work.try_charge(1));
        }
        self.read_partial_length(input, width, work, cancel, refusal)
    }

    #[inline(never)]
    fn read_partial_length(
        &mut self,
        input: &[u8],
        width: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        refusal: &mut Option<NalPacketReceiverErrorKind>,
    ) -> Result<bool> {
        while self.prefix_read < width {
            let count = (width - self.prefix_read)
                .min(usize::try_from(work.remaining()).unwrap_or(usize::MAX))
                .min(input.len().saturating_sub(self.offset));
            if count == 0 {
                // The failed read costs one unit, including at a partial prefix.
                if !work.try_charge(1) {
                    return Ok(false);
                }
                *refusal = Some(NalPacketReceiverErrorKind::IncompleteLengthPrefix);
                return Err(Error::Truncated);
            }
            work.take(count as u64);
            self.length = extend_length(self.length, &input[self.offset..self.offset + count]);
            self.offset += count;
            self.prefix_read += count;
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
        }
        Ok(work.try_charge(1))
    }

    #[inline]
    fn lookahead(
        &mut self,
        state: &mut PacketState<'_>,
        bytes: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step> {
        let width = usize::from(
            state
                .length_width
                .ok_or(Error::Invalid("missing packet width"))?
                .bytes(),
        );
        if self.offset == bytes.len() && self.prefix_read == 0 {
            self.offset = 0;
            self.length = 0;
            self.phase = PlanPhase::Scan;
            return Ok(NEXT);
        }
        if !self.read_length(bytes, width, work, cancel, &mut state.receiver_failure)? {
            return Ok(YIELD);
        }
        if self.length == 0 {
            state.receiver_failure = Some(NalPacketReceiverErrorKind::EmptyHevcNal);
            return Err(Error::Invalid("pinned HEVC empty NAL"));
        }
        if self.look_nals == state.session.limits.maximum_nals {
            return Err(Error::Limit {
                requested: self.look_nals.saturating_add(1),
                maximum: state.session.limits.maximum_nals,
            });
        }
        let end = self
            .offset
            .checked_add(self.length)
            .ok_or(Error::Overflow)?;
        if end > bytes.len() {
            state.receiver_failure = Some(NalPacketReceiverErrorKind::IncompleteNalPayload);
            return Err(Error::Truncated);
        }
        let kind = match NalHeader::parse_compact(Codec::Hevc, &bytes[self.offset..end]) {
            Ok(h) => h.nal_unit_type(),
            Err(e) if state.observed_packet => {
                state.packet_source_refusal.get_or_insert(e);
                (bytes[self.offset] >> 1) & 63
            }
            Err(e) => return Err(e.error()),
        };
        self.has_irap |= (16..=23).contains(&kind);
        self.offset = end;
        self.prefix_read = 0;
        self.length = 0;
        self.look_nals += 1;
        if end == bytes.len() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            self.offset = 0;
            self.phase = PlanPhase::Scan;
        }
        Ok(NEXT)
    }

    #[inline]
    fn scan_nal(
        &mut self,
        state: &mut PacketState<'_>,
        input: PacketInput<'_>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step> {
        let bytes = input.bytes.as_ref();
        let Some(width) = state.length_width else {
            if !work.try_charge(1) {
                return Ok(YIELD);
            }
            self.segment_hint = Some(1);
            let segment = NalPacketSegment::passthrough(bytes.len());
            self.append(state, input, segment)?;
            self.phase = PlanPhase::Reserve;
            return Ok(NEXT);
        };
        let width = usize::from(width.bytes());
        if self.offset == 0 && bytes.is_empty() && state.codec == Codec::Avc {
            state.receiver_failure = Some(NalPacketReceiverErrorKind::EmptyAvcPacket);
            return Err(Error::Invalid("pinned AVC empty packet"));
        }
        if self.offset == bytes.len() && self.prefix_read == 0 {
            self.phase = PlanPhase::Reserve;
            return Ok(NEXT);
        }
        // The HEVC lookahead validated the whole immutable packet. For a
        // single NAL its checked length is the remainder after the prefix.
        if self.look_nals == 1 && self.prefix_read == 0 && work.try_charge(width as u64) {
            self.length = bytes.len() - width;
            self.offset = width;
            self.prefix_read = width;
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(YIELD);
            }
        } else if !self.read_length(bytes, width, work, cancel, &mut state.receiver_failure)? {
            return Ok(YIELD);
        }
        let codec = state.codec;
        let end = if self.look_nals != 0 {
            self.offset + self.length
        } else {
            let end = self
                .offset
                .checked_add(self.length)
                .ok_or(Error::Overflow)?;
            if end > bytes.len() {
                state.receiver_failure = Some(NalPacketReceiverErrorKind::IncompleteNalPayload);
                return Err(Error::Truncated);
            }
            if self.length == 0 {
                self.prefix_read = 0;
                return Ok(NEXT);
            }
            if self.nals == state.session.limits.maximum_nals {
                return Err(Error::Limit {
                    requested: self.nals.saturating_add(1),
                    maximum: state.session.limits.maximum_nals,
                });
            }
            end
        };
        let header = bytes[self.offset];
        let kind = if codec == Codec::Hevc {
            // Width-framed HEVC reaches Scan only after Lookahead
            // accepted every header or retained the first observed
            // refusal. The exclusively borrowed source is immutable.
            (header >> 1) & 63
        } else {
            // The length check above already admitted the one-byte AVC header.
            if header & 0x80 != 0 {
                let refusal = NalHeaderError::ForbiddenZeroBit;
                if !state.observed_packet {
                    return Err(refusal.error());
                }
                state.packet_source_refusal.get_or_insert(refusal);
            }
            header & 31
        };
        let mut inject = None;
        if codec == Codec::Hevc {
            if self.has_irap && matches!(kind,16..=23|32..=34) && !self.injected_irap {
                inject = Some(Injection::All);
            }
        } else {
            match kind {
                avc::SEI
                    if state.observed_packet
                        && !state.avc.has_any(
                            AvcState::SPS | AvcState::PPS | AvcState::BUFFERING_PERIOD,
                        )
                        && bytes.get(self.offset + 1).is_none_or(|&first| first == 0) =>
                {
                    // H.264 Annex D assigns payload type zero to buffering
                    // period. The receiver reads the next byte and treats end
                    // of packet as a zero type. It does not parse the message.
                    state.session.avc_buffering = Some(AvcBufferingLookahead {
                        offset: end,
                        length: 0,
                        prefix_read: 0,
                        remaining_nals: state.session.limits.maximum_nals - self.nals - 1,
                        sps: None,
                        pps: None,
                        insertion_index: 0,
                    });
                    self.nal = Some(kind);
                    self.nals += 1;
                    self.phase = PlanPhase::AvcBuffering;
                    return Ok(NEXT);
                }
                7 => {
                    state.avc.set(AvcState::SPS | AvcState::NEW_IDR);
                }
                8 => {
                    state.avc.set(AvcState::PPS | AvcState::NEW_IDR);
                    if !state
                        .avc
                        .has_any(AvcState::SPS | AvcState::BUFFERING_PERIOD)
                    {
                        inject = Some(Injection::Sps);
                    }
                }
                5 => {
                    let next = self.offset + 1;
                    // The receiver reads the next packet byte, with zero
                    // padding at the end. The strict path needs a slice byte.
                    let first_byte = if self.length > 1 {
                        bytes[next]
                    } else if state.observed_packet {
                        bytes.get(next).copied().unwrap_or(0)
                    } else {
                        return Err(Error::Truncated);
                    };
                    if (state.avc.has_any(AvcState::NEW_IDR) || first_byte & 0x80 != 0)
                        && !state
                            .avc
                            .has_any(AvcState::PPS | AvcState::BUFFERING_PERIOD)
                    {
                        inject = Some(if state.avc.has_any(AvcState::SPS) {
                            Injection::Pps
                        } else {
                            Injection::All
                        });
                    }
                    state.avc.clear(AvcState::NEW_IDR);
                }
                1 => {
                    state.avc = AvcState::default();
                }
                _ => {}
            }
        }
        self.nal = Some(kind);
        self.nals += 1;
        if codec == Codec::Hevc && self.nals == 1 {
            // Lookahead counted all packet NALs. IRAP packets inject the full
            // configuration once, including duplicates in the packet.
            let injected = if self.has_irap {
                state.config().config_nal_count()
            } else {
                0
            };
            self.segment_hint = Some(self.look_nals + injected);
        } else if codec == Codec::Avc
            && end == bytes.len()
            && !matches!(state.session.segments, PacketSegmentStorage::Heap(_))
        {
            let inserted = if let Some(which) = inject
                && let ConfigurationRef::Avc(config) = state.config()
            {
                let (sps, pps) = config.base_parameter_set_counts();
                match which {
                    Injection::Sps => sps,
                    Injection::Pps => pps,
                    Injection::All => sps + pps,
                }
            } else {
                0
            };
            // The last NAL gives the exact segment count. A first spill can
            // use it without taking capacity from the output buffer.
            self.segment_hint = Some(state.session.segments.len() + 1 + inserted);
        }
        if let Some(i) = inject {
            self.injection = Some((i, 0));
            self.phase = PlanPhase::Inject;
        } else {
            self.phase = PlanPhase::Nal;
        }
        Ok(NEXT)
    }

    #[cold]
    #[inline(never)]
    fn lookahead_avc_buffering(
        &mut self,
        state: &mut PacketState<'_>,
        bytes: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step> {
        let width = usize::from(
            state
                .length_width
                .ok_or(Error::Invalid("missing packet width"))?
                .bytes(),
        );
        let valid = state
            .session
            .avc_buffering
            .as_mut()
            .ok_or(Error::Invalid("missing AVC buffering lookahead"))?;

        // Only the rest of this packet can already supply the parameter sets.
        // Charge each length and header. The regular scan still owns the input
        // and reports malformed lengths and headers at its original boundary.
        while valid.remaining_nals != 0 {
            if valid.offset == bytes.len() && valid.prefix_read == 0 {
                break;
            }
            let available = bytes.len() - valid.offset;
            let missing = width - valid.prefix_read;
            if available < missing {
                break;
            }
            if missing > 0 {
                let count = work_count(work).min(missing);
                if count == 0 {
                    return Ok(YIELD);
                }
                valid.length =
                    extend_length(valid.length, &bytes[valid.offset..valid.offset + count]);
                work.take(count as u64);
                valid.offset += count;
                valid.prefix_read += count;
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                if valid.prefix_read < width {
                    return Ok(YIELD);
                }
            }
            if !work.try_charge(1) {
                return Ok(YIELD);
            }
            let Some(end) = valid
                .offset
                .checked_add(valid.length)
                .filter(|&end| end <= bytes.len())
            else {
                break;
            };
            if valid.length > 0 {
                valid.remaining_nals -= 1;
                let kind = bytes[valid.offset] & 31;
                if matches!(kind, avc::IDR | avc::SLICE) {
                    break;
                }
                match kind {
                    avc::SPS => valid.sps = Some(valid.offset..end),
                    avc::PPS => valid.pps = Some(valid.offset..end),
                    _ => {}
                }
            }
            valid.offset = end;
            valid.length = 0;
            valid.prefix_read = 0;
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
        }
        state.avc.set(AvcState::BUFFERING_PERIOD);
        self.phase = PlanPhase::AvcBufferingInject;
        Ok(NEXT)
    }

    #[inline]
    fn inject_configuration(
        &mut self,
        state: &mut PacketState<'_>,
        input: PacketInput<'_>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step> {
        let (which, mut index) = self
            .injection
            .ok_or(Error::Invalid("missing injection plan"))?;
        loop {
            if !work.try_charge(1) {
                return Ok(YIELD);
            }
            let Some(n) = state.config().config_nal(index) else {
                if matches!(which, Injection::Sps) {
                    state.avc.set(AvcState::SPS);
                }
                self.injection = None;
                self.injected_irap = true;
                self.phase = PlanPhase::Nal;
                return Ok(NEXT);
            };
            let selected = match (n.kind, which) {
                (None, _) => true,
                (Some(kind), Injection::All) => kind != AvcConfigArrayKind::SequenceExtension,
                (Some(kind), Injection::Sps) => kind == AvcConfigArrayKind::Sequence,
                (Some(kind), Injection::Pps) => kind == AvcConfigArrayKind::Picture,
            };
            if selected {
                let segment = NalPacketSegment::configuration(index, n.range, state.total);
                self.append(state, input, segment)?;
            }
            index += 1;
            self.injection = Some((which, index));
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
        }
    }

    #[cold]
    #[inline(never)]
    fn inject_for_avc_buffering(
        &mut self,
        state: &mut PacketState<'_>,
        input: PacketInput<'_>,
        work: &mut WorkBudget,
    ) -> Result<Step> {
        if !work.try_charge(1) {
            return Ok(YIELD);
        }
        let lookahead = state
            .session
            .avc_buffering
            .as_ref()
            .ok_or(Error::Invalid("missing AVC buffering lookahead"))?;
        let index = lookahead.insertion_index;
        let count = state.config().config_nal_count();
        if index >= count + 2 {
            state.session.avc_buffering = None;
            self.phase = PlanPhase::Nal;
            return Ok(NEXT);
        }
        // Insert the sequence set before the picture set. An in-band copy
        // uses the packet range; it never changes the configuration record.
        let (config_index, packet_range) = if index == 0 {
            (None, lookahead.sps.as_ref())
        } else if index == count + 1 {
            (None, lookahead.pps.as_ref())
        } else {
            (Some(index - 1), None)
        };
        let segment = if let Some(range) = packet_range {
            Some(NalPacketSegment::packet(range.clone(), 4, state.total))
        } else if let Some(index) = config_index {
            let n = state
                .config()
                .config_nal(index)
                .ok_or(Error::Invalid("missing AVC parameter set"))?;
            let include = match n.kind {
                Some(AvcConfigArrayKind::Sequence) => lookahead.sps.is_none(),
                Some(AvcConfigArrayKind::Picture) => lookahead.pps.is_none(),
                _ => false,
            };
            include.then(|| NalPacketSegment::configuration(index, n.range, state.total))
        } else {
            None
        };
        if let Some(segment) = segment {
            self.append(state, input, segment)?;
        }
        state
            .session
            .avc_buffering
            .as_mut()
            .ok_or(Error::Invalid("missing AVC buffering lookahead"))?
            .insertion_index += 1;
        Ok(NEXT)
    }

    #[inline]
    fn append_nal(
        &mut self,
        state: &mut PacketState<'_>,
        input: PacketInput<'_>,
        work: &mut WorkBudget,
    ) -> Result<Step> {
        if !work.try_charge(1) {
            return Ok(YIELD);
        }
        let kind = self.nal.ok_or(Error::Invalid("missing source NAL"))?;
        let prefix = if state.codec == Codec::Hevc || matches!(kind, 7 | 8) || state.total == 0 {
            4
        } else {
            3
        };
        // Scan verified the complete NAL before entering this phase.
        let end = self.offset + self.length;
        let segment = NalPacketSegment::packet(self.offset..end, prefix, state.total);
        self.append(state, input, segment)?;
        self.offset = end;
        self.length = 0;
        self.prefix_read = 0;
        self.nal = None;
        self.phase = PlanPhase::Scan;
        Ok(NEXT)
    }
}

impl PacketEmission {
    fn advance<M>(
        &mut self,
        state: &mut PacketState<'_>,
        source: &mut Option<NalPacket<M>>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        ready: &mut Option<NalPacketOutput<M>>,
    ) -> Result<NalPacketProgress> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let step = match self.phase {
                EmissionPhase::Reserve => {
                    let packet = source
                        .as_ref()
                        .ok_or(Error::Invalid("missing packet source"))?;
                    self.reserve_output(state, &packet.bytes, work)?
                }
                EmissionPhase::Copy => {
                    let packet = source
                        .as_ref()
                        .ok_or(Error::Invalid("missing packet source"))?;
                    self.copy_output(state, &packet.bytes, work, cancel)?
                }
                EmissionPhase::Freeze => self.freeze_output(state, source, work, ready)?,
                EmissionPhase::Complete => return Ok(NalPacketProgress::Complete),
            };
            if let Step::Break(status) = step {
                return Ok(status);
            }
        }
    }

    #[inline]
    fn reserve_output(
        &mut self,
        state: &PacketState<'_>,
        bytes: &Bytes,
        work: &mut WorkBudget,
    ) -> Result<Step> {
        if !work.try_charge(1) {
            return Ok(YIELD);
        }
        // A passthrough packet can share its charged payload.
        if state.length_width.is_none() {
            self.phase = EmissionPhase::Freeze;
            return Ok(NEXT);
        }
        self.output = Some(start_annex_b_output(bytes.budget(), state.total)?);
        self.phase = EmissionPhase::Copy;
        Ok(NEXT)
    }

    #[inline]
    fn copy_output(
        &mut self,
        state: &PacketState<'_>,
        bytes: &Bytes,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step> {
        let parts = state.session.segments.as_slice();
        let configuration = state.config().bytes();
        let buffer = self
            .output
            .as_mut()
            .ok_or(Error::Invalid("missing packet buffer"))?;
        if self.copy_segment == 0
            && self.copied == 0
            && copy_annex_b_whole(
                buffer,
                state.total,
                parts.len(),
                parts.iter().map(|part| part.copy(bytes, configuration)),
                work,
                cancel,
            )?
        {
            self.copy_segment = parts.len();
        }
        while let Some(part) = parts.get(self.copy_segment) {
            if self.copied < part.framed_len() {
                if work.remaining() == 0 {
                    return Ok(YIELD);
                }
                let (prefix, payload) = part.copy(bytes, configuration)?;
                if !copy_annex_b_segment(prefix, payload, buffer, &mut self.copied, work, cancel)? {
                    return Ok(YIELD);
                }
            }
            if !work.try_charge(1) {
                return Ok(YIELD);
            }
            self.copy_segment += 1;
            self.copied = 0;
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
        }
        self.phase = EmissionPhase::Freeze;
        Ok(NEXT)
    }

    #[inline]
    fn freeze_output<M>(
        &mut self,
        state: &mut PacketState<'_>,
        source: &mut Option<NalPacket<M>>,
        work: &mut WorkBudget,
        ready: &mut Option<NalPacketOutput<M>>,
    ) -> Result<Step> {
        if !work.try_charge(1) {
            return Ok(YIELD);
        }
        let bytes = if state.length_width.is_none() {
            source
                .as_ref()
                .ok_or(Error::Invalid("missing packet source"))?
                .bytes
                .clone()
        } else {
            freeze_annex_b_output(&mut self.output, "missing packet buffer")?
        };
        let source = source
            .take()
            .ok_or(Error::Invalid("missing packet source"))?;
        let record = state.config();
        let configuration = record.bytes().clone();
        let configuration_identity = record.source_identity();
        let configuration_source_refusal = match &state.session.pending_update {
            Some(update) => update.refusal(),
            None => state.session.configuration.refusal(),
        };
        let forward_new_extradata = state.codec == Codec::Hevc
            || source.new_extradata.as_ref().is_some_and(Bytes::is_empty);
        if let Some(c) = state.session.pending_update.take() {
            state.session.install_avc(c, state.length_width);
        }
        state.session.avc = state.avc;
        state.session.receiver_epoch = state.epoch;
        self.phase = EmissionPhase::Complete;
        *ready = Some(NalPacketOutput {
            bytes,
            source,
            configuration,
            configuration_identity: configuration_identity.unwrap_or((
                SourceLocation {
                    source: SourceId(0),
                    offset: 0,
                },
                0,
            )),
            has_configuration_identity: configuration_identity.is_some(),
            configuration_source_refusal,
            packet_source_refusal: state.packet_source_refusal,
            segments: std::mem::replace(&mut state.session.segments, empty_packet_segments()),
            receiver_epoch: state.epoch,
            forward_new_extradata,
        });
        Ok(Step::Break(NalPacketProgress::Ready))
    }
}

fn work_count(work: &WorkBudget) -> usize {
    work.remaining().min(4096) as usize
}
