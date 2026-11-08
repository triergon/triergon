//! Subframes (RFC 9639 section 9.2): header, warm-up, residual and prediction.
use crate::DecodeError;
use crate::predict::{Accumulator, MAX_ORDER, Restore, restore_fixed};
use crate::residual;
use crate::sample::Sample;
use triergon_bitstream::CachedBitReader;

/// How a subframe codes its samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Constant,
    Verbatim,
    /// Fixed polynomial predictor of order 0 to 4.
    Fixed(usize),
    /// Linear predictor of order 1 to 32 with coded coefficients.
    Lpc(usize),
}

impl Kind {
    fn from_code(code: u32) -> Result<Kind, DecodeError> {
        Ok(match code {
            0 => Kind::Constant,
            1 => Kind::Verbatim,
            8..=12 => Kind::Fixed(code as usize - 8),
            32..=63 => Kind::Lpc(code as usize - 31),
            _ => return Err(DecodeError::Invalid("reserved subframe type")),
        })
    }
}

/// Decodes one subframe of a channel with `bits` bits per sample (the frame's sample size,
/// plus one for a side channel) into `samples`, one per sample of the block.
pub(crate) fn decode<S: Restore>(
    reader: &mut CachedBitReader<'_>,
    bits: u32,
    samples: &mut [S],
) -> Result<(), DecodeError> {
    if reader.bit() {
        return Err(DecodeError::Invalid("subframe padding bit"));
    }
    let kind = Kind::from_code(reader.read(6))?;
    let wasted = if reader.bit() {
        read_wasted_bits(reader)?
    } else {
        0
    };
    let sample_bits = bits
        .checked_sub(wasted)
        .filter(|&bits| bits > 0)
        .ok_or(DecodeError::Invalid("wasted bits above the sample size"))?;
    match kind {
        Kind::Constant => samples.fill(S::read(reader, sample_bits)),
        Kind::Verbatim => {
            for x in samples.iter_mut() {
                *x = S::read(reader, sample_bits);
            }
        }
        Kind::Fixed(order) => {
            read_warm_up(reader, sample_bits, order, samples)?;
            residual::read(reader, order, samples)?;
            restore_fixed(samples, order);
        }
        Kind::Lpc(order) => {
            read_warm_up(reader, sample_bits, order, samples)?;
            let (coefficients, shift, accumulator) = read_lpc(reader, sample_bits, order)?;
            residual::read(reader, order, samples)?;
            S::restore(samples, &coefficients[..order], shift, accumulator);
        }
    }
    if reader.overrun() {
        return Err(DecodeError::Invalid("subframe past the end of the frame"));
    }
    if wasted > 0 {
        for x in samples.iter_mut() {
            *x = x.wrapping_shl(wasted);
        }
    }
    Ok(())
}

/// The wasted-bits count after its flag: unary, one more than the zeros before the one.
fn read_wasted_bits(reader: &mut CachedBitReader<'_>) -> Result<u32, DecodeError> {
    let zeros = reader.peek32().leading_zeros();
    if zeros >= 32 {
        return Err(DecodeError::Invalid("wasted bits above the sample size"));
    }
    reader.skip(zeros as usize + 1);
    Ok(zeros + 1)
}

fn read_warm_up<S: Sample>(
    reader: &mut CachedBitReader<'_>,
    sample_bits: u32,
    order: usize,
    samples: &mut [S],
) -> Result<(), DecodeError> {
    let warm_up = samples
        .get_mut(..order)
        .ok_or(DecodeError::Invalid("predictor order above the block size"))?;
    for x in warm_up {
        *x = S::read(reader, sample_bits);
    }
    Ok(())
}

/// Precision, shift and coefficients of an LPC subframe. The coefficients are returned
/// newest sample first, as coded.
fn read_lpc(
    reader: &mut CachedBitReader<'_>,
    sample_bits: u32,
    order: usize,
) -> Result<([i32; MAX_ORDER], u32, Accumulator), DecodeError> {
    let precision = match reader.read(4) {
        15 => return Err(DecodeError::Invalid("reserved LPC precision")),
        code => code + 1,
    };
    // A negative shift is refused, as FFmpeg refuses it.
    let shift = u32::try_from(reader.read_signed(5))
        .map_err(|_| DecodeError::Unsupported("negative LPC shift"))?;
    let mut coefficients = [0; MAX_ORDER];
    for c in &mut coefficients[..order] {
        *c = reader.read_signed(precision);
    }
    let accumulator = Accumulator::for_lpc(sample_bits, precision, order);
    Ok((coefficients, shift, accumulator))
}
