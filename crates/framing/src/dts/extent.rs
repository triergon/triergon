//! Declared DTS frame extents at an established frame boundary; extension sizes
//! follow ETSI TS 102 114 V1.6.1 §7.5.2, Table 7-2. The probes read size fields
//! only: the extension header CRC, assets and alignment are the caller's to check.
use triergon_bitstream::{BitCursor, BitSource};
use triergon_core::{Error, Result};

use crate::dts::{DtsCoreHeader, probe_core_header};

/// Policies checked before the declared extents can be used for buffering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsFrameExtentLimits {
    /// Largest header accepted, in bytes: a core header rounded up to whole bytes
    /// (13 or 15), or the declared header size of an extension substream. Must
    /// not be zero.
    pub maximum_header_bytes: usize,
    /// Largest declared frame accepted, in bytes. Must not be zero.
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
    /// `UserDefinedBits`, the eight bits after the sync word, as coded.
    pub user_defined: u8,
    /// `nExtSSIndex`, the extension substream index: 0 to 3.
    pub substream_index: u8,
    /// `bHeaderSizeType`: `true` when the header size is coded in 12 bits and
    /// the frame size in 20, `false` for 8 and 16.
    pub long_sizes: bool,
    /// `nuExtSSHeaderSize`, the declared header size in bytes: at least 11, or 12
    /// with long sizes, and at most 256, or 4096 with long sizes.
    pub header_bytes: usize,
    /// `nuExtSSFsize`, the declared size of the whole extension substream frame
    /// in bytes: more than `header_bytes` and at most 65536, or 1048576 with long
    /// sizes.
    pub frame_bytes: usize,
}

/// Declared extent of a normal core frame or an extension substream frame. A
/// core frame is validated as [`probe_core_header`] validates it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtsFrameExtent {
    /// A core frame, with its header.
    Core(DtsCoreHeader),
    /// An extension substream frame, with its size fields.
    Extension(DtsExtensionSubstreamExtent),
}

impl DtsFrameExtent {
    /// Declared size of the whole frame in bytes, sync word included.
    pub const fn frame_bytes(self) -> usize {
        match self {
            Self::Core(header) => header.frame_bytes,
            Self::Extension(extent) => extent.frame_bytes,
        }
    }
}

/// The declared extent of the canonical big-endian DTS frame at the start of `input`.
/// The probe allocates nothing, inspects at most fifteen bytes and does not search
/// for a sync word; `Truncated` means an incomplete prefix, not an incomplete frame.
/// Word-swapped and 14-bit packings are not read.
///
/// Fails with `Truncated` when `input` ends inside the prefix; `Invalid` when it begins
/// with neither the core nor the extension substream sync word, and when a limit is
/// zero; `Limit` when the header or the frame is larger than `limits` allow. Otherwise
/// whatever [`probe_core_header`] or [`probe_extension_substream_extent`] refuses the
/// prefix with.
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

/// Reads the size prefix of an extension substream header: nine bytes with short
/// sizes, ten with long ones. The sizes are coded minus one; the index is kept even
/// when nonzero. Neither the rest of the header nor any order or dependency between
/// substreams is checked.
///
/// Fails with `Truncated` when `input` ends inside the size prefix; `Invalid` when it
/// does not begin with the extension substream sync word `0x64582025`, when the
/// declared sizes cannot hold a header and data after it, and when a limit is zero;
/// `Limit` when the header or the frame is larger than `limits` allow.
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
