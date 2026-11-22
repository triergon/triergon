use super::{PictureState, VvcNalHeader, embedded_header, slice_kind};
use crate::RawAnnexBLimits;
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, WorkBudget,
};

/// A complete source access unit with its original Annex B bytes.
#[derive(Debug)]
pub struct VvcStreamAccessUnit {
    pub source: SourceId,
    pub range: Range<u64>,
    /// First start code including its required zero byte, after leading padding.
    pub first_start: u64,
    pub bytes: Bytes,
}

/// Incremental layer-zero VVC Annex B framing with explicit picture headers.
/// Input starvation can separate any two bytes and never finishes a picture.
/// The last picture is published only after the source sends its logical EOF.
#[derive(Debug)]
pub struct VvcAnnexBReader {
    input: InputCursor,
    bytes: BudgetVec<u8>,
    limits: RawAnnexBLimits,
    scan: Scan,
    start: u64,
    tail: usize,
    boundary: Option<usize>,
    end: Option<SourceLocation>,
    allow_short_embedded_prefix: bool,
    failure: Option<Error>,
}

// Four bytes of prefix followed by the two header bytes and the embedded flag.
const LOOKAHEAD: usize = 7;

#[derive(Clone, Copy, Debug)]
struct Nal {
    start: usize,
    zero_byte: bool,
    head: [u8; 3],
    bad_payload: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct Scan {
    picture: PictureState,
    first_start: Option<usize>,
    nal: Option<Nal>,
    zeroes: usize,
    unit_nals: usize,
    total_nals: usize,
}

impl VvcAnnexBReader {
    /// Start strict single-layer framing with the existing Annex B limits.
    pub fn new(
        pool: &Budget,
        source: SourceId,
        caps: SourceCapabilities,
        limits: RawAnnexBLimits,
    ) -> Result<Self> {
        if limits.maximum_access_unit_bytes == 0
            || limits
                .maximum_access_unit_bytes
                .checked_add(LOOKAHEAD)
                .is_none()
            || limits.maximum_access_unit_nals == 0
            || limits.nal.maximum_nal_bytes == 0
            || limits.nal.maximum_nal_units == 0
        {
            return Err(Error::Invalid("VVC Annex B limits"));
        }
        Ok(Self {
            input: InputCursor::new(pool, source, caps, limits.nal.maximum_input_window)?,
            bytes: BudgetVec::new(pool),
            limits,
            scan: Scan::default(),
            start: 0,
            tail: 0,
            boundary: None,
            end: None,
            allow_short_embedded_prefix: false,
            failure: None,
        })
    }

    pub(crate) fn with_short_embedded_prefix(mut self) -> Self {
        self.allow_short_embedded_prefix = true;
        self
    }

    pub fn provide_owned(&mut self, window: InputWindow) -> Result<()> {
        self.healthy()?;
        self.input.provide_owned(window)
    }

    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        self.healthy()?;
        let result = self.input.finish(source, token);
        if let Err(error @ Error::Truncated) = result {
            self.stop(error);
        }
        result
    }

    /// Finish with a sticky source failure and discard any unpublished bytes.
    pub fn fail(
        &mut self,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        self.healthy()?;
        self.input.fail(source, token, failure)?;
        self.stop(match failure {
            SourceFailure::Transport => Error::Transport,
            SourceFailure::Cancelled => Error::Cancelled,
        });
        Ok(())
    }

    /// Capacity retained across an input request. The transport keeps the
    /// underlying provided source owners accounted for separately.
    pub fn input_wait_buffer_capacity(&self) -> usize {
        self.bytes.capacity()
    }

    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<VvcStreamAccessUnit>> {
        self.healthy()?;
        let result = self.advance(work, cancel);
        if let Err(error) = result
            && !matches!(error, Error::Backpressure { .. } | Error::Allocation)
        {
            self.stop(error);
        }
        result
    }

    fn healthy(&self) -> Result<()> {
        self.failure.map_or(Ok(()), Err)
    }

    fn stop(&mut self, error: Error) {
        self.failure = Some(error);
        self.bytes = BudgetVec::new(self.input.budget());
        let source = self.input.source();
        let token = self.input.token();
        let _ = self.input.fail(source, token, SourceFailure::Cancelled);
    }

    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<VvcStreamAccessUnit>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if let Some(boundary) = self.boundary {
                return self.publish(boundary).map(Step::Ready);
            }
            if let Some(end) = self.end {
                return Ok(Step::End(end));
            }
            if self.tail != 0 {
                self.resume_tail()?;
            }
            let max = self.limits.maximum_access_unit_bytes + LOOKAHEAD;
            let at = self.bytes.len();
            let cap = max - at;
            let available = self.input.buffered();
            let limit = available
                .len()
                .min(cap)
                .min(work.remaining().min(256) as usize);
            if limit != 0 {
                self.bytes.try_reserve(limit)?;
                let mut scan = self.scan;
                let mut boundary = None;
                let mut count = limit;
                for (index, &byte) in available[..limit].iter().enumerate() {
                    boundary = match scan.byte(
                        at + index,
                        byte,
                        self.limits,
                        self.allow_short_embedded_prefix,
                    ) {
                        Ok(boundary) => boundary,
                        Err(error) => {
                            work.take((index + 1) as u64);
                            return Err(error);
                        }
                    };
                    if boundary.is_some() {
                        count = index + 1;
                        break;
                    }
                }
                let Step::Ready(written) =
                    self.input
                        .read_append(&mut self.bytes, count, work, cancel)?
                else {
                    return Err(Error::Invalid("VVC input cursor changed"));
                };
                debug_assert_eq!(written, count);
                self.scan = scan;
                self.boundary = boundary;
                continue;
            }
            if !available.is_empty() && cap == 0 {
                return Err(Error::Limit {
                    requested: at,
                    maximum: self.limits.maximum_access_unit_bytes,
                });
            }
            match self.input.read_discard(1, work, cancel)? {
                Step::End(end) => {
                    if !self.bytes.is_empty() {
                        self.scan.complete(at, self.limits)?;
                        self.boundary = Some(at);
                    }
                    self.end = Some(end);
                }
                Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                Step::Yield => return Ok(Step::Yield),
                Step::Backpressure(never) => match never {},
                Step::Ready(_) => return Err(Error::Invalid("VVC input cursor changed")),
            }
        }
    }

    fn publish(&mut self, split: usize) -> Result<VvcStreamAccessUnit> {
        let maximum = self.limits.maximum_access_unit_bytes;
        if split > maximum {
            return Err(Error::Limit {
                requested: split,
                maximum,
            });
        }
        let begin = self.start;
        let end = begin.checked_add(split as u64).ok_or(Error::Overflow)?;
        let first_start = begin
            .checked_add(self.scan.first_start.ok_or(Error::Truncated)? as u64)
            .ok_or(Error::Overflow)?;
        let tail = &self.bytes.as_slice()[split..];
        let mut next = BudgetVec::new(self.input.budget());
        next.try_extend_from_slice(tail)?;
        let previous = std::mem::replace(&mut self.bytes, next);
        let bytes = match previous.freeze() {
            Ok(bytes) => bytes.into_slice(0..split)?,
            Err(error) => {
                self.bytes = error.buffer;
                return Err(error.error);
            }
        };
        self.start = end;
        self.tail = self.bytes.len();
        self.boundary = None;
        self.scan = Scan {
            total_nals: self.scan.total_nals,
            ..Scan::default()
        };
        Ok(VvcStreamAccessUnit {
            source: self.input.source(),
            range: begin..end,
            first_start,
            bytes,
        })
    }

    fn resume_tail(&mut self) -> Result<()> {
        // These bytes have already consumed work. They consist of at most
        // one prefix and its header, so this can never rescan a payload.
        let mut scan = self.scan;
        for (at, &byte) in self.bytes.as_slice()[..self.tail].iter().enumerate() {
            scan.byte(at, byte, self.limits, self.allow_short_embedded_prefix)?;
        }
        self.scan = scan;
        self.tail = 0;
        Ok(())
    }
}

impl Scan {
    fn byte(
        &mut self,
        at: usize,
        byte: u8,
        limits: RawAnnexBLimits,
        allow_short: bool,
    ) -> Result<Option<usize>> {
        if byte == 1 && self.zeroes >= 2 {
            let zero_byte = self.zeroes >= 3;
            if self.nal.is_some() {
                self.finish_nal(at - self.zeroes, true, limits)?;
            } else {
                if !zero_byte && self.total_nals == 0 {
                    return Err(Error::Invalid("missing VVC start-code zero byte"));
                }
                let padding = self.zeroes.saturating_sub(2 + usize::from(zero_byte));
                Self::check_limit(padding, limits.nal.maximum_padding_bytes)?;
            }
            self.first_start
                .get_or_insert(at - 2 - usize::from(zero_byte));
            self.nal = Some(Nal {
                start: at + 1,
                zero_byte,
                head: [0; 3],
                bad_payload: false,
            });
            self.zeroes = 0;
            return Ok(None);
        }
        let Some(nal) = self.nal.as_mut() else {
            if byte != 0 {
                return Err(Error::Invalid("missing VVC start-code zero byte"));
            }
            self.zeroes += 1;
            return Ok(None);
        };

        let following = at - nal.start;
        if let Some(slot) = nal.head.get_mut(following) {
            *slot = byte;
        }
        let mut boundary = None;
        if following == 1 || following == 2 {
            let kind = nal.head[1] >> 3;
            if (following == 1 && !slice_kind(kind)) || (following == 2 && slice_kind(kind)) {
                let embedded = following == 2 && embedded_header(kind, Some(byte));
                if self.picture.boundary(kind, embedded, false) {
                    if !self.picture.vcl || !(nal.zero_byte || (allow_short && embedded)) {
                        return Err(Error::Invalid("ambiguous VVC access-unit boundary"));
                    }
                    boundary = Some(nal.start - 3 - usize::from(nal.zero_byte));
                }
            }
        }
        if byte == 0 {
            self.zeroes += 1;
        } else {
            nal.bad_payload |= self.zeroes >= 3 || (self.zeroes >= 2 && byte <= 2);
            self.zeroes = 0;
        }
        Ok(boundary)
    }

    fn finish_nal(&mut self, end: usize, has_next: bool, limits: RawAnnexBLimits) -> Result<()> {
        let nal = self.nal.ok_or(Error::Truncated)?;
        let len = end.checked_sub(nal.start).ok_or(Error::Truncated)?;
        let prefix_bytes = usize::from(has_next) * (2 + usize::from(self.zeroes >= 3));
        let padding = self.zeroes.saturating_sub(prefix_bytes);
        Self::check_limit(len, limits.nal.maximum_nal_bytes)?;
        Self::check_limit(padding, limits.nal.maximum_padding_bytes)?;
        let unit_nals = self.unit_nals.checked_add(1).ok_or(Error::Overflow)?;
        let total_nals = self.total_nals.checked_add(1).ok_or(Error::Overflow)?;
        Self::check_limit(unit_nals, limits.maximum_access_unit_nals)?;
        Self::check_limit(total_nals, limits.nal.maximum_nal_units)?;
        if nal.bad_payload {
            return Err(Error::Invalid("invalid VVC Annex B payload"));
        }
        let header = VvcNalHeader::inspect(&nal.head[..len.min(3)])?;
        self.picture.check(
            nal.zero_byte,
            len,
            (len >= 3).then_some(nal.head[2]),
            header,
            false,
            has_next,
        )?;
        self.unit_nals = unit_nals;
        self.total_nals = total_nals;
        Ok(())
    }

    fn complete(&mut self, end: usize, limits: RawAnnexBLimits) -> Result<()> {
        self.finish_nal(end - self.zeroes, false, limits)?;
        if !self.picture.vcl {
            return Err(Error::Invalid("VVC access unit without a slice"));
        }
        Ok(())
    }

    fn check_limit(requested: usize, maximum: usize) -> Result<()> {
        if requested > maximum {
            Err(Error::Limit { requested, maximum })
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nal_count_overflows_are_checked_before_publication() {
        let limits = RawAnnexBLimits::default();
        for (unit_nals, total_nals) in [(usize::MAX, 0), (0, usize::MAX)] {
            let mut scan = Scan {
                nal: Some(Nal {
                    start: 4,
                    zero_byte: true,
                    head: [0, 1, 0x80],
                    bad_payload: false,
                }),
                unit_nals,
                total_nals,
                ..Scan::default()
            };
            assert_eq!(scan.finish_nal(7, false, limits), Err(Error::Overflow));
        }
    }
}
