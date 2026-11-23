//! Recognising an elementary stream from its first bytes.
use triergon_bitstream::crc::crc16_a52;
use triergon_framing::{
    aac::{AacAdtsHeader, inspect_adts_header},
    ac3,
    flac::{FlacBlocking, FlacFrameHeader},
    mpeg_audio::MpegAudioHeader,
};

/// The elementary stream formats this crate reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Native FLAC, read by [`crate::FlacReader`].
    Flac,
    /// MPEG audio, read by [`crate::MpegAudioReader`].
    MpegAudio,
    /// AAC in ADTS, read by [`crate::AdtsReader`].
    Adts,
    /// AC-3 or E-AC-3, read by [`crate::Ac3Reader`].
    Ac3,
}

/// The elementary stream `bytes` holds, after any ID3v2 tag: native FLAC by its marker,
/// MPEG audio, ADTS and AC-3 by two frames that follow each other from the first byte,
/// and FLAC without its marker by a frame header the next header numbers on from.
/// Every format may also start after other bytes, as FFmpeg's probe allows: MPEG audio,
/// ADTS and AC-3 then need three frames that agree on the stream (MPEG audio its
/// version, layer and rate; ADTS its MPEG version and rate; AC-3 its rate), FLAC a
/// first frame its CRC-16 confirms. `None` when `bytes` is too short to tell, including
/// an ID3v2 tag that does not fit in it and a second frame header cut short.
pub fn probe(bytes: &[u8]) -> Option<Kind> {
    let bytes = bytes.get(id3v2_len(bytes).unwrap_or(0)..)?;
    if bytes.starts_with(b"fLaC") {
        return Some(Kind::Flac);
    }
    if consecutive(bytes, 2, adts_frame, |_, _| true) {
        return Some(Kind::Adts);
    }
    if consecutive(bytes, 2, mpeg_frame, MpegAudioHeader::same_stream) {
        return Some(Kind::MpegAudio);
    }
    if consecutive(bytes, 2, ac3_frame, |_, _| true) {
        return Some(Kind::Ac3);
    }
    if flac_frames(bytes, false) {
        return Some(Kind::Flac);
    }
    let later = (1..bytes.len()).filter(|&at| bytes[at] == 0xff);
    for at in later.clone() {
        if consecutive(&bytes[at..], 3, mpeg_frame, MpegAudioHeader::same_stream) {
            return Some(Kind::MpegAudio);
        }
    }
    if later
        .filter(|&at| bytes.get(at + 1).is_some_and(|&b| b & 0xfe == 0xf8))
        .any(|at| flac_frames(&bytes[at..], true))
    {
        return Some(Kind::Flac);
    }
    for at in 1..bytes.len() {
        let rest = &bytes[at..];
        match rest[0] {
            0xff if consecutive(rest, 3, adts_frame, |a, b| {
                a.mpeg2_id() == b.mpeg2_id() && a.frequency_index() == b.frequency_index()
            }) =>
            {
                return Some(Kind::Adts);
            }
            0x0b if consecutive(rest, 3, ac3_frame, PartialEq::eq) => return Some(Kind::Ac3),
            _ => {}
        }
    }
    None
}

/// Whether `bytes` starts with a FLAC frame header (sync, reserved values, CRC-8) and
/// holds a later header with the same blocking that numbers on from it. `confirm` also
/// asks the first frame's CRC-16 to hold, for a header found after other bytes.
fn flac_frames(bytes: &[u8], confirm: bool) -> bool {
    let Ok(first) = FlacFrameHeader::parse(bytes) else {
        return false;
    };
    let next_number = match first.blocking {
        FlacBlocking::Fixed => first.number + 1,
        FlacBlocking::Variable => first.number + u64::from(first.block_size),
    };
    (first.len + 3..bytes.len().saturating_sub(1))
        .filter(|&at| bytes[at] == 0xff && bytes[at + 1] & 0xfe == 0xf8)
        .any(|at| {
            FlacFrameHeader::parse(&bytes[at..]).is_ok_and(|next| {
                next.blocking == first.blocking
                    && next.number == next_number
                    && (!confirm || crc16_a52(0, &bytes[..at]) == 0)
            })
        })
}

/// Whether `count` frames follow each other from the start of `bytes`, each agreeing
/// with the first; the last needs only its header.
fn consecutive<H>(
    bytes: &[u8],
    count: usize,
    frame: impl Fn(&[u8]) -> Option<(H, usize)>,
    agree: impl Fn(&H, &H) -> bool,
) -> bool {
    let Some((first, mut len)) = frame(bytes) else {
        return false;
    };
    let mut at = 0;
    for _ in 1..count {
        at += len;
        match bytes.get(at..).and_then(&frame) {
            Some((header, next)) if agree(&first, &header) => len = next,
            _ => return false,
        }
    }
    true
}

/// Probe by wire extent and sync. The reader also requires a known sample rate and
/// room for audio after the header and CRC.
fn adts_frame(b: &[u8]) -> Option<(AacAdtsHeader, usize)> {
    let header = inspect_adts_header(b).ok()?;
    let len = usize::from(header.frame_length());
    (header.syncword() == 0xfff && header.layer() == 0 && len >= 7).then_some((header, len))
}

fn mpeg_frame(b: &[u8]) -> Option<(MpegAudioHeader, usize)> {
    let header = MpegAudioHeader::from_bytes(b)?;
    Some((header, header.frame_bytes()?))
}

/// An AC-3 or E-AC-3 syncframe's sample rate and length.
fn ac3_frame(b: &[u8]) -> Option<(u32, usize)> {
    let header = ac3::probe_header(b).ok()?;
    (header.frame_bytes > 0).then_some((header.sample_rate_hz, header.frame_bytes))
}

/// Bytes to skip while probing, counting a signalled footer for any version.
/// [`crate::tags::id3v2`] validates version bytes and counts a footer only from v2.4.
fn id3v2_len(bytes: &[u8]) -> Option<usize> {
    let header = bytes.get(..10)?;
    if &header[..3] != b"ID3" || header[6..10].iter().any(|&b| b & 0x80 != 0) {
        return None;
    }
    let size = header[6..10]
        .iter()
        .fold(0usize, |size, &b| (size << 7) | usize::from(b));
    let footer = if header[5] & 0x10 != 0 { 10 } else { 0 };
    Some(10 + size + footer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mp3_frames(count: usize) -> Vec<u8> {
        let mut frame = vec![0u8; 417];
        frame[..4].copy_from_slice(&0xfffb_9064u32.to_be_bytes());
        frame.repeat(count)
    }

    #[test]
    fn streams_are_recognised_by_two_frames() {
        assert_eq!(probe(b"fLaC\0\0\0\x22"), Some(Kind::Flac));
        assert_eq!(probe(&mp3_frames(2)[..421]), Some(Kind::MpegAudio));
        assert_eq!(probe(&mp3_frames(1)), None, "one frame is not enough");
        let mut tagged = b"ID3\x04\0\0\0\0\0\x05".to_vec();
        tagged.extend_from_slice(&[0; 5]);
        tagged.extend_from_slice(&mp3_frames(2));
        assert_eq!(probe(&tagged), Some(Kind::MpegAudio));
        assert_eq!(probe(&tagged[..12]), None);
        // ADTS: 7-byte header, frame length 9.
        let adts = [0xff, 0xf1, 0x50, 0x80, 0x01, 0x3f, 0xfc, 0, 0];
        assert_eq!(probe(&adts.repeat(2)), Some(Kind::Adts));
        assert_eq!(probe(&[0x47; 400]), None);
        // MPEG audio after junk needs three frames.
        let mut junk = vec![0x55; 215];
        junk.extend_from_slice(&mp3_frames(2));
        assert_eq!(probe(&junk), None);
        junk.extend_from_slice(&mp3_frames(1));
        assert_eq!(probe(&junk[..215 + 2 * 417 + 4]), Some(Kind::MpegAudio));
    }

    /// A FLAC frame (fixed blocking, 4096 samples, 44.1 kHz, mono, 16 bits) with
    /// `len` body bytes and a valid CRC-16, or a wrong one.
    fn flac_frame(number: u8, len: usize, crc_ok: bool) -> Vec<u8> {
        let mut frame = vec![0xff, 0xf8, 0xc9, 0x08, number];
        frame.push(triergon_bitstream::crc::crc8(0, &frame));
        frame.extend((0..len).map(|i| (i * 7 + usize::from(number)) as u8 & 0x7f));
        let crc = crc16_a52(0, &frame) ^ u16::from(!crc_ok);
        frame.extend(crc.to_be_bytes());
        frame
    }

    #[test]
    fn flac_without_its_marker_is_recognised_by_frames_that_number_on() {
        let frames = [flac_frame(7, 300, true), flac_frame(8, 300, true)].concat();
        assert_eq!(probe(&frames[..frames.len() - 290]), Some(Kind::Flac));
        assert_eq!(probe(&frames[..306]), None, "one header is not enough");
        let skipped = [flac_frame(7, 300, true), flac_frame(9, 300, true)].concat();
        assert_eq!(probe(&skipped), None, "the second header must number on");
        // After other bytes the first frame's CRC-16 must hold too.
        let mut junk = vec![0x21; 500];
        junk.extend_from_slice(&frames);
        assert_eq!(probe(&junk), Some(Kind::Flac));
        let mut damaged = vec![0x21; 500];
        damaged.extend([flac_frame(7, 300, false), flac_frame(8, 300, true)].concat());
        assert_eq!(probe(&damaged), None);
        assert_eq!(
            probe(&damaged[500..]),
            Some(Kind::Flac),
            "at the start the headers suffice"
        );
    }
}
