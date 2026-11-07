//! Bounded HDR10+ Application 4 syntax carried in a Matroska/WebM ITU-T T.35
//! addition. Quantized values retain their wire units and source ownership.
use crate::{Error, Result, incremental::ParseStep};
use std::sync::atomic::AtomicBool;
use triergon_core::WorkBudget;

pub(crate) const T35_HEADER: [u8; 6] = [0xb5, 0, 0x3c, 0, 1, 4];
// Structural maximum for three windows, both 25 × 25 grids and every
// optional field, excluding the registered six-byte terminal code.
pub(crate) const MAX_BODY: usize = 907;

struct Bits<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn advance(&mut self, count: usize) -> Result<usize> {
        let start = self.at;
        let end = start + count;
        if end > self.bytes.len() * 8 {
            return Err(Error::Truncated);
        }
        self.at = end;
        Ok(start)
    }

    fn read(&mut self, width: usize) -> Result<u32> {
        let at = self.advance(width)?;
        let start = at / 8;
        let end = self.at.div_ceil(8);
        let raw = self.bytes[start..end]
            .iter()
            .fold(0_u64, |bits, &byte| (bits << 8) | u64::from(byte));
        Ok(((raw >> ((8 - self.at % 8) % 8)) & ((1_u64 << width) - 1)) as u32)
    }

    fn flag(&mut self) -> Result<bool> {
        Ok(self.read(1)? != 0)
    }
}

/// Quantized peak-light samples in row-major order. Each value represents
/// value/15 of the display peak.
#[derive(Clone, Copy, Debug)]
pub struct Hdr10PlusPeakGrid<'a> {
    bytes: &'a [u8],
    offset: usize,
    rows: u8,
    columns: u8,
}

impl Hdr10PlusPeakGrid<'_> {
    pub fn rows(&self) -> u8 {
        self.rows
    }

    pub fn columns(&self) -> u8 {
        self.columns
    }

    pub fn values(&self) -> impl ExactSizeIterator<Item = u8> + '_ {
        let count = usize::from(self.rows) * usize::from(self.columns);
        (0..count).map(|index| {
            let bit = self.offset + index * 4;
            let byte = bit / 8;
            let high = self.bytes[byte];
            let low = self.bytes.get(byte + 1).copied().unwrap_or(0);
            ((u16::from_be_bytes([high, low]) >> (12 - bit % 8)) & 15) as u8
        })
    }
}

fn peak_grid<'a>(bits: &mut Bits<'a>) -> Result<Option<Hdr10PlusPeakGrid<'a>>> {
    if !bits.flag()? {
        return Ok(None);
    }
    let rows = bits.read(5)? as u8;
    let columns = bits.read(5)? as u8;
    if !(2..=25).contains(&rows) || !(2..=25).contains(&columns) {
        return Err(Error::Invalid("HDR10+ peak grid dimensions"));
    }
    let offset = bits.advance(usize::from(rows) * usize::from(columns) * 4)?;
    Ok(Some(Hdr10PlusPeakGrid {
        bytes: bits.bytes,
        offset,
        rows,
        columns,
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hdr10PlusGeometry {
    /// Left, top, right, bottom coordinates in pixels.
    pub rectangle: [u16; 4],
    pub ellipse_center: [u16; 2],
    pub rotation: u8,
    pub internal_semimajor_axis: u16,
    pub external_semimajor_axis: u16,
    pub external_semiminor_axis: u16,
    pub overlap_process_option: bool,
}

impl Hdr10PlusGeometry {
    fn read(bits: &mut Bits<'_>) -> Result<Self> {
        let mut rectangle = [0; 4];
        for value in &mut rectangle {
            *value = bits.read(16)? as u16;
        }
        Ok(Self {
            rectangle,
            ellipse_center: [bits.read(16)? as u16, bits.read(16)? as u16],
            rotation: bits.read(8)? as u8,
            internal_semimajor_axis: bits.read(16)? as u16,
            external_semimajor_axis: bits.read(16)? as u16,
            external_semiminor_axis: bits.read(16)? as u16,
            overlap_process_option: bits.flag()?,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hdr10PlusPercentile {
    pub percentage: u8,
    /// Linearized RGB in units of 1/100000.
    pub max_rgb: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hdr10PlusToneCurve {
    /// Knee coordinates in units of 1/4095.
    pub knee: [u16; 2],
    anchors: [u16; 15],
    count: u8,
}

impl Hdr10PlusToneCurve {
    /// Anchor coordinates in units of 1/1023.
    pub fn anchors(&self) -> &[u16] {
        &self.anchors[..usize::from(self.count)]
    }

    fn read(bits: &mut Bits<'_>) -> Result<Option<Self>> {
        if !bits.flag()? {
            return Ok(None);
        }
        let knee = [bits.read(12)? as u16, bits.read(12)? as u16];
        let count = bits.read(4)? as u8;
        let mut anchors = [0; 15];
        for anchor in &mut anchors[..usize::from(count)] {
            *anchor = bits.read(10)? as u16;
        }
        Ok(Some(Self {
            knee,
            anchors,
            count,
        }))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hdr10PlusWindow {
    /// The first window covers the frame and has no transmitted geometry.
    pub geometry: Option<Hdr10PlusGeometry>,
    /// Linearized RGB in units of 1/100000.
    pub max_scl: [u32; 3],
    pub average_max_rgb: u32,
    /// In units of 1/1000.
    pub fraction_bright_pixels: u16,
    pub tone_curve: Option<Hdr10PlusToneCurve>,
    /// In units of 1/8.
    pub saturation_weight: Option<u8>,
    percentiles: [Hdr10PlusPercentile; 15],
    percentile_count: u8,
}

impl Hdr10PlusWindow {
    pub fn percentiles(&self) -> &[Hdr10PlusPercentile] {
        &self.percentiles[..usize::from(self.percentile_count)]
    }

    fn read_distribution(&mut self, bits: &mut Bits<'_>) -> Result<()> {
        for channel in &mut self.max_scl {
            *channel = bits.read(17)?;
        }
        self.average_max_rgb = bits.read(17)?;
        self.percentile_count = bits.read(4)? as u8;
        for entry in &mut self.percentiles[..usize::from(self.percentile_count)] {
            *entry = Hdr10PlusPercentile {
                percentage: bits.read(7)? as u8,
                max_rgb: bits.read(17)?,
            };
        }
        self.fraction_bright_pixels = bits.read(10)? as u16;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Hdr10Plus<'a> {
    raw: &'a [u8],
    body_bits: usize,
    pub application_version: u8,
    /// Targeted display maximum luminance in cd/m².
    pub target_luminance: u32,
    pub target_grid: Option<Hdr10PlusPeakGrid<'a>>,
    pub mastering_grid: Option<Hdr10PlusPeakGrid<'a>>,
    windows: [Hdr10PlusWindow; 3],
    window_count: u8,
}

impl<'a> Hdr10Plus<'a> {
    /// Complete T.35 record, including the registered terminal code and any
    /// unconsumed tail bytes.
    pub fn raw(&self) -> &'a [u8] {
        self.raw
    }

    pub fn body_bits(&self) -> usize {
        self.body_bits
    }

    pub fn windows(&self) -> &[Hdr10PlusWindow] {
        &self.windows[..usize::from(self.window_count)]
    }

    /// Parse the complete registered ITU-T T.35 record. No heap allocation is
    /// needed. Both grids borrow validated bit ranges from the original bytes.
    pub fn parse_t35(
        raw: &'a [u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Self>> {
        super::syntax::check(cancel)?;
        let cost = Self::cost(raw)?;
        if !work.try_charge(cost) {
            return Ok(ParseStep::Yield);
        }
        Ok(ParseStep::Event(Self::decode(raw, cancel)?))
    }

    pub(crate) fn cost(raw: &[u8]) -> Result<u64> {
        let body = raw
            .strip_prefix(&T35_HEADER)
            .ok_or(Error::Invalid("HDR10+ terminal code"))?;
        if body.len() > MAX_BODY {
            return Err(Error::Policy("HDR10+ payload"));
        }
        Ok((raw.len() as u64) * 8)
    }

    pub(crate) fn decode(raw: &'a [u8], cancel: &AtomicBool) -> Result<Self> {
        let mut bits = Bits {
            bytes: raw
                .strip_prefix(&T35_HEADER)
                .ok_or(Error::Invalid("HDR10+ terminal code"))?,
            at: 0,
        };
        let application_version = bits.read(8)? as u8;
        let window_count = bits.read(2)? as u8;
        if window_count == 0 {
            return Err(Error::Invalid("HDR10+ window count"));
        }
        let mut windows = [Hdr10PlusWindow::default(); 3];
        let active = &mut windows[..usize::from(window_count)];
        for window in &mut active[1..] {
            window.geometry = Some(Hdr10PlusGeometry::read(&mut bits)?);
        }
        let target_luminance = bits.read(27)?;
        let target_grid = peak_grid(&mut bits)?;
        for window in active.iter_mut() {
            super::syntax::check(cancel)?;
            window.read_distribution(&mut bits)?;
        }
        let mastering_grid = peak_grid(&mut bits)?;
        for window in active {
            window.tone_curve = Hdr10PlusToneCurve::read(&mut bits)?;
            window.saturation_weight = if bits.flag()? {
                Some(bits.read(6)? as u8)
            } else {
                None
            };
        }
        Ok(Self {
            raw,
            body_bits: bits.at,
            application_version,
            target_luminance,
            target_grid,
            mastering_grid,
            windows,
            window_count,
        })
    }
}
