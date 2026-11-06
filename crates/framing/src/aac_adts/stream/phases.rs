//! The ADTS stream state machine: one method per [`Phase`], driven by
//! [`AacAdtsStreamReader::advance`]. Each method is one step of the original
//! loop; `Flow::Continue` re-enters the loop and `Flow::Break` returns.
use super::{AacAdtsStreamReader, Phase, StreamOutput};
use crate::AacSource;
use crate::aac_adts::carrier::{AacAdtsCarrier, AacAdtsCarrierBoundary};
use crate::aac_adts::{AacAdtsFrame, inspect_aac_adts_header, validate_selected_header};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{BudgetVec, Bytes, Error, Result, SourceLocation, Step, WorkBudget};

/// Outcome of one phase step.
type Flow = ControlFlow<Step<StreamOutput>>;
const NEXT: Flow = Flow::Continue(());
const YIELD: Flow = Flow::Break(Step::Yield);

impl AacAdtsStreamReader {
    pub(super) fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<StreamOutput>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let step = match self.phase {
                Phase::Start => self.start_frame()?,
                Phase::Prefix => self.read_prefix(work, cancel)?,
                Phase::Validate => self.validate_header(work)?,
                Phase::ReserveHeader => self.reserve_header(work)?,
                Phase::CopyHeader => self.copy_header(work)?,
                Phase::Body => self.read_body(work, cancel)?,
                Phase::ScanCheck => self.scan_check(work)?,
                Phase::EmitPrefix => self.emit_prefix(work)?,
                Phase::Grow => self.grow_buffer(work)?,
                Phase::Freeze => self.freeze_frame(work)?,
                Phase::End => self.end_of_input()?,
            };
            if let Flow::Break(status) = step {
                return Ok(status);
            }
        }
    }

    #[inline]
    fn start_frame(&mut self) -> Result<Flow> {
        self.frame_start = self.position();
        self.used = 0;
        self.copied = 0;
        self.frame_bytes = 0;
        self.header = None;
        self.growth_target = 0;
        self.growth_copy_credit = 0;
        self.header_offset = 0;
        self.eof_fragment = false;
        self.selected_error = None;
        self.phase = Phase::Prefix;
        Ok(NEXT)
    }

    #[inline]
    fn read_prefix(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Flow> {
        if self.used == 7 {
            self.phase = Phase::Validate;
            return Ok(NEXT);
        }
        if self.carrier && self.used == self.limits.maximum_frame_bytes {
            if !work.try_charge(1) {
                return Ok(YIELD);
            }
            match self.probe_bound(cancel)? {
                Step::End(_) => {
                    self.fragment_prefix()?;
                    return Ok(NEXT);
                }
                Step::NeedInput(r) => return Ok(Flow::Break(Step::NeedInput(r))),
                Step::Yield => return Err(self.packet_limit()),
                Step::Ready(_) => {
                    return Err(Error::Invalid("zero-work bound probe consumed input"));
                }
            }
        }
        let cursor = self
            .cursor
            .as_mut()
            .ok_or(Error::Invalid("missing ADTS input cursor"))?;
        let end = if self.carrier {
            self.limits.maximum_frame_bytes.min(7)
        } else {
            7
        };
        match cursor.read_into(&mut self.prefix[self.used..end], work, cancel)? {
            Step::Ready(n) => {
                if n == 0 {
                    return Err(Error::Invalid("zero ADTS prefix read"));
                }
                self.used = self.used.checked_add(n).ok_or(Error::Overflow)?;
            }
            Step::NeedInput(r) => return Ok(Flow::Break(Step::NeedInput(r))),
            Step::Yield => return Ok(YIELD),
            Step::End(e) => {
                if self.used == 0 {
                    self.phase = Phase::End;
                    return Ok(Flow::Break(Step::End(e)));
                }
                if self.carrier {
                    self.fragment_prefix()?;
                    return Ok(NEXT);
                }
                return Err(Error::Truncated);
            }
        }
        Ok(NEXT)
    }

    #[inline]
    fn validate_header(&mut self, work: &mut WorkBudget) -> Result<Flow> {
        if !work.try_charge(1) {
            return Ok(YIELD);
        }
        if self.completed >= self.limits.maximum_frames {
            return Err(Error::Limit {
                requested: self.completed.checked_add(1).ok_or(Error::Overflow)?,
                maximum: self.limits.maximum_frames,
            });
        }
        let h = inspect_aac_adts_header(&self.prefix)?;
        if self.carrier {
            self.selected_error = validate_selected_header(h).err();
            if self.plausible_envelope(h) {
                let n = usize::from(h.frame_length());
                if n > self.limits.maximum_frame_bytes {
                    return Err(Error::Limit {
                        requested: n,
                        maximum: self.limits.maximum_frame_bytes,
                    });
                }
                if h.single_block_header_bytes().is_some_and(|v| n < v) {
                    self.selected_error =
                        Some(Error::Invalid("ADTS declared extent smaller than header"));
                }
                self.header = Some(h);
                self.frame_bytes = n;
            } else {
                self.selected_error = self
                    .selected_error
                    .or(Some(Error::Invalid("ADTS envelope extent")));
                self.frame_bytes = self.limits.maximum_frame_bytes;
            }
            self.phase = Phase::ReserveHeader;
            return Ok(NEXT);
        }
        validate_selected_header(h)?;
        let frame_bytes = usize::from(h.frame_length());
        let header_bytes = h
            .single_block_header_bytes()
            .ok_or(Error::Unsupported("ADTS block layout"))?;
        if frame_bytes < header_bytes {
            return Err(Error::Invalid("ADTS declared extent smaller than header"));
        }
        if frame_bytes > self.limits.maximum_frame_bytes {
            return Err(Error::Limit {
                requested: frame_bytes,
                maximum: self.limits.maximum_frame_bytes,
            });
        }
        let end = self
            .frame_start
            .checked_add(u64::try_from(frame_bytes).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        if self
            .cursor
            .as_ref()
            .ok_or(Error::Invalid("missing ADTS input cursor"))?
            .capabilities()
            .length
            .is_some_and(|n| end > n)
        {
            return Err(Error::Truncated);
        }
        self.header = Some(h);
        self.frame_bytes = frame_bytes;
        self.phase = Phase::ReserveHeader;
        Ok(NEXT)
    }

    #[inline]
    fn reserve_header(&mut self, work: &mut WorkBudget) -> Result<Flow> {
        if !work.try_charge(1) {
            return Ok(YIELD);
        }
        if self.carrier && self.completed >= self.limits.maximum_frames {
            return Err(Error::Limit {
                requested: self.completed.checked_add(1).ok_or(Error::Overflow)?,
                maximum: self.limits.maximum_frames,
            });
        }
        if self.buffer.is_none() {
            self.buffer = Some(BudgetVec::new(&self.pool));
        }
        self.check_minimum(self.used)?;
        self.buffer
            .as_mut()
            .ok_or(Error::Invalid("missing ADTS builder"))?
            .try_reserve_exact(self.used)?;
        self.phase = Phase::CopyHeader;
        Ok(NEXT)
    }

    #[inline]
    fn copy_header(&mut self, work: &mut WorkBudget) -> Result<Flow> {
        if self.copied == self.used {
            self.phase = Phase::Body;
            return Ok(NEXT);
        }
        let n = usize::try_from(
            work.take(u64::try_from(self.used - self.copied).map_err(|_| Error::Overflow)?),
        )
        .map_err(|_| Error::Overflow)?;
        if n == 0 {
            return Ok(YIELD);
        }
        self.buffer
            .as_mut()
            .ok_or(Error::Invalid("missing ADTS builder"))?
            .try_extend_from_slice(&self.prefix[self.copied..self.copied + n])?;
        self.copied += n;
        Ok(NEXT)
    }

    #[inline]
    fn read_body(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Flow> {
        let b = self
            .buffer
            .as_ref()
            .ok_or(Error::Invalid("missing ADTS builder"))?;
        let remaining = self
            .frame_bytes
            .checked_sub(b.len())
            .ok_or(Error::Overflow)?;
        let scanning = self.carrier && self.header.is_none() && !self.eof_fragment;
        if remaining == 0 {
            if scanning {
                if !work.try_charge(1) {
                    return Ok(YIELD);
                }
                match self.probe_bound(cancel)? {
                    Step::End(_) => {
                        self.eof_fragment = true;
                        self.phase = Phase::Freeze;
                        return Ok(NEXT);
                    }
                    Step::NeedInput(r) => return Ok(Flow::Break(Step::NeedInput(r))),
                    Step::Yield => return Err(self.packet_limit()),
                    Step::Ready(_) => {
                        return Err(Error::Invalid("zero-work bound probe consumed input"));
                    }
                }
            }
            self.phase = Phase::Freeze;
            return Ok(NEXT);
        }
        let room = b.capacity().checked_sub(b.len()).ok_or(Error::Overflow)?;
        if room == 0 {
            let needed = b.len().checked_add(1).ok_or(Error::Overflow)?;
            let doubled = b.capacity().checked_mul(2).ok_or(Error::Overflow)?;
            self.growth_target = needed.max(doubled).min(self.frame_bytes);
            self.growth_copy_credit = b.len();
            self.phase = Phase::Grow;
            return Ok(NEXT);
        }
        let cursor = self
            .cursor
            .as_mut()
            .ok_or(Error::Invalid("missing ADTS input cursor"))?;
        let buffer = self
            .buffer
            .as_mut()
            .ok_or(Error::Invalid("missing ADTS builder"))?;
        let maximum = if scanning {
            1
        } else {
            remaining.min(room).min(4096)
        };
        match cursor.read_append(buffer, maximum, work, cancel)? {
            Step::Ready(n) => {
                if n == 0 {
                    return Err(Error::Invalid("zero ADTS body read"));
                }
                if scanning {
                    self.prefix.copy_within(1..7, 0);
                    self.prefix[6] = *buffer
                        .as_slice()
                        .last()
                        .ok_or(Error::Invalid("missing ADTS scan byte"))?;
                    self.phase = Phase::ScanCheck;
                }
            }
            Step::NeedInput(r) => return Ok(Flow::Break(Step::NeedInput(r))),
            Step::Yield => return Ok(YIELD),
            Step::End(_) => {
                if !self.carrier {
                    return Err(Error::Truncated);
                }
                self.frame_bytes = buffer.len();
                self.eof_fragment = true;
                self.selected_error = self.selected_error.or(Some(Error::Truncated));
                self.phase = Phase::Freeze;
            }
        }
        Ok(NEXT)
    }

    #[inline]
    fn scan_check(&mut self, work: &mut WorkBudget) -> Result<Flow> {
        if !work.try_charge(1) {
            return Ok(YIELD);
        }
        let h = inspect_aac_adts_header(&self.prefix)?;
        if self.plausible_envelope(h) {
            let b = self
                .buffer
                .as_ref()
                .ok_or(Error::Invalid("missing ADTS scan builder"))?;
            let start = b.len().checked_sub(7).ok_or(Error::Overflow)?;
            let end = start
                .checked_add(usize::from(h.frame_length()))
                .ok_or(Error::Overflow)?;
            if end > self.limits.maximum_frame_bytes {
                return Err(Error::Limit {
                    requested: end,
                    maximum: self.limits.maximum_frame_bytes,
                });
            }
            self.header_offset = start;
            self.header = Some(h);
            self.frame_bytes = end;
            if self.split_prefix {
                self.phase = Phase::EmitPrefix;
                return Ok(NEXT);
            }
        }
        self.phase = Phase::Body;
        Ok(NEXT)
    }

    #[inline]
    fn emit_prefix(&mut self, work: &mut WorkBudget) -> Result<Flow> {
        if !work.try_charge(1) {
            return Ok(YIELD);
        }
        self.check_minimum(Bytes::owner_overhead_bytes())?;
        let b = self
            .buffer
            .take()
            .ok_or(Error::Invalid("missing recovery prefix"))?;
        let raw = match b.freeze() {
            Ok(b) => b,
            Err(e) => {
                self.buffer = Some(e.buffer);
                return Err(e.error);
            }
        };
        let prefix = raw.slice(0..self.header_offset)?;
        let cursor = self
            .cursor
            .as_ref()
            .ok_or(Error::Invalid("missing ADTS input cursor"))?;
        let source = AacSource {
            source: cursor.source(),
            generation: cursor.generation(),
            offset: self.frame_start,
        };
        let inspection_end = cursor.position();
        let error = self.selected_error;
        self.frame_start = self
            .frame_start
            .checked_add(u64::try_from(self.header_offset).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        let h = self
            .header
            .ok_or(Error::Invalid("missing recovered header"))?;
        self.frame_bytes = usize::from(h.frame_length());
        self.header_offset = 0;
        self.used = 7;
        self.copied = 0;
        self.selected_error = validate_selected_header(h).err();
        if h.single_block_header_bytes()
            .is_some_and(|n| self.frame_bytes < n)
        {
            self.selected_error = Some(Error::Invalid("ADTS declared extent smaller than header"));
        }
        self.completed = self.completed.checked_add(1).ok_or(Error::Overflow)?;
        self.phase = Phase::ReserveHeader;
        Ok(Flow::Break(Step::Ready(StreamOutput::Carrier(
            AacAdtsCarrier {
                raw: prefix,
                source,
                header: None,
                boundary: AacAdtsCarrierBoundary::FollowingEnvelope,
                selected_error: error,
                inspection_end,
            },
        ))))
    }

    #[inline]
    fn grow_buffer(&mut self, work: &mut WorkBudget) -> Result<Flow> {
        // Growth moves the real old contents. Credit that copy
        // across small work slices once; each reserve attempt then
        // costs one unit. A retry never repeats source/setup work.
        let paid = usize::try_from(
            work.take(u64::try_from(self.growth_copy_credit).map_err(|_| Error::Overflow)?),
        )
        .map_err(|_| Error::Overflow)?;
        self.growth_copy_credit -= paid;
        if self.growth_copy_credit != 0 || !work.try_charge(1) {
            return Ok(YIELD);
        }
        self.check_minimum(self.growth_target)?;
        let b = self
            .buffer
            .as_mut()
            .ok_or(Error::Invalid("missing ADTS builder"))?;
        b.try_reserve_exact(
            self.growth_target
                .checked_sub(b.len())
                .ok_or(Error::Overflow)?,
        )?;
        self.phase = Phase::Body;
        Ok(NEXT)
    }

    #[inline]
    fn freeze_frame(&mut self, work: &mut WorkBudget) -> Result<Flow> {
        if !work.try_charge(1) {
            return Ok(YIELD);
        }
        self.check_minimum(Bytes::owner_overhead_bytes())?;
        let cursor = self
            .cursor
            .as_ref()
            .ok_or(Error::Invalid("missing ADTS input cursor"))?;
        let end = self
            .frame_start
            .checked_add(u64::try_from(self.frame_bytes).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        if cursor.position() != end {
            return Err(Error::Invalid("ADTS physical source extent"));
        }
        let source = AacSource {
            source: cursor.source(),
            generation: cursor.generation(),
            offset: self.frame_start,
        };
        let b = self
            .buffer
            .take()
            .ok_or(Error::Invalid("missing ADTS builder"))?;
        if b.len() != self.frame_bytes {
            return Err(Error::Invalid("ADTS complete output extent"));
        }
        let raw = match b.freeze() {
            Ok(b) => b,
            Err(e) => {
                self.buffer = Some(e.buffer);
                return Err(e.error);
            }
        };
        if self.carrier {
            let boundary = if self.eof_fragment {
                AacAdtsCarrierBoundary::ExplicitEofFragment
            } else if self.header.is_some_and(|h| h.layer() != 0) {
                AacAdtsCarrierBoundary::ParserLengthCandidate
            } else {
                AacAdtsCarrierBoundary::DeclaredEnvelope {
                    header_offset: self.header_offset,
                }
            };
            self.completed = self.completed.checked_add(1).ok_or(Error::Overflow)?;
            self.phase = Phase::Start;
            return Ok(Flow::Break(Step::Ready(StreamOutput::Carrier(
                AacAdtsCarrier {
                    raw,
                    source,
                    header: self.header,
                    boundary,
                    selected_error: self.selected_error,
                    inspection_end: cursor.position(),
                },
            ))));
        }
        let h = self
            .header
            .ok_or(Error::Invalid("missing ADTS stream header"))?;
        let header_bytes = h
            .single_block_header_bytes()
            .ok_or(Error::Unsupported("ADTS block layout"))?;
        let crc = if h.protection_absent() {
            None
        } else {
            let bytes = raw.get(7..9).ok_or(Error::Truncated)?;
            Some(u16::from_be_bytes([bytes[0], bytes[1]]))
        };
        let payload = raw.slice(header_bytes..raw.len())?;
        self.completed = self.completed.checked_add(1).ok_or(Error::Overflow)?;
        self.phase = Phase::Start;
        Ok(Flow::Break(Step::Ready(StreamOutput::Selected(
            AacAdtsFrame {
                raw,
                payload,
                source,
                header: h,
                crc,
            },
        ))))
    }

    #[inline]
    fn end_of_input(&mut self) -> Result<Flow> {
        Ok(Flow::Break(Step::End(SourceLocation {
            source: self.parked.source,
            offset: self.position(),
        })))
    }
}
