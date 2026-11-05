//! Complete source-unit boundaries within a supplied packet, without copying.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpuEncoding {
    Traditional,
    Extended,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpuSpan {
    pub offset: usize,
    pub bytes: usize,
    pub encoding: SpuEncoding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpuTailReason {
    ShortHeader,
    DeclaredSizeTooSmall { declared: usize, minimum: usize },
    Incomplete { declared: usize, available: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpuTail {
    pub offset: usize,
    pub reason: SpuTailReason,
}

#[derive(Clone, Copy, Debug)]
pub struct SpuPacketLimits {
    pub max_packet_bytes: usize,
    pub max_units: usize,
    pub max_headers: usize,
}
impl Default for SpuPacketLimits {
    fn default() -> Self {
        Self {
            max_packet_bytes: 16 * 1024 * 1024,
            max_units: 1024,
            max_headers: 1024,
        }
    }
}

/// All supplied bytes and every complete declared-size unit remain accessible.
/// This establishes framing, not command/RLE validity or one event per unit.
/// A receiver selecting the first unit must keep this complete source record.
#[derive(Debug)]
pub struct SpuPacket {
    raw: Bytes,
    units: BudgetVec<SpuSpan>,
    tail: Option<SpuTail>,
}
impl SpuPacket {
    /// Release the source owner and return the empty, charged unit table.
    pub fn into_unit_storage(mut self) -> BudgetVec<SpuSpan> {
        self.units.clear();
        self.units
    }

    pub fn raw(&self) -> &Bytes {
        &self.raw
    }
    pub fn units(&self) -> &[SpuSpan] {
        self.units.as_slice()
    }
    pub fn tail(&self) -> Option<SpuTail> {
        self.tail
    }
    pub fn unit(&self, index: usize) -> Result<Bytes> {
        let span = self
            .units
            .as_slice()
            .get(index)
            .ok_or(Error::Invalid("DVD source unit index"))?;
        self.raw
            .slice(span.offset..span.offset.checked_add(span.bytes).ok_or(Error::Overflow)?)
    }
    pub fn tail_bytes(&self) -> &[u8] {
        self.tail.map_or(&[], |tail| &self.raw[tail.offset..])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpuPacketStatus {
    Yield,
    Ready,
}

#[derive(Debug)]
pub struct SpuPacketJob {
    raw: Option<Bytes>,
    units: Option<BudgetVec<SpuSpan>>,
    limits: SpuPacketLimits,
    offset: usize,
    headers: usize,
    pending: Option<SpuSpan>,
    tail: Option<SpuTail>,
    ready: bool,
    finished: bool,
    failed: Option<Error>,
}
impl SpuPacketJob {
    pub fn new(raw: Bytes, budget: &Budget, limits: SpuPacketLimits) -> Result<Self> {
        Self::with_unit_storage(raw, budget, limits, BudgetVec::new(budget))
    }

    /// Reuse a unit table after its previous source packet has been released.
    pub fn with_unit_storage(
        raw: Bytes,
        budget: &Budget,
        limits: SpuPacketLimits,
        mut units: BudgetVec<SpuSpan>,
    ) -> Result<Self> {
        if !raw.belongs_to(budget) || !units.budget().same_pool(budget) {
            return Err(Error::ForeignPool);
        }
        if raw.len() > limits.max_packet_bytes {
            return Err(Error::Limit {
                requested: raw.len(),
                maximum: limits.max_packet_bytes,
            });
        }
        units.clear();
        Ok(Self {
            raw: Some(raw),
            units: Some(units),
            limits,
            offset: 0,
            headers: 0,
            pending: None,
            tail: None,
            ready: false,
            finished: false,
            failed: None,
        })
    }
    pub fn headers_processed(&self) -> usize {
        self.headers
    }
    fn poison(&mut self, error: Error) -> Error {
        self.raw = None;
        self.units = None;
        self.pending = None;
        self.failed = Some(error);
        error
    }
    fn stop_at(&mut self, reason: SpuTailReason) {
        self.tail = Some(SpuTail {
            offset: self.offset,
            reason,
        });
        self.ready = true;
    }
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<SpuPacketStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.finished {
            return Err(self.poison(Error::Invalid("DVD packet already finished")));
        }
        if self.ready {
            return Ok(SpuPacketStatus::Ready);
        }
        match self.inner(work, cancel) {
            Err(error @ Error::Backpressure { .. }) => Err(error),
            Err(error) => Err(self.poison(error)),
            status => status,
        }
    }
    fn inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<SpuPacketStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if work.remaining() == 0 {
                return Ok(SpuPacketStatus::Yield);
            }
            let raw = self
                .raw
                .as_ref()
                .ok_or(Error::Invalid("DVD packet source absent"))?;
            let raw_len = raw.len();
            let span = if let Some(span) = self.pending {
                span
            } else {
                if self.offset == raw.len() {
                    self.ready = true;
                    return Ok(SpuPacketStatus::Ready);
                }
                let next = self.headers.checked_add(1).ok_or(Error::Overflow)?;
                if next > self.limits.max_headers {
                    return Err(Error::Limit {
                        requested: next,
                        maximum: self.limits.max_headers,
                    });
                }
                work.take(1);
                self.headers = next;
                let available = raw.len() - self.offset;
                if available < 2 {
                    self.stop_at(SpuTailReason::ShortHeader);
                    return Ok(SpuPacketStatus::Ready);
                }
                let input = &raw[self.offset..];
                let short = usize::from(u16::from_be_bytes([input[0], input[1]]));
                let (declared, minimum, encoding) = if short == 0 {
                    if available < 10 {
                        self.stop_at(SpuTailReason::ShortHeader);
                        return Ok(SpuPacketStatus::Ready);
                    }
                    (
                        usize::try_from(u32::from_be_bytes([
                            input[2], input[3], input[4], input[5],
                        ]))
                        .map_err(|_| Error::Overflow)?,
                        10,
                        SpuEncoding::Extended,
                    )
                } else {
                    (short, 4, SpuEncoding::Traditional)
                };
                if declared < minimum {
                    self.stop_at(SpuTailReason::DeclaredSizeTooSmall { declared, minimum });
                    return Ok(SpuPacketStatus::Ready);
                }
                if declared > available {
                    self.stop_at(SpuTailReason::Incomplete {
                        declared,
                        available,
                    });
                    return Ok(SpuPacketStatus::Ready);
                }
                let count = self
                    .units
                    .as_ref()
                    .ok_or(Error::Invalid("DVD packet units absent"))?
                    .len();
                if count == self.limits.max_units {
                    return Err(Error::Limit {
                        requested: count.checked_add(1).ok_or(Error::Overflow)?,
                        maximum: self.limits.max_units,
                    });
                }
                let span = SpuSpan {
                    offset: self.offset,
                    bytes: declared,
                    encoding,
                };
                self.pending = Some(span);
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                if work.remaining() == 0 {
                    return Ok(SpuPacketStatus::Yield);
                }
                span
            };
            self.units
                .as_mut()
                .ok_or(Error::Invalid("DVD packet units absent"))?
                .try_push(span)
                .map_err(|e| e.error)?;
            // A span was admitted only if its end was within this source.
            self.offset = span.offset + span.bytes;
            self.pending = None;
            if self.offset == raw_len {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                self.ready = true;
                return Ok(SpuPacketStatus::Ready);
            }
        }
    }
    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<SpuPacket> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if !self.ready || self.finished {
            return Err(self.poison(Error::Invalid("DVD packet not ready")));
        }
        self.finished = true;
        Ok(SpuPacket {
            raw: self
                .raw
                .take()
                .ok_or(Error::Invalid("DVD packet source absent"))?,
            units: self
                .units
                .take()
                .ok_or(Error::Invalid("DVD packet units absent"))?,
            tail: self.tail,
        })
    }
}
