//! Vorbis header packets and Xiph lacing.
//!
//! A Vorbis stream starts with three header packets (Vorbis I section 4.2): the
//! identification header ([`VorbisIdentification`]), the comment header and the setup
//! header. Ogg carries them as its first three packets; Matroska and the canonical
//! decoder configuration join them with Xiph lacing ([`xiph_split`], [`xiph_join`]).
//!
//! [`VorbisModes`] recovers the block size of each mode from the setup header, and
//! [`VorbisModes::block`] reads which block an audio packet codes, so a container can
//! derive packet durations without decoding.
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

/// The modes of a setup header: how many there are and which code long blocks.
///
/// The mode configurations end the setup header (Vorbis I section 4.2.4), each one a
/// block flag, two 16-bit fields that must be zero, and an 8-bit mapping number,
/// followed by the framing bit. [`VorbisModes::from_setup`] reads them backwards from
/// the framing bit, so the codebooks, floors, residues and mappings before them need
/// not be parsed. Reading backwards cannot see where the mode list starts; the longest
/// run of well-formed modes whose preceding 6-bit count agrees is taken, with mapping
/// numbers below 64 as the specification bounds them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VorbisModes {
    count: u8,
    /// Bit `m` is the block flag of mode `m`.
    long: u64,
}

/// The block an audio packet codes (Vorbis I section 4.3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VorbisBlock {
    pub long: bool,
    /// For a long block, whether the previous window is long, as its window flag states;
    /// `None` for a short block, whose window does not say.
    pub previous_long: Option<bool>,
}

impl VorbisBlock {
    /// The block size, given the identification header's short and long sizes.
    pub fn size(self, blocksize: [u16; 2]) -> u16 {
        blocksize[usize::from(self.long)]
    }

    /// Samples a packet coding this block completes after a block of size `previous`
    /// (Vorbis I section 4.3.8): from the centre of the previous window to the centre
    /// of this one. The first audio packet of a stream completes none.
    pub fn samples_after(self, previous: u16, blocksize: [u16; 2]) -> u32 {
        (u32::from(previous) + u32::from(self.size(blocksize))) / 4
    }
}

impl VorbisModes {
    /// The modes of a setup header packet.
    pub fn from_setup(setup: &[u8]) -> Result<VorbisModes> {
        if vorbis_header_type(setup) != Some(5) {
            return Err(Error::Invalid("Vorbis setup header"));
        }
        let bit = |at: usize| setup[at >> 3] >> (at & 7) & 1 != 0;
        let field = |at: usize, width: usize| {
            (0..width).fold(0u32, |value, k| value | u32::from(bit(at + k)) << k)
        };
        // The framing bit is the last bit set; padding after it is zero.
        let last = setup
            .iter()
            .rposition(|&byte| byte != 0)
            .ok_or(Error::Truncated)?;
        let framing = last * 8 + 7 - setup[last].leading_zeros() as usize;
        // Bits of the header body after the 7-byte packet header.
        let body = 7 * 8;
        const MODE_BITS: usize = 41;
        // Bit `k` of `from_end` is the block flag of the `k`-th mode before the framing bit.
        let mut found = 0;
        let mut from_end = 0u64;
        while found < 64 {
            let Some(start) = framing.checked_sub(MODE_BITS * (found + 1)) else {
                break;
            };
            if start < body
                || field(start + 1, 16) != 0
                || field(start + 17, 16) != 0
                || field(start + 33, 8) >= 64
            {
                break;
            }
            from_end |= u64::from(bit(start)) << found;
            found += 1;
        }
        for count in (1..=found).rev() {
            let start = framing - MODE_BITS * count;
            if start >= body + 6 && field(start - 6, 6) as usize == count - 1 {
                let mask = u64::MAX >> (64 - count);
                return Ok(VorbisModes {
                    count: count as u8,
                    long: (from_end & mask).reverse_bits() >> (64 - count),
                });
            }
        }
        Err(Error::Invalid("Vorbis setup header mode configuration"))
    }

    /// Number of modes, 1 to 64.
    pub fn count(&self) -> u8 {
        self.count
    }

    /// Whether mode `mode` codes long blocks.
    pub fn is_long(&self, mode: u8) -> bool {
        mode < self.count && self.long >> mode & 1 != 0
    }

    /// The block an audio packet codes: its mode number follows the packet type bit,
    /// and a long block's window flags follow the mode number.
    pub fn block(&self, packet: &[u8]) -> Result<VorbisBlock> {
        let first = *packet.first().ok_or(Error::Truncated)?;
        if first & 1 != 0 {
            return Err(Error::Invalid("Vorbis audio packet type"));
        }
        let bits = u32::from(packet.get(1).copied().unwrap_or(0)) << 8 | u32::from(first);
        // ilog(count - 1) bits of mode number.
        let width = u8::BITS - (self.count - 1).leading_zeros();
        let mode = (bits >> 1 & ((1 << width) - 1)) as u8;
        if mode >= self.count {
            return Err(Error::Invalid("Vorbis mode number"));
        }
        let long = self.is_long(mode);
        let flag_at = 1 + width;
        if long && packet.len() * 8 < (flag_at + 2) as usize {
            return Err(Error::Truncated);
        }
        Ok(VorbisBlock {
            long,
            previous_long: long.then_some(bits >> flag_at & 1 != 0),
        })
    }
}

/// The block size an audio packet codes, from the identification header's block sizes
/// and the setup header's modes.
pub fn vorbis_packet_blocksize(
    modes: &VorbisModes,
    blocksize: [u16; 2],
    packet: &[u8],
) -> Result<u16> {
    Ok(modes.block(packet)?.size(blocksize))
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

    /// A setup header whose body is `prefix` bits, then the mode count and modes, then
    /// the framing bit.
    fn setup_with_modes(prefix: &[(u32, usize)], modes: &[(bool, u8)]) -> Vec<u8> {
        let mut bits = Vec::new();
        let mut put =
            |value: u32, width: usize| bits.extend((0..width).map(|k| value >> k & 1 != 0));
        for &(value, width) in prefix {
            put(value, width);
        }
        put(modes.len() as u32 - 1, 6);
        for &(long, mapping) in modes {
            put(u32::from(long), 1);
            put(0, 32);
            put(u32::from(mapping), 8);
        }
        put(1, 1);
        let mut packet = b"\x05vorbis".to_vec();
        packet.extend(bits.chunks(8).map(|byte| {
            byte.iter()
                .enumerate()
                .fold(0u8, |v, (k, &b)| v | u8::from(b) << k)
        }));
        packet
    }

    #[test]
    fn setup_modes_are_read_back_from_the_framing_bit() {
        let setup = setup_with_modes(&[(0x2a5, 11), (0, 6)], &[(false, 0), (true, 1)]);
        let modes = VorbisModes::from_setup(&setup).unwrap();
        assert_eq!(modes.count(), 2);
        assert!(!modes.is_long(0) && modes.is_long(1) && !modes.is_long(2));
        // Mode 1 (long), previous window short, next long; then mode 0 (short).
        let block = modes.block(&[0b0000_1010]).unwrap();
        assert_eq!(
            block,
            VorbisBlock {
                long: true,
                previous_long: Some(false)
            }
        );
        assert_eq!(block.samples_after(256, [256, 2048]), 576);
        assert_eq!(
            vorbis_packet_blocksize(&modes, [256, 2048], &[0]).unwrap(),
            256
        );
        assert!(modes.block(&[1]).is_err());
        assert!(modes.block(&[]).is_err());

        // Zero bits before the list look like more modes; the count field decides.
        let many: Vec<_> = (0..5).map(|m| (m % 2 == 1, m)).collect();
        let setup = setup_with_modes(&[(0xffff, 16)], &many);
        let modes = VorbisModes::from_setup(&setup).unwrap();
        assert_eq!(modes.count(), 5);
        assert!((0..5).all(|m| modes.is_long(m) == (m % 2 == 1)));
        // Mode 3 of 5 takes three bits after the type bit.
        assert!(modes.block(&[0b0000_0110]).unwrap().long);
        assert!(modes.block(&[0b0000_1110]).is_err());

        let single = setup_with_modes(&[(0x7fff, 15)], &[(true, 0)]);
        assert_eq!(VorbisModes::from_setup(&single).unwrap().count(), 1);
        assert!(VorbisModes::from_setup(b"\x05vorbis").is_err());
        assert!(VorbisModes::from_setup(b"\x05vorbis\x01").is_err());
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
