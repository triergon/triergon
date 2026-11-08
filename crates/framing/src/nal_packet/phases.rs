//! The packet job state machine: one method per [`Phase`], driven by
//! [`NalPacketJob::advance_into`]. Each method is one step of the original
//! loop; `Step::Continue` re-enters the loop and `Step::Break` returns.
use super::{
    AvcState, Injection, NalPacketJob, NalPacketOutput, NalPacketProgress,
    NalPacketReceiverErrorKind, NalPacketSegment, Phase,
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
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if matches!(self.phase, Phase::Complete) {
                return Ok(NalPacketProgress::Complete);
            }
            let step = match self.phase {
                Phase::Verify => self.verify_update(work)?,
                Phase::Arrays => self.check_arrays(work)?,
                Phase::Lookahead => self.lookahead(work, cancel)?,
                Phase::Scan => self.scan_nal(work, cancel)?,
                Phase::Inject => self.inject_configuration(work)?,
                Phase::Nal => self.append_nal(work)?,
                Phase::Reserve => self.reserve_output(work)?,
                Phase::Copy => self.copy_output(work, cancel)?,
                Phase::Freeze => self.freeze_output(work, ready)?,
                Phase::Complete => Step::Break(NalPacketProgress::Complete),
            };
            if let Step::Break(status) = step {
                return Ok(status);
            }
        }
    }

    #[inline]
    fn verify_update(&mut self, work: &mut WorkBudget) -> Result<Step> {
        if let Some(update) = &self.update {
            let extra = self
                .source
                .as_ref()
                .and_then(|s| s.new_extradata.as_ref())
                .ok_or(Error::Invalid("missing source update"))?;
            let raw = update.bytes();
            if extra.len() != raw.len() {
                return Err(Error::Invalid("configuration update bytes differ"));
            }
            if self.verify < raw.len() {
                let count = (raw.len() - self.verify)
                    .min(4096)
                    .min(usize::try_from(work.remaining()).unwrap_or(usize::MAX));
                if count == 0 {
                    return Ok(YIELD);
                }
                // Parsed side data usually retains the same immutable input span.
                if !std::ptr::eq(extra.as_ref(), raw.as_ref())
                    && extra[self.verify..self.verify + count]
                        != raw[self.verify..self.verify + count]
                {
                    return Err(Error::Invalid("configuration update bytes differ"));
                }
                work.take(count as u64);
                self.verify += count;
                if self.verify < raw.len() {
                    return Ok(NEXT);
                }
            }
        }
        self.update_verified = true;
        self.phase = Phase::Scan;
        Ok(NEXT)
    }

    #[inline]
    fn check_arrays(&mut self, work: &mut WorkBudget) -> Result<Step> {
        if let Some(declared) = self.config().hevc_array_type(self.array) {
            if !work.try_charge(1) {
                return Ok(YIELD);
            }
            if !matches!(declared, 32 | 33 | 34 | 39 | 40) {
                return Err(Error::Unsupported("pinned HEVC decoder array type"));
            }
            self.array += 1;
            return Ok(NEXT);
        }
        self.phase = if self.length_width.is_some() {
            Phase::Lookahead
        } else {
            Phase::Scan
        };
        Ok(NEXT)
    }

    #[inline]
    fn lookahead(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        let source = self
            .source
            .as_ref()
            .ok_or(Error::Invalid("missing packet source"))?;
        let width = usize::from(
            self.length_width
                .ok_or(Error::Invalid("missing packet width"))?
                .bytes(),
        );
        if self.offset == source.bytes.len() && self.prefix_read == 0 {
            self.offset = 0;
            self.length = 0;
            self.phase = Phase::Scan;
            return Ok(NEXT);
        }
        if self.prefix_read < width {
            let count = (width - self.prefix_read)
                .min(usize::try_from(work.remaining()).unwrap_or(usize::MAX))
                .min(source.bytes.len().saturating_sub(self.offset));
            if count == 0 {
                // Missing input still consumes the old failed-read
                // token; exhausted work yields before that refusal.
                if !work.try_charge(1) {
                    return Ok(YIELD);
                }
                self.receiver_failure = Some(NalPacketReceiverErrorKind::IncompleteLengthPrefix);
                return Err(Error::Truncated);
            }
            work.take(count as u64);
            self.length =
                extend_length(self.length, &source.bytes[self.offset..self.offset + count]);
            self.offset += count;
            self.prefix_read += count;
            if self.prefix_read < width {
                return Ok(NEXT);
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
        }
        if !work.try_charge(1) {
            return Ok(YIELD);
        }
        if self.length == 0 {
            self.receiver_failure = Some(NalPacketReceiverErrorKind::EmptyHevcNal);
            return Err(Error::Invalid("pinned HEVC empty NAL"));
        }
        if self.look_nals == self.session.limits.maximum_nals {
            return Err(Error::Limit {
                requested: self.look_nals.saturating_add(1),
                maximum: self.session.limits.maximum_nals,
            });
        }
        let end = self
            .offset
            .checked_add(self.length)
            .ok_or(Error::Overflow)?;
        if end > source.bytes.len() {
            self.receiver_failure = Some(NalPacketReceiverErrorKind::IncompleteNalPayload);
            return Err(Error::Truncated);
        }
        let kind = match NalHeader::parse_compact(Codec::Hevc, &source.bytes[self.offset..end]) {
            Ok(h) => h.nal_unit_type(),
            Err(e) if self.observed_packet => {
                self.packet_source_refusal.get_or_insert(e);
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
    fn scan_nal(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        let source = self
            .source
            .as_ref()
            .ok_or(Error::Invalid("missing packet source"))?;
        let Some(width) = self.length_width else {
            if !work.try_charge(1) {
                return Ok(YIELD);
            }
            self.segment_hint = Some(1);
            self.append(NalPacketSegment::passthrough(source.bytes.len()))?;
            self.phase = Phase::Reserve;
            return Ok(NEXT);
        };
        let width = usize::from(width.bytes());
        if self.offset == 0 && source.bytes.is_empty() && self.codec == Codec::Avc {
            self.receiver_failure = Some(NalPacketReceiverErrorKind::EmptyAvcPacket);
            return Err(Error::Invalid("pinned AVC empty packet"));
        }
        if self.offset == source.bytes.len() && self.prefix_read == 0 {
            self.phase = Phase::Reserve;
            return Ok(NEXT);
        }
        if self.prefix_read < width {
            let count = (width - self.prefix_read)
                .min(usize::try_from(work.remaining()).unwrap_or(usize::MAX))
                .min(source.bytes.len().saturating_sub(self.offset));
            if count == 0 {
                // Missing input still consumes the old failed-read
                // token; exhausted work yields before that refusal.
                if !work.try_charge(1) {
                    return Ok(YIELD);
                }
                self.receiver_failure = Some(NalPacketReceiverErrorKind::IncompleteLengthPrefix);
                return Err(Error::Truncated);
            }
            work.take(count as u64);
            self.length =
                extend_length(self.length, &source.bytes[self.offset..self.offset + count]);
            self.offset += count;
            self.prefix_read += count;
            if self.prefix_read < width {
                return Ok(NEXT);
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
        }
        if !work.try_charge(1) {
            return Ok(YIELD);
        }
        let end = self
            .offset
            .checked_add(self.length)
            .ok_or(Error::Overflow)?;
        if end > source.bytes.len() {
            self.receiver_failure = Some(NalPacketReceiverErrorKind::IncompleteNalPayload);
            return Err(Error::Truncated);
        }
        if self.length == 0 {
            if self.codec == Codec::Hevc {
                self.receiver_failure = Some(NalPacketReceiverErrorKind::EmptyHevcNal);
                return Err(Error::Invalid("pinned HEVC empty NAL"));
            }
            self.prefix_read = 0;
            return Ok(NEXT);
        }
        if self.nals == self.session.limits.maximum_nals {
            return Err(Error::Limit {
                requested: self.nals.saturating_add(1),
                maximum: self.session.limits.maximum_nals,
            });
        }
        let codec = self.codec;
        let kind = if codec == Codec::Hevc {
            // Width-framed HEVC reaches Scan only after Lookahead
            // accepted every header or retained the first observed
            // refusal. The exclusively borrowed source is immutable.
            (source.bytes[self.offset] >> 1) & 63
        } else {
            match NalHeader::parse_compact(codec, &source.bytes[self.offset..end]) {
                Ok(header) => header.nal_unit_type(),
                Err(e) if self.observed_packet => {
                    self.packet_source_refusal.get_or_insert(e);
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
                    self.avc.sps = true;
                    self.avc.new_idr = true;
                }
                8 => {
                    self.avc.pps = true;
                    self.avc.new_idr = true;
                    if !self.avc.sps {
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
                        self.avc.new_idr = true;
                    }
                    if self.avc.new_idr && !self.avc.pps {
                        inject = Some(if self.avc.sps {
                            Injection::Pps
                        } else {
                            Injection::All
                        });
                    }
                    self.avc.new_idr = false;
                }
                1 => {
                    self.avc = AvcState::default();
                }
                _ => {}
            }
        }
        self.nal = Some(kind);
        self.nals += 1;
        if self.nals == 1 {
            let injected = if inject.is_some() || (codec == Codec::Hevc && self.has_irap) {
                self.config().config_nal_count()
            } else {
                0
            };
            if codec == Codec::Hevc && self.look_nals > 0 {
                // Lookahead counted every packet NAL; an IRAP packet
                // injects the whole configuration exactly once.
                self.segment_hint = Some(self.look_nals + injected);
            } else if end == source.bytes.len() {
                self.segment_hint = Some(1 + injected);
            }
        }
        if let Some(i) = inject {
            self.injection = Some((i, 0));
            self.phase = Phase::Inject;
        } else {
            self.phase = Phase::Nal;
        }
        Ok(NEXT)
    }

    #[inline]
    fn inject_configuration(&mut self, work: &mut WorkBudget) -> Result<Step> {
        let (which, index) = self
            .injection
            .ok_or(Error::Invalid("missing injection plan"))?;
        if !work.try_charge(1) {
            return Ok(YIELD);
        }
        if let Some(n) = self.config().config_nal(index) {
            let selected = match (n.kind, which) {
                (None, _) => true,
                (Some(kind), Injection::All) => kind != AvcConfigArrayKind::SequenceExtension,
                (Some(kind), Injection::Sps) => kind == AvcConfigArrayKind::Sequence,
                (Some(kind), Injection::Pps) => kind == AvcConfigArrayKind::Picture,
            };
            if selected {
                self.append(NalPacketSegment::configuration(index, n.range, self.total))?;
            }
            self.injection = Some((which, index + 1));
        } else {
            if matches!(which, Injection::Sps) {
                self.avc.sps = true;
            }
            self.injection = None;
            self.injected_irap = true;
            self.phase = Phase::Nal;
        }
        Ok(NEXT)
    }

    #[inline]
    fn append_nal(&mut self, work: &mut WorkBudget) -> Result<Step> {
        if !work.try_charge(1) {
            return Ok(YIELD);
        }
        let kind = self.nal.ok_or(Error::Invalid("missing source NAL"))?;
        let prefix = if self.codec == Codec::Hevc || matches!(kind, 7 | 8) || self.total == 0 {
            4
        } else {
            3
        };
        let end = self
            .offset
            .checked_add(self.length)
            .ok_or(Error::Overflow)?;
        let segment = NalPacketSegment::packet(self.offset..end, prefix, self.total);
        self.append(segment)?;
        self.offset = end;
        self.length = 0;
        self.prefix_read = 0;
        self.nal = None;
        self.phase = Phase::Scan;
        Ok(NEXT)
    }

    #[inline]
    fn reserve_output(&mut self, work: &mut WorkBudget) -> Result<Step> {
        if !work.try_charge(1) {
            return Ok(YIELD);
        }
        // A passthrough packet can share its charged payload.
        if self.length_width.is_none() {
            self.phase = Phase::Freeze;
            return Ok(NEXT);
        }
        let minimum = self
            .total
            .checked_add(Bytes::owner_overhead_bytes())
            .and_then(|v| v.checked_add(Budget::control_storage_bytes()))
            .ok_or(Error::Overflow)?;
        if minimum > self.session.pool.limit() {
            return Err(Error::Limit {
                requested: minimum,
                maximum: self.session.pool.limit(),
            });
        }
        self.output
            .get_or_insert_with(|| BudgetVec::new(&self.session.pool))
            .try_reserve_exact(self.total)?;
        self.phase = Phase::Copy;
        Ok(NEXT)
    }

    #[inline]
    fn copy_output(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step> {
        // One unit per pass exactly as the outer loop runs it: the
        // cancellation check precedes every unit after the first, and
        // every yield, charge and error is unchanged. Only the phase
        // dispatch between units is skipped.
        let parts = self.segments.as_ref().map_or(&[][..], BudgetVec::as_slice);
        let mut first = true;
        loop {
            if !first && cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            first = false;
            let Some(p) = parts.get(self.copy_segment) else {
                self.phase = Phase::Freeze;
                break;
            };
            let prefix = usize::from(p.prefix_bytes());
            let n = prefix.checked_add(p.len()).ok_or(Error::Overflow)?;
            if self.copied == n {
                if !work.try_charge(1) {
                    return Ok(YIELD);
                }
                self.copy_segment += 1;
                self.copied = 0;
                continue;
            }
            let count = if self.copied < prefix {
                prefix - self.copied
            } else {
                (n - self.copied).min(4096)
            }
            .min(usize::try_from(work.remaining()).unwrap_or(usize::MAX));
            if count == 0 {
                return Ok(YIELD);
            }
            let b = self
                .output
                .as_mut()
                .ok_or(Error::Invalid("missing packet buffer"))?;
            if self.copied < prefix {
                let start = 4 - prefix + self.copied;
                b.try_extend_from_slice(&[0, 0, 0, 1][start..start + count])?;
            } else {
                let start = self.copied - prefix;
                let source = &self
                    .source
                    .as_ref()
                    .ok_or(Error::Invalid("missing packet source"))?
                    .bytes;
                let configuration = self.update.as_ref().map_or_else(
                    || self.session.configuration.bytes(),
                    super::AvcUpdate::bytes,
                );
                let payload = p.payload(source, configuration)?;
                b.try_extend_from_slice(&payload[start..start + count])?;
            }
            work.take(count as u64);
            self.copied += count;
        }
        Ok(NEXT)
    }

    #[inline]
    fn freeze_output(
        &mut self,
        work: &mut WorkBudget,
        ready: &mut Option<NalPacketOutput<M>>,
    ) -> Result<Step> {
        if !work.try_charge(1) {
            return Ok(YIELD);
        }
        let bytes = if self.length_width.is_none() {
            self.source
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
        let configuration = self.config().bytes().clone();
        let configuration_identity = self.config().source_identity();
        let configuration_source_refusal = self.config().refusal();
        let forward_new_extradata = self.codec == Codec::Hevc
            || self
                .source
                .as_ref()
                .and_then(|s| s.new_extradata.as_ref())
                .is_some_and(Bytes::is_empty);
        if let Some(c) = self.update.take() {
            c.install(&mut self.session.configuration);
        }
        self.session.avc = self.avc;
        self.session.receiver_epoch = self.epoch;
        self.phase = Phase::Complete;
        *ready = Some(NalPacketOutput {
            bytes,
            source: self
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
            packet_source_refusal: self.packet_source_refusal,
            segments: self
                .segments
                .take()
                .unwrap_or_else(|| BudgetVec::new(&self.session.pool)),
            receiver_epoch: self.epoch,
            forward_new_extradata,
        });
        Ok(Step::Break(NalPacketProgress::Ready))
    }
}
