//! Recognising an elementary stream from its first bytes.
use triergon_framing::{MpegAudioHeader, probe_ac3_header};

/// The elementary stream formats this crate reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Flac,
    MpegAudio,
    Adts,
    Ac3,
}

/// The elementary stream `bytes` starts, after any ID3v2 tag. MPEG audio, ADTS and AC-3
/// are recognised only once `bytes` holds the start of a second frame that continues
/// the first, and an ID3v2 tag must fit in `bytes` whole; `None` otherwise.
pub fn probe(bytes: &[u8]) -> Option<Kind> {
    let start = id3v2_len(bytes).unwrap_or(0);
    let bytes = bytes.get(start..)?;
    if bytes.starts_with(b"fLaC") {
        return Some(Kind::Flac);
    }
    let confirmed = |len: Option<usize>, next: &dyn Fn(&[u8]) -> bool| {
        len.is_some_and(|len| len > 0 && bytes.get(len..).is_some_and(next))
    };
    // ADTS sets the two layer bits MPEG audio reserves.
    let adts = |b: &[u8]| b.len() >= 7 && b[0] == 0xff && b[1] & 0xf6 == 0xf0;
    if adts(bytes) {
        let len = (usize::from(bytes[3] & 3) << 11)
            | (usize::from(bytes[4]) << 3)
            | usize::from(bytes[5] >> 5);
        if confirmed(Some(len), &adts) {
            return Some(Kind::Adts);
        }
        return None;
    }
    let mpeg = |b: &[u8]| MpegAudioHeader::from_bytes(b);
    if let Some(header) = mpeg(bytes) {
        let next = |b: &[u8]| mpeg(b).is_some_and(|h| h.same_stream(&header));
        if confirmed(header.frame_bytes(), &next) {
            return Some(Kind::MpegAudio);
        }
        return None;
    }
    let ac3 = |b: &[u8]| probe_ac3_header(b).ok();
    if let Some(header) = ac3(bytes)
        && confirmed(Some(header.frame_bytes), &|b: &[u8]| ac3(b).is_some())
    {
        return Some(Kind::Ac3);
    }
    None
}

/// Bytes of an ID3v2 tag at the start of `bytes`, footer included.
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
    }
}
