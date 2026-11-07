//! Opus identification header and packet durations.
//!
//! [`OpusHead`] is the identification header of RFC 7845 section 5.1, which Ogg carries as
//! its first packet and Matroska as CodecPrivate; ISO BMFF carries the same fields
//! big-endian in a `dOps` box ([`OpusHead::from_dops`]). [`opus_packet_samples`] reads the
//! duration of a packet from its TOC (RFC 6716 section 3.1).
use triergon_core::{Error, Result};

/// Opus decodes at 48 kHz whatever the input rate was.
pub const OPUS_RATE: u32 = 48_000;

/// The fields of an Opus identification header.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct OpusHead {
    pub version: u8,
    pub channels: u8,
    /// Samples at 48 kHz to discard from the start of the decoded stream.
    pub pre_skip: u16,
    /// The encoder's input rate, informational only; 0 when unspecified.
    pub input_sample_rate: u32,
    /// Output gain in Q7.8 dB.
    pub output_gain: i16,
    pub mapping_family: u8,
    pub streams: u8,
    pub coupled_streams: u8,
    mapping: [u8; 255],
}

impl core::fmt::Debug for OpusHead {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OpusHead")
            .field("version", &self.version)
            .field("channels", &self.channels)
            .field("pre_skip", &self.pre_skip)
            .field("input_sample_rate", &self.input_sample_rate)
            .field("output_gain", &self.output_gain)
            .field("mapping_family", &self.mapping_family)
            .field("streams", &self.streams)
            .field("coupled_streams", &self.coupled_streams)
            .field("mapping", &self.mapping())
            .finish()
    }
}

impl OpusHead {
    /// Size of an `OpusHead` with channel mapping family 0.
    pub const FAMILY_ZERO_BYTES: usize = 19;

    /// Parse an `OpusHead` packet (RFC 7845 section 5.1). Bytes after the mapping table
    /// are ignored, as the specification requires of a version 0 header.
    pub fn parse(bytes: &[u8]) -> Result<OpusHead> {
        if !bytes.starts_with(b"OpusHead") {
            return Err(Error::Invalid("Opus identification header magic"));
        }
        let field = |range: core::ops::Range<usize>| bytes.get(range).ok_or(Error::Truncated);
        let version = field(8..9)?[0];
        let channels = field(9..10)?[0];
        let pre_skip = u16::from_le_bytes(field(10..12)?.try_into().map_err(|_| Error::Truncated)?);
        let input_sample_rate =
            u32::from_le_bytes(field(12..16)?.try_into().map_err(|_| Error::Truncated)?);
        let output_gain =
            i16::from_le_bytes(field(16..18)?.try_into().map_err(|_| Error::Truncated)?);
        let family = field(18..19)?[0];
        Self::finish(
            version,
            channels,
            pre_skip,
            input_sample_rate,
            output_gain,
            family,
            bytes.get(19..).unwrap_or(&[]),
        )
    }

    /// Parse the payload of an ISO BMFF `dOps` box (Opus in ISO BMFF, section 4.3.2):
    /// the same fields, big-endian, without the magic.
    pub fn from_dops(bytes: &[u8]) -> Result<OpusHead> {
        let field = |range: core::ops::Range<usize>| bytes.get(range).ok_or(Error::Truncated);
        let version = field(0..1)?[0];
        if version != 0 {
            return Err(Error::Unsupported("dOps version"));
        }
        let channels = field(1..2)?[0];
        let pre_skip = u16::from_be_bytes(field(2..4)?.try_into().map_err(|_| Error::Truncated)?);
        let input_sample_rate =
            u32::from_be_bytes(field(4..8)?.try_into().map_err(|_| Error::Truncated)?);
        let output_gain =
            i16::from_be_bytes(field(8..10)?.try_into().map_err(|_| Error::Truncated)?);
        let family = field(10..11)?[0];
        // The identification header version a dOps box describes is 1.
        Self::finish(
            1,
            channels,
            pre_skip,
            input_sample_rate,
            output_gain,
            family,
            bytes.get(11..).unwrap_or(&[]),
        )
    }

    fn finish(
        version: u8,
        channels: u8,
        pre_skip: u16,
        input_sample_rate: u32,
        output_gain: i16,
        mapping_family: u8,
        table: &[u8],
    ) -> Result<OpusHead> {
        // RFC 7845: major version 0 (values 0-15) is the only one a decoder may accept.
        if version > 15 {
            return Err(Error::Unsupported("Opus identification header version"));
        }
        if channels == 0 {
            return Err(Error::Invalid("Opus channel count"));
        }
        let mut mapping = [0u8; 255];
        let (streams, coupled_streams) = if mapping_family == 0 {
            if channels > 2 {
                return Err(Error::Invalid("Opus family 0 channel count"));
            }
            mapping[0] = 0;
            mapping[1] = 1;
            (1, channels - 1)
        } else {
            let streams = *table.first().ok_or(Error::Truncated)?;
            let coupled = *table.get(1).ok_or(Error::Truncated)?;
            if streams == 0 || coupled > streams || u16::from(streams) + u16::from(coupled) > 255 {
                return Err(Error::Invalid("Opus stream counts"));
            }
            let table = table
                .get(2..2 + usize::from(channels))
                .ok_or(Error::Truncated)?;
            for (slot, &index) in mapping.iter_mut().zip(table) {
                if index != 255 && u16::from(index) >= u16::from(streams) + u16::from(coupled) {
                    return Err(Error::Invalid("Opus channel mapping index"));
                }
                *slot = index;
            }
            (streams, coupled)
        };
        Ok(OpusHead {
            version,
            channels,
            pre_skip,
            input_sample_rate,
            output_gain,
            mapping_family,
            streams,
            coupled_streams,
            mapping,
        })
    }

    /// For each output channel, the decoded stream channel it takes (255: silence).
    /// Index `i < 2 * coupled_streams` is channel `i % 2` of coupled stream `i / 2`; a
    /// later index is the mono stream `i - coupled_streams`.
    pub fn mapping(&self) -> &[u8] {
        &self.mapping[..usize::from(self.channels)]
    }

    /// The header as an `OpusHead` packet: the canonical configuration the decoder takes.
    pub fn to_bytes(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"OpusHead");
        out.push(self.version);
        out.push(self.channels);
        out.extend_from_slice(&self.pre_skip.to_le_bytes());
        out.extend_from_slice(&self.input_sample_rate.to_le_bytes());
        out.extend_from_slice(&self.output_gain.to_le_bytes());
        out.push(self.mapping_family);
        if self.mapping_family != 0 {
            out.push(self.streams);
            out.push(self.coupled_streams);
            out.extend_from_slice(self.mapping());
        }
    }
}

/// Samples per frame at 48 kHz for a TOC byte's configuration (RFC 6716 Table 2).
pub fn opus_frame_samples(toc: u8) -> u32 {
    let config = toc >> 3;
    match config {
        // SILK-only: 10, 20, 40, 60 ms.
        0..=11 => [480, 960, 1920, 2880][usize::from(config & 3)],
        // Hybrid: 10, 20 ms.
        12..=15 => [480, 960][usize::from(config & 1)],
        // CELT-only: 2.5, 5, 10, 20 ms.
        _ => [120, 240, 480, 960][usize::from(config & 3)],
    }
}

/// Frames in a packet from its TOC and frame-count byte (RFC 6716 section 3.2).
pub fn opus_packet_frames(packet: &[u8]) -> Result<u32> {
    let toc = *packet.first().ok_or(Error::Truncated)?;
    Ok(match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => {
            let count = u32::from(*packet.get(1).ok_or(Error::Truncated)? & 0x3f);
            if count == 0 {
                return Err(Error::Invalid("Opus frame count"));
            }
            count
        }
    })
}

/// Duration of a packet at 48 kHz. A packet longer than 120 ms is invalid (RFC 6716
/// section 3.2.5).
pub fn opus_packet_samples(packet: &[u8]) -> Result<u32> {
    let frames = opus_packet_frames(packet)?;
    let samples = frames * opus_frame_samples(packet[0]);
    if samples > 5760 {
        return Err(Error::Invalid("Opus packet duration"));
    }
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_zero_and_one_headers_round_trip() {
        let mut head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let parsed = OpusHead::parse(&head).unwrap();
        assert_eq!(
            (parsed.channels, parsed.pre_skip, parsed.input_sample_rate),
            (2, 312, 48000)
        );
        assert_eq!((parsed.streams, parsed.coupled_streams), (1, 1));
        assert_eq!(parsed.mapping(), &[0, 1]);
        let mut out = Vec::new();
        parsed.to_bytes(&mut out);
        assert_eq!(out, head);

        head[9] = 6;
        head[18] = 1;
        head.extend_from_slice(&[4, 2, 0, 4, 1, 2, 3, 5]);
        let six = OpusHead::parse(&head).unwrap();
        assert_eq!(six.mapping(), &[0, 4, 1, 2, 3, 5]);
        let mut dops = vec![0, 6, 0x01, 0x38, 0, 0, 0xbb, 0x80, 0, 0, 1, 4, 2];
        dops.extend_from_slice(&[0, 4, 1, 2, 3, 5]);
        assert_eq!(OpusHead::from_dops(&dops).unwrap(), six);
        head[9] = 3;
        head[18] = 0;
        assert!(OpusHead::parse(&head).is_err());
    }

    #[test]
    fn packet_durations_follow_the_toc() {
        assert_eq!(opus_packet_samples(&[0x00]).unwrap(), 480); // SILK NB 10 ms
        assert_eq!(opus_packet_samples(&[0x19]).unwrap(), 2880 * 2); // SILK 60 ms, two frames
        assert_eq!(opus_packet_samples(&[0x78 | 2]).unwrap(), 960 * 2); // hybrid FB 20 ms, two frames
        assert_eq!(opus_packet_samples(&[0xf8 | 3, 6]).unwrap(), 960 * 6);
        assert_eq!(opus_packet_samples(&[0x80 | 3, 48]).unwrap(), 120 * 48);
        assert!(opus_packet_samples(&[0xf8 | 3, 7]).is_err());
        assert!(opus_packet_samples(&[0xf8 | 3, 0]).is_err());
        assert!(opus_packet_samples(&[]).is_err());
    }
}
