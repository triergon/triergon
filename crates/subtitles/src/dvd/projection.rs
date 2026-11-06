//! The receiver's bitmap selection and transparent-border crop.
//! Full source indices remain owned and unchanged; cropped rows are borrowed.
use super::{DvdArea, DvdBitmap};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub struct ProjectionLimits {
    pub max_pixels: usize,
    pub max_work: usize,
}

impl Default for ProjectionLimits {
    fn default() -> Self {
        Self {
            max_pixels: 8 * 1024 * 1024,
            max_work: 8 * 1024 * 1024 + 1,
        }
    }
}

/// Native menu handling bypasses transparent-border removal. This is separate
/// from individual control/rectangle forced flags and source validity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CropMode {
    Ordinary,
    Menu,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CropSelection {
    Rendered,
    ReceiverOneRowOmission,
    ReceiverTransparentOmission,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crop {
    pub area: DvdArea,
    pub width: u16,
    pub height: u16,
    pub source_x: u16,
    pub source_y: u16,
}

#[derive(Debug)]
pub struct ProjectedBitmap<const N: usize> {
    source: DvdBitmap,
    palette: [u32; N],
    selection: CropSelection,
    crop: Option<Crop>,
}

impl<const N: usize> ProjectedBitmap<N> {
    pub(super) fn into_source(self) -> DvdBitmap {
        self.source
    }

    pub fn source(&self) -> &DvdBitmap {
        &self.source
    }
    pub fn palette(&self) -> &[u32; N] {
        &self.palette
    }
    pub fn selection(&self) -> CropSelection {
        self.selection
    }
    pub fn cropped_view(&self) -> Option<CroppedView<'_, N>> {
        self.crop.map(|crop| CroppedView {
            source: &self.source,
            palette: &self.palette,
            crop,
        })
    }
}

/// Resolve a crop without a scan when the validated raster and palette
/// determine it. Leave the image and the budget untouched on fallback.
pub(super) fn project_traditional_raster(
    source: DvdBitmap,
    palette: [u32; 4],
    mode: CropMode,
    limits: ProjectionLimits,
    work: &mut WorkBudget,
) -> std::result::Result<ProjectedBitmap<4>, DvdBitmap> {
    let count = source.pixels().len();
    if count > limits.max_pixels {
        return Err(source);
    }
    let (selection, visible, units) = if source.height == 1 {
        (CropSelection::ReceiverOneRowOmission, false, 1)
    } else if mode == CropMode::Menu {
        (CropSelection::Rendered, true, 1)
    } else {
        let opaque = palette[0] >> 24 != 0;
        if !palette.iter().all(|color| (*color >> 24 != 0) == opaque) {
            return Err(source);
        }
        let selection = if opaque {
            CropSelection::Rendered
        } else {
            CropSelection::ReceiverTransparentOmission
        };
        // A traditional raster fits within the dimensions encoded on the wire.
        (selection, opaque, count + 1)
    };
    if units > limits.max_work || !work.try_charge(units as u64) {
        return Err(source);
    }
    let crop = visible.then_some(Crop {
        area: source.area,
        width: source.width,
        height: source.height,
        source_x: 0,
        source_y: 0,
    });
    Ok(ProjectedBitmap {
        source,
        palette,
        selection,
        crop,
    })
}

#[derive(Clone, Copy, Debug)]
pub struct CroppedView<'a, const N: usize> {
    source: &'a DvdBitmap,
    palette: &'a [u32; N],
    crop: Crop,
}

impl<'a, const N: usize> CroppedView<'a, N> {
    pub fn crop(self) -> Crop {
        self.crop
    }
    pub fn palette(self) -> &'a [u32; N] {
        self.palette
    }

    /// A row is a direct source span. A crop never materializes another plane.
    pub fn indices_row(self, row: u16) -> Result<&'a [u8]> {
        if row >= self.crop.height {
            return Err(Error::Invalid("DVD native row index"));
        }
        let start = usize::from(self.crop.source_y)
            .checked_add(usize::from(row))
            .and_then(|y| y.checked_mul(usize::from(self.source.width)))
            .and_then(|v| v.checked_add(usize::from(self.crop.source_x)))
            .ok_or(Error::Overflow)?;
        let end = start
            .checked_add(usize::from(self.crop.width))
            .ok_or(Error::Overflow)?;
        self.source
            .pixels()
            .get(start..end)
            .ok_or(Error::Invalid("DVD native source span"))
    }

    pub fn argb_at(self, x: u16, y: u16) -> Result<u32> {
        let index = *self
            .indices_row(y)?
            .get(usize::from(x))
            .ok_or(Error::Invalid("DVD native column index"))?;
        self.palette
            .get(usize::from(index))
            .copied()
            .ok_or(Error::Invalid("DVD palette index"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectionStatus {
    Yield,
    Ready,
}

/// Four-entry traditional and 256-entry extended palettes are explicit native
/// ARGB words supplied by the decoder/consumer. No missing source RGB is guessed.
/// Immutable indices come from the bounded raster decoder or an explicitly
/// supplied, geometry-checked plane. Supplied narrow-palette indices are checked
/// cooperatively before a view or receiver omission can be published.
#[derive(Debug)]
pub struct CropJob<const N: usize> {
    source: Option<DvdBitmap>,
    palette: [u32; N],
    mode: CropMode,
    validate_indices: bool,
    uniform_alpha: bool,
    limits: ProjectionLimits,
    at: usize,
    count: usize,
    left: usize,
    top: usize,
    right: usize,
    bottom: usize,
    setup: bool,
    work: usize,
    selection: CropSelection,
    crop: Option<Crop>,
    ready: bool,
    finished: bool,
    failed: Option<Error>,
}

impl<const N: usize> CropJob<N> {
    pub fn new(
        source: DvdBitmap,
        palette: [u32; N],
        mode: CropMode,
        budget: &Budget,
        limits: ProjectionLimits,
    ) -> Result<Self> {
        if N != 4 && N != 256 {
            return Err(Error::Unsupported("DVD native palette size"));
        }
        if !source.pixels().belongs_to(budget) {
            return Err(Error::ForeignPool);
        }
        if source.area.dimensions()? != (source.width, source.height) {
            return Err(Error::Invalid("DVD bitmap geometry mismatch"));
        }
        let count = usize::from(source.width)
            .checked_mul(usize::from(source.height))
            .ok_or(Error::Overflow)?;
        if count != source.pixels().len() {
            return Err(Error::Invalid("DVD bitmap plane length"));
        }
        if count > limits.max_pixels {
            return Err(Error::Limit {
                requested: count,
                maximum: limits.max_pixels,
            });
        }
        let (left, top) = (usize::from(source.width), usize::from(source.height));
        let validate_indices = usize::from(source.index_upper_bound()) >= N;
        let uniform_alpha = N == 4
            && !validate_indices
            && palette
                .iter()
                .all(|colour| (*colour >> 24 != 0) == (palette[0] >> 24 != 0));
        Ok(Self {
            source: Some(source),
            palette,
            mode,
            validate_indices,
            uniform_alpha,
            limits,
            at: 0,
            count,
            left,
            top,
            right: 0,
            bottom: 0,
            setup: false,
            work: 0,
            selection: CropSelection::Rendered,
            crop: None,
            ready: false,
            finished: false,
            failed: None,
        })
    }

    pub fn work_spent(&self) -> usize {
        self.work
    }
    pub fn pixels_scanned(&self) -> usize {
        self.at
    }
    fn poison(&mut self, error: Error) -> Error {
        self.source = None;
        self.failed = Some(error);
        error
    }

    fn pay(&mut self, work: &mut WorkBudget) -> Result<bool> {
        if work.remaining() == 0 {
            return Ok(false);
        }
        let next = self.work.checked_add(1).ok_or(Error::Overflow)?;
        if next > self.limits.max_work {
            return Err(Error::Limit {
                requested: next,
                maximum: self.limits.max_work,
            });
        }
        if !work.try_charge(1) {
            return Ok(false);
        }
        self.work = next;
        Ok(true)
    }

    fn set_crop(&mut self, x: usize, y: usize, width: usize, height: usize) -> Result<()> {
        let source = self
            .source
            .as_ref()
            .ok_or(Error::Invalid("DVD projection source absent"))?;
        let x = u16::try_from(x).map_err(|_| Error::Overflow)?;
        let y = u16::try_from(y).map_err(|_| Error::Overflow)?;
        let width = u16::try_from(width).map_err(|_| Error::Overflow)?;
        let height = u16::try_from(height).map_err(|_| Error::Overflow)?;
        let left = source.area.left.checked_add(x).ok_or(Error::Overflow)?;
        let top = source.area.top.checked_add(y).ok_or(Error::Overflow)?;
        let right = left
            .checked_add(
                width
                    .checked_sub(1)
                    .ok_or(Error::Invalid("DVD zero native width"))?,
            )
            .ok_or(Error::Overflow)?;
        let bottom = top
            .checked_add(
                height
                    .checked_sub(1)
                    .ok_or(Error::Invalid("DVD zero native height"))?,
            )
            .ok_or(Error::Overflow)?;
        self.crop = Some(Crop {
            area: DvdArea {
                left,
                right,
                top,
                bottom,
            },
            width,
            height,
            source_x: x,
            source_y: y,
        });
        self.ready = true;
        Ok(())
    }

    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ProjectionStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.ready {
            return Ok(ProjectionStatus::Ready);
        }
        if self.finished {
            return Err(self.poison(Error::Invalid("DVD projection already finished")));
        }
        match self.inner(work, cancel) {
            Ok(status) => Ok(status),
            Err(error) => Err(self.poison(error)),
        }
    }

    fn inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ProjectionStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.ready {
                return Ok(ProjectionStatus::Ready);
            }
            if !self.setup {
                if !self.pay(work)? {
                    return Ok(ProjectionStatus::Yield);
                }
                self.setup = true;
                let source = self
                    .source
                    .as_ref()
                    .ok_or(Error::Invalid("DVD projection source absent"))?;
                if source.height == 1 && !self.validate_indices {
                    self.selection = CropSelection::ReceiverOneRowOmission;
                    self.ready = true;
                    continue;
                }
                if self.mode == CropMode::Menu && !self.validate_indices {
                    self.set_crop(0, 0, usize::from(source.width), usize::from(source.height))?;
                    continue;
                }
                if self.uniform_alpha && self.palette[0] >> 24 != 0 {
                    self.right = usize::from(source.width);
                    self.bottom = usize::from(source.height);
                    self.left = 0;
                    self.top = 0;
                }
            }
            if self.at == self.count {
                let source = self
                    .source
                    .as_ref()
                    .ok_or(Error::Invalid("DVD projection source absent"))?;
                if source.height == 1 {
                    self.selection = CropSelection::ReceiverOneRowOmission;
                    self.ready = true;
                } else if self.mode == CropMode::Menu {
                    self.set_crop(0, 0, usize::from(source.width), usize::from(source.height))?;
                } else if self.right == 0 || self.bottom == 0 {
                    self.selection = CropSelection::ReceiverTransparentOmission;
                    self.ready = true;
                } else {
                    self.set_crop(
                        self.left,
                        self.top,
                        self.right - self.left,
                        self.bottom - self.top,
                    )?;
                }
                continue;
            }
            if self.uniform_alpha {
                // The raster already proved every index is in range. With
                // uniform opacity the crop is known, but charge and report
                // the same pixel work so limits and partial polls still apply.
                let pixels = (self.count - self.at)
                    .min(self.limits.max_work - self.work)
                    .min(usize::try_from(work.remaining()).unwrap_or(usize::MAX));
                if pixels != 0 {
                    work.take(pixels as u64);
                    self.work += pixels;
                    self.at += pixels;
                    continue;
                }
            }
            if !self.pay(work)? {
                return Ok(ProjectionStatus::Yield);
            }
            let source = self
                .source
                .as_ref()
                .ok_or(Error::Invalid("DVD projection source absent"))?;
            let index = *source
                .pixels()
                .get(self.at)
                .ok_or(Error::Invalid("DVD projection pixel bounds"))?;
            let colour = *self
                .palette
                .get(usize::from(index))
                .ok_or(Error::Invalid("DVD palette index"))?;
            if colour >> 24 != 0 {
                let x = self.at % usize::from(source.width);
                let y = self.at / usize::from(source.width);
                self.left = self.left.min(x);
                self.top = self.top.min(y);
                self.right = self.right.max(x + 1);
                self.bottom = self.bottom.max(y + 1);
            }
            self.at += 1;
        }
    }

    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<ProjectedBitmap<N>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if !self.ready || self.finished {
            return Err(self.poison(Error::Invalid("DVD projection not ready")));
        }
        let source = self
            .source
            .take()
            .ok_or(Error::Invalid("DVD projection source absent"))?;
        self.ready = false;
        self.finished = true;
        Ok(ProjectedBitmap {
            source,
            palette: self.palette,
            selection: self.selection,
            crop: self.crop,
        })
    }
}
