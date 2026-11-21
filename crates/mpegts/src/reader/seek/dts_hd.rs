//! A self-contained XLL entry, from ETSI TS 102 114 V1.6.1.
//!
//! Annex F.4.3 alignment establishes an AU boundary, not decoder recovery.
//! Tables 7-2, 7-5, 7-6 and 7-7 identify the active asset and its navigation.
//! Section 7.5.3.4 requires XLL sync and specifies the initial decoding delay.
//! Sections 8.5.2.1 and 8.5.4 keep XLL predictor history within a frame.
use super::Packet;
use triergon_bitstream::{BitCursor, BitSource};
use triergon_core::{Error, Result, WorkBudget};
use triergon_framing::{DtsFrameExtentLimits, probe_dts_extension_substream_extent};

const UNSUPPORTED: Error = Error::Unsupported("MPEG-TS DTS-HD seek recovery layout");

// The common header is at most 256 bytes (Table 8-1). Keep its CRC cooperative
// even when the caller supplies one work unit per step. The AU owns the bytes.
pub(super) struct Header {
    at: usize,
    end: usize,
    crc: u16,
}

impl Header {
    pub(super) fn step(&mut self, bytes: &[u8], work: &mut WorkBudget) -> Result<bool> {
        let end = self.at + work.take((self.end - self.at) as u64) as usize;
        for &byte in &bytes[self.at..end] {
            // Annex B: CRC-CCITT, preset 0xffff, no final inversion.
            let high = (self.crc >> 8) ^ u16::from(byte);
            let folded = high ^ (high >> 4);
            self.crc = (self.crc << 8) ^ (folded << 12) ^ (folded << 5) ^ folded;
        }
        self.at = end;
        if end != self.end {
            Ok(false)
        } else if self.crc == 0 {
            Ok(true)
        } else {
            Err(Error::Invalid("DTS-HD seek XLL header CRC"))
        }
    }
}

fn read(bits: &mut BitCursor<'_>, width: u32) -> Result<u32> {
    bits.read(width).map_err(|_| Error::Truncated)
}

fn skip(bits: &mut BitCursor<'_>, width: usize) -> Result<()> {
    bits.skip(width).map_err(|_| Error::Truncated)
}

/// The framer already checked the extension header CRC and complete extent.
/// Refuse other layouts rather than treating a core HFLAG as extension proof.
pub(super) fn entry(packet: &Packet) -> Result<Option<Header>> {
    let descriptor = packet
        .track_info()
        .stream()
        .dvb_dts_hd()?
        .ok_or(UNSUPPORTED)?;
    if descriptor.core.is_some() || descriptor.extensions[1..].iter().any(Option::is_some) {
        return Err(UNSUPPORTED);
    }
    let mut assets = descriptor.extensions[0].ok_or(UNSUPPORTED)?.assets();
    // Annex G.3.1 maps DVB asset_construction to Table E-2: 17 is XLL
    // without a core. Do not reinterpret LBR or another advertised asset.
    if assets.next().is_none_or(|asset| asset.construction != 17) || assets.next().is_some() {
        return Err(UNSUPPORTED);
    }
    entry_bytes(packet.bytes().as_ref())
}

fn entry_bytes(bytes: &[u8]) -> Result<Option<Header>> {
    let extent = probe_dts_extension_substream_extent(
        bytes,
        DtsFrameExtentLimits {
            maximum_header_bytes: 4096,
            maximum_frame_bytes: bytes.len(),
        },
    )?;
    if extent.substream_index != 0
        || extent.frame_bytes != bytes.len()
        || extent.header_bytes % 4 != 0
    {
        // Section 8.3.1 requires DWORD-aligned XLL sync. With offset zero,
        // the extension header must end on that boundary (sections 7.5.1/F.4.3).
        return Err(UNSUPPORTED);
    }
    let header = bytes
        .get(..extent.header_bytes - 2)
        .ok_or(Error::Truncated)?;
    let mut bits = BitCursor::at(header, if extent.long_sizes { 75 } else { 67 });
    if read(&mut bits, 1)? == 0 {
        return Ok(None); // Decoder setup is inherited, so this is not an entry.
    }
    if read(&mut bits, 2)? == 3 {
        return Err(UNSUPPORTED); // Reserved reference clock.
    }
    skip(&mut bits, 3)?; // Frame duration.
    if read(&mut bits, 1)? != 0 {
        skip(&mut bits, 36)?; // Optional timecode, not the source PES clock.
    }
    // One presentation, one asset, extension zero and active asset zero.
    if read(&mut bits, 6)? != 0 || read(&mut bits, 1)? != 1 || read(&mut bits, 8)? != 1 {
        return Err(UNSUPPORTED);
    }
    if read(&mut bits, 1)? != 0 {
        return Err(UNSUPPORTED); // No mixer dependencies in this slice.
    }
    let size_bits = if extent.long_sizes { 20 } else { 16 };
    let asset_bytes = read(&mut bits, size_bits)? as usize + 1;
    if asset_bytes != bytes.len() - extent.header_bytes {
        return Err(UNSUPPORTED);
    }
    let descriptor_start = bits.position();
    let descriptor_end = descriptor_start + (read(&mut bits, 9)? as usize + 1) * 8;
    if descriptor_end >= header.len() * 8 || read(&mut bits, 3)? != 0 {
        return Err(UNSUPPORTED);
    }
    // Table 7-5: descriptive fields do not affect decoder recovery.
    if read(&mut bits, 1)? != 0 {
        skip(&mut bits, 4)?;
    }
    if read(&mut bits, 1)? != 0 {
        skip(&mut bits, 24)?;
    }
    if read(&mut bits, 1)? != 0 {
        let text_bytes = read(&mut bits, 10)? as usize + 1;
        skip(&mut bits, text_bytes * 8)?;
    }
    skip(&mut bits, 9)?; // Bit resolution and maximum sampling frequency.
    let channels = read(&mut bits, 8)? + 1;
    if read(&mut bits, 1)? == 0 {
        return Err(UNSUPPORTED); // Non-speaker representations.
    }
    let stereo = channels > 2 && read(&mut bits, 1)? != 0;
    if channels > 6 {
        skip(&mut bits, 1)?;
    }
    if read(&mut bits, 1)? != 0 {
        let mask_bits = (read(&mut bits, 2)? as usize + 1) * 4;
        skip(&mut bits, mask_bits)?;
    }
    if read(&mut bits, 3)? != 0 {
        return Err(UNSUPPORTED); // Speaker remapping tables.
    }
    // Table 7-6, with mixing disabled.
    let drc = read(&mut bits, 1)? != 0;
    if drc {
        skip(&mut bits, 8)?;
    }
    if read(&mut bits, 1)? != 0 {
        skip(&mut bits, 5)?;
    }
    if drc && stereo {
        skip(&mut bits, 8)?;
    }
    // Table 7-7: lossless without a CBR component. No core, residual coding
    // component, auxiliary codec or other asset is silently discarded.
    if read(&mut bits, 2)? != 1 {
        return Err(UNSUPPORTED);
    }
    if read(&mut bits, size_bits)? as usize + 1 != asset_bytes {
        return Err(UNSUPPORTED);
    }
    let sync = read(&mut bits, 1)? != 0;
    let mut immediate = false;
    if sync {
        let buffer = read(&mut bits, 4)?;
        let delay_bits = read(&mut bits, 5)? + 1;
        let delay = read(&mut bits, delay_bits)?;
        let offset = read(&mut bits, size_bits)?;
        // No smoothing buffer, delayed decoding or partial preceding frame.
        immediate = buffer == 0 && delay == 0 && offset == 0;
    }
    skip(&mut bits, 3)?; // DTS-HD stream ID; no prior decoder state at entry.
    if read(&mut bits, 2)? != 0 {
        return Err(UNSUPPORTED); // Secondary decoder or Rev2 DRC metadata.
    }
    if bits.position() > descriptor_end {
        return Err(Error::Invalid("DTS-HD seek asset descriptor extent"));
    }
    bits = BitCursor::at(header, descriptor_end);
    if read(&mut bits, 1)? != 0 {
        return Err(UNSUPPORTED); // Backward-compatible core present.
    }
    if !immediate {
        return Ok(None);
    }
    let start = extent.header_bytes;
    let xll = &bytes[start..];
    if xll.get(..4) != Some(&[0x41, 0xa2, 0x95, 0x47]) {
        return Err(Error::Invalid("DTS-HD seek XLL sync"));
    }
    let mut bits = BitCursor::at(xll, 32);
    if read(&mut bits, 4)? != 0 {
        return Err(UNSUPPORTED); // XLL version one (Table 8-1).
    }
    let header_bytes = read(&mut bits, 8)? as usize + 1;
    let frame_size_bits = read(&mut bits, 5)? + 1;
    let frame_bytes = u64::from(read(&mut bits, frame_size_bits)?) + 1;
    if frame_bytes != asset_bytes as u64 || header_bytes > xll.len() {
        return Err(Error::Invalid("DTS-HD seek XLL frame extent"));
    }
    skip(&mut bits, 4 + 4 + 4 + 5 + 2)?;
    let scalable = read(&mut bits, 1)? != 0;
    skip(&mut bits, 5 + if scalable { 4 } else { 0 })?;
    if bits.position() + 16 > header_bytes * 8 {
        return Err(Error::Invalid("DTS-HD seek XLL header extent"));
    }
    Ok(Some(Header {
        at: start + 4,
        end: start + header_bytes,
        crc: u16::MAX,
    }))
}
