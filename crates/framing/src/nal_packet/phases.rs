//! The packet job state machine: separate preparation and emission scratch, driven by
//! [`NalPacketJob::advance_into`]. Each method is one step of the original
//! loop; `Step::Continue` re-enters the loop and `Step::Break` returns.
use super::{
    AvcState, EmissionPhase, Injection, NalPacketJob, NalPacketOutput, NalPacketProgress,
    NalPacketReceiverErrorKind, NalPacketSegment, PacketEmission, PacketPhase, PacketPlan,
    PacketSegmentStorage, PacketState, PlanPhase,
};
use crate::{AvcConfigArrayKind, Codec, NalHeader};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, Result, SourceId, SourceLocation, WorkBudget,
};

/// Outcome of one phase step.
type Step = ControlFlow<NalPacketProgress>;
const NEXT: Step = Step::Continue(());
const YIELD: Step = Step::Break(NalPacketProgress::Yield);

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
            if let Some(status) = plan.advance(&mut self.state, work, cancel)? {
                return Ok(status);
            }
            // Preparation scratch contains no allocations.
            // The next emission loop performs the original Reserve cancel check.
            self.phase = PacketPhase::Emit(PacketEmission::new());
        }
        match &mut self.phase {
            PacketPhase::Emit(emission) => emission.advance(&mut self.state, work, cancel, ready),
            PacketPhase::Prepare(_) => Err(Error::Invalid("packet emission phase missing")),
        }
    }
}

impl PacketPlan {
    fn advance<M>(
        &mut self,
        state: &mut PacketState<'_, M>,
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
                PlanPhase::Verify => self.verify_update(state, work)?,
                PlanPhase::Arrays => self.check_arrays(state, work)?,
                PlanPhase::Lookahead => self.lookahead(state, work, cancel)?,
                PlanPhase::Scan => self.scan_nal(state, work, cancel)?,
                PlanPhase::Inject => self.inject_configuration(state, work)?,
                PlanPhase::Nal => self.append_nal(state, work)?,
                PlanPhase::Reserve => return Ok(None),
            };
            if let Step::Break(status) = step {
                return Ok(Some(status));
            }
        }
    }

    #[inline]
    fn verify_update<M>(
        &mut self,
        state: &mut PacketState<'_, M>,
        work: &mut WorkBudget,
    ) -> Result<Step> {
        if let Some(update) = &state.update {
            let extra = state
                .source
                .as_ref()
                .and_then(|s| s.new_extradata.as_ref())
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
    fn check_arrays<M>(
        &mut self,
        state: &mut PacketState<'_, M>,
        work: &mut WorkBudget,
    ) -> Result<Step> {
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
    fn lookahead<M>(
        &mut self,
        state: &mut PacketState<'_, M>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step> {
        let source = state
            .source
            .as_ref()
            .ok_or(Error::Invalid("missing packet source"))?;
        let width = usize::from(
            state
                .length_width
                .ok_or(Error::Invalid("missing packet width"))?
                .bytes(),
        );
        if self.offset == source.bytes.len() && self.prefix_read == 0 {
            self.offset = 0;
            self.length = 0;
            self.phase = PlanPhase::Scan;
            return Ok(NEXT);
        }
        if !self.read_length(
            &source.bytes,
            width,
            work,
            cancel,
            &mut state.receiver_failure,
        )? {
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
        if end > source.bytes.len() {
            state.receiver_failure = Some(NalPacketReceiverErrorKind::IncompleteNalPayload);
            return Err(Error::Truncated);
        }
        let kind = match NalHeader::parse_compact(Codec::Hevc, &source.bytes[self.offset..end]) {
            Ok(h) => h.nal_unit_type(),
            Err(e) if state.observed_packet => {
                state.packet_source_refusal.get_or_insert(e);
                (source.bytes[self.offset] >> 1) & 63
            }
            Err(e) => return Err(e.error()),
        };
        self.has_irap |= (16..=23).contains(&kind);
        self.offset = end;
        self.prefix_read = 0;
        self.length = 0;
        self.look_nals += 1;
        Ok(NEXT)
    }

    #[inline]
    fn scan_nal<M>(
        &mut self,
        state: &mut PacketState<'_, M>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step> {
        let source = state
            .source
            .as_ref()
            .ok_or(Error::Invalid("missing packet source"))?;
        let Some(width) = state.length_width else {
            if !work.try_charge(1) {
                return Ok(YIELD);
            }
            self.segment_hint = Some(1);
            let segment = NalPacketSegment::passthrough(source.bytes.len());
            self.append(state, segment)?;
            self.phase = PlanPhase::Reserve;
            return Ok(NEXT);
        };
        let width = usize::from(width.bytes());
        if self.offset == 0 && source.bytes.is_empty() && state.codec == Codec::Avc {
            state.receiver_failure = Some(NalPacketReceiverErrorKind::EmptyAvcPacket);
            return Err(Error::Invalid("pinned AVC empty packet"));
        }
        if self.offset == source.bytes.len() && self.prefix_read == 0 {
            self.phase = PlanPhase::Reserve;
            return Ok(NEXT);
        }
        if !self.read_length(
            &source.bytes,
            width,
            work,
            cancel,
            &mut state.receiver_failure,
        )? {
            return Ok(YIELD);
        }
        let end = self
            .offset
            .checked_add(self.length)
            .ok_or(Error::Overflow)?;
        if end > source.bytes.len() {
            state.receiver_failure = Some(NalPacketReceiverErrorKind::IncompleteNalPayload);
            return Err(Error::Truncated);
        }
        if self.length == 0 {
            if state.codec == Codec::Hevc {
                state.receiver_failure = Some(NalPacketReceiverErrorKind::EmptyHevcNal);
                return Err(Error::Invalid("pinned HEVC empty NAL"));
            }
            self.prefix_read = 0;
            return Ok(NEXT);
        }
        if self.nals == state.session.limits.maximum_nals {
            return Err(Error::Limit {
                requested: self.nals.saturating_add(1),
                maximum: state.session.limits.maximum_nals,
            });
        }
        let codec = state.codec;
        let kind = if codec == Codec::Hevc {
            // Width-framed HEVC reaches Scan only after Lookahead
            // accepted every header or retained the first observed
            // refusal. The exclusively borrowed source is immutable.
            (source.bytes[self.offset] >> 1) & 63
        } else {
            match NalHeader::parse_compact(codec, &source.bytes[self.offset..end]) {
                Ok(header) => header.nal_unit_type(),
                Err(e) if state.observed_packet => {
                    state.packet_source_refusal.get_or_insert(e);
                    match codec {
                        Codec::Avc => source.bytes[self.offset] & 31,
                        Codec::Hevc => (source.bytes[self.offset] >> 1) & 63,
                    }
                }
                Err(e) => return Err(e.error()),
            }
        };
        let mut inject = None;
        if codec == Codec::Hevc {
            if self.has_irap && matches!(kind,16..=23|32..=34) && !self.injected_irap {
                inject = Some(Injection::All);
            }
        } else {
            match kind {
                7 => {
                    state.avc.sps = true;
                    state.avc.new_idr = true;
                }
                8 => {
                    state.avc.pps = true;
                    state.avc.new_idr = true;
                    if !state.avc.sps {
                        inject = Some(Injection::Sps);
                    }
                }
                5 => {
                    let first = *source
                        .bytes
                        .get(self.offset + 1)
                        .filter(|_| self.length > 1)
                        .ok_or(Error::Truncated)?
                        & 128
                        != 0;
                    if first {
                        state.avc.new_idr = true;
                    }
                    if state.avc.new_idr && !state.avc.pps {
                        inject = Some(if state.avc.sps {
                            Injection::Pps
                        } else {
                            Injection::All
                        });
                    }
                    state.avc.new_idr = false;
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
            && end == source.bytes.len()
            && inject.is_some()
            && !matches!(state.segments, Some(PacketSegmentStorage::Heap(_)))
        {
            // The final source NAL and its inserted configuration may cross
            // the inline boundary together. Size the first allocation for
            // both. The count bounds an AVC injection that selects only SPS,
            // PPS or non-extension NALs.
            let preceding = state.segments.as_ref().map_or(0, PacketSegmentStorage::len);
            self.segment_hint = Some(preceding + 1 + state.config().config_nal_count());
        }
        if let Some(i) = inject {
            self.injection = Some((i, 0));
            self.phase = PlanPhase::Inject;
        } else {
            self.phase = PlanPhase::Nal;
        }
        Ok(NEXT)
    }

    #[inline]
    fn inject_configuration<M>(
        &mut self,
        state: &mut PacketState<'_, M>,
        work: &mut WorkBudget,
    ) -> Result<Step> {
        let (which, index) = self
            .injection
            .ok_or(Error::Invalid("missing injection plan"))?;
        if !work.try_charge(1) {
            return Ok(YIELD);
        }
        if let Some(n) = state.config().config_nal(index) {
            let selected = match (n.kind, which) {
                (None, _) => true,
                (Some(kind), Injection::All) => kind != AvcConfigArrayKind::SequenceExtension,
                (Some(kind), Injection::Sps) => kind == AvcConfigArrayKind::Sequence,
                (Some(kind), Injection::Pps) => kind == AvcConfigArrayKind::Picture,
            };
            if selected {
                let segment = NalPacketSegment::configuration(index, n.range, state.total);
                self.append(state, segment)?;
            }
            self.injection = Some((which, index + 1));
        } else {
            if matches!(which, Injection::Sps) {
                state.avc.sps = true;
            }
            self.injection = None;
            self.injected_irap = true;
            self.phase = PlanPhase::Nal;
        }
        Ok(NEXT)
    }

    #[inline]
    fn append_nal<M>(
        &mut self,
        state: &mut PacketState<'_, M>,
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
        let end = self
            .offset
            .checked_add(self.length)
            .ok_or(Error::Overflow)?;
        let segment = NalPacketSegment::packet(self.offset..end, prefix, state.total);
        self.append(state, segment)?;
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
        state: &mut PacketState<'_, M>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        ready: &mut Option<NalPacketOutput<M>>,
    ) -> Result<NalPacketProgress> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let step = match self.phase {
                EmissionPhase::Reserve => self.reserve_output(state, work)?,
                EmissionPhase::Copy => self.copy_output(state, work, cancel)?,
                EmissionPhase::Freeze => self.freeze_output(state, work, ready)?,
                EmissionPhase::Complete => return Ok(NalPacketProgress::Complete),
            };
            if let Step::Break(status) = step {
                return Ok(status);
            }
        }
    }

    #[inline]
    fn reserve_output<M>(
        &mut self,
        state: &mut PacketState<'_, M>,
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
        let minimum = state
            .total
            .checked_add(Bytes::owner_overhead_bytes())
            .and_then(|v| v.checked_add(Budget::control_storage_bytes()))
            .ok_or(Error::Overflow)?;
        if minimum > state.session.pool.limit() {
            return Err(Error::Limit {
                requested: minimum,
                maximum: state.session.pool.limit(),
            });
        }
        self.output
            .get_or_insert_with(|| BudgetVec::new(&state.session.pool))
            .try_reserve_exact(state.total)?;
        self.phase = EmissionPhase::Copy;
        Ok(NEXT)
    }

    #[inline]
    fn copy_output<M>(
        &mut self,
        state: &mut PacketState<'_, M>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step> {
        let parts = state
            .segments
            .as_ref()
            .map_or(&[][..], PacketSegmentStorage::as_slice);
        while let Some(part) = parts.get(self.copy_segment) {
            let prefix = usize::from(part.prefix_bytes());
            let end = prefix.checked_add(part.len()).ok_or(Error::Overflow)?;
            let buffer = self
                .output
                .as_mut()
                .ok_or(Error::Invalid("missing packet buffer"))?;
            // Charge and check cancellation at the prefix, payload chunks
            // and segment boundary, including when resuming a partial copy.
            while self.copied < prefix {
                let count = work_count(work).min(prefix - self.copied);
                if count == 0 {
                    return Ok(YIELD);
                }
                let start = 4 - prefix + self.copied;
                buffer.try_extend_from_slice(&[0, 0, 0, 1][start..start + count])?;
                work.take(count as u64);
                self.copied += count;
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
            }
            if self.copied < end {
                if work.remaining() == 0 {
                    return Ok(YIELD);
                }
                let source = &state
                    .source
                    .as_ref()
                    .ok_or(Error::Invalid("missing packet source"))?
                    .bytes;
                let backing = if (part.nal_and_prefix >> 24) as u8
                    & NalPacketSegment::CONFIGURATION_SOURCE
                    == 0
                {
                    source
                } else {
                    state.update.as_ref().map_or_else(
                        || state.session.configuration.bytes(),
                        super::AvcUpdate::bytes,
                    )
                };
                let payload = part.payload(backing)?;
                while self.copied < end {
                    let count = work_count(work).min(end - self.copied);
                    if count == 0 {
                        return Ok(YIELD);
                    }
                    let start = self.copied - prefix;
                    buffer.try_extend_from_slice(&payload[start..start + count])?;
                    work.take(count as u64);
                    self.copied += count;
                    if cancel.load(Ordering::Relaxed) {
                        return Err(Error::Cancelled);
                    }
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
        state: &mut PacketState<'_, M>,
        work: &mut WorkBudget,
        ready: &mut Option<NalPacketOutput<M>>,
    ) -> Result<Step> {
        if !work.try_charge(1) {
            return Ok(YIELD);
        }
        let bytes = if state.length_width.is_none() {
            state
                .source
                .as_ref()
                .ok_or(Error::Invalid("missing packet source"))?
                .bytes
                .clone()
        } else {
            let b = self
                .output
                .take()
                .ok_or(Error::Invalid("missing packet buffer"))?;
            match b.freeze() {
                Ok(v) => v,
                Err(e) => {
                    self.output = Some(e.buffer);
                    return Err(e.error);
                }
            }
        };
        let configuration = state.config().bytes().clone();
        let configuration_identity = state.config().source_identity();
        let configuration_source_refusal = state.config().refusal();
        let forward_new_extradata = state.codec == Codec::Hevc
            || state
                .source
                .as_ref()
                .and_then(|s| s.new_extradata.as_ref())
                .is_some_and(Bytes::is_empty);
        if let Some(c) = state.update.take() {
            c.install(&mut state.session.configuration);
        }
        state.session.avc = state.avc;
        state.session.receiver_epoch = state.epoch;
        self.phase = EmissionPhase::Complete;
        *ready = Some(NalPacketOutput {
            bytes,
            source: state
                .source
                .take()
                .ok_or(Error::Invalid("missing packet source"))?,
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
            segments: state
                .segments
                .take()
                .unwrap_or_else(PacketSegmentStorage::new),
            receiver_epoch: state.epoch,
            forward_new_extradata,
        });
        Ok(Step::Break(NalPacketProgress::Ready))
    }
}

fn work_count(work: &WorkBudget) -> usize {
    work.remaining().min(4096) as usize
}
