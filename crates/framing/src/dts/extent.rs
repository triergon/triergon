//! Declared DTS substream extents at an already established frame boundary.
//!
//! Extension sizes follow ETSI TS 102 114 V1.6.1 §7.5.2, Table 7-2.
//! The probes read the size fields from established boundaries. A caller
//! validates extension header CRC, assets and alignment before assembling an
//! access unit and assigning container clocks.
use triergon_bitstream::{BitCursor, BitSource};
use triergon_core::{Error, Result};

use crate::dts::{DtsCoreHeader, probe_core_header};

/// Policies checked before the declared extents can be used for buffering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsFrameExtentLimits {
    pub maximum_header_bytes: usize,
    pub maximum_frame_bytes: usize,
}

impl DtsFrameExtentLimits {
    fn check(self, header: usize, frame: usize) -> Result<()> {
        if self.maximum_header_bytes == 0 || self.maximum_frame_bytes == 0 {
            return Err(Error::Invalid("zero DTS extent limit"));
        }
        for (requested, maximum) in [
            (header, self.maximum_header_bytes),
            (frame, self.maximum_frame_bytes),
        ] {
            if requested > maximum {
                return Err(Error::Limit { requested, maximum });
            }
        }
        Ok(())
    }
}

/// Source fields and declared byte lengths for one extension substream frame.
/// Both lengths include the sync word; `header_bytes` includes the header CRC.
/// A successful probe does not mean these bytes have arrived or are valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsExtensionSubstreamExtent {
    pub user_defined: u8,
    pub substream_index: u8,
    pub long_sizes: bool,
    pub header_bytes: usize,
    pub frame_bytes: usize,
}

/// Declared extent of a selected normal core or an extension substream frame.
/// The core variant uses the existing core header validation unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtsFrameExtent {
    Core(DtsCoreHeader),
    Extension(DtsExtensionSubstreamExtent),
}

impl DtsFrameExtent {
    pub const fn frame_bytes(self) -> usize {
        match self {
            Self::Core(header) => header.frame_bytes,
            Self::Extension(extent) => extent.frame_bytes,
        }
    }
}

/// Probe an already aligned canonical big-endian DTS frame prefix without
/// allocation, scanning, copying payload or altering source bytes. Inspects at
/// most fifteen bytes. `Truncated` means an incomplete prefix, not an incomplete
/// frame body; the caller must separately acquire the returned declared extent.
/// Word-swapped and 14-bit carriers are outside this API.
pub fn probe_frame_extent(input: &[u8], limits: DtsFrameExtentLimits) -> Result<DtsFrameExtent> {
    match input.get(..4).ok_or(Error::Truncated)? {
        [0x7f, 0xfe, 0x80, 0x01] => {
            let header = probe_core_header(input)?;
            limits.check(
                usize::from(header.header_bits).div_ceil(8),
                header.frame_bytes,
            )?;
            Ok(DtsFrameExtent::Core(header))
        }
        [0x64, 0x58, 0x20, 0x25] => {
            probe_extension_substream_extent(input, limits).map(DtsFrameExtent::Extension)
        }
        _ => Err(Error::Invalid("canonical DTS substream sync")),
    }
}

/// Probe only the extension size prefix (nine bytes for short sizes, ten for
/// long sizes). The sizes are encoded minus one; the index is retained even
/// when nonzero. No ordering or dependency between substreams is inferred.
///
/// Rejects a header that overlaps its size prefix/CRC or leaves no asset data
/// in the frame. This is an extent sanity check, not full header validation.
pub fn probe_extension_substream_extent(
    input: &[u8],
    limits: DtsFrameExtentLimits,
) -> Result<DtsExtensionSubstreamExtent> {
    if input.get(..4).ok_or(Error::Truncated)? != [0x64, 0x58, 0x20, 0x25] {
        return Err(Error::Invalid("DTS extension-substream sync"));
    }
    let long_sizes = input.get(5).ok_or(Error::Truncated)? & 0x20 != 0;
    let prefix_bytes = if long_sizes { 10 } else { 9 };
    let prefix = input.get(..prefix_bytes).ok_or(Error::Truncated)?;
    let mut bits = BitCursor::at(prefix, 32);
    let mut read = |width| bits.read(width).map_err(|_| Error::Truncated);
    let user_defined = read(8)? as u8;
    let substream_index = read(2)? as u8;
    let _size_type = read(1)?;
    let header_bytes = usize::try_from(read(if long_sizes { 12 } else { 8 })?)
        .map_err(|_| Error::Overflow)?
        .checked_add(1)
        .ok_or(Error::Overflow)?;
    let frame_bytes = usize::try_from(read(if long_sizes { 20 } else { 16 })?)
        .map_err(|_| Error::Overflow)?
        .checked_add(1)
        .ok_or(Error::Overflow)?;
    // At least the fixed size fields and the trailing 16-bit CRC must fit.
    // Variable metadata can require a larger header; it remains unparsed.
    if header_bytes < prefix_bytes + 2 || frame_bytes <= header_bytes {
        return Err(Error::Invalid("DTS extension-substream extent geometry"));
    }
    limits.check(header_bytes, frame_bytes)?;
    Ok(DtsExtensionSubstreamExtent {
        user_defined,
        substream_index,
        long_sizes,
        header_bytes,
        frame_bytes,
    })
}
