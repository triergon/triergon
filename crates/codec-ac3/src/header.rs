//! Syncframe header probing (syncinfo plus the leading bsi fields) used for framing.

use crate::error::{DecodeError, Feature};

/// Nominal bit rates in kbit/s per frmsizecod/2 (A/52:2018 Table 5.18).
pub(crate) const BITRATE_KBPS: [u16; 19] = [
    32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640,
];

/// 44.1 kHz words/syncframe for even frmsizecod (A/52:2018 Table 5.18); odd codes add one word.
const WORDS_44K: [u16; 19] = [
    69, 87, 104, 121, 139, 174, 208, 243, 278, 348, 417, 487, 557, 696, 835, 975, 1114, 1253, 1393,
];

/// Sample rates for fscod (A/52:2018 Table 5.6 / Annex E Table E2.2).
pub(crate) const SAMPLE_RATES: [u32; 3] = [48000, 44100, 32000];
/// Reduced sample rates for fscod2 (Annex E Table E2.3).
pub(crate) const SAMPLE_RATES_HALF: [u32; 3] = [24000, 22050, 16000];

/// AC-3 syncframe size in 16-bit words (A/52:2018 Table 5.18).
pub(crate) fn ac3_frame_words(fscod: u8, frmsizecod: u8) -> Option<usize> {
    let idx = (frmsizecod >> 1) as usize;
    if fscod > 2 || idx >= BITRATE_KBPS.len() {
        return None;
    }
    let br = BITRATE_KBPS[idx] as usize;
    Some(match fscod {
        0 => 2 * br,
        1 => WORDS_44K[idx] as usize + (frmsizecod & 1) as usize,
        _ => 3 * br,
    })
}

/// Sample-rate shift of the AC-3 syntax for a bsid: 0 for bsid 0..=8, 1 for bsid 9
/// (half rate), 2 for bsid 10 (quarter rate). See [`Syntax::Ac3`].
pub(crate) fn ac3_rate_shift(bsid: u8) -> u32 {
    bsid.saturating_sub(8) as u32
}

/// Which syntax a syncframe uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Syntax {
    /// A/52 main body (bsid 0..=8; bsid 6 = Annex D alternate bsi), and bsid 9/10: the
    /// reduced-sample-rate AC-3 variants (half/quarter rate; RealAudio "dnet"). These are not in
    /// A/52:2018, whose Annex E 2.3.1.6 says to mute on them, but FFmpeg 9.0.2 decodes them with
    /// the bsid 8 syntax, the Table 5.18 frame size in bytes and the sample rate divided by 2/4
    /// (black-box, research/d4/ac3/STATUS.md).
    Ac3,
    /// A/52 Annex E (bsid 11..=16).
    Eac3,
}

/// E-AC-3 stream type (Annex E Table E2.1); AC-3 frames report `Independent`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamType {
    Independent,
    Dependent,
    /// Independent substream converted from AC-3 (strmtyp 2).
    Converted,
}

/// Leading header fields of one syncframe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub syntax: Syntax,
    pub bsid: u8,
    /// Total syncframe length in bytes.
    pub frame_bytes: usize,
    pub sample_rate: u32,
    pub fscod: u8,
    /// Audio blocks (of 256 samples) per syncframe.
    pub num_blocks: usize,
    pub acmod: u8,
    pub lfeon: bool,
    pub stream_type: StreamType,
    pub substream_id: u8,
    /// AC-3 only: frmsizecod.
    pub frmsizecod: u8,
}

impl FrameHeader {
    /// Samples per channel produced by this syncframe.
    pub fn samples(&self) -> usize {
        self.num_blocks * 256
    }
}

/// Parse the header of the syncframe at the start of `data` (at least 8 bytes needed).
pub fn parse_header(data: &[u8]) -> Result<FrameHeader, DecodeError> {
    if data.is_empty() {
        return Err(DecodeError::Empty);
    }
    if data.len() < 8 {
        return Err(DecodeError::Truncated {
            needed: 8,
            available: data.len(),
        });
    }
    if data[0] != 0x0B || data[1] != 0x77 {
        return Err(DecodeError::NoSync);
    }
    let bsid = data[5] >> 3;
    if bsid <= 10 {
        let fscod = data[4] >> 6;
        let frmsizecod = data[4] & 0x3f;
        if fscod == 3 {
            return Err(DecodeError::InvalidData("reserved fscod"));
        }
        let words =
            ac3_frame_words(fscod, frmsizecod).ok_or(DecodeError::InvalidData("frmsizecod"))?;
        let acmod = data[6] >> 5;
        // lfeon follows optional cmixlev/surmixlev/dsurmod fields (2 bits each).
        let mut bit = 3u32; // bits of data[6] consumed after acmod
        if (acmod & 1) != 0 && acmod != 1 {
            bit += 2;
        }
        if acmod & 4 != 0 {
            bit += 2;
        }
        if acmod == 2 {
            bit += 2;
        }
        let w = u16::from_be_bytes([data[6], data[7]]);
        let lfeon = (w >> (15 - bit)) & 1 != 0;
        return Ok(FrameHeader {
            syntax: Syntax::Ac3,
            bsid,
            frame_bytes: words * 2,
            sample_rate: SAMPLE_RATES[fscod as usize] >> ac3_rate_shift(bsid),
            fscod,
            num_blocks: 6,
            acmod,
            lfeon,
            stream_type: StreamType::Independent,
            substream_id: 0,
            frmsizecod,
        });
    }
    if bsid > 16 {
        return Err(DecodeError::Unsupported(Feature::BitstreamId(bsid)));
    }
    let strmtyp = data[2] >> 6;
    let substream_id = (data[2] >> 3) & 7;
    let frmsiz = (((data[2] & 7) as usize) << 8) | data[3] as usize;
    let fscod = data[4] >> 6;
    let code2 = (data[4] >> 4) & 3;
    let (sample_rate, num_blocks) = if fscod == 3 {
        if code2 == 3 {
            return Err(DecodeError::Unsupported(Feature::ReservedSampleRate));
        }
        (SAMPLE_RATES_HALF[code2 as usize], 6)
    } else {
        (SAMPLE_RATES[fscod as usize], [1, 2, 3, 6][code2 as usize])
    };
    let stream_type = match strmtyp {
        0 => StreamType::Independent,
        1 => StreamType::Dependent,
        2 => StreamType::Converted,
        _ => return Err(DecodeError::Unsupported(Feature::ReservedStreamType)),
    };
    let acmod = (data[4] >> 1) & 7;
    let lfeon = data[4] & 1 != 0;
    let frame_bytes = (frmsiz + 1) * 2;
    if frame_bytes < 8 {
        return Err(DecodeError::InvalidData("frmsiz too small"));
    }
    Ok(FrameHeader {
        syntax: Syntax::Eac3,
        bsid,
        frame_bytes,
        sample_rate,
        fscod,
        num_blocks,
        acmod,
        lfeon,
        stream_type,
        substream_id,
        frmsizecod: 0,
    })
}

/// Split the leading access unit off `data`: one AC-3 syncframe, or one E-AC-3
/// independent syncframe together with all following dependent/other-program syncframes
/// up to the next independent substream 0 (the grouping FFmpeg's parser uses).
/// Returns the AU length in bytes (may exceed `data.len()` for a truncated final frame).
pub fn access_unit_len(data: &[u8]) -> Result<usize, DecodeError> {
    let first = parse_header(data)?;
    let mut len = first.frame_bytes;
    // An AC-3 syncframe acts as independent substream 0 and may be followed by E-AC-3
    // dependent substreams (A/52:2018 Annex E 2.3.1.2).
    while len < data.len() {
        let Ok(h) = parse_header(&data[len..]) else {
            break;
        };
        if h.syntax == Syntax::Eac3 && h.stream_type != StreamType::Dependent && h.substream_id == 0
        {
            break;
        }
        if h.syntax == Syntax::Ac3 {
            break;
        }
        len += h.frame_bytes;
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_size_table_matches_spot_values() {
        // A/52:2018 Table 5.18 rows.
        assert_eq!(ac3_frame_words(0, 0), Some(64));
        assert_eq!(ac3_frame_words(1, 0), Some(69));
        assert_eq!(ac3_frame_words(1, 1), Some(70));
        assert_eq!(ac3_frame_words(2, 1), Some(96));
        assert_eq!(ac3_frame_words(1, 30), Some(975));
        assert_eq!(ac3_frame_words(1, 31), Some(976));
        assert_eq!(ac3_frame_words(0, 37), Some(1280));
        assert_eq!(ac3_frame_words(1, 37), Some(1394));
        assert_eq!(ac3_frame_words(2, 36), Some(1920));
        assert_eq!(ac3_frame_words(0, 38), None);
        assert_eq!(ac3_frame_words(3, 0), None);
    }

    #[test]
    fn header_rejects_bad_sync_and_short_input() {
        assert_eq!(parse_header(&[]), Err(DecodeError::Empty));
        assert!(matches!(
            parse_header(&[0x0b, 0x77, 0]),
            Err(DecodeError::Truncated { .. })
        ));
        assert_eq!(
            parse_header(&[0x0b, 0x78, 0, 0, 0, 0, 0, 0]),
            Err(DecodeError::NoSync)
        );
        // fscod = 3 is reserved for AC-3.
        assert!(matches!(
            parse_header(&[0x0b, 0x77, 0, 0, 0xc0, 0x40, 0, 0]),
            Err(DecodeError::InvalidData(_))
        ));
        // frmsizecod 38 does not exist.
        assert!(matches!(
            parse_header(&[0x0b, 0x77, 0, 0, 38, 0x40, 0, 0]),
            Err(DecodeError::InvalidData(_))
        ));
        // bsid 17.
        assert_eq!(
            parse_header(&[0x0b, 0x77, 0, 0, 0, 17 << 3, 0, 0]),
            Err(DecodeError::Unsupported(Feature::BitstreamId(17)))
        );
    }

    #[test]
    fn ac3_header_lfe_position_depends_on_acmod() {
        // acmod 7 (3/2): cmixlev + surmixlev precede lfeon.
        // byte6 = acmod(3)=111, cmix(2)=00, surmix(2)=00, lfeon=1 -> 1110_0001
        let h = parse_header(&[0x0b, 0x77, 0, 0, 0x1c, 0x40, 0xe1, 0]).unwrap();
        assert_eq!((h.acmod, h.lfeon, h.frame_bytes), (7, true, 1536));
        // acmod 2: dsurmod precedes lfeon. byte6 = 010 00 1xx
        let h = parse_header(&[0x0b, 0x77, 0, 0, 0x1c, 0x40, 0x44, 0]).unwrap();
        assert_eq!((h.acmod, h.lfeon), (2, true));
    }
}
