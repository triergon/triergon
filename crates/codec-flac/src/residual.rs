//! Partitioned Rice residuals (RFC 9639 section 9.2.7).
use crate::DecodeError;
use crate::sample::Sample;
use triergon_bitstream::CachedBitReader;

/// The two Rice parameter widths of the residual coding method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Coding {
    /// 4-bit parameters, escape code 15.
    Rice,
    /// 5-bit parameters, escape code 31.
    Rice2,
}

impl Coding {
    fn parameter_bits(self) -> u32 {
        match self {
            Coding::Rice => 4,
            Coding::Rice2 => 5,
        }
    }
}

/// Reads the residual of a subframe with `order` warm-up samples into `samples[order..]`.
pub(crate) fn read<S: Sample>(
    reader: &mut CachedBitReader<'_>,
    order: usize,
    samples: &mut [S],
) -> Result<(), DecodeError> {
    let coding = match reader.read(2) {
        0 => Coding::Rice,
        1 => Coding::Rice2,
        _ => return Err(DecodeError::Invalid("reserved residual coding method")),
    };
    let partition_order = reader.read(4);
    let partition_len = samples.len() >> partition_order;
    if partition_len << partition_order != samples.len() {
        return Err(DecodeError::Invalid(
            "block size not divisible by partitions",
        ));
    }
    let first = partition_len
        .checked_sub(order)
        .ok_or(DecodeError::Invalid(
            "predictor order above the first partition",
        ))?;
    let escape = (1 << coding.parameter_bits()) - 1;
    let mut rest = samples.get_mut(order..).unwrap_or_default();
    let mut len = first;
    while !rest.is_empty() {
        let (partition, tail) = rest.split_at_mut(len.min(rest.len()));
        let parameter = reader.read(coding.parameter_bits());
        if parameter == escape {
            read_escaped(reader, partition);
        } else {
            read_rice(reader, parameter, partition)?;
        }
        if reader.overrun() {
            return Err(DecodeError::Invalid("residual past the end of the frame"));
        }
        rest = tail;
        len = partition_len;
    }
    Ok(())
}

/// An escaped partition: one 5-bit width, then that many bits per sample.
fn read_escaped<S: Sample>(reader: &mut CachedBitReader<'_>, partition: &mut [S]) {
    match reader.read(5) {
        0 => partition.fill(S::default()),
        bits => {
            for x in partition {
                *x = S::read(reader, bits);
            }
        }
    }
}

/// Rice codes with parameter `k`: a unary quotient, `k` low bits, zigzag folded.
#[inline]
fn read_rice<S: Sample>(
    reader: &mut CachedBitReader<'_>,
    k: u32,
    partition: &mut [S],
) -> Result<(), DecodeError> {
    for x in partition {
        let window = reader.peek32();
        let zeros = window.leading_zeros();
        let folded = if zeros + 1 + k <= 32 {
            // The quotient, its stop bit and the low bits are all in the window.
            let low = (u64::from(window << zeros << 1) << k) >> 32;
            reader.skip((zeros + 1 + k) as usize);
            (u64::from(zeros) << k) | low
        } else {
            read_long_rice(reader, k)?
        };
        *x = S::from(unfold(folded));
    }
    Ok(())
}

/// A Rice code longer than the 32-bit window.
#[cold]
fn read_long_rice(reader: &mut CachedBitReader<'_>, k: u32) -> Result<u64, DecodeError> {
    let mut quotient = 0u64;
    loop {
        let window = reader.peek32();
        if window != 0 {
            let zeros = window.leading_zeros();
            quotient += u64::from(zeros);
            reader.skip(zeros as usize + 1);
            break;
        }
        quotient += 32;
        reader.skip(32);
        if reader.overrun() {
            return Err(DecodeError::Invalid("residual past the end of the frame"));
        }
    }
    let folded = (quotient << k) | u64::from(reader.read(k));
    if folded > u64::from(u32::MAX) {
        // RFC 9639 section 9.2.7.3: a residual fits 32-bit two's complement.
        return Err(DecodeError::Invalid("residual above 32 bits"));
    }
    Ok(folded)
}

/// Zigzag decoding: 0, 1, 2, 3, ... to 0, -1, 1, -2, ...
#[inline(always)]
fn unfold(folded: u64) -> i32 {
    let folded = folded as u32;
    ((folded >> 1) as i32) ^ -((folded & 1) as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfold_covers_the_32_bit_range() {
        assert_eq!([0, 1, 2, 3, 4].map(unfold), [0, -1, 1, -2, 2]);
        assert_eq!(unfold(u64::from(u32::MAX)), i32::MIN);
        assert_eq!(unfold(u64::from(u32::MAX - 1)), i32::MAX);
    }
}
