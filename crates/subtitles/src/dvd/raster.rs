use super::{DvdArea, DvdPacket, FieldOffsets};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, BytesWriter, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub struct DvdRasterLimits {
    pub max_axis: u16,
    pub max_pixels: usize,
    pub max_work: usize,
}

impl Default for DvdRasterLimits {
    fn default() -> Self {
        Self {
            max_axis: 4096,
            max_pixels: 8 * 1024 * 1024,
            max_work: 32 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub struct DvdBitmap {
    pub area: DvdArea,
    pub width: u16,
    pub height: u16,
    pixels: Bytes,
    index_upper_bound: u8,
}

impl DvdBitmap {
    /// Transfer an already indexed plane under an explicit pixel ceiling.
    /// This validates storage/geometry; it does not claim RLE or wire decoding.
    pub fn from_indices(
        area: DvdArea,
        pixels: Bytes,
        budget: &Budget,
        maximum: usize,
    ) -> Result<Self> {
        if !pixels.belongs_to(budget) {
            return Err(Error::ForeignPool);
        }
        let (width, height) = area.dimensions()?;
        let count = usize::from(width)
            .checked_mul(usize::from(height))
            .ok_or(Error::Overflow)?;
        if count > maximum {
            return Err(Error::Limit {
                requested: count,
                maximum,
            });
        }
        if pixels.len() != count {
            return Err(Error::Invalid("DVD indexed plane length"));
        }
        Ok(Self {
            area,
            width,
            height,
            pixels,
            index_upper_bound: u8::MAX,
        })
    }

    pub fn pixels(&self) -> &Bytes {
        &self.pixels
    }

    pub(super) fn into_pixels(self) -> Bytes {
        self.pixels
    }

    pub(super) fn index_upper_bound(&self) -> u8 {
        self.index_upper_bound
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DvdRasterStatus {
    Yield,
    Ready,
}

#[derive(Clone, Copy, Debug)]
enum State {
    Reserve,
    Allocate,
    Token { value: u16, nibbles: u8 },
    Run { count: u16, index: u8 },
    Ready,
    Finished,
}

#[derive(Debug)]
enum RasterPixels {
    Fresh(BudgetVec<u8>),
    Reused(BytesWriter),
}

/// Two independently positioned field streams are expanded directly into one
/// row-major output. Each pixel is written once; no full-plane zero fill or
/// second interlace buffer is needed. A partial code/run survives every yield.
#[derive(Debug)]
pub struct DvdRasterJob {
    packet: Option<Bytes>,
    decode: RasterDecode,
}

// The traditional receiver can borrow its control program's immutable SPU
// for each poll. The public raster job owns the input for standalone use.
#[derive(Debug)]
pub(super) struct RasterDecode {
    pixels: Option<RasterPixels>,
    area: DvdArea,
    width: u16,
    height: u16,
    count: usize,
    bound: usize,
    nibble_at: [usize; 2],
    row: u16,
    column: u16,
    nibbles_read: usize,
    work: usize,
    limits: DvdRasterLimits,
    state: State,
    failed: Option<Error>,
}

impl DvdRasterJob {
    pub fn new(
        packet: Bytes,
        area: DvdArea,
        fields: FieldOffsets,
        budget: &Budget,
        limits: DvdRasterLimits,
    ) -> Result<Self> {
        if !packet.belongs_to(budget) {
            return Err(Error::ForeignPool);
        }
        let control_offset = DvdPacket::parse(&packet)?.control_offset();
        let dimensions = area.dimensions()?;
        let decode = RasterDecode::new(
            area,
            fields,
            dimensions,
            budget,
            limits,
            None,
            control_offset,
        )?;
        Ok(Self {
            packet: Some(packet),
            decode,
        })
    }

    pub fn work_spent(&self) -> usize {
        self.decode.work
    }

    pub fn nibbles_read(&self) -> usize {
        self.decode.nibbles_read
    }

    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<DvdRasterStatus> {
        let result = self
            .decode
            .poll(self.packet.as_deref().unwrap_or_default(), work, cancel);
        if self.decode.failed.is_some() {
            self.packet = None;
        }
        result
    }

    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<DvdBitmap> {
        let result = self.decode.try_finish(cancel);
        if result.is_ok() || self.decode.failed.is_some() {
            self.packet = None;
        }
        result
    }
}

impl RasterDecode {
    pub(super) fn new(
        area: DvdArea,
        fields: FieldOffsets,
        dimensions: (u16, u16),
        budget: &Budget,
        limits: DvdRasterLimits,
        storage: Option<BytesWriter>,
        control_offset: usize,
    ) -> Result<Self> {
        let bound = control_offset.checked_mul(2).ok_or(Error::Overflow)?;
        let (width, height) = dimensions;
        for value in [width, height] {
            if value > limits.max_axis {
                return Err(Error::Limit {
                    requested: usize::from(value),
                    maximum: usize::from(limits.max_axis),
                });
            }
        }
        let count = usize::from(width)
            .checked_mul(usize::from(height))
            .ok_or(Error::Overflow)?;
        if count > limits.max_pixels {
            return Err(Error::Limit {
                requested: count,
                maximum: limits.max_pixels,
            });
        }
        let nibble_at = [usize::from(fields.top) * 2, usize::from(fields.bottom) * 2];
        for (field, at) in nibble_at.into_iter().enumerate() {
            // An unused bottom field of a one-line object may point at the
            // control boundary; a field with actual rows must address pixels.
            let rows = if field == 0 {
                height.div_ceil(2)
            } else {
                height / 2
            };
            if at < 8 || at > bound || (rows != 0 && at == bound) {
                return Err(Error::Invalid("DVD field offset outside pixel data"));
            }
        }
        let pixels = match storage {
            Some(writer) if writer.capacity() >= count => RasterPixels::Reused(writer),
            _ => RasterPixels::Fresh(BudgetVec::new(budget)),
        };
        Ok(Self {
            pixels: Some(pixels),
            area,
            width,
            height,
            count,
            bound,
            nibble_at,
            row: 0,
            column: 0,
            nibbles_read: 0,
            work: 0,
            limits,
            state: State::Reserve,
            failed: None,
        })
    }

    fn poison(&mut self, error: Error) -> Error {
        self.pixels = None;
        self.failed = Some(error);
        error
    }

    fn charge(&mut self, n: usize, work: &mut WorkBudget) -> Result<bool> {
        let total = self.work.checked_add(n).ok_or(Error::Overflow)?;
        if total > self.limits.max_work {
            return Err(Error::Limit {
                requested: total,
                maximum: self.limits.max_work,
            });
        }
        if !work.try_charge(u64::try_from(n).map_err(|_| Error::Overflow)?) {
            return Ok(false);
        }
        self.work = total;
        Ok(true)
    }

    fn nibble(&mut self, packet: &[u8], work: &mut WorkBudget) -> Result<Option<u8>> {
        if work.remaining() == 0 {
            return Ok(None);
        }
        let field = usize::from(self.row & 1);
        let at = self.nibble_at[field];
        if at >= self.bound {
            return Err(Error::Truncated);
        }
        let byte = *packet.get(at / 2).ok_or(Error::Truncated)?;
        if !self.charge(1, work)? {
            return Ok(None);
        }
        self.nibble_at[field] += 1;
        self.nibbles_read += 1;
        Ok(Some(if at & 1 == 0 { byte >> 4 } else { byte & 15 }))
    }

    fn aligned_short_token(packet: &[u8], at: usize, bound: usize) -> Option<(u8, usize)> {
        if at & 1 != 0 || at >= bound {
            return None;
        }
        let byte = *packet.get(at / 2)?;
        match byte {
            0x40..=0xff => Some((byte >> 4, 1)),
            0x10..=0x3f => Some((byte, 2)),
            _ => None,
        }
    }

    /// Process one row from each interlaced field when both aligned tokens
    /// describe complete rows. Partial runs and small work budgets use the
    /// regular decoder, including its precise limit and error reporting.
    fn paired_rows(&mut self, packet: &[u8], work: &mut WorkBudget) -> Result<bool> {
        if self.column != 0 || self.height - self.row < 2 {
            return Ok(false);
        }
        let Some(RasterPixels::Reused(writer)) = self.pixels.as_mut() else {
            return Ok(false);
        };
        let field = usize::from(self.row & 1);
        let other = field ^ 1;
        let Some((first, n)) = Self::aligned_short_token(packet, self.nibble_at[field], self.bound)
        else {
            return Ok(false);
        };
        let Some((second, m)) =
            Self::aligned_short_token(packet, self.nibble_at[other], self.bound)
        else {
            return Ok(false);
        };
        if u16::from(first >> 2) != self.width || u16::from(second >> 2) != self.width {
            return Ok(false);
        }
        let width = usize::from(self.width);
        let units = n + m + 2 * width;
        if units > self.limits.max_work - self.work || !work.try_charge(units as u64) {
            return Ok(false);
        }
        writer.append_runs([(first & 3, width), (second & 3, width)])?;
        self.nibble_at[field] += 2;
        self.nibble_at[other] += 2;
        self.nibbles_read += n + m;
        self.work += units;
        self.row += 2;
        if self.row == self.height {
            self.state = State::Ready;
        }
        Ok(true)
    }

    fn short_token(&mut self, packet: &[u8], work: &mut WorkBudget) -> Option<u16> {
        let field = usize::from(self.row & 1);
        let at = self.nibble_at[field];
        if work.remaining() < 2 || self.limits.max_work - self.work < 2 {
            return None;
        }
        let (value, nibbles) = Self::aligned_short_token(packet, at, self.bound)?;
        work.take(nibbles as u64);
        self.work += nibbles;
        self.nibble_at[field] += nibbles;
        self.nibbles_read += nibbles;
        Some(u16::from(value))
    }

    pub(super) fn poll(
        &mut self,
        packet: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<DvdRasterStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        match self.poll_inner(packet, work, cancel) {
            Ok(value) => Ok(value),
            Err(error @ Error::Backpressure { .. }) => Err(error),
            Err(error) => Err(self.poison(error)),
        }
    }

    fn poll_inner(
        &mut self,
        packet: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<DvdRasterStatus> {
        let allow_allocation = work.remaining() != 0;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let (count, index) = match self.state {
                State::Ready => return Ok(DvdRasterStatus::Ready),
                State::Finished => return Err(Error::Invalid("DVD raster already finished")),
                State::Reserve => {
                    if !self.charge(1, work)? {
                        return Ok(DvdRasterStatus::Yield);
                    }
                    self.state = State::Allocate;
                    continue;
                }
                State::Allocate => {
                    if !allow_allocation {
                        return Ok(DvdRasterStatus::Yield);
                    }
                    let pixels = self
                        .pixels
                        .as_mut()
                        .ok_or(Error::Invalid("DVD private pixels absent"))?;
                    if let RasterPixels::Fresh(pixels) = pixels {
                        pixels.try_reserve_exact(self.count)?;
                    }
                    self.state = State::Token {
                        value: 0,
                        nibbles: 0,
                    };
                    continue;
                }
                State::Token {
                    mut value,
                    mut nibbles,
                } => {
                    let short = if nibbles == 0 {
                        if self.paired_rows(packet, work)? {
                            continue;
                        }
                        self.short_token(packet, work)
                    } else {
                        None
                    };
                    if let Some(short) = short {
                        value = short;
                    } else {
                        loop {
                            let Some(next) = self.nibble(packet, work)? else {
                                self.state = State::Token { value, nibbles };
                                return Ok(DvdRasterStatus::Yield);
                            };
                            value = value << 4 | u16::from(next);
                            nibbles += 1;
                            let threshold = match nibbles {
                                1 => 4,
                                2 => 16,
                                3 => 64,
                                _ => 0,
                            };
                            if value >= threshold {
                                break;
                            }
                            if cancel.load(Ordering::Relaxed) {
                                return Err(Error::Cancelled);
                            }
                        }
                    }
                    let remaining = self.width - self.column;
                    let count = if value < 4 { remaining } else { value >> 2 };
                    if count == 0 || count > remaining {
                        return Err(Error::Invalid("DVD run exceeds display row"));
                    }
                    let index = (value & 3) as u8;
                    self.state = State::Run { count, index };
                    if cancel.load(Ordering::Relaxed) {
                        return Err(Error::Cancelled);
                    }
                    (count, index)
                }
                State::Run { count, index } => (count, index),
            };
            let take = u64::from(count).min(work.remaining());
            if take == 0 {
                return Ok(DvdRasterStatus::Yield);
            }
            let left = self.limits.max_work - self.work;
            if left == 0 {
                return Err(Error::Limit {
                    requested: self.work.checked_add(1).ok_or(Error::Overflow)?,
                    maximum: self.limits.max_work,
                });
            }
            let take = (take as usize).min(left);
            work.take(take as u64);
            self.work += take;
            let pixels = self
                .pixels
                .as_mut()
                .ok_or(Error::Invalid("DVD private pixels absent"))?;
            // The full output capacity was reserved before decoding;
            // each run writes only its own pixels.
            match pixels {
                RasterPixels::Fresh(pixels) => {
                    let next = pixels.len().checked_add(take).ok_or(Error::Overflow)?;
                    pixels.try_resize(next, index)?;
                }
                RasterPixels::Reused(writer) => writer.append_repeated(index, take)?,
            }
            let take = u16::try_from(take).map_err(|_| Error::Overflow)?;
            self.column += take;
            if take != count {
                self.state = State::Run {
                    count: count - take,
                    index,
                };
            } else if self.column == self.width {
                let field = usize::from(self.row & 1);
                self.nibble_at[field] = (self.nibble_at[field] + 1) & !1;
                self.column = 0;
                self.row += 1;
                self.state = if self.row == self.height {
                    State::Ready
                } else {
                    State::Token {
                        value: 0,
                        nibbles: 0,
                    }
                };
            } else {
                self.state = State::Token {
                    value: 0,
                    nibbles: 0,
                };
            }
        }
    }

    pub(super) fn try_finish(&mut self, cancel: &AtomicBool) -> Result<DvdBitmap> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if !matches!(self.state, State::Ready) {
            return Err(Error::Invalid("DVD raster not ready"));
        }
        let pixels = self
            .pixels
            .take()
            .ok_or(Error::Invalid("DVD private pixels absent"))?;
        let pixels = match pixels {
            RasterPixels::Reused(writer) => writer.finish(),
            RasterPixels::Fresh(buffer) => match buffer.freeze() {
                Ok(value) => value,
                Err(error) => {
                    let reason = error.error;
                    self.pixels = Some(RasterPixels::Fresh(error.buffer));
                    return match reason {
                        Error::Backpressure { .. } => Err(reason),
                        _ => Err(self.poison(reason)),
                    };
                }
            },
        };
        self.state = State::Finished;
        Ok(DvdBitmap {
            area: self.area,
            width: self.width,
            height: self.height,
            pixels,
            index_upper_bound: 3,
        })
    }
}
