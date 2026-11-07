//! Vorbis header packets and Xiph lacing.
//!
//! A Vorbis stream starts with three header packets (Vorbis I section 4.2): the
//! identification header ([`VorbisIdentification`]), the comment header and the setup
//! header. Ogg carries them as its first three packets; Matroska and the canonical
//! decoder configuration join them with Xiph lacing ([`xiph_split`], [`xiph_join`]).
use triergon_core::{Error, Result};

/// The fields of a Vorbis identification header (Vorbis I section 4.2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VorbisIdentification {
    pub channels: u8,
    pub sample_rate: u32,
    pub bitrate_maximum: i32,
    pub bitrate_nominal: i32,
    pub bitrate_minimum: i32,
    /// Short and long block sizes, powers of two from 64 to 8192.
    pub blocksize: [u16; 2],
}

impl VorbisIdentification {
    /// Parse an identification header packet.
    pub fn parse(packet: &[u8]) -> Result<VorbisIdentification> {
        if packet.len() < 30 {
            return Err(Error::Truncated);
        }
        if &packet[..7] != b"\x01vorbis" {
            return Err(Error::Invalid("Vorbis identification header"));
        }
        let le32 = |at: usize| {
            u32::from_le_bytes([packet[at], packet[at + 1], packet[at + 2], packet[at + 3]])
        };
        if le32(7) != 0 {
            return Err(Error::Unsupported("Vorbis version"));
        }
        let channels = packet[11];
        let sample_rate = le32(12);
        let sizes = packet[28];
        let blocksize = [1u16 << (sizes & 15).min(15), 1u16 << (sizes >> 4).min(15)];
        if channels == 0 || sample_rate == 0 {
            return Err(Error::Invalid("Vorbis channels or sample rate"));
        }
        let valid = |size: u16| (64..=8192).contains(&size);
        if !valid(blocksize[0]) || !valid(blocksize[1]) || blocksize[0] > blocksize[1] {
            return Err(Error::Invalid("Vorbis block sizes"));
        }
        if packet[29] & 1 == 0 {
            return Err(Error::Invalid("Vorbis framing bit"));
        }
        Ok(VorbisIdentification {
            channels,
            sample_rate,
            bitrate_maximum: le32(16) as i32,
            bitrate_nominal: le32(20) as i32,
            bitrate_minimum: le32(24) as i32,
            blocksize,
        })
    }
}

/// The type byte a Vorbis header packet starts with: 1, 3 or 5 for identification,
/// comment and setup.
pub fn vorbis_header_type(packet: &[u8]) -> Option<u8> {
    match packet {
        [kind @ (1 | 3 | 5), b'v', b'o', b'r', b'b', b'i', b's', ..] => Some(*kind),
        _ => None,
    }
}

/// Split Xiph-laced packets (Matroska CodecPrivate for `A_VORBIS`): a byte holding the
/// packet count minus one, the size of each packet but the last as runs of 255 ending in
/// a smaller byte, then the packets.
pub fn xiph_split(bytes: &[u8], out: &mut Vec<core::ops::Range<usize>>) -> Result<()> {
    out.clear();
    let count = usize::from(*bytes.first().ok_or(Error::Truncated)?) + 1;
    let mut at = 1;
    let mut sizes = [0usize; 256];
    for size in sizes.iter_mut().take(count - 1) {
        loop {
            let byte = *bytes.get(at).ok_or(Error::Truncated)?;
            at += 1;
            *size = size.checked_add(usize::from(byte)).ok_or(Error::Overflow)?;
            if byte != 255 {
                break;
            }
        }
    }
    for &size in &sizes[..count - 1] {
        let end = at.checked_add(size).ok_or(Error::Overflow)?;
        if end > bytes.len() {
            return Err(Error::Truncated);
        }
        out.push(at..end);
        at = end;
    }
    out.push(at..bytes.len());
    Ok(())
}

/// Join packets with Xiph lacing, the inverse of [`xiph_split`]. At most 256 packets.
pub fn xiph_join(packets: &[&[u8]], out: &mut Vec<u8>) -> Result<()> {
    let Some((last, rest)) = packets.split_last() else {
        return Err(Error::Invalid("no packets to lace"));
    };
    if packets.len() > 256 {
        return Err(Error::Invalid("more than 256 laced packets"));
    }
    out.push((packets.len() - 1) as u8);
    for packet in rest {
        let mut size = packet.len();
        while size >= 255 {
            out.push(255);
            size -= 255;
        }
        out.push(size as u8);
    }
    for packet in rest {
        out.extend_from_slice(packet);
    }
    out.extend_from_slice(last);
    Ok(())
}

/// The three Vorbis header packets of a canonical configuration.
#[derive(Clone, Copy, Debug)]
pub struct VorbisHeaders<'a> {
    pub identification: &'a [u8],
    pub comment: &'a [u8],
    pub setup: &'a [u8],
}

impl<'a> VorbisHeaders<'a> {
    /// The headers of a Xiph-laced configuration, each checked for its type byte.
    pub fn from_laced(bytes: &'a [u8]) -> Result<VorbisHeaders<'a>> {
        let mut ranges = Vec::with_capacity(3);
        xiph_split(bytes, &mut ranges)?;
        let [a, b, c] = ranges.as_slice() else {
            return Err(Error::Invalid("Vorbis configuration packet count"));
        };
        let headers = VorbisHeaders {
            identification: &bytes[a.clone()],
            comment: &bytes[b.clone()],
            setup: &bytes[c.clone()],
        };
        if vorbis_header_type(headers.identification) != Some(1)
            || vorbis_header_type(headers.comment) != Some(3)
            || vorbis_header_type(headers.setup) != Some(5)
        {
            return Err(Error::Invalid("Vorbis header packet types"));
        }
        Ok(headers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xiph_lacing_round_trips() {
        let a = vec![1u8; 30];
        let b = vec![3u8; 600];
        let c = vec![5u8; 7];
        let mut laced = Vec::new();
        xiph_join(&[&a, &b, &c], &mut laced).unwrap();
        assert_eq!(&laced[..5], &[2, 30, 255, 255, 90]);
        let mut ranges = Vec::new();
        xiph_split(&laced, &mut ranges).unwrap();
        assert_eq!(ranges.len(), 3);
        assert_eq!(&laced[ranges[1].clone()], &b[..]);
        assert_eq!(&laced[ranges[2].clone()], &c[..]);
        assert!(xiph_split(&laced[..40], &mut ranges).is_err());
    }

    #[test]
    fn identification_header_fields() {
        let mut id = b"\x01vorbis\0\0\0\0\x02\x44\xac\0\0".to_vec();
        id.extend_from_slice(&[0; 12]);
        id.extend_from_slice(&[0xb8, 1]);
        let parsed = VorbisIdentification::parse(&id).unwrap();
        assert_eq!(parsed.channels, 2);
        assert_eq!(parsed.sample_rate, 44100);
        assert_eq!(parsed.blocksize, [256, 2048]);
        id[28] = 0x8b;
        assert!(VorbisIdentification::parse(&id).is_err());
    }
}
