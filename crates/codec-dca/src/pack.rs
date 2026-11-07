//! DTS core stream packings (ETSI TS 102 114 clause 5.3 sync word; 14-bit and
//! little-endian word orders as carried by raw `.dts`/`.cpt`/DTS-WAV payloads).
//!
//! Every packing is normalized to the canonical 16-bit big-endian bitstream before
//! parsing. Normalization reuses a caller-owned scratch buffer.

use triergon_codec::{BudgetVec, StorageError};

/// Word packing of a DTS core frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packing {
    /// 16-bit words, big endian: sync `7F FE 80 01`.
    Be16,
    /// 16-bit words, little endian: sync `FE 7F 01 80`.
    Le16,
    /// 14 payload bits in 16-bit big-endian words: sync `1F FF E8 00 07 Fx`.
    Be14,
    /// 14 payload bits in 16-bit little-endian words: sync `FF 1F 00 E8 Fx 07`.
    Le14,
}

impl Packing {
    /// Detect the packing from the first bytes of a frame.
    pub fn detect(au: &[u8]) -> Option<Self> {
        if au.len() < 6 {
            return None;
        }
        match (au[0], au[1], au[2], au[3]) {
            (0x7F, 0xFE, 0x80, 0x01) => Some(Packing::Be16),
            (0xFE, 0x7F, 0x01, 0x80) => Some(Packing::Le16),
            (0x1F, 0xFF, 0xE8, 0x00) if au[4] == 0x07 && (au[5] & 0xF0) == 0xF0 => {
                Some(Packing::Be14)
            }
            (0xFF, 0x1F, 0x00, 0xE8) if (au[4] & 0xF0) == 0xF0 && au[5] == 0x07 => {
                Some(Packing::Le14)
            }
            _ => None,
        }
    }

    /// True for the 14-bit packings.
    pub fn is_14bit(self) -> bool {
        matches!(self, Packing::Be14 | Packing::Le14)
    }
}

/// Normalize `au` into `scratch` (canonical 16-bit big-endian bitstream) and return
/// the normalized frame. `scratch` only grows, charged to its budget first.
pub(crate) fn normalize<'a>(
    au: &[u8],
    packing: Packing,
    scratch: &'a mut BudgetVec<u8>,
) -> Result<&'a [u8], StorageError> {
    let len = match packing {
        Packing::Be16 => au.len(),
        Packing::Le16 => au.len() / 2 * 2,
        Packing::Be14 | Packing::Le14 => au.len() / 2 * 14 / 8,
    };
    if scratch.len() < len {
        scratch.try_resize(len, 0)?;
    }
    let out = &mut scratch[..len];
    match packing {
        Packing::Be16 => out.copy_from_slice(au),
        Packing::Le16 => {
            for (o, w) in out.chunks_exact_mut(2).zip(au.chunks_exact(2)) {
                o[0] = w[1];
                o[1] = w[0];
            }
        }
        Packing::Be14 | Packing::Le14 => {
            let le = packing == Packing::Le14;
            let mut acc: u32 = 0;
            let mut nbits = 0u32;
            let mut n = 0;
            for w in au.chunks_exact(2) {
                let word = if le {
                    u16::from_le_bytes([w[0], w[1]])
                } else {
                    u16::from_be_bytes([w[0], w[1]])
                };
                acc = (acc << 14) | u32::from(word & 0x3FFF);
                nbits += 14;
                while nbits >= 8 {
                    nbits -= 8;
                    out[n] = (acc >> nbits) as u8;
                    n += 1;
                }
                acc &= (1 << nbits) - 1;
            }
        }
    }
    Ok(&scratch[..len])
}
