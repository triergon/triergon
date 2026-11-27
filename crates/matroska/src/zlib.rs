//! Independent bounded zlib/DEFLATE reader, specified by RFC 1950 and RFC 1951.
//! No preset dictionary is available in Matroska ContentCompression.
// The public items are reachable through `reference_trace::zlib`.
#![cfg_attr(not(feature = "reference-trace"), allow(unreachable_pub))]
use crate::{Error, Result};
use std::sync::atomic::{AtomicBool, Ordering};

mod inflater;
pub(crate) use inflater::Inflater;

/// The bounds of one inflated stream.
#[derive(Clone, Copy, Debug)]
pub struct InflateLimits {
    /// Most decoded bytes.
    pub output_bytes: usize,
    /// Most decoded bytes per byte of the whole stream; the output bound is the
    /// smaller of this times the stream length and `output_bytes`.
    pub expansion_ratio: usize,
    /// Most work units: one per block and per decoded symbol, and one per byte a
    /// stored block or a match copies.
    pub work: u64,
}

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
    reservoir: u64,
    available: usize,
}
impl Bits<'_> {
    #[inline]
    #[expect(
        clippy::expect_used,
        reason = "eight bytes always convert; `first_chunk` grows the inlined decode loop"
    )]
    fn fill(&mut self, width: usize) {
        debug_assert!(width <= 16);
        if self.available >= width {
            return;
        }
        let remaining = &self.data[self.at..];
        if remaining.len() >= 8 {
            let word = u64::from_le_bytes(remaining[..8].try_into().expect("eight-byte slice"));
            // Six complete bytes fit even with fifteen buffered bits. The other
            // two loaded bytes are masked out, preserving a zero high-bit invariant.
            self.reservoir |= (word & 0x0000_ffff_ffff_ffff) << self.available;
            self.at += 6;
            self.available += 48;
        } else {
            while self.available < width && self.at < self.data.len() {
                self.reservoir |= u64::from(self.data[self.at]) << self.available;
                self.at += 1;
                self.available += 8;
            }
        }
    }
    #[inline]
    fn peek(&mut self, width: usize) -> u32 {
        self.fill(width);
        (self.reservoir & ((1_u64 << width) - 1)) as u32
    }
    #[inline]
    fn take(&mut self, width: usize) -> Result<u32> {
        self.fill(width);
        if self.available < width {
            return Err(Error::Truncated);
        }
        let value = (self.reservoir & ((1_u64 << width) - 1)) as u32;
        self.reservoir >>= width;
        self.available -= width;
        Ok(value)
    }
    fn align(&mut self) -> Result<()> {
        self.take(self.available % 8)?;
        Ok(())
    }
    fn position(&self) -> usize {
        self.at - self.available / 8
    }
    fn skip_bytes(&mut self, length: usize) -> Result<()> {
        let end = self.position().checked_add(length).ok_or(Error::Overflow)?;
        if end > self.data.len() {
            return Err(Error::Truncated);
        }
        self.at = end;
        self.reservoir = 0;
        self.available = 0;
        Ok(())
    }
}

fn adler32(bytes: &[u8], cancel: &AtomicBool) -> Result<u32> {
    let (mut a, mut b) = (1_u32, 0_u32);
    // Combine four groups before updating the sixteen independent lanes. This
    // shortens the sum recurrence while retaining safe, compiler-vectorized math.
    // A 4096-byte chunk bounds first <= 65280; wide combination avoids overflow.
    for chunk in bytes.chunks(4096) {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let mut first = [0_u16; 16];
        let mut second = [0_u32; 16];
        let mut groups = chunk.chunks_exact(64);
        let count = groups.len();
        for group in &mut groups {
            for i in 0..16 {
                let x = u16::from(group[i]);
                let y = u16::from(group[i + 16]);
                let z = u16::from(group[i + 32]);
                let w = u16::from(group[i + 48]);
                second[i] += 4 * u32::from(first[i]) + u32::from(4 * x + 3 * y + 2 * z + w);
                first[i] += x + y + z + w;
            }
        }
        let mut sum = 0_u32;
        let mut weighted = 0_u64;
        let mut repeated = 0_u64;
        for i in 0..16 {
            sum += u32::from(first[i]);
            weighted += i as u64 * u64::from(first[i]);
            repeated += u64::from(second[i]);
        }
        b = ((u64::from(b) + count as u64 * 64 * u64::from(a) + 16 * repeated - weighted) % 65521)
            as u32;
        a = (a + sum) % 65521;
        for &byte in groups.remainder() {
            a += u32::from(byte);
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    Ok((b << 16) | a)
}

fn header(data: &[u8]) -> Result<usize> {
    if data.len() < 6 {
        return Err(Error::Truncated);
    }
    if data[0] & 15 != 8
        || data[0] >> 4 > 7
        || !u16::from_be_bytes([data[0], data[1]]).is_multiple_of(31)
    {
        return Err(Error::Invalid("zlib header"));
    }
    if data[1] & 32 != 0 {
        return Err(Error::Unsupported("preset zlib dictionary"));
    }
    Ok(1_usize << ((data[0] >> 4) + 8))
}

/// Decodes one complete RFC 1950 stream within `limits`, as the packet path does:
/// a single final stored block is checked whole before it is copied, any other
/// stream goes through the resumable inflater. No bytes are returned until the Adler-32
/// checksum matches. Fails with [`Error::Truncated`] or [`Error::Invalid`] for a
/// malformed stream, [`Error::Unsupported`] for a preset dictionary,
/// [`Error::Policy`] past `limits` and [`Error::Cancelled`].
#[cfg(any(test, feature = "reference-trace"))]
pub fn decompress(data: &[u8], limits: InflateLimits, cancel: &AtomicBool) -> Result<Vec<u8>> {
    use crate::{step::ParseStep, storage::SharedBytes};
    use triergon_core::{Backpressure, Budget, WorkBudget};
    let pool = Budget::new(usize::MAX)?;
    let reader_limits = crate::Limits {
        decompression_ratio: limits.expansion_ratio,
        decompression_work: limits.work,
        ..crate::Limits::default()
    };
    let mut job = restore::RestoreJob::new(
        SharedBytes::copy_from_slice(&pool, data)?,
        restore::RestorePlan::zlib(),
        reader_limits,
        limits.output_bytes,
        &pool,
    )?;
    loop {
        match job.step(&mut WorkBudget::new(u64::MAX), cancel)? {
            ParseStep::Event(bytes) => return Ok(bytes.to_vec()),
            ParseStep::Yield => {}
            ParseStep::Backpressure {
                requested,
                available,
            } => {
                return Err(Error::Backpressure(Backpressure {
                    requested,
                    available,
                }));
            }
            ParseStep::NeedInput(_) | ParseStep::Finished => return Err(Error::Truncated),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checksum_lanes_match_the_scalar_definition_at_every_boundary() {
        let cancel = AtomicBool::new(false);
        let mut data = vec![255; 16_385];
        for pattern in 0..3 {
            if pattern > 0 {
                for (i, byte) in data.iter_mut().enumerate() {
                    *byte = if pattern == 1 {
                        i as u8
                    } else {
                        ((i * 193 + 71) ^ (i >> 3)) as u8
                    };
                }
            }
            for length in (0..=130).chain([4095, 4096, 4097, 8191, 8192, 8193, 16_384, 16_385]) {
                let (mut a, mut b) = (1_u32, 0_u32);
                for byte in &data[..length] {
                    a = (a + u32::from(*byte)) % 65521;
                    b = (b + a) % 65521;
                }
                assert_eq!(adler32(&data[..length], &cancel).unwrap(), (b << 16) | a);
            }
        }
    }

    #[test]
    fn decompress_checks_cancellation_and_returns_an_empty_stored_stream() {
        let input = [120, 1, 1, 0, 0, 255, 255, 0, 0, 0, 1];
        let limits = InflateLimits {
            output_bytes: 32,
            expansion_ratio: 1024,
            work: 100,
        };
        assert!(matches!(
            decompress(&input, limits, &AtomicBool::new(true)),
            Err(Error::Cancelled)
        ));
        assert!(
            decompress(&input, limits, &AtomicBool::new(false))
                .unwrap()
                .is_empty()
        );
    }
}

pub(crate) mod restore;
