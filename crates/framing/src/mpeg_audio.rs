//! MPEG audio frame headers and the VBR information frames encoders write first.
//!
//! [`MpegAudioHeader`] reads the 32-bit header of ISO/IEC 11172-3 section 2.4.2.3 and its
//! ISO/IEC 13818-3 low sampling frequency extension, including the unofficial MPEG-2.5
//! rates. [`MpegAudioInfoFrame`] reads a Xing or Info frame (with its LAME tag) or a VBRI
//! frame: a valid frame whose payload describes the stream instead of carrying audio.
use triergon_core::{Error, Result};

/// The MPEG audio version a header names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MpegVersion {
    /// ISO/IEC 11172-3.
    Mpeg1,
    /// ISO/IEC 13818-3 low sampling frequencies.
    Mpeg2,
    /// The unofficial extension to 8, 11.025 and 12 kHz.
    Mpeg25,
}

/// Channel mode of a frame header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MpegChannelMode {
    Stereo,
    JointStereo,
    DualChannel,
    Mono,
}

/// A decoded MPEG audio frame header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MpegAudioHeader {
    pub version: MpegVersion,
    /// 1, 2 or 3.
    pub layer: u8,
    /// A 16-bit CRC follows the header.
    pub crc: bool,
    /// Kilobits per second; 0 for a free-format frame.
    pub bitrate_kbps: u16,
    pub sample_rate: u32,
    pub padding: bool,
    pub channel_mode: MpegChannelMode,
    pub mode_extension: u8,
    pub copyright: bool,
    pub original: bool,
    pub emphasis: u8,
}

const BITRATES: [[[u16; 15]; 3]; 2] = [
    // MPEG-1 Layer I, II, III.
    [
        [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448],
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384],
        [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],
    ],
    // MPEG-2 and 2.5 Layer I, II and III.
    [
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
    ],
];

impl MpegAudioHeader {
    /// Parse a big-endian header word. `None` for a word that is not a valid header:
    /// no 11-bit sync, a reserved version, layer, bit rate, sample rate or emphasis.
    pub fn parse(word: u32) -> Option<MpegAudioHeader> {
        if word >> 21 != 0x7ff {
            return None;
        }
        let version = match (word >> 19) & 3 {
            0 => MpegVersion::Mpeg25,
            2 => MpegVersion::Mpeg2,
            3 => MpegVersion::Mpeg1,
            _ => return None,
        };
        let layer = match (word >> 17) & 3 {
            1 => 3,
            2 => 2,
            3 => 1,
            _ => return None,
        };
        let bitrate_index = ((word >> 12) & 15) as usize;
        let rate_index = ((word >> 10) & 3) as usize;
        if bitrate_index == 15 || rate_index == 3 || word & 3 == 2 {
            return None;
        }
        let base = [44_100, 48_000, 32_000][rate_index];
        let sample_rate = match version {
            MpegVersion::Mpeg1 => base,
            MpegVersion::Mpeg2 => base / 2,
            MpegVersion::Mpeg25 => base / 4,
        };
        let table = usize::from(version != MpegVersion::Mpeg1);
        Some(MpegAudioHeader {
            version,
            layer,
            crc: word & (1 << 16) == 0,
            bitrate_kbps: BITRATES[table][usize::from(layer - 1)][bitrate_index],
            sample_rate,
            padding: word & (1 << 9) != 0,
            channel_mode: match (word >> 6) & 3 {
                0 => MpegChannelMode::Stereo,
                1 => MpegChannelMode::JointStereo,
                2 => MpegChannelMode::DualChannel,
                _ => MpegChannelMode::Mono,
            },
            mode_extension: ((word >> 4) & 3) as u8,
            copyright: word & 8 != 0,
            original: word & 4 != 0,
            emphasis: (word & 3) as u8,
        })
    }

    /// Parse the header at the start of `bytes`.
    pub fn from_bytes(bytes: &[u8]) -> Option<MpegAudioHeader> {
        let word = bytes.get(..4)?;
        Self::parse(u32::from_be_bytes([word[0], word[1], word[2], word[3]]))
    }

    /// Output channels.
    pub fn channels(&self) -> u8 {
        if self.channel_mode == MpegChannelMode::Mono {
            1
        } else {
            2
        }
    }

    /// Samples per channel a frame decodes to.
    pub fn samples_per_frame(&self) -> u32 {
        match (self.layer, self.version) {
            (1, _) => 384,
            (2, _) | (3, MpegVersion::Mpeg1) => 1152,
            _ => 576,
        }
    }

    /// Frame length in bytes, header included; `None` for free format.
    pub fn frame_bytes(&self) -> Option<usize> {
        if self.bitrate_kbps == 0 {
            return None;
        }
        let bitrate = u64::from(self.bitrate_kbps) * 1000;
        let rate = u64::from(self.sample_rate);
        let padding = u64::from(self.padding);
        let bytes = if self.layer == 1 {
            (12 * bitrate / rate + padding) * 4
        } else {
            let slots = u64::from(self.samples_per_frame()) / 8;
            slots * bitrate / rate + padding
        };
        usize::try_from(bytes).ok()
    }

    /// Bytes of Layer III side information after the header and its CRC.
    pub fn side_info_bytes(&self) -> usize {
        match (self.version == MpegVersion::Mpeg1, self.channels() == 1) {
            (true, false) => 32,
            (true, true) => 17,
            (false, false) => 17,
            (false, true) => 9,
        }
    }

    /// True when `other` may continue the same stream: same version, layer and sample
    /// rate (FFmpeg's parser and demuxer hold these fixed between frames).
    pub fn same_stream(&self, other: &MpegAudioHeader) -> bool {
        self.version == other.version
            && self.layer == other.layer
            && self.sample_rate == other.sample_rate
    }
}

/// Fields of the LAME extension of a Xing or Info frame that change decoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LameTag {
    /// The nine-byte encoder string, such as `LAME3.100`.
    pub encoder: [u8; 9],
    /// Samples the encoder added before the first input sample.
    pub encoder_delay: u16,
    /// Samples the encoder added after the last input sample.
    pub encoder_padding: u16,
}

/// Which information frame a stream starts with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MpegInfoKind {
    /// `Xing`: variable bit rate.
    Xing,
    /// `Info`: constant bit rate, written by LAME.
    Info,
    /// `VBRI`: the Fraunhofer encoder's header.
    Vbri,
}

/// The stream description of a Xing, Info or VBRI frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MpegInfoFrame {
    pub kind: MpegInfoKind,
    /// Audio frames in the stream, not counting this one.
    pub frames: Option<u32>,
    /// Bytes of the stream, this frame included for Xing and Info.
    pub bytes: Option<u32>,
    /// The 100-entry seek table of a Xing or Info frame.
    pub toc: Option<[u8; 100]>,
    pub lame: Option<LameTag>,
}

/// The information frame carried by `frame`, one whole frame with header `header`.
pub fn mpeg_info_frame(frame: &[u8], header: &MpegAudioHeader) -> Option<MpegInfoFrame> {
    if header.layer != 3 {
        return None;
    }
    let at = 4 + if header.crc { 2 } else { 0 } + header.side_info_bytes();
    let be32 = |at: usize| {
        frame
            .get(at..at + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    let tag = frame.get(at..at + 4)?;
    if tag == b"Xing" || tag == b"Info" {
        let flags = be32(at + 4)?;
        let mut next = at + 8;
        let mut field = |present: bool| -> Option<Option<u32>> {
            if !present {
                return Some(None);
            }
            let value = be32(next)?;
            next += 4;
            Some(Some(value))
        };
        let frames = field(flags & 1 != 0)?;
        let bytes = field(flags & 2 != 0)?;
        let toc = if flags & 4 != 0 {
            let table: [u8; 100] = frame.get(next..next + 100)?.try_into().ok()?;
            next += 100;
            Some(table)
        } else {
            None
        };
        if flags & 8 != 0 {
            next += 4;
        }
        let lame = frame.get(next..next + 24).and_then(|tag| {
            let encoder: [u8; 9] = tag[..9].try_into().ok()?;
            if !encoder.iter().all(|&b| b.is_ascii_graphic() || b == b' ' || b == 0) {
                return None;
            }
            let delay = (u16::from(tag[21]) << 4) | u16::from(tag[22] >> 4);
            let padding = (u16::from(tag[22] & 15) << 8) | u16::from(tag[23]);
            Some(LameTag {
                encoder,
                encoder_delay: delay,
                encoder_padding: padding,
            })
        });
        return Some(MpegInfoFrame {
            kind: if tag == b"Xing" {
                MpegInfoKind::Xing
            } else {
                MpegInfoKind::Info
            },
            frames,
            bytes,
            toc,
            lame,
        });
    }
    // VBRI sits 32 bytes after the header whatever the mode.
    if frame.get(36..40)? == b"VBRI" {
        let version = u16::from_be_bytes([*frame.get(40)?, *frame.get(41)?]);
        if version != 1 {
            return None;
        }
        return Some(MpegInfoFrame {
            kind: MpegInfoKind::Vbri,
            bytes: be32(46),
            frames: be32(50),
            toc: None,
            lame: None,
        });
    }
    None
}

/// Check the CRC of a frame whose header says it carries one (ISO/IEC 11172-3 2.4.3.1):
/// the CRC covers the last two header bytes and the first `protected` bytes after it.
pub fn mpeg_audio_crc_ok(frame: &[u8], protected: usize) -> Result<bool> {
    let stored = frame.get(4..6).ok_or(Error::Truncated)?;
    let body = frame.get(6..6 + protected).ok_or(Error::Truncated)?;
    let crc = triergon_bitstream::crc::crc16_a52(0xffff, &frame[2..4]);
    let crc = triergon_bitstream::crc::crc16_a52(crc, body);
    Ok(crc == u16::from_be_bytes([stored[0], stored[1]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_fields_and_frame_lengths() {
        // MPEG-1 Layer III, 128 kbit/s, 44.1 kHz, joint stereo, no CRC.
        let h = MpegAudioHeader::parse(0xfffb_9064).unwrap();
        assert_eq!((h.version, h.layer, h.bitrate_kbps, h.sample_rate), (MpegVersion::Mpeg1, 3, 128, 44100));
        assert_eq!(h.channel_mode, MpegChannelMode::JointStereo);
        assert_eq!(h.frame_bytes(), Some(417));
        assert_eq!(h.samples_per_frame(), 1152);
        assert_eq!(h.side_info_bytes(), 32);
        // MPEG-2 Layer III, 64 kbit/s, 22.05 kHz, mono, padded.
        let h = MpegAudioHeader::parse(0xfff3_82c4).unwrap();
        assert_eq!((h.version, h.sample_rate, h.channels()), (MpegVersion::Mpeg2, 22050, 1));
        assert_eq!(h.frame_bytes(), Some(209));
        assert_eq!(h.samples_per_frame(), 576);
        // MPEG-1 Layer I, 384 kbit/s, 48 kHz.
        let h = MpegAudioHeader::parse(0xffff_c400).unwrap();
        assert_eq!((h.layer, h.frame_bytes()), (1, Some(384)));
        assert!(MpegAudioHeader::parse(0xfffb_f064).is_none()); // bit rate index 15
        assert!(MpegAudioHeader::parse(0xffeb_9064).is_none()); // reserved version
    }

    #[test]
    fn xing_frame_with_lame_tag() {
        let header = MpegAudioHeader::parse(0xfffb_9064).unwrap();
        let mut frame = vec![0u8; 417];
        frame[..4].copy_from_slice(&0xfffb_9064u32.to_be_bytes());
        let at = 36;
        frame[at..at + 4].copy_from_slice(b"Info");
        frame[at + 4..at + 8].copy_from_slice(&15u32.to_be_bytes());
        frame[at + 8..at + 12].copy_from_slice(&100u32.to_be_bytes());
        frame[at + 12..at + 16].copy_from_slice(&41700u32.to_be_bytes());
        let lame = at + 16 + 100 + 4;
        frame[lame..lame + 9].copy_from_slice(b"LAME3.100");
        frame[lame + 21..lame + 24].copy_from_slice(&[0x24, 0x01, 0x23]);
        let info = mpeg_info_frame(&frame, &header).unwrap();
        assert_eq!(info.kind, MpegInfoKind::Info);
        assert_eq!((info.frames, info.bytes), (Some(100), Some(41700)));
        let tag = info.lame.unwrap();
        assert_eq!((tag.encoder_delay, tag.encoder_padding), (576, 291));
        assert!(mpeg_info_frame(&frame[..30], &header).is_none());
    }
}
