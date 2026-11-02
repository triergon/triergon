//! Original NativeD32 default/automatic palette construction from pinned public
//! observations. Source facts/projections are never changed by this local view.
use super::colour::{ColourProjection, ColourSource, PaletteEntry, palette_size};
use super::display_state::DecodeWork;
use super::native_palette_buffer::{NativePaletteBuffer, RgbaStorage};
use super::{Depth, NativePalette};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result};

#[derive(Debug)]
struct Census {
    pixels: Bytes,
    counts: BudgetVec<u32>,
    count: usize,
    pixel_at: usize,
    distinct: u32,
    credit: usize,
}

impl Census {
    fn new(budget: &Budget, count: usize, pixels: Bytes) -> Result<Self> {
        if !pixels.belongs_to(budget) {
            return Err(Error::ForeignPool);
        }
        let mut counts = BudgetVec::new(budget);
        counts.try_reserve_exact(count)?;
        Ok(Self {
            pixels,
            counts,
            count,
            pixel_at: 0,
            distinct: 0,
            credit: 0,
        })
    }

    fn step(&mut self, work: &mut DecodeWork<'_>) -> Result<bool> {
        while self.counts.len() < self.count {
            if !work.pay(&mut self.credit, 1)? {
                return Ok(false);
            }
            self.counts.try_push(0).map_err(|error| error.error)?;
            self.credit = 0;
        }
        while self.pixel_at < self.pixels.len() {
            if !work.pay(&mut self.credit, 1)? {
                return Ok(false);
            }
            let index = usize::from(self.pixels.as_ref()[self.pixel_at]);
            let old = *self
                .counts
                .as_slice()
                .get(index)
                .ok_or(Error::Invalid("DVB native automatic index"))?;
            let next = old.checked_add(1).ok_or(Error::Overflow)?;
            if old == 0 {
                self.distinct = self.distinct.checked_add(1).ok_or(Error::Overflow)?;
            }
            self.counts.as_mut_slice()[index] = next;
            self.pixel_at += 1;
            self.credit = 0;
        }
        Ok(true)
    }
}

#[derive(Debug)]
pub(super) struct NativePalettePlan {
    budget: Budget,
    depth: Depth,
    count: usize,
    automatic: bool,
    scan_at: usize,
    needs_owned_rgba: bool,
    census: Option<Census>,
    pixels: Option<Bytes>,
    buffer: Option<NativePaletteBuffer>,
    at: usize,
    rank: u32,
    credit: usize,
}

impl NativePalettePlan {
    pub(super) fn new(
        budget: &Budget,
        depth: Depth,
        automatic: bool,
        pixels: Bytes,
    ) -> Result<Self> {
        if !pixels.belongs_to(budget) {
            return Err(Error::ForeignPool);
        }
        Ok(Self {
            budget: budget.clone(),
            depth,
            count: palette_size(depth),
            automatic,
            scan_at: 0,
            needs_owned_rgba: automatic,
            census: None,
            pixels: Some(pixels),
            buffer: None,
            at: 0,
            rank: 0,
            credit: 0,
        })
    }

    /// Explicit plan/census cursors pay for actual metadata/pixels. A freeze
    /// retry after all entries never recreates or rescans the released census.
    pub(super) fn prepare(
        &mut self,
        entries: &[PaletteEntry],
        effective: &[ColourSource],
        work: &mut DecodeWork<'_>,
    ) -> Result<bool> {
        if entries.len() != self.count || effective.len() != self.count {
            return Err(Error::Invalid("DVB native palette source shape"));
        }
        if self.at == self.count {
            return Ok(true);
        }
        while self.scan_at < self.count {
            let source = effective[self.scan_at];
            let cost = if matches!(source, ColourSource::YCrCbT { .. }) {
                12
            } else {
                1
            };
            if !work.pay(&mut self.credit, cost)? {
                return Ok(false);
            }
            self.needs_owned_rgba |= match source {
                ColourSource::Default { .. } => true,
                ColourSource::YCrCbT { .. } => {
                    encoded_rgba(source)?
                        != entries[self.scan_at]
                            .source
                            .rgba(ColourProjection::StudioUnrepaired8)?
                }
            };
            self.scan_at += 1;
            self.credit = 0;
        }
        if self.buffer.is_none() {
            let storage = if self.needs_owned_rgba {
                RgbaStorage::OwnedNative
            } else {
                RgbaStorage::AliasStudioUnrepaired
            };
            self.buffer = Some(NativePaletteBuffer::new(&self.budget, self.count, storage)?);
        }
        if self.automatic {
            if self.census.is_none() {
                let pixels = self
                    .pixels
                    .as_ref()
                    .ok_or(Error::Invalid("DVB native plane absent"))?
                    .clone();
                self.census = Some(Census::new(&self.budget, self.count, pixels)?);
            }
            return self
                .census
                .as_mut()
                .ok_or(Error::Invalid("DVB native census absent"))?
                .step(work);
        }
        Ok(true)
    }

    /// The observed per-region automatic palette ranks distinct used indices
    /// numerically, independent of frequency/encounter order. Lowest used is
    /// transparent; one-colour and unused entries are literal zero. This is a
    /// versioned receiver fact, not ETSI default CLUT replacement semantics.
    pub(super) fn push_entry(
        &mut self,
        index: usize,
        source: ColourSource,
        studio: [u8; 4],
    ) -> Result<()> {
        if index != self.at || index >= self.count {
            return Err(Error::Invalid("DVB native palette position"));
        }
        let mut used = false;
        let native = if self.automatic {
            let census = self
                .census
                .as_ref()
                .ok_or(Error::Invalid("DVB native census absent"))?;
            if census.pixel_at != census.pixels.len() || census.counts.len() != self.count {
                return Err(Error::Invalid("DVB native census incomplete"));
            }
            used = census.counts.as_slice()[index] != 0;
            if !used || census.distinct <= 1 {
                [0, 0, 0, 0]
            } else {
                let alpha = u8::try_from(
                    self.rank.checked_mul(255).ok_or(Error::Overflow)? / (census.distinct - 1),
                )
                .map_err(|_| Error::Overflow)?;
                [alpha >> 1, alpha, alpha >> 1, alpha]
            }
        } else {
            match source {
                ColourSource::Default { depth, index } => {
                    if depth != self.depth {
                        return Err(Error::Invalid("DVB native default depth"));
                    }
                    fixed_default(depth, index)?
                }
                ColourSource::YCrCbT { .. } => encoded_rgba(source)?,
            }
        };
        self.buffer
            .as_mut()
            .ok_or(Error::Invalid("DVB native buffer absent"))?
            .push_entry(index, native, studio)?;
        if used {
            self.rank = self.rank.checked_add(1).ok_or(Error::Overflow)?;
        }
        self.at += 1;
        Ok(())
    }

    pub(super) fn try_finish(
        &mut self,
        studio: &Bytes,
        all_entries_explicit: bool,
    ) -> Result<NativePalette> {
        if self.at != self.count {
            return Err(Error::Invalid("DVB native palette incomplete"));
        }
        // Collection state is no longer needed. Release it before reserving
        // the immutable owner control; retries keep the completed output builder.
        self.census = None;
        self.pixels = None;
        self.buffer
            .as_mut()
            .ok_or(Error::Invalid("DVB native buffer absent"))?
            .try_finish(studio, all_entries_explicit)
    }
}

/// Original observed D32 encoded-colour view. Independently quantizing the
/// primary studio inverse to10 fractional bits supplied this candidate; all
/// 16,777,216 fullY/Cr/Cb triples were then compared with frozen public-API raw
/// palettes. This is neither normative rounding nor a change to source views.
fn encoded_rgba(source: ColourSource) -> Result<[u8; 4]> {
    let ColourSource::YCrCbT {
        y,
        cr,
        cb,
        transparency,
        full_resolution,
    } = source
    else {
        return Err(Error::Invalid("DVB native encoded colour"));
    };
    if !full_resolution && (y > 63 || cr > 15 || cb > 15 || transparency > 3) {
        return Err(Error::Invalid("DVB reduced palette component"));
    }
    let (y, cr, cb, transparency) = if full_resolution {
        (y, cr, cb, transparency)
    } else {
        (y << 2, cr << 4, cb << 4, transparency << 6)
    };
    let yy = i32::from(y) - 16;
    let rr = i32::from(cr) - 128;
    let bb = i32::from(cb) - 128;
    let component = |numerator: i32| ((numerator + 512) / 1024).clamp(0, 255) as u8;
    Ok([
        component(1192 * yy + 1634 * rr),
        component(1192 * yy - 832 * rr - 401 * bb),
        component(1192 * yy + 2066 * bb),
        if y == 0 { 0 } else { 255 - transparency },
    ])
}

/// Independently tabulated full fixed-default grids from the preserved native
/// captures, interpreted with EN300743 Tables36-38 source bit assignments.
fn fixed_default(depth: Depth, index: u8) -> Result<[u8; 4]> {
    if index > depth.maximum() {
        return Err(Error::Invalid("DVB native default palette index"));
    }
    if index == 0 {
        return Ok([0, 0, 0, 0]);
    }
    Ok(match depth {
        Depth::Two => match index {
            1 => [255, 255, 255, 255],
            2 => [0, 0, 0, 255],
            _ => [127, 127, 127, 255],
        },
        Depth::Four => {
            let on = if index < 8 { 255 } else { 127 };
            [
                if index & 1 != 0 { on } else { 0 },
                if index & 2 != 0 { on } else { 0 },
                if index & 4 != 0 { on } else { 0 },
                255,
            ]
        }
        Depth::Eight if index < 8 => [
            if index & 1 != 0 { 255 } else { 0 },
            if index & 2 != 0 { 255 } else { 0 },
            if index & 4 != 0 { 255 } else { 0 },
            63,
        ],
        Depth::Eight => {
            let (components, alpha) = match index & 0x88 {
                0 => ([0, 85, 170, 255], 255),
                8 => ([0, 85, 170, 255], 127),
                128 => ([127, 170, 212, 255], 255),
                _ => ([0, 43, 85, 128], 255),
            };
            let pair = |low: u8, high: u8| {
                usize::from(((index >> low) & 1) | (((index >> high) & 1) << 1))
            };
            [
                components[pair(0, 4)],
                components[pair(1, 5)],
                components[pair(2, 6)],
                alpha,
            ]
        }
    })
}
