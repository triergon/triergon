//! FLAC stream configuration and frame headers (RFC 9639).
//!
//! The canonical decoder configuration is the native stream header: the `fLaC` marker
//! followed by metadata blocks, of which the first is STREAMINFO. Matroska CodecPrivate
//! holds exactly that; [`flac_configuration_from_dfla`] and
//! [`flac_configuration_from_ogg`] rebuild it from an ISO BMFF `dfLa` box and the first
//! packet of an Ogg FLAC stream. [`FlacFrameHeader`] reads and checks a frame header.
use triergon_bitstream::crc::crc8;
use triergon_core::{Error, Result};

/// Metadata block type of STREAMINFO.
pub const FLAC_STREAMINFO: u8 = 0;
/// Metadata block type of VORBIS_COMMENT.
pub const FLAC_VORBIS_COMMENT: u8 = 4;

/// The STREAMINFO metadata block (RFC 9639 section 8.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlacStreamInfo {
    pub min_block_size: u16,
    pub max_block_size: u16,
    /// Bytes; 0 when unknown.
    pub min_frame_size: u32,
    pub max_frame_size: u32,
    pub sample_rate: u32,
    /// 1 to 8.
    pub channels: u8,
    /// 4 to 32.
    pub bits_per_sample: u8,
    /// Inter-channel samples in the stream; 0 when unknown.
    pub total_samples: u64,
    pub md5: [u8; 16],
}

impl FlacStreamInfo {
    /// Size of the block body.
    pub const BYTES: usize = 34;

    /// Parse a STREAMINFO block body.
    pub fn parse(body: &[u8]) -> Result<FlacStreamInfo> {
        let b = body.get(..Self::BYTES).ok_or(Error::Truncated)?;
        let be24 = |at: usize| u32::from_be_bytes([0, b[at], b[at + 1], b[at + 2]]);
        let packed = u64::from_be_bytes([b[10], b[11], b[12], b[13], b[14], b[15], b[16], b[17]]);
        let info = FlacStreamInfo {
            min_block_size: u16::from_be_bytes([b[0], b[1]]),
            max_block_size: u16::from_be_bytes([b[2], b[3]]),
            min_frame_size: be24(4),
            max_frame_size: be24(7),
            sample_rate: (packed >> 44) as u32,
            channels: ((packed >> 41) & 7) as u8 + 1,
            bits_per_sample: ((packed >> 36) & 31) as u8 + 1,
            total_samples: packed & ((1 << 36) - 1),
            md5: b[18..34].try_into().map_err(|_| Error::Truncated)?,
        };
        if info.bits_per_sample < 4 {
            return Err(Error::Invalid("FLAC bits per sample"));
        }
        if info.max_block_size < 16 || info.min_block_size > info.max_block_size {
            // RFC 9639: both at least 16 except a final block; FFmpeg accepts these too,
            // so only an inverted pair is refused.
            if info.min_block_size > info.max_block_size {
                return Err(Error::Invalid("FLAC block size range"));
            }
        }
        Ok(info)
    }
}

/// One metadata block of a configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlacMetadataBlock<'a> {
    pub kind: u8,
    pub last: bool,
    pub body: &'a [u8],
}

/// The metadata blocks after an `fLaC` marker, in order, ending at the block marked last
/// or at the end of `bytes`.
pub fn flac_metadata_blocks(
    bytes: &[u8],
) -> Result<impl Iterator<Item = Result<FlacMetadataBlock<'_>>>> {
    let mut rest = bytes
        .strip_prefix(FLAC_MARKER)
        .ok_or(Error::Invalid("FLAC marker"))?;
    let mut done = false;
    Ok(core::iter::from_fn(move || {
        if done || rest.is_empty() {
            return None;
        }
        let Some(header) = rest.get(..4) else {
            done = true;
            return Some(Err(Error::Truncated));
        };
        let length = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
        let Some(body) = rest.get(4..4 + length) else {
            done = true;
            return Some(Err(Error::Truncated));
        };
        let block = FlacMetadataBlock {
            kind: header[0] & 0x7f,
            last: header[0] & 0x80 != 0,
            body,
        };
        rest = &rest[4 + length..];
        done = block.last;
        Some(Ok(block))
    }))
}

/// The STREAMINFO of a canonical configuration: its first metadata block.
pub fn flac_stream_info(configuration: &[u8]) -> Result<FlacStreamInfo> {
    let first = flac_metadata_blocks(configuration)?
        .next()
        .ok_or(Error::Truncated)??;
    if first.kind != FLAC_STREAMINFO {
        return Err(Error::Invalid(
            "FLAC first metadata block is not STREAMINFO",
        ));
    }
    FlacStreamInfo::parse(first.body)
}

/// The marker a native FLAC stream, and its canonical configuration, starts with.
pub(crate) const FLAC_MARKER: &[u8; 4] = b"fLaC";

/// The metadata blocks of an ISO BMFF `dfLa` box payload (version, flags, then blocks).
pub(crate) fn dfla_blocks(payload: &[u8]) -> Result<&[u8]> {
    let (header, blocks) = payload.split_at_checked(4).ok_or(Error::Truncated)?;
    if header[0] != 0 {
        return Err(Error::Unsupported("dfLa version"));
    }
    Ok(blocks)
}

/// The native stream header inside the first packet of an Ogg FLAC stream (the Ogg
/// mapping: `0x7f "FLAC"`, version 1.0, a header packet count, `fLaC` and STREAMINFO),
/// and the number of further header packets it announces (0: unknown).
pub(crate) fn ogg_native(packet: &[u8]) -> Result<(&[u8], u16)> {
    let rest = packet
        .strip_prefix(b"\x7fFLAC")
        .ok_or(Error::Invalid("Ogg FLAC mapping header"))?;
    let header = rest.get(..4).ok_or(Error::Truncated)?;
    if header[0] != 1 {
        return Err(Error::Unsupported("Ogg FLAC mapping version"));
    }
    let headers = u16::from_be_bytes([header[2], header[3]]);
    let native = &rest[4..];
    flac_stream_info(native)?;
    Ok((native, headers))
}

/// The canonical configuration of an ISO BMFF `dfLa` box payload (version, flags, then
/// metadata blocks).
pub fn flac_configuration_from_dfla(payload: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let blocks = dfla_blocks(payload)?;
    out.extend_from_slice(FLAC_MARKER);
    out.extend_from_slice(blocks);
    Ok(())
}

/// The canonical configuration of the first packet of an Ogg FLAC stream. Returns the
/// number of further header packets the stream announces (0: unknown).
pub fn flac_configuration_from_ogg(packet: &[u8], out: &mut Vec<u8>) -> Result<u16> {
    let (native, headers) = ogg_native(packet)?;
    out.extend_from_slice(native);
    Ok(headers)
}

/// How the block sizes of a stream vary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlacBlocking {
    /// The coded number counts frames.
    Fixed,
    /// The coded number counts samples.
    Variable,
}

/// How a frame codes its channels (RFC 9639 section 9.1.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlacChannelAssignment {
    /// 1 to 8 channels, each coded on its own.
    Independent(u8),
    /// Left and side (left minus right).
    LeftSide,
    /// Side and right.
    SideRight,
    /// Mid (the average, rounded down) and side.
    MidSide,
}

impl FlacChannelAssignment {
    /// Channels the frame codes.
    pub fn channels(self) -> u8 {
        match self {
            FlacChannelAssignment::Independent(count) => count,
            _ => 2,
        }
    }
}

/// A frame header (RFC 9639 section 9.1) with its CRC-8 checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlacFrameHeader {
    pub blocking: FlacBlocking,
    pub block_size: u32,
    /// `None` when the header defers to STREAMINFO.
    pub sample_rate: Option<u32>,
    pub channels: FlacChannelAssignment,
    /// `None` when the header defers to STREAMINFO.
    pub bits_per_sample: Option<u8>,
    /// Frame number (fixed blocking) or first sample number (variable blocking).
    pub number: u64,
    /// Header bytes including the CRC-8.
    pub len: usize,
}

impl FlacFrameHeader {
    /// The first sample of the frame, given STREAMINFO's maximum block size for fixed
    /// blocking.
    pub fn first_sample(&self, max_block_size: u16) -> Option<u64> {
        match self.blocking {
            FlacBlocking::Variable => Some(self.number),
            FlacBlocking::Fixed => self.number.checked_mul(u64::from(max_block_size)),
        }
    }

    /// Parse the header at the start of `bytes`. `Err(Truncated)` asks for more bytes;
    /// any other error means no valid header starts here.
    pub fn parse(bytes: &[u8]) -> Result<FlacFrameHeader> {
        let fixed = bytes.get(..4).ok_or(Error::Truncated)?;
        if fixed[0] != 0xff || fixed[1] & 0xfe != 0xf8 {
            return Err(Error::Invalid("FLAC frame sync"));
        }
        let blocking = if fixed[1] & 1 == 0 {
            FlacBlocking::Fixed
        } else {
            FlacBlocking::Variable
        };
        let size_code = fixed[2] >> 4;
        let rate_code = fixed[2] & 15;
        let channels = match fixed[3] >> 4 {
            code @ 0..=7 => FlacChannelAssignment::Independent(code + 1),
            8 => FlacChannelAssignment::LeftSide,
            9 => FlacChannelAssignment::SideRight,
            10 => FlacChannelAssignment::MidSide,
            _ => return Err(Error::Invalid("FLAC reserved channel assignment")),
        };
        let depth_code = (fixed[3] >> 1) & 7;
        if size_code == 0 {
            return Err(Error::Invalid("FLAC reserved block size"));
        }
        if rate_code == 15 {
            return Err(Error::Invalid("FLAC invalid sample rate code"));
        }
        if depth_code == 3 {
            return Err(Error::Invalid("FLAC reserved sample size"));
        }
        if fixed[3] & 1 != 0 {
            return Err(Error::Invalid("FLAC reserved header bit"));
        }
        // Coded number: UTF-8-like, up to 7 bytes and 36 bits.
        let mut at = 4;
        let lead = *bytes.get(at).ok_or(Error::Truncated)?;
        at += 1;
        let extra = lead.leading_ones() as usize;
        let (mut number, extra) = match extra {
            0 => (u64::from(lead), 0),
            1 => return Err(Error::Invalid("FLAC coded number")),
            2..=7 => (u64::from(lead) & ((1 << (7 - extra)) - 1), extra - 1),
            _ => return Err(Error::Invalid("FLAC coded number")),
        };
        for _ in 0..extra {
            let byte = *bytes.get(at).ok_or(Error::Truncated)?;
            at += 1;
            if byte & 0xc0 != 0x80 {
                return Err(Error::Invalid("FLAC coded number"));
            }
            number = (number << 6) | u64::from(byte & 0x3f);
        }
        if blocking == FlacBlocking::Fixed && number >= 1 << 31 {
            return Err(Error::Invalid("FLAC frame number"));
        }
        let mut take = |count: usize| -> Result<u32> {
            let field = bytes.get(at..at + count).ok_or(Error::Truncated)?;
            at += count;
            Ok(field.iter().fold(0, |v, &b| (v << 8) | u32::from(b)))
        };
        let block_size = match size_code {
            1 => 192,
            2..=5 => 576 << (size_code - 2),
            6 => take(1)? + 1,
            7 => take(2)? + 1,
            _ => 256 << (size_code - 8),
        };
        let sample_rate = match rate_code {
            0 => None,
            1 => Some(88_200),
            2 => Some(176_400),
            3 => Some(192_000),
            4 => Some(8_000),
            5 => Some(16_000),
            6 => Some(22_050),
            7 => Some(24_000),
            8 => Some(32_000),
            9 => Some(44_100),
            10 => Some(48_000),
            11 => Some(96_000),
            12 => Some(take(1)? * 1000),
            13 => Some(take(2)?),
            _ => Some(take(2)? * 10),
        };
        let bits_per_sample = match depth_code {
            0 => None,
            1 => Some(8),
            2 => Some(12),
            4 => Some(16),
            5 => Some(20),
            6 => Some(24),
            _ => Some(32),
        };
        let crc = *bytes.get(at).ok_or(Error::Truncated)?;
        if crc8(0, &bytes[..at]) != crc {
            return Err(Error::Invalid("FLAC frame header CRC"));
        }
        Ok(FlacFrameHeader {
            blocking,
            block_size,
            sample_rate,
            channels,
            bits_per_sample,
            number,
            len: at + 1,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn streaminfo() -> [u8; 34] {
        let mut b = [0u8; 34];
        b[..4].copy_from_slice(&[0x10, 0, 0x10, 0]);
        // 44100 Hz, 2 channels, 16 bits, 1000 samples.
        let packed: u64 = (44100 << 44) + (1 << 41) + (15 << 36) + 1000;
        b[10..18].copy_from_slice(&packed.to_be_bytes());
        b
    }

    #[test]
    fn configurations_from_every_carrier_agree() {
        let mut native = b"fLaC\x80\0\0\x22".to_vec();
        native.extend_from_slice(&streaminfo());
        let info = flac_stream_info(&native).unwrap();
        assert_eq!(
            (info.sample_rate, info.channels, info.bits_per_sample),
            (44100, 2, 16)
        );
        assert_eq!((info.max_block_size, info.total_samples), (4096, 1000));

        let mut dfla = vec![0, 0, 0, 0];
        dfla.extend_from_slice(&native[4..]);
        let mut out = Vec::new();
        flac_configuration_from_dfla(&dfla, &mut out).unwrap();
        assert_eq!(out, native);

        let mut ogg = b"\x7fFLAC\x01\x00\x00\x01".to_vec();
        ogg.extend_from_slice(&native);
        out.clear();
        assert_eq!(flac_configuration_from_ogg(&ogg, &mut out).unwrap(), 1);
        assert_eq!(out, native);
    }

    #[test]
    fn frame_headers_check_their_crc() {
        // Fixed blocking, 4096 samples, 44.1 kHz, stereo, 16 bits, frame 0.
        let mut header = vec![0xff, 0xf8, 0xc9, 0x18, 0x00];
        header.push(crc8(0, &header));
        let parsed = FlacFrameHeader::parse(&header).unwrap();
        assert_eq!(parsed.block_size, 4096);
        assert_eq!(parsed.sample_rate, Some(44100));
        assert_eq!(parsed.channels, FlacChannelAssignment::Independent(2));
        assert_eq!(parsed.bits_per_sample, Some(16));
        assert_eq!(parsed.len, 6);
        header[5] ^= 1;
        assert!(FlacFrameHeader::parse(&header).is_err());
        // Variable blocking, explicit 16-bit block size, two-byte coded number.
        let mut header = vec![0xff, 0xf9, 0x79, 0x08, 0xc2, 0x80, 0x01, 0x00];
        header.push(crc8(0, &header));
        let parsed = FlacFrameHeader::parse(&header).unwrap();
        assert_eq!((parsed.number, parsed.block_size), (128, 257));
        assert_eq!(parsed.first_sample(4096), Some(128));
        assert!(matches!(
            FlacFrameHeader::parse(&header[..5]),
            Err(Error::Truncated)
        ));
    }
}
