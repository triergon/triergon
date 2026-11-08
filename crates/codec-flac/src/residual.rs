//! Partitioned Rice residuals (RFC 9639 section 9.2.7).
use crate::DecodeError;
use crate::rice::RiceReader;
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
    let end = reader.end();
    let mut bits = RiceReader::at(reader.data(), reader.position());
    let coding = match bits.take(2) {
        0 => Coding::Rice,
        1 => Coding::Rice2,
        _ => return Err(DecodeError::Invalid("reserved residual coding method")),
    };
    let partition_order = bits.take(4);
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
        bits.refill();
        match bits.take(coding.parameter_bits()) {
            parameter if parameter == escape => {
                let width = bits.take(5);
                read_escaped(&mut bits, width, partition);
            }
            parameter => read_rice(&mut bits, parameter, end, partition)?,
        }
        if bits.position() > end {
            return Err(PAST_THE_END);
        }
        rest = tail;
        len = partition_len;
    }
    reader.seek(bits.position());
    Ok(())
}

const PAST_THE_END: DecodeError = DecodeError::Invalid("residual past the end of the frame");

/// An escaped partition: samples of `width` bits (0 to 31) each.
fn read_escaped<S: Sample>(bits: &mut RiceReader<'_>, width: u32, partition: &mut [S]) {
    if width == 0 {
        partition.fill(S::default());
        return;
    }
    let unused = 32 - width;
    for x in partition {
        bits.refill();
        *x = S::from(((bits.take(width) << unused) as i32) >> unused);
    }
}

/// Rice codes with parameter `k`: a unary quotient, `k` low bits, zigzag folded.
///
/// A refill leaves at least 56 bits, enough for several codes of a typical length
/// (`k` plus a short quotient and the stop bit), so codes are read in groups of one
/// refill each; a code that does not fit takes the slow path, which refills.
fn read_rice<S: Sample>(
    bits: &mut RiceReader<'_>,
    k: u32,
    end: usize,
    partition: &mut [S],
) -> Result<(), DecodeError> {
    match k {
        0..=8 => read_rice_groups::<S, 4>(bits, k, end, partition),
        9..=13 => read_rice_groups::<S, 3>(bits, k, end, partition),
        _ => read_rice_groups::<S, 2>(bits, k, end, partition),
    }
}

#[inline(always)]
fn read_rice_groups<S: Sample, const GROUP: usize>(
    bits: &mut RiceReader<'_>,
    k: u32,
    end: usize,
    partition: &mut [S],
) -> Result<(), DecodeError> {
    let mut groups = partition.chunks_exact_mut(GROUP);
    for group in &mut groups {
        bits.refill();
        for x in group {
            *x = S::from(unfold(bits.rice(k, end).ok_or(PAST_THE_END)?));
        }
    }
    for x in groups.into_remainder() {
        bits.refill();
        *x = S::from(unfold(bits.rice(k, end).ok_or(PAST_THE_END)?));
    }
    Ok(())
}

/// Zigzag decoding: 0, 1, 2, 3, ... to 0, -1, 1, -2, ...
#[inline(always)]
fn unfold(folded: u32) -> i32 {
    ((folded >> 1) as i32) ^ -((folded & 1) as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfold_covers_the_32_bit_range() {
        assert_eq!([0, 1, 2, 3, 4].map(unfold), [0, -1, 1, -2, 2]);
        assert_eq!(unfold(u32::MAX), i32::MIN);
        assert_eq!(unfold(u32::MAX - 1), i32::MAX);
    }
}
