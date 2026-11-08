//! Opus packets (RFC 6716 section 3): the TOC byte, frame packing and padding, and the
//! self-delimiting framing of multistream packets (RFC 6716 Appendix B).

/// Coding mode of a frame (RFC 6716 section 3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Linear prediction only, up to wideband.
    Silk,
    /// SILK below 8 kHz and CELT above.
    Hybrid,
    /// MDCT only.
    Celt,
}

/// Audio bandwidth of a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Bandwidth {
    /// 4 kHz.
    Narrow,
    /// 6 kHz.
    Medium,
    /// 8 kHz.
    Wide,
    /// 12 kHz.
    SuperWide,
    /// 20 kHz.
    Full,
}

impl Bandwidth {
    /// One past the last CELT band coded at this bandwidth.
    pub(crate) fn celt_end_band(self) -> usize {
        match self {
            Bandwidth::Narrow => 13,
            Bandwidth::Medium | Bandwidth::Wide => 17,
            Bandwidth::SuperWide => 19,
            Bandwidth::Full => 21,
        }
    }

    /// The SILK internal sample rate (hybrid frames code SILK at wideband).
    pub(crate) fn silk_rate(self) -> u32 {
        match self {
            Bandwidth::Narrow => 8000,
            Bandwidth::Medium => 12000,
            _ => 16000,
        }
    }
}

/// Duration of one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum FrameDuration {
    Ms2_5,
    Ms5,
    Ms10,
    Ms20,
    Ms40,
    Ms60,
}

impl FrameDuration {
    /// Samples at 48 kHz.
    pub(crate) fn samples(self) -> usize {
        match self {
            FrameDuration::Ms2_5 => 120,
            FrameDuration::Ms5 => 240,
            FrameDuration::Ms10 => 480,
            FrameDuration::Ms20 => 960,
            FrameDuration::Ms40 => 1920,
            FrameDuration::Ms60 => 2880,
        }
    }

    /// The CELT frame size `120 << lm`; `None` beyond 20 ms.
    pub(crate) fn celt_lm(self) -> Option<usize> {
        match self {
            FrameDuration::Ms2_5 => Some(0),
            FrameDuration::Ms5 => Some(1),
            FrameDuration::Ms10 => Some(2),
            FrameDuration::Ms20 => Some(3),
            FrameDuration::Ms40 | FrameDuration::Ms60 => None,
        }
    }
}

/// The table of contents byte (RFC 6716 section 3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Toc {
    pub(crate) mode: Mode,
    pub(crate) bandwidth: Bandwidth,
    pub(crate) duration: FrameDuration,
    pub(crate) stereo: bool,
}

impl Toc {
    pub(crate) fn parse(byte: u8) -> Toc {
        use Bandwidth::{Full, Medium, Narrow, SuperWide, Wide};
        use FrameDuration::{Ms2_5, Ms5, Ms10, Ms20, Ms40, Ms60};
        let config = byte >> 3;
        let (mode, bandwidth, duration) = match config {
            0..=11 => (
                Mode::Silk,
                [Narrow, Medium, Wide][usize::from(config >> 2)],
                [Ms10, Ms20, Ms40, Ms60][usize::from(config & 3)],
            ),
            12..=15 => (
                Mode::Hybrid,
                [SuperWide, Full][usize::from((config >> 1) & 1)],
                [Ms10, Ms20][usize::from(config & 1)],
            ),
            _ => (
                Mode::Celt,
                [Narrow, Wide, SuperWide, Full][usize::from((config >> 2) & 3)],
                [Ms2_5, Ms5, Ms10, Ms20][usize::from(config & 3)],
            ),
        };
        Toc {
            mode,
            bandwidth,
            duration,
            stereo: byte & 4 != 0,
        }
    }

    /// Coded channels.
    pub(crate) fn channels(self) -> usize {
        1 + usize::from(self.stereo)
    }
}

/// Why a packet is malformed (RFC 6716 section 3.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PacketError {
    /// R1: no TOC byte.
    Empty,
    /// R2: an implicit frame length above 1275 bytes.
    FrameTooLong,
    /// R3: code 1 with an odd payload.
    OddPayload,
    /// R4/R7: a frame length runs past the packet.
    Truncated,
    /// R5: no frames, or more than 120 ms.
    FrameCount,
    /// R6: a constant-bitrate payload not divisible by the frame count.
    UnevenFrames,
}

impl PacketError {
    pub(crate) fn message(self) -> &'static str {
        match self {
            PacketError::Empty => "empty packet",
            PacketError::FrameTooLong => "frame longer than 1275 bytes",
            PacketError::OddPayload => "two equal frames of odd total length",
            PacketError::Truncated => "frame lengths past the packet end",
            PacketError::FrameCount => "frame count",
            PacketError::UnevenFrames => "constant-bitrate frames of unequal length",
        }
    }
}

/// Most frames in a packet: 120 ms of 2.5 ms frames.
pub(crate) const MAX_FRAMES: usize = 48;
/// Longest frame.
const MAX_FRAME_BYTES: usize = 1275;

/// A parsed packet: its TOC and frames, which borrow the packet bytes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Packet<'a> {
    pub(crate) toc: Toc,
    frames: [&'a [u8]; MAX_FRAMES],
    count: usize,
    /// Bytes of the input the packet occupies (all of it unless self-delimited).
    pub(crate) len: usize,
}

impl<'a> Packet<'a> {
    pub(crate) fn frames(&self) -> &[&'a [u8]] {
        &self.frames[..self.count]
    }

    /// Parse `data`. A self-delimited packet (all but the last of a multistream packet)
    /// codes the length of its last frame and may be followed by more data.
    pub(crate) fn parse(data: &'a [u8], self_delimited: bool) -> Result<Packet<'a>, PacketError> {
        let (&toc_byte, mut rest) = data.split_first().ok_or(PacketError::Empty)?;
        let toc = Toc::parse(toc_byte);
        let mut sizes = [0usize; MAX_FRAMES];
        let count;
        // Code 1 and CBR code 3: all frames share one length.
        let mut cbr = false;
        let mut last_size = rest.len();
        let mut padding = 0usize;
        match toc_byte & 3 {
            0 => count = 1,
            1 => {
                count = 2;
                cbr = true;
                if !self_delimited {
                    if rest.len() % 2 != 0 {
                        return Err(PacketError::OddPayload);
                    }
                    last_size = rest.len() / 2;
                    sizes[0] = last_size;
                }
            }
            2 => {
                count = 2;
                sizes[0] = read_size(&mut rest)?;
                if sizes[0] > rest.len() {
                    return Err(PacketError::Truncated);
                }
                last_size = rest.len() - sizes[0];
            }
            _ => {
                let (&frames, tail) = rest.split_first().ok_or(PacketError::FrameCount)?;
                rest = tail;
                count = usize::from(frames & 0x3f);
                if count == 0 || count * toc.duration.samples() > 5760 {
                    return Err(PacketError::FrameCount);
                }
                // Padding: each 255 adds 254 bytes and continues.
                if frames & 0x40 != 0 {
                    loop {
                        let (&p, tail) = rest.split_first().ok_or(PacketError::Truncated)?;
                        rest = tail;
                        padding += if p == 255 { 254 } else { usize::from(p) };
                        if p != 255 {
                            break;
                        }
                    }
                }
                if frames & 0x80 != 0 {
                    // Variable bitrate: lengths of all but the last frame.
                    let mut available = rest
                        .len()
                        .checked_sub(padding)
                        .ok_or(PacketError::Truncated)?;
                    let mut sum = 0usize;
                    for size in &mut sizes[..count - 1] {
                        let before = rest.len();
                        *size = read_size(&mut rest)?;
                        available = available
                            .checked_sub(before - rest.len())
                            .ok_or(PacketError::Truncated)?;
                        if *size > available {
                            return Err(PacketError::Truncated);
                        }
                        sum += *size;
                    }
                    last_size = available.checked_sub(sum).ok_or(PacketError::Truncated)?;
                } else {
                    cbr = true;
                    let payload = rest
                        .len()
                        .checked_sub(padding)
                        .ok_or(PacketError::Truncated)?;
                    if !self_delimited {
                        last_size = payload / count;
                        if last_size * count != payload {
                            return Err(PacketError::UnevenFrames);
                        }
                        sizes[..count - 1].fill(last_size);
                    } else {
                        last_size = payload;
                    }
                }
            }
        }
        // Frame data follows the headers; the padding ends the packet.
        let start = data.len() - rest.len();
        if self_delimited {
            let before = rest.len();
            let size = read_size(&mut rest)?;
            let bytes = before - rest.len();
            let available = rest
                .len()
                .checked_sub(padding)
                .ok_or(PacketError::Truncated)?;
            if size > available {
                return Err(PacketError::Truncated);
            }
            if cbr {
                if size * count > available {
                    return Err(PacketError::Truncated);
                }
                sizes[..count].fill(size);
            } else {
                // The length bytes come out of the last frame's share.
                if bytes + size > last_size {
                    return Err(PacketError::Truncated);
                }
                sizes[count - 1] = size;
            }
        } else {
            if last_size > MAX_FRAME_BYTES {
                return Err(PacketError::FrameTooLong);
            }
            sizes[count - 1] = last_size;
        }

        let mut frames = [&data[..0]; MAX_FRAMES];
        let mut at = if self_delimited {
            data.len() - rest.len()
        } else {
            start
        };
        for (frame, &size) in frames.iter_mut().zip(&sizes[..count]) {
            *frame = &data[at..at + size];
            at += size;
        }
        // A self-delimited packet ends after its frames and their padding; otherwise it
        // takes all of `data`.
        let len = if self_delimited {
            at + padding
        } else {
            data.len()
        };
        Ok(Packet {
            toc,
            frames,
            count,
            len,
        })
    }
}

/// A frame length: one byte below 252, else two (RFC 6716 section 3.2.1).
fn read_size(rest: &mut &[u8]) -> Result<usize, PacketError> {
    match **rest {
        [first, ..] if first < 252 => {
            *rest = &rest[1..];
            Ok(usize::from(first))
        }
        [first, second, ..] => {
            *rest = &rest[2..];
            Ok(4 * usize::from(second) + usize::from(first))
        }
        _ => Err(PacketError::Truncated),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_lengths(data: &[u8]) -> Result<Vec<usize>, PacketError> {
        Packet::parse(data, false).map(|p| p.frames().iter().map(|f| f.len()).collect())
    }

    #[test]
    fn toc_bytes_map_to_modes_bandwidths_and_durations() {
        let toc = Toc::parse(0);
        assert_eq!(
            (toc.mode, toc.bandwidth, toc.duration, toc.stereo),
            (Mode::Silk, Bandwidth::Narrow, FrameDuration::Ms10, false)
        );
        let toc = Toc::parse(11 << 3 | 4);
        assert_eq!(
            (toc.mode, toc.bandwidth, toc.duration, toc.channels()),
            (Mode::Silk, Bandwidth::Wide, FrameDuration::Ms60, 2)
        );
        let toc = Toc::parse(15 << 3);
        assert_eq!(
            (toc.mode, toc.bandwidth, toc.duration),
            (Mode::Hybrid, Bandwidth::Full, FrameDuration::Ms20)
        );
        let toc = Toc::parse(16 << 3);
        assert_eq!(
            (toc.mode, toc.bandwidth, toc.duration),
            (Mode::Celt, Bandwidth::Narrow, FrameDuration::Ms2_5)
        );
        let toc = Toc::parse(31 << 3);
        assert_eq!(
            (toc.mode, toc.bandwidth, toc.duration),
            (Mode::Celt, Bandwidth::Full, FrameDuration::Ms20)
        );
        assert_eq!(Bandwidth::Medium.celt_end_band(), 17);
        assert_eq!(FrameDuration::Ms40.celt_lm(), None);
    }

    #[test]
    fn frame_packing_codes_follow_the_validity_rules() {
        // R1.
        assert_eq!(frame_lengths(&[]), Err(PacketError::Empty));
        // Code 0: one frame, possibly empty; R2 caps it.
        assert_eq!(frame_lengths(&[0xf8]), Ok(vec![0]));
        assert_eq!(frame_lengths(&[0xf8; 1276]), Ok(vec![1275]));
        assert_eq!(frame_lengths(&[0xf8; 1277]), Err(PacketError::FrameTooLong));
        // Code 1: R3.
        assert_eq!(frame_lengths(&[0xf9, 1, 2]), Ok(vec![1, 1]));
        assert_eq!(
            frame_lengths(&[0xf9, 1, 2, 3]),
            Err(PacketError::OddPayload)
        );
        // Code 2: R4, one- and two-byte lengths.
        assert_eq!(frame_lengths(&[0xfa, 1, 9, 8, 7]), Ok(vec![1, 2]));
        assert_eq!(frame_lengths(&[0xfa, 4, 9]), Err(PacketError::Truncated));
        let mut long = vec![0xfa, 252, 1];
        long.extend(std::iter::repeat_n(0, 256 + 3));
        assert_eq!(frame_lengths(&long), Ok(vec![256, 3]));
        assert_eq!(frame_lengths(&[0xfa, 252]), Err(PacketError::Truncated));
        // Code 3: R5.
        assert_eq!(frame_lengths(&[0xfb]), Err(PacketError::FrameCount));
        assert_eq!(frame_lengths(&[0xfb, 0]), Err(PacketError::FrameCount));
        assert_eq!(frame_lengths(&[0xfb, 7]), Err(PacketError::FrameCount)); // 140 ms
        assert_eq!(frame_lengths(&[0x83, 48]), Ok(vec![0; 48])); // 48 x 2.5 ms
        // R6: CBR with padding.
        assert_eq!(
            frame_lengths(&[0xfb, 0x42, 2, 1, 2, 3, 4, 0, 0]),
            Ok(vec![2, 2])
        );
        assert_eq!(
            frame_lengths(&[0xfb, 0x02, 1, 2, 3]),
            Err(PacketError::UnevenFrames)
        );
        assert_eq!(
            frame_lengths(&[0xfb, 0x42, 9, 1]),
            Err(PacketError::Truncated)
        );
        // Padding lengths of 255 add 254 and continue.
        let mut padded = vec![0xfb, 0x41, 255, 1];
        padded.extend(std::iter::repeat_n(0, 255 + 5));
        assert_eq!(frame_lengths(&padded), Ok(vec![5]));
        // R7: VBR.
        assert_eq!(
            frame_lengths(&[0xfb, 0x83, 1, 2, 9, 8, 8, 7]),
            Ok(vec![1, 2, 1])
        );
        assert_eq!(
            frame_lengths(&[0xfb, 0x83, 1, 3, 9, 8, 8]),
            Err(PacketError::Truncated)
        );
        assert_eq!(frame_lengths(&[0xfb, 0xc2, 1, 1, 9, 0]), Ok(vec![1, 0]));
        assert_eq!(
            frame_lengths(&[0xfb, 0xc2, 3, 1, 9]),
            Err(PacketError::Truncated)
        );
    }

    #[test]
    fn self_delimited_packets_code_their_last_length() {
        let data = [0xf8, 2, 7, 7, 0xf8, 1];
        let packet = Packet::parse(&data, true).unwrap();
        assert_eq!((packet.frames(), packet.len), (&[&[7u8, 7][..]][..], 4));
        let data = [0xf9, 1, 5, 6, 0xaa];
        let packet = Packet::parse(&data, true).unwrap();
        assert_eq!((packet.frames().len(), packet.len), (2, 4));
        assert!(Packet::parse(&[0xf8, 3, 1], true).is_err());
        // CBR code 3 with padding: the padding follows the frames.
        let data = [0xfb, 0x42, 1, 1, 7, 8, 0, 9];
        let packet = Packet::parse(&data, true).unwrap();
        assert_eq!(
            (packet.frames(), packet.len),
            (&[&[7u8][..], &[8u8][..]][..], 7)
        );
    }

    #[test]
    fn malformed_packets_never_panic() {
        let mut seed = 1u32;
        for len in 0..64 {
            for _ in 0..200 {
                let data: Vec<u8> = (0..len)
                    .map(|_| {
                        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                        (seed >> 16) as u8
                    })
                    .collect();
                for delimited in [false, true] {
                    if let Ok(packet) = Packet::parse(&data, delimited) {
                        assert!(packet.len <= data.len());
                        assert!(packet.frames().iter().all(|f| f.len() <= 1275));
                    }
                }
            }
        }
    }
}
