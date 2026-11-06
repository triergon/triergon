use super::display_model::{
    PaletteSnapshot, SegmentStamp, StoredDisplayObject, VisibleGeometry, geometry,
};
use super::display_work::DisplayWork;
use super::indexed_argb::ArgbStorage;
use super::{CompositionObject, Window};
use std::marker::PhantomData;
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result};

#[derive(Clone, Debug)]
pub(crate) struct CachedObject {
    pub version: u8,
    pub width: u16,
    pub height: u16,
    pub pixels: Bytes,
    /// Retained only when the real backing is smaller than strict-decoded pixels.
    /// This is actual owned input, not a hash, ID or inferred validation token.
    pub validated_rle: Option<Bytes>,
    pub first: SegmentStamp,
    pub last: SegmentStamp,
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    ReserveNative,
    AllocateNative,
    NativePixels,
    FreezeNative,
    VisibleSetup,
    ReserveVisible,
    AllocateVisible,
    VisiblePixels,
    FreezeVisible,
    Ready,
}

#[derive(Debug)]
pub(crate) struct StoredRenderBuild<A: ArgbStorage> {
    storage: PhantomData<A>,
    budget: Budget,
    object: CachedObject,
    reference: CompositionObject,
    window: Option<Window>,
    palette: PaletteSnapshot,
    visible: VisibleGeometry,
    native_x: u16,
    native_y: u16,
    suppressed: bool,
    native: Option<BudgetVec<u8>>,
    native_frozen: Option<Bytes>,
    visible_buffer: Option<BudgetVec<u8>>,
    visible_frozen: Option<Bytes>,
    phase: Phase,
    credit: usize,
    at: usize,
    visible_at: usize,
}

impl<A: ArgbStorage> StoredRenderBuild<A> {
    pub(super) fn new(
        budget: &Budget,
        object: CachedObject,
        reference: CompositionObject,
        window: Option<Window>,
        palette: PaletteSnapshot,
        canvas: (u16, u16),
        forced_only: bool,
    ) -> Result<Self> {
        if !object.pixels.belongs_to(budget) || !palette.belongs_to(budget) {
            return Err(Error::ForeignPool);
        }
        let visible = geometry(reference, window, (object.width, object.height), canvas)?;
        let (native_x, native_y) = if reference.x > canvas.0 || reference.y > canvas.1 {
            (0, 0)
        } else {
            (reference.x, reference.y)
        };
        // Validate representable byte extent even when no eager plane is made.
        object.pixels.len().checked_mul(4).ok_or(Error::Overflow)?;
        Ok(Self {
            storage: PhantomData,
            budget: budget.clone(),
            object,
            reference,
            window,
            palette,
            visible,
            native_x,
            native_y,
            suppressed: forced_only && !reference.forced(),
            native: A::MATERIALIZE.then(|| BudgetVec::new(budget)),
            native_frozen: None,
            visible_buffer: None,
            visible_frozen: None,
            phase: if A::MATERIALIZE {
                Phase::ReserveNative
            } else {
                Phase::VisibleSetup
            },
            credit: 0,
            at: 0,
            visible_at: 0,
        })
    }

    pub(super) fn step(&mut self, work: &mut DisplayWork<'_>) -> Result<bool> {
        loop {
            match self.phase {
                Phase::ReserveNative => {
                    if !work.pay(&mut self.credit, 1)? {
                        return Ok(false);
                    }
                    self.phase = Phase::AllocateNative;
                }
                Phase::AllocateNative => {
                    let length = self
                        .object
                        .pixels
                        .len()
                        .checked_mul(4)
                        .ok_or(Error::Overflow)?;
                    self.native
                        .as_mut()
                        .ok_or(Error::Invalid("PGS render buffer absent"))?
                        .try_reserve_exact(length)?;
                    self.credit = 0;
                    self.phase = Phase::NativePixels;
                }
                Phase::NativePixels => {
                    let length = self
                        .object
                        .pixels
                        .len()
                        .checked_mul(4)
                        .ok_or(Error::Overflow)?;
                    if self.at == length {
                        self.phase = Phase::FreezeNative;
                        continue;
                    }
                    let count = (length - self.at)
                        .min(256)
                        .min(usize::try_from(work.caller.remaining()).unwrap_or(usize::MAX))
                        .min(work.maximum.saturating_sub(*work.spent))
                        .max(1);
                    if !work.pay(&mut self.credit, count)? {
                        return Ok(false);
                    }
                    let mut bytes = [0; 256];
                    let mut written = 0;
                    while written < count {
                        let at = self.at + written;
                        let offset = at % 4;
                        let word = if self.suppressed {
                            0
                        } else {
                            self.palette.native_argb(self.object.pixels[at / 4])
                        }
                        .to_be_bytes();
                        let take = (4 - offset).min(count - written);
                        bytes[written..written + take]
                            .copy_from_slice(&word[offset..offset + take]);
                        written += take;
                    }
                    self.native
                        .as_mut()
                        .ok_or(Error::Invalid("PGS render buffer absent"))?
                        .try_extend_from_slice(&bytes[..count])?;
                    self.at += count;
                    self.credit = 0;
                }
                Phase::FreezeNative => {
                    if !work.pay(&mut self.credit, 1)? {
                        return Ok(false);
                    }
                    let buffer = self
                        .native
                        .take()
                        .ok_or(Error::Invalid("PGS render buffer absent"))?;
                    match buffer.freeze() {
                        Ok(bytes) => {
                            self.native_frozen = Some(bytes);
                            self.credit = 0;
                            self.phase = Phase::VisibleSetup;
                        }
                        Err(e) => {
                            self.native = Some(e.buffer);
                            return Err(e.error);
                        }
                    }
                }
                Phase::VisibleSetup => {
                    if !work.pay(&mut self.credit, 1)? {
                        return Ok(false);
                    }
                    let width = usize::from(self.visible.width);
                    let height = usize::from(self.visible.height);
                    let area = width.checked_mul(height).ok_or(Error::Overflow)?;
                    if area == 0 {
                        self.visible_frozen = Some(self.object.pixels.slice(0..0)?);
                        self.phase = Phase::Ready;
                    } else if height == 1 || self.visible.width == self.object.width {
                        let start = usize::from(self.visible.source_y)
                            * usize::from(self.object.width)
                            + usize::from(self.visible.source_x);
                        let end = start.checked_add(area).ok_or(Error::Overflow)?;
                        self.visible_frozen = Some(self.object.pixels.slice(start..end)?);
                        self.phase = Phase::Ready;
                    } else {
                        self.visible_buffer = Some(BudgetVec::new(&self.budget));
                        self.phase = Phase::ReserveVisible;
                    }
                    self.credit = 0;
                }
                Phase::ReserveVisible => {
                    if !work.pay(&mut self.credit, 1)? {
                        return Ok(false);
                    }
                    self.phase = Phase::AllocateVisible;
                }
                Phase::AllocateVisible => {
                    let area = usize::from(self.visible.width) * usize::from(self.visible.height);
                    self.visible_buffer
                        .as_mut()
                        .ok_or(Error::Invalid("PGS visible buffer absent"))?
                        .try_reserve_exact(area)?;
                    self.credit = 0;
                    self.phase = Phase::VisiblePixels;
                }
                Phase::VisiblePixels => {
                    let width = usize::from(self.visible.width);
                    let area = width * usize::from(self.visible.height);
                    if self.visible_at == area {
                        self.phase = Phase::FreezeVisible;
                        continue;
                    }
                    if !work.pay(&mut self.credit, 1)? {
                        return Ok(false);
                    }
                    let x = usize::from(self.visible.source_x) + self.visible_at % width;
                    let y = usize::from(self.visible.source_y) + self.visible_at / width;
                    let index = y * usize::from(self.object.width) + x;
                    let byte = *self
                        .object
                        .pixels
                        .get(index)
                        .ok_or(Error::Invalid("PGS visible source bounds"))?;
                    self.visible_buffer
                        .as_mut()
                        .ok_or(Error::Invalid("PGS visible buffer absent"))?
                        .try_push(byte)
                        .map_err(|e| e.error)?;
                    self.visible_at += 1;
                    self.credit = 0;
                }
                Phase::FreezeVisible => {
                    if !work.pay(&mut self.credit, 1)? {
                        return Ok(false);
                    }
                    let buffer = self
                        .visible_buffer
                        .take()
                        .ok_or(Error::Invalid("PGS visible buffer absent"))?;
                    match buffer.freeze() {
                        Ok(bytes) => {
                            self.visible_frozen = Some(bytes);
                            self.credit = 0;
                            self.phase = Phase::Ready;
                        }
                        Err(e) => {
                            self.visible_buffer = Some(e.buffer);
                            return Err(e.error);
                        }
                    }
                }
                Phase::Ready => return Ok(true),
            }
        }
    }

    pub(super) fn take(&mut self) -> Result<StoredDisplayObject<A>> {
        if !matches!(self.phase, Phase::Ready) {
            return Err(Error::Invalid("PGS rendered object not ready"));
        }
        Ok(StoredDisplayObject {
            composition: self.reference,
            window: self.window,
            object_version: self.object.version,
            object_width: self.object.width,
            object_height: self.object.height,
            object_first_stamp: self.object.first,
            object_last_stamp: self.object.last,
            visible: self.visible,
            visibility_window_known: self.window.is_some(),
            native_x: self.native_x,
            native_y: self.native_y,
            native_forced_only_suppressed: self.suppressed,
            pixels: self.object.pixels.clone(),
            visible_pixels: self
                .visible_frozen
                .take()
                .ok_or(Error::Invalid("PGS visible output absent"))?,
            native_argb_pixels: A::finish(self.native_frozen.take())?,
            palette: self.palette.clone(),
        })
    }
}
