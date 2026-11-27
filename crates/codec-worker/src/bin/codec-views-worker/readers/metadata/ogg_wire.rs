//! Compare source-only Ogg header views with the bytes of the original pages.

use triergon::ogg::Track;
use triergon::{CodecId, Error, TrackInfo};

pub(super) fn check(input: &[u8], track: &Track) -> Result<(), String> {
    let codec = track.codec();
    let mut packets = Packets::new(input, track.serial());
    let first = packets
        .next()
        .ok_or("track has no original identification packet")?;
    let comments = expected_comments(codec, &first, &mut packets, track)?;
    let retained = track.comments();
    if (track.is_configured() || retained.is_some()) && retained != comments.as_deref() {
        return Err("Ogg comment view differs from the original logical packet".into());
    }
    Ok(())
}

fn expected_comments(
    codec: CodecId,
    first: &[u8],
    packets: &mut Packets<'_>,
    track: &Track,
) -> Result<Option<Vec<u8>>, String> {
    let comments = match codec {
        CodecId::Opus => {
            let second = packets.next();
            if !track.configuration().is_empty() && track.configuration() != first {
                return Err("Opus configuration differs from the original OpusHead".into());
            }
            if !track.configuration().is_empty() && track.channels() != first.get(9).copied() {
                return Err("Opus channel count differs from the identification packet".into());
            }
            if track.is_configured()
                && !track.configuration().is_empty()
                && second
                    .as_ref()
                    .is_some_and(|packet| !packet.starts_with(b"OpusTags"))
                && track.header_error() != Some(Error::Invalid("Opus comment header"))
            {
                return Err("invalid Opus comment packet lost its typed error".into());
            }
            second.filter(|packet| packet.starts_with(b"OpusTags"))
        }
        CodecId::Vorbis => {
            if track.channels().is_some() && track.channels() != first.get(11).copied() {
                return Err("Vorbis channel count differs from the identification packet".into());
            }
            packets
                .next()
                .filter(|packet| packet.starts_with(b"\x03vorbis"))
        }
        CodecId::Theora => {
            if let Some(rate) = first.get(22..30) {
                let num = u32::from_be_bytes(rate[..4].try_into().map_err(|_| "Theora FRN")?);
                let den = u32::from_be_bytes(rate[4..].try_into().map_err(|_| "Theora FRD")?);
                if (num == 0 || den == 0) && first.get(7..9) == Some(&[3, 2]) {
                    if track.frame_rate().is_some()
                        || track.header_error() != Some(Error::Invalid("Theora frame rate"))
                    {
                        return Err("zero Theora frame rate lost its typed error".into());
                    }
                    return Ok(None);
                }
                if track.frame_rate().is_some() && track.frame_rate() != Some((num, den)) {
                    return Err("Theora frame rate differs from its coded fraction".into());
                }
            }
            packets
                .next()
                .filter(|packet| packet.starts_with(b"\x81theora"))
        }
        CodecId::Flac => {
            if let Some(packed) = first.get(29) {
                let count = ((packed >> 1) & 7) + 1;
                if track.channels().is_some() && track.channels() != Some(count) {
                    return Err("FLAC channel count differs from STREAMINFO".into());
                }
            }
            flac_comments(packets)
        }
        CodecId::Speex => packets.next(),
        _ => None,
    };
    Ok(comments)
}

fn flac_comments(packets: &mut Packets<'_>) -> Option<Vec<u8>> {
    let mut comments = None;
    for mut packet in packets {
        let Some((head, metadata)) = packet.split_first_chunk::<4>() else {
            break;
        };
        let len = u32::from_be_bytes([0, head[1], head[2], head[3]]) as usize;
        if head[0] & 0x7f == 127 || metadata.len() != len {
            break;
        }
        let last = head[0] & 128 != 0;
        if head[0] & 0x7f == 4 {
            comments = Some(packet.split_off(4));
        }
        if last {
            break;
        }
    }
    comments
}

struct Packets<'a> {
    pages: &'a [u8],
    serial: [u8; 4],
    laces: &'a [u8],
    body: &'a [u8],
    expected: u32,
}

impl<'a> Packets<'a> {
    fn new(pages: &'a [u8], serial: u32) -> Self {
        Self {
            pages,
            serial: serial.to_le_bytes(),
            laces: &[],
            body: &[],
            expected: 0,
        }
    }

    fn next_page(&mut self, continued: bool) -> Option<()> {
        loop {
            let (header, tail) = self.pages.split_first_chunk::<27>()?;
            if !header.starts_with(b"OggS\0") {
                return None;
            }
            let (laces, tail) = tail.split_at_checked(usize::from(header[26]))?;
            let body_len = laces.iter().map(|&lace| usize::from(lace)).sum();
            let (body, pages) = tail.split_at_checked(body_len)?;
            self.pages = pages;
            if header[14..18] != self.serial {
                continue;
            }
            let sequence = u32::from_le_bytes(header[18..22].try_into().ok()?);
            if sequence != self.expected || (header[5] & 1 != 0) != continued {
                return None;
            }
            self.expected = sequence.wrapping_add(1);
            self.laces = laces;
            self.body = body;
            return Some(());
        }
    }
}

impl Iterator for Packets<'_> {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut packet = Vec::new();
        loop {
            if self.laces.is_empty() {
                self.next_page(!packet.is_empty())?;
                continue;
            }
            let (&length, laces) = self.laces.split_first()?;
            let (bytes, body) = self.body.split_at_checked(usize::from(length))?;
            self.laces = laces;
            self.body = body;
            packet.extend(bytes);
            if length != 255 {
                return Some(packet);
            }
        }
    }
}
