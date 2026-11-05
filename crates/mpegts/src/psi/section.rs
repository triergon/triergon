use crate::{
    AssemblyStep, ParseLimits, SourceRange, checked_pid, commit_range, limit, prepare_range,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, PushError, Result, SourceId, WorkBudget};

// H.222.0 Annex A uses the most-significant bit first and polynomial
// 0x04c11db7. Compute the eight shifts for each possible high byte once.
const CRC_OCTET: [u32; 256] = {
    let mut table = [0; 256];
    let mut byte = 0;
    while byte < table.len() {
        let mut remainder = (byte as u32) << 24;
        let mut bits = 0;
        while bits < 8 {
            remainder = (remainder << 1)
                ^ if remainder & 0x8000_0000 != 0 {
                    0x04c1_1db7
                } else {
                    0
                };
            bits += 1;
        }
        table[byte] = remainder;
        byte += 1;
    }
    table
};

#[derive(Clone, Copy, Debug)]
pub struct PsiCrc32(u32);
impl Default for PsiCrc32 {
    fn default() -> Self {
        Self(0xffff_ffff)
    }
}
impl PsiCrc32 {
    pub fn update_byte(&mut self, byte: u8) {
        let index = (self.0 >> 24) as u8 ^ byte;
        self.0 = (self.0 << 8) ^ CRC_OCTET[usize::from(index)];
    }
    pub fn remainder(self) -> u32 {
        self.0
    }
    /// Convenience for small bounded buffers. Incremental assembly updates per byte.
    pub fn of(bytes: &[u8]) -> u32 {
        let mut crc = Self::default();
        for byte in bytes {
            crc.update_byte(*byte);
        }
        crc.remainder()
    }
}

#[derive(Debug)]
pub struct Section {
    pid: u16,
    source: Bytes,
    ranges: BudgetVec<SourceRange>,
    /// Known CRC meaning only for PAT and PMT; do not guess private/SI semantics.
    crc: Option<u32>,
}
impl Section {
    pub fn pid(&self) -> u16 {
        self.pid
    }
    pub fn bytes(&self) -> &Bytes {
        &self.source
    }
    pub fn source_ranges(&self) -> &[SourceRange] {
        self.ranges.as_slice()
    }
    pub fn crc_remainder(&self) -> Option<u32> {
        self.crc
    }
    /// Internal fallible clone of the actual immutable source section. It adds
    /// no validation or acceptance and preserves the original owners and ranges.
    pub(crate) fn try_clone_charged_v1(&self, budget: &Budget) -> Result<Self> {
        if !self.source.belongs_to(budget) {
            return Err(Error::ForeignPool);
        }
        let mut ranges = BudgetVec::new(budget);
        ranges.try_extend_from_slice(self.source_ranges())?;
        Ok(Self {
            pid: self.pid,
            source: self.source.clone(),
            ranges,
            crc: self.crc,
        })
    }
}

#[derive(Debug)]
struct Payload {
    bytes: Bytes,
    position: usize,
    fresh_at: Option<usize>,
    fresh: bool,
    stuffing: bool,
    entity: SourceId,
    offset: u64,
}

/// One known section PID. Caller routes PAT/PMT PIDs and resets this on continuity
/// loss. Several complete sections can be returned from a single supplied payload.
#[derive(Debug)]
pub struct SectionAssembler {
    pid: u16,
    budget: Budget,
    limits: ParseLimits,
    buffer: Option<BudgetVec<u8>>,
    ranges: BudgetVec<SourceRange>,
    target: Option<usize>,
    crc: PsiCrc32,
    payload: Option<Payload>,
    ended: bool,
    fault: Option<Error>,
}

impl SectionAssembler {
    pub fn new(pid: u16, budget: Budget, limits: ParseLimits) -> Result<Self> {
        checked_pid(pid)?;
        limits.validate()?;
        Ok(Self {
            pid,
            buffer: Some(BudgetVec::new(&budget)),
            ranges: BudgetVec::new(&budget),
            budget,
            limits,
            target: None,
            crc: PsiCrc32::default(),
            payload: None,
            ended: false,
            fault: None,
        })
    }
    /// Rejection returns the exact supplied charged span; no input was accepted.
    pub fn push(
        &mut self,
        bytes: Bytes,
        pusi: bool,
        entity: SourceId,
        offset: u64,
    ) -> std::result::Result<(), PushError<Bytes>> {
        let failure = if self.payload.is_some() {
            Some(Error::Invalid("PSI payload not drained"))
        } else if self.ended {
            Some(Error::Invalid("PSI after EOF"))
        } else if self.fault.is_some() {
            self.fault
        } else if !bytes.belongs_to(&self.budget) {
            Some(Error::Invalid("foreign PSI byte pool"))
        } else if offset.checked_add(bytes.len() as u64).is_none() {
            Some(Error::Overflow)
        } else if pusi && (bytes.is_empty() || usize::from(bytes.as_ref()[0]) + 1 > bytes.len()) {
            Some(Error::Invalid("PSI pointer"))
        } else {
            None
        };
        if let Some(error) = failure {
            return Err(PushError {
                error,
                value: bytes,
            });
        }
        let fresh_at = if pusi {
            Some(1 + usize::from(bytes.as_ref()[0]))
        } else {
            None
        };
        self.payload = Some(Payload {
            bytes,
            position: usize::from(pusi),
            fresh_at,
            fresh: false,
            stuffing: false,
            entity,
            offset,
        });
        Ok(())
    }
    pub fn end_of_source(&mut self) {
        self.ended = true;
    }
    pub fn partial_bytes(&self) -> &[u8] {
        self.buffer.as_ref().map_or(&[], |buffer| buffer.as_slice())
    }
    pub(crate) fn idle(&self) -> bool {
        self.payload.is_none()
            && self.target.is_none()
            && self.fault.is_none()
            && !self.ended
            && self.buffer.as_ref().is_some_and(BudgetVec::is_empty)
    }
    /// Explicit loss/recovery transition; normal EndSegment must not call this.
    pub fn discard(&mut self) {
        self.buffer = Some(BudgetVec::new(&self.budget));
        self.ranges = BudgetVec::new(&self.budget);
        self.payload = None;
        self.target = None;
        self.crc = PsiCrc32::default();
        self.fault = None;
        self.ended = false;
    }
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<Section>> {
        if let Some(error) = self.fault {
            return Err(error);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(error) = result.as_ref() {
            if !matches!(
                error,
                Error::Backpressure { .. } | Error::Allocation | Error::Cancelled
            ) {
                self.fault = Some(*error);
            }
        }
        result
    }
    fn poll_inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<Section>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let length = self
                .buffer
                .as_ref()
                .ok_or(Error::Invalid("PSI builder missing"))?
                .len();
            if self.target == Some(length) {
                let builder = self
                    .buffer
                    .take()
                    .ok_or(Error::Invalid("PSI builder missing"))?;
                let source = match builder.freeze() {
                    Ok(source) => source,
                    Err(rejected) => {
                        self.buffer = Some(rejected.buffer);
                        return Err(rejected.error);
                    }
                };
                let crc = if matches!(source.as_ref()[0], 0 | 2) {
                    Some(self.crc.remainder())
                } else {
                    None
                };
                let ranges = std::mem::replace(&mut self.ranges, BudgetVec::new(&self.budget));
                self.buffer = Some(BudgetVec::new(&self.budget));
                self.target = None;
                self.crc = PsiCrc32::default();
                if let Some(payload) = self.payload.as_mut() {
                    if !payload.fresh {
                        payload.stuffing = true;
                    }
                }
                return Ok(AssemblyStep::Output(Section {
                    pid: self.pid,
                    source,
                    ranges,
                    crc,
                }));
            }
            let payload = match self.payload.as_ref() {
                Some(payload) => payload,
                None => {
                    return if self.ended {
                        if length != 0 {
                            Err(Error::Truncated)
                        } else {
                            Ok(AssemblyStep::Finished)
                        }
                    } else {
                        Ok(AssemblyStep::NeedPayload)
                    };
                }
            };
            if payload.fresh_at == Some(payload.position) && !payload.fresh {
                if length != 0 {
                    return Err(Error::Invalid("PSI pointer before partial end"));
                }
                let payload = self
                    .payload
                    .as_mut()
                    .ok_or(Error::Invalid("PSI payload missing"))?;
                payload.fresh = true;
                payload.stuffing = false;
                continue;
            }
            if payload.position == payload.bytes.len() {
                self.payload = None;
                continue;
            }
            let byte = payload.bytes.as_ref()[payload.position];
            let prefix = payload.fresh_at.is_some_and(|end| payload.position < end);
            if length == 0 && (prefix || payload.fresh_at.is_none()) && !payload.stuffing {
                let end = payload
                    .fresh_at
                    .unwrap_or(payload.bytes.len())
                    .min(payload.bytes.len());
                let count = work.take((end - payload.position) as u64) as usize;
                if count == 0 {
                    return Ok(AssemblyStep::Yield);
                }
                self.payload
                    .as_mut()
                    .ok_or(Error::Invalid("PSI payload missing"))?
                    .position += count;
                continue;
            }
            if payload.stuffing || (length == 0 && byte == 0xff) {
                // Check a bounded prefix of the stuffing run per cancellation
                // poll. The pointer may start a fresh section inside this payload.
                let end = payload
                    .fresh_at
                    .filter(|&start| payload.position < start)
                    .unwrap_or(payload.bytes.len());
                let available = (end - payload.position).min(64) as u64;
                let count = available.min(work.remaining()) as usize;
                if count == 0 {
                    return Ok(AssemblyStep::Yield);
                }
                let run = &payload.bytes.as_ref()[payload.position..payload.position + count];
                if let Some(invalid) = run.iter().position(|&byte| byte != 0xff) {
                    work.take((invalid + 1) as u64);
                    return Err(Error::Invalid("PSI stuffing tail"));
                }
                work.take(count as u64);
                let payload = self
                    .payload
                    .as_mut()
                    .ok_or(Error::Invalid("PSI payload missing"))?;
                payload.stuffing = true;
                payload.position += count;
                continue;
            }
            if !work.try_charge(1) {
                return Ok(AssemblyStep::Yield);
            }
            let target = self.target.unwrap_or(3);
            let stop = payload
                .fresh_at
                .filter(|&start| payload.position < start)
                .unwrap_or(payload.bytes.len());
            let count = (target - length)
                .min(stop - payload.position)
                .min(work.remaining().min(63) as usize + 1);
            let run = &payload.bytes.as_ref()[payload.position..payload.position + count];
            let physical = payload
                .offset
                .checked_add(payload.position as u64)
                .ok_or(Error::Overflow)?;
            let extent = SourceRange {
                entity: payload.entity,
                offset: physical,
                bytes: count,
            };
            let builder = self
                .buffer
                .as_mut()
                .ok_or(Error::Invalid("PSI builder missing"))?;
            if length == self.limits.section_bytes {
                return Err(limit(length.saturating_add(1), self.limits.section_bytes));
            }
            builder.try_reserve_exact(target - length)?;
            let merge = prepare_range(&mut self.ranges, extent, self.limits.source_spans)?;
            builder.try_extend_from_slice(run)?;
            commit_range(&mut self.ranges, extent, merge)?;
            for &byte in run {
                self.crc.update_byte(byte);
            }
            work.take(count as u64 - 1);
            self.payload
                .as_mut()
                .ok_or(Error::Invalid("PSI payload missing"))?
                .position += count;
            if length + count == 3 {
                let data = self
                    .buffer
                    .as_ref()
                    .ok_or(Error::Invalid("PSI builder missing"))?
                    .as_slice();
                let count = (usize::from(data[1] & 15) << 8) | usize::from(data[2]);
                let total = 3 + count;
                if (data[0] == 0 && count < 9)
                    || (data[0] == 2 && count < 13)
                    || (matches!(data[0], 0 | 2) && total > 1024)
                    || (data[1] & 128 != 0 && count < 9)
                {
                    return Err(Error::Invalid("PSI section length"));
                }
                if total > self.limits.section_bytes {
                    return Err(limit(total, self.limits.section_bytes));
                }
                self.target = Some(total);
            }
        }
    }
}
