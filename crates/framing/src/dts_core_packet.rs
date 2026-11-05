//! Core delivery from a bounded DTS container packet (ETSI TS 102 114).

use crate::DtsPacking;
use triergon_core::{Error, Result};

const CORE_SYNC: &[u8; 4] = b"\x7f\xfe\x80\x01";
const EXTENSION_SYNC: &[u8; 4] = b"\x64\x58\x20\x25";

/// A complete core prefix and its original container packet. The 14-bit
/// FSIZE field bounds canonical output to 16 KiB. Bytes after the core stay
/// in the source view and are outside the core decoder's submission.
#[derive(Clone, Copy, Debug)]
pub struct DtsCorePacket<'a> {
    source: &'a [u8],
    packed_bytes: usize,
    canonical_bytes: usize,
    packing: DtsPacking,
}

impl<'a> DtsCorePacket<'a> {
    /// Check sync and the core extent without validating or decoding its body.
    /// The packing calculation applies to the core only, even when a packet
    /// also carries an extension in a different storage representation.
    pub fn inspect(source: &'a [u8]) -> Result<Self> {
        let packing = if source.starts_with(CORE_SYNC) {
            DtsPacking::CanonicalBe
        } else if source.starts_with(EXTENSION_SYNC) {
            return Err(Error::Unsupported("DTS packet has no core"));
        } else {
            DtsPacking::detect(source)?
        };
        let fsize = if packing == DtsPacking::CanonicalBe {
            let [high, middle, low] = source.get(5..8).ok_or(Error::Truncated)? else {
                return Err(Error::Truncated);
            };
            (u16::from(*high & 3) << 12) | (u16::from(*middle) << 4) | u16::from(*low >> 4)
        } else {
            (46..60).try_fold(0_u16, |value, bit| {
                Ok((value << 1) | u16::from(packing.bit(source, bit)?))
            })?
        };
        let canonical_bytes = usize::from(fsize) + 1;
        if canonical_bytes < 96 {
            return Err(Error::Invalid("DTS core frame size"));
        }
        let packed_bytes = (canonical_bytes * 8).div_ceil(packing.word_payload_bits())
            * packing.source_unit_bytes();
        if packed_bytes > source.len() {
            return Err(Error::Truncated);
        }
        Ok(Self {
            source,
            packed_bytes,
            canonical_bytes,
            packing,
        })
    }

    pub fn source(self) -> &'a [u8] {
        self.source
    }

    pub fn packed_core(self) -> &'a [u8] {
        &self.source[..self.packed_bytes]
    }

    pub const fn canonical_len(self) -> usize {
        self.canonical_bytes
    }

    /// Borrow the common 16-bit big-endian core directly from its packet.
    pub fn canonical(self) -> Option<&'a [u8]> {
        (self.packing == DtsPacking::CanonicalBe).then(|| self.packed_core())
    }

    /// Write the canonical core into caller-owned storage. For the common
    /// canonical packing, use `canonical()` to deliver without a copy.
    pub fn write_canonical(self, buffer: &mut [u8]) -> Result<&[u8]> {
        let maximum = buffer.len();
        let output = buffer.get_mut(..self.canonical_bytes).ok_or(Error::Limit {
            requested: self.canonical_bytes,
            maximum,
        })?;
        let input = self.packed_core();
        match self.packing {
            DtsPacking::CanonicalBe => output.copy_from_slice(input),
            DtsPacking::Words16Le => {
                let mut pairs = output.chunks_exact_mut(2);
                for (dst, src) in pairs.by_ref().zip(input.chunks_exact(2)) {
                    dst[0] = src[1];
                    dst[1] = src[0];
                }
                if let [last] = pairs.into_remainder() {
                    *last = input[self.canonical_bytes];
                }
            }
            DtsPacking::Words14Be | DtsPacking::Words14Le => {
                let little = self.packing == DtsPacking::Words14Le;
                // Four 14-bit payload words make seven complete bytes.
                for (dst, src) in output.chunks_mut(7).zip(input.chunks(8)) {
                    let mut group = 0_u64;
                    for word in src.chunks_exact(2) {
                        let pair = [word[0], word[1]];
                        let value = if little {
                            u16::from_le_bytes(pair)
                        } else {
                            u16::from_be_bytes(pair)
                        };
                        group = (group << 14) | u64::from(value & 0x3fff);
                    }
                    let bytes = (group << ((8 - src.len()) / 2 * 14)).to_be_bytes();
                    dst.copy_from_slice(&bytes[1..1 + dst.len()]);
                }
            }
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core(size: usize) -> Vec<u8> {
        let mut data: Vec<_> = (0..size).map(|n| n.wrapping_mul(17) as u8).collect();
        data[..4].copy_from_slice(CORE_SYNC);
        let fsize = size - 1;
        data[5] = (fsize >> 12) as u8;
        data[6] = (fsize >> 4) as u8;
        data[7] = (fsize << 4) as u8;
        data
    }

    fn pack(core: &[u8], packing: DtsPacking) -> Vec<u8> {
        let width = packing.word_payload_bits();
        if packing == DtsPacking::CanonicalBe {
            return core.to_vec();
        }
        let length = core.len() * 8;
        (0..length.div_ceil(width))
            .flat_map(|word| {
                let start = word * width;
                let payload = (start..start + width).fold(0_u16, |value, bit| {
                    let next = if bit < length {
                        (core[bit / 8] >> (7 - bit % 8)) & 1
                    } else {
                        0
                    };
                    (value << 1) | u16::from(next)
                });
                if packing == DtsPacking::Words14Be {
                    (payload | 0x8000).to_be_bytes()
                } else if packing == DtsPacking::Words14Le {
                    (payload | 0x8000).to_le_bytes()
                } else {
                    payload.to_le_bytes()
                }
            })
            .collect()
    }

    #[test]
    fn dts_packet_core_normalizes_four_packings_without_reading_the_extension() {
        let mut buffer = [0; 16 * 1024];
        for size in [96, 97, 98, 1024, 16 * 1024] {
            let canonical = core(size);
            for packing in [
                DtsPacking::CanonicalBe,
                DtsPacking::Words16Le,
                DtsPacking::Words14Be,
                DtsPacking::Words14Le,
            ] {
                let mut source = pack(&canonical, packing);
                let extent = source.len();
                assert!(matches!(
                    DtsCorePacket::inspect(&source[..extent - 1]),
                    Err(Error::Truncated)
                ));
                source.extend_from_slice(EXTENSION_SYNC);
                source.push(1);
                let packet = DtsCorePacket::inspect(&source).unwrap();
                assert_eq!(packet.source(), source);
                assert_eq!(packet.packed_core().len(), extent);
                assert_eq!(packet.canonical_len(), size);
                if let Some(core) = packet.canonical() {
                    assert_eq!(packing, DtsPacking::CanonicalBe);
                    assert_eq!(core.as_ptr(), source.as_ptr());
                }
                assert_eq!(packet.write_canonical(&mut buffer).unwrap(), canonical);
                assert!(matches!(
                    packet.write_canonical(&mut buffer[..size - 1]),
                    Err(Error::Limit { .. })
                ));
            }
        }
    }

    #[test]
    fn dts_packet_core_refuses_extension_only_and_an_undersized_core() {
        assert!(matches!(
            DtsCorePacket::inspect(EXTENSION_SYNC),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            DtsCorePacket::inspect(&core(95)),
            Err(Error::Invalid(_))
        ));
    }
}
