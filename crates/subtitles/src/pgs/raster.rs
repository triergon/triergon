use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub struct RasterLimits {
    pub max_axis: u16,
    pub max_pixels: usize,
    pub max_encoded: usize,
    pub max_work: usize,
}

impl Default for RasterLimits {
    fn default() -> Self {
        Self {
            max_axis: 4096,
            max_pixels: 8 * 1024 * 1024,
            max_encoded: 4 * 1024 * 1024,
            max_work: 32 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub struct IndexedObject {
    pub width: u16,
    pub height: u16,
    pixels: Bytes,
}

impl IndexedObject {
    pub fn pixels(&self) -> &Bytes {
        &self.pixels
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RasterStatus {
    Yield,
    Ready,
}

#[derive(Clone, Copy, Debug)]
enum State {
    Reserve,
    Allocate,
    Token,
    Control,
    Extended(u8),
    Colour(u16),
    Run { count: u16, index: u8 },
    Ready,
    Finished,
}

/// Reads each encoded byte and writes each output pixel once. A pending run is
/// explicit so even a one-unit work quantum can resume without rereading input.
/// Runs must fit their source row; only the final row may omit its EOL marker.
#[derive(Debug)]
pub struct RasterJob {
    encoded: Option<Bytes>,
    pixels: Option<BudgetVec<u8>>,
    width: u16,
    height: u16,
    area: usize,
    column: u16,
    row: u16,
    at: usize,
    work: usize,
    limits: RasterLimits,
    state: State,
    failed: Option<Error>,
    receiver_tail: bool,
}

impl RasterJob {
    pub fn new(
        encoded: Bytes,
        width: u16,
        height: u16,
        budget: &Budget,
        limits: RasterLimits,
    ) -> Result<Self> {
        if !encoded.belongs_to(budget) {
            return Err(Error::ForeignPool);
        }
        if width == 0 || height == 0 {
            return Err(Error::Invalid("PGS zero object dimension"));
        }
        for axis in [width, height] {
            if axis > limits.max_axis {
                return Err(Error::Limit {
                    requested: usize::from(axis),
                    maximum: usize::from(limits.max_axis),
                });
            }
        }
        let area = usize::from(width)
            .checked_mul(usize::from(height))
            .ok_or(Error::Overflow)?;
        for (requested, maximum) in [
            (area, limits.max_pixels),
            (encoded.len(), limits.max_encoded),
        ] {
            if requested > maximum {
                return Err(Error::Limit { requested, maximum });
            }
        }
        Ok(Self {
            encoded: Some(encoded),
            pixels: Some(BudgetVec::new(budget)),
            width,
            height,
            area,
            column: 0,
            row: 0,
            at: 0,
            work: 0,
            limits,
            state: State::Reserve,
            failed: None,
            receiver_tail: false,
        })
    }

    /// The explicit receiver stops at the completed image, retaining the full
    /// original encoded owner while leaving trailing source bytes unvalidated.
    pub(crate) fn native_receiver(
        encoded: Bytes,
        width: u16,
        height: u16,
        budget: &Budget,
        limits: RasterLimits,
    ) -> Result<Self> {
        let mut job = Self::new(encoded, width, height, budget, limits)?;
        job.receiver_tail = true;
        Ok(job)
    }

    /// Private evidence only: the strict grammar consumed the full encoding
    /// and completed every row. Receiver-tail acceptance never grants this.
    pub(crate) fn strict_validated_encoded(&self) -> Option<&Bytes> {
        if matches!(self.state, State::Ready) && !self.receiver_tail {
            self.encoded.as_ref()
        } else {
            None
        }
    }

    pub fn encoded_consumed(&self) -> usize {
        self.at
    }

    pub fn work_spent(&self) -> usize {
        self.work
    }

    fn poison(&mut self, error: Error) -> Error {
        self.encoded = None;
        self.pixels = None;
        self.failed = Some(error);
        error
    }

    fn charge(&mut self, count: usize, work: &mut WorkBudget) -> Result<bool> {
        let total = self.work.checked_add(count).ok_or(Error::Overflow)?;
        if total > self.limits.max_work {
            return Err(Error::Limit {
                requested: total,
                maximum: self.limits.max_work,
            });
        }
        if !work.try_charge(u64::try_from(count).map_err(|_| Error::Overflow)?) {
            return Ok(false);
        }
        self.work = total;
        Ok(true)
    }

    fn byte(&mut self, work: &mut WorkBudget) -> Result<Option<u8>> {
        if work.remaining() == 0 {
            return Ok(None);
        }
        let value = self
            .encoded
            .as_ref()
            .and_then(|input| input.get(self.at))
            .copied()
            .ok_or(Error::Truncated)?;
        if !self.charge(1, work)? {
            return Ok(None);
        }
        self.at += 1;
        Ok(Some(value))
    }

    fn run(&mut self, count: u16, index: u8) -> Result<()> {
        if count == 0 {
            return Err(Error::Invalid("PGS zero-length run"));
        }
        if self.row >= self.height
            || u32::from(self.column) + u32::from(count) > u32::from(self.width)
        {
            return Err(Error::Invalid("PGS run exceeds object row"));
        }
        self.state = State::Run { count, index };
        Ok(())
    }

    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<RasterStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        match self.poll_inner(work, cancel) {
            Ok(status) => Ok(status),
            Err(error @ Error::Backpressure { .. }) => Err(error),
            Err(error) => Err(self.poison(error)),
        }
    }

    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<RasterStatus> {
        let allow_allocation = work.remaining() != 0;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match self.state {
                State::Ready => return Ok(RasterStatus::Ready),
                State::Finished => return Err(Error::Invalid("PGS raster already finished")),
                State::Reserve => {
                    if !self.charge(1, work)? {
                        return Ok(RasterStatus::Yield);
                    }
                    self.state = State::Allocate;
                }
                State::Allocate => {
                    if !allow_allocation {
                        return Ok(RasterStatus::Yield);
                    }
                    self.pixels
                        .as_mut()
                        .ok_or(Error::Invalid("PGS private pixels absent"))?
                        .try_reserve_exact(self.area)?;
                    self.state = State::Token;
                }
                State::Token => {
                    let input = self
                        .encoded
                        .as_ref()
                        .ok_or(Error::Invalid("PGS private source absent"))?;
                    if self.at == input.len() {
                        let final_row = self.row.checked_add(1) == Some(self.height)
                            && self.column == self.width;
                        if self.row == self.height || final_row {
                            self.state = State::Ready;
                            continue;
                        }
                        return Err(Error::Truncated);
                    }
                    if self.row == self.height {
                        if self.receiver_tail {
                            self.state = State::Ready;
                            continue;
                        }
                        return Err(Error::Invalid("PGS trailing object RLE"));
                    }
                    let Some(byte) = self.byte(work)? else {
                        return Ok(RasterStatus::Yield);
                    };
                    if byte == 0 {
                        self.state = State::Control;
                    } else {
                        self.run(1, byte)?;
                    }
                }
                State::Control => {
                    let Some(flags) = self.byte(work)? else {
                        return Ok(RasterStatus::Yield);
                    };
                    if flags == 0 {
                        if self.column != self.width {
                            return Err(Error::Invalid("PGS incomplete object row"));
                        }
                        self.column = 0;
                        self.row = self.row.checked_add(1).ok_or(Error::Overflow)?;
                        self.state = State::Token;
                    } else if flags & 0x40 != 0 {
                        self.state = State::Extended(flags);
                    } else if flags & 0x80 != 0 {
                        self.state = State::Colour(u16::from(flags & 0x3f));
                    } else {
                        self.run(u16::from(flags & 0x3f), 0)?;
                    }
                }
                State::Extended(flags) => {
                    let Some(low) = self.byte(work)? else {
                        return Ok(RasterStatus::Yield);
                    };
                    let count = (u16::from(flags & 0x3f) << 8) | u16::from(low);
                    if flags & 0x80 != 0 {
                        self.state = State::Colour(count);
                    } else {
                        self.run(count, 0)?;
                    }
                }
                State::Colour(count) => {
                    let Some(index) = self.byte(work)? else {
                        return Ok(RasterStatus::Yield);
                    };
                    self.run(count, index)?;
                }
                State::Run { count, index } => {
                    if work.remaining() == 0 {
                        return Ok(RasterStatus::Yield);
                    }
                    let resource = self.limits.max_work.saturating_sub(self.work);
                    let amount = usize::from(count)
                        .min(usize::try_from(work.remaining()).unwrap_or(usize::MAX))
                        .min(resource);
                    if amount == 0 {
                        return Err(Error::Limit {
                            requested: self.work.checked_add(1).ok_or(Error::Overflow)?,
                            maximum: self.limits.max_work,
                        });
                    }
                    if !self.charge(amount, work)? {
                        return Ok(RasterStatus::Yield);
                    }
                    let pixels = self
                        .pixels
                        .as_mut()
                        .ok_or(Error::Invalid("PGS private pixels absent"))?;
                    let length = pixels.len().checked_add(amount).ok_or(Error::Overflow)?;
                    pixels.try_resize(length, index)?;
                    let amount = u16::try_from(amount).map_err(|_| Error::Overflow)?;
                    self.column = self.column.checked_add(amount).ok_or(Error::Overflow)?;
                    self.state = if amount == count {
                        State::Token
                    } else {
                        State::Run {
                            count: count - amount,
                            index,
                        }
                    };
                }
            }
        }
    }

    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<IndexedObject> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if !matches!(self.state, State::Ready) {
            return Err(Error::Invalid("PGS raster not ready"));
        }
        let pixels = self
            .pixels
            .take()
            .ok_or(Error::Invalid("PGS private pixels absent"))?;
        match pixels.freeze() {
            Ok(pixels) => {
                self.encoded = None;
                self.state = State::Finished;
                Ok(IndexedObject {
                    width: self.width,
                    height: self.height,
                    pixels,
                })
            }
            Err(failure) => {
                self.pixels = Some(failure.buffer);
                Err(failure.error)
            }
        }
    }
}
