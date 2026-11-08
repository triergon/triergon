//! Floor type 0 (Vorbis I section 6): the frequency response of an LSP filter.

use core::f32::consts::PI;

use triergon_bitstream::LsbReader;
use triergon_codec::{Budget, BudgetVec};

use crate::codebook::Codebook;
use crate::window::Block;
use crate::{DecodeError, ilog, push};

/// The highest LSP order the 8-bit field allows.
pub(crate) const MAX_ORDER: usize = 255;

/// The LSP coefficients of one decoded floor 0.
pub(crate) type Coefficients = [f32; MAX_ORDER];

#[derive(Debug)]
pub(crate) struct Floor0 {
    order: usize,
    amplitude_bits: u32,
    amplitude_offset: f32,
    /// `amplitude_offset / (2^amplitude_bits - 1)`: the curve level of amplitude 1.
    amplitude_step: f64,
    books: BudgetVec<u8>,
    /// The spectral lines of the short and long block in runs that share one Bark map
    /// value (section 6.2.3).
    bands: [BudgetVec<Band>; 2],
}

/// Consecutive spectral lines with one map value and so one curve value.
#[derive(Clone, Copy, Debug)]
struct Band {
    lines: usize,
    /// `cos(ω)` with `ω = π · map / bark_map_size`.
    cos_omega: f32,
}

impl Floor0 {
    /// Reads a floor 0 configuration (section 6.2.1) and builds its Bark maps for both
    /// block sizes.
    pub(crate) fn read(
        r: &mut LsbReader<'_>,
        codebooks: &[Codebook],
        blocksizes: [usize; 2],
        budget: &Budget,
    ) -> Result<Floor0, DecodeError> {
        let order = r.read(8) as usize;
        let rate = r.read(16);
        let bark_map_size = r.read(16);
        let amplitude_bits = r.read(6);
        let amplitude_offset = r.read(8);
        // At most 16 books.
        let mut books = BudgetVec::try_filled(budget, 0, r.read(4) as usize + 1)?;
        for book in books.iter_mut() {
            *book = r.read(8) as u8;
        }
        if r.overrun() {
            return Err(DecodeError::Invalid("truncated floor 0"));
        }
        if books.iter().any(|&book| {
            !codebooks
                .get(usize::from(book))
                .is_some_and(Codebook::has_vectors)
        }) {
            return Err(DecodeError::Invalid("floor 0 codebook"));
        }
        if order == 0 || rate == 0 || bark_map_size == 0 {
            return Err(DecodeError::Invalid("floor 0 parameters"));
        }
        let bark = |x: f64| {
            13.1 * (0.00074 * x).atan() + 2.24 * (0.000_000_018_5 * x * x).atan() + 0.0001 * x
        };
        let rate = f64::from(rate);
        let size = f64::from(bark_map_size);
        let mut bands = [BudgetVec::new(budget), BudgetVec::new(budget)];
        for (bands, blocksize) in bands.iter_mut().zip(blocksizes) {
            let n = blocksize / 2;
            let map = |i: usize| {
                let bin =
                    (bark(rate * i as f64 / (2.0 * n as f64)) * size / bark(0.5 * rate)).floor();
                bin.min(size - 1.0) as u32
            };
            let count = (1..n).filter(|&i| map(i) != map(i - 1)).count() + 1;
            bands.try_reserve_exact(count)?;
            let mut start = 0;
            while start < n {
                let value = map(start);
                let lines = (start..n).take_while(|&i| map(i) == value).count();
                let band = Band {
                    lines,
                    cos_omega: (PI * value as f32 / size as f32).cos(),
                };
                push(bands, band)?;
                start += lines;
            }
        }
        Ok(Floor0 {
            order,
            amplitude_bits,
            amplitude_offset: amplitude_offset as f32,
            amplitude_step: f64::from(amplitude_offset) / (2f64.powi(amplitude_bits as i32) - 1.0),
            books,
            bands,
        })
    }

    /// Reads the amplitude and coefficients of one channel (section 6.2.2). `Ok(None)`
    /// if the channel is unused in this packet.
    pub(crate) fn decode(
        &self,
        r: &mut LsbReader<'_>,
        codebooks: &[Codebook],
        coefficients: &mut Coefficients,
    ) -> Result<Option<u64>, DecodeError> {
        let amplitude = r.read_u64(self.amplitude_bits);
        if amplitude == 0 {
            return Ok(None);
        }
        let number = r.read(ilog(self.books.len() as u32)) as usize;
        let book = self
            .books
            .get(number)
            .map(|&book| &codebooks[usize::from(book)])
            .ok_or(DecodeError::Invalid("floor 0 book number"))?;
        let mut filled = 0;
        let mut last = 0.0;
        while filled < self.order {
            let Some(vector) = book.vector(r) else {
                return Ok(None);
            };
            for (slot, &value) in coefficients[filled..self.order].iter_mut().zip(vector) {
                *slot = value + last;
            }
            filled += vector.len();
            last += vector.last().copied().unwrap_or(0.0);
        }
        Ok(Some(amplitude))
    }

    /// Multiplies `spectrum` by the curve of `amplitude` and `coefficients` (section 6.2.3).
    ///
    /// The curve is evaluated in single precision, as FFmpeg evaluates it: in double
    /// precision the long LSP products drift by up to 3e-5 of full scale from its output.
    pub(crate) fn apply(
        &self,
        block: Block,
        amplitude: u64,
        coefficients: &Coefficients,
        spectrum: &mut [f32],
    ) {
        let mut cos_lsp = [0f32; MAX_ORDER];
        for (c, &w) in cos_lsp.iter_mut().zip(&coefficients[..self.order]) {
            *c = w.cos();
        }
        let cos_lsp = &cos_lsp[..self.order];
        let level = (amplitude as f64 * self.amplitude_step) as f32;
        let mut values = spectrum;
        for band in self.bands[block as usize].iter() {
            if values.is_empty() {
                break;
            }
            let cos_w = band.cos_omega;
            let product = |start: usize| {
                cos_lsp
                    .iter()
                    .skip(start)
                    .step_by(2)
                    .map(|&c| 4.0 * (c - cos_w) * (c - cos_w))
                    .product::<f32>()
            };
            let (p, q) = if self.order % 2 == 1 {
                ((1.0 - cos_w * cos_w) * product(1), 0.25 * product(0))
            } else {
                (
                    (1.0 - cos_w) / 2.0 * product(1),
                    (1.0 + cos_w) / 2.0 * product(0),
                )
            };
            let gain = (0.115_129_25 * (level / (p + q).sqrt() - self.amplitude_offset)).exp();
            let lines = band.lines.min(values.len());
            let (head, rest) = core::mem::take(&mut values).split_at_mut(lines);
            for value in head {
                *value *= gain;
            }
            values = rest;
        }
    }
}
