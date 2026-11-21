//! ADTS header (ISO/IEC 14496-3 1.A.2.2 / ISO/IEC 13818-7 6.2).

use crate::error::{DecodeError, Result};

/// The fixed and variable header of one ADTS frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdtsHeader {
    /// 0 = MPEG-4, 1 = MPEG-2.
    pub id: u8,
    /// No CRC follows the header (and, with several raw data blocks, none follows each
    /// block).
    pub protection_absent: bool,
    /// profile_ObjectType (audio object type - 1): 0 Main, 1 LC, 2 SSR, 3 LTP.
    pub profile: u8,
    /// sampling_frequency_index, 0 to 15 (13 to 15 are reserved).
    pub sf_index: u8,
    /// channel_configuration, 0 to 7; 0 means a PCE in the payload gives the layout.
    pub channel_config: u8,
    /// Total frame length in bytes, header included.
    pub frame_length: usize,
    /// adts_buffer_fullness, 0x7ff for a variable-rate stream.
    pub buffer_fullness: u16,
    /// number_of_raw_data_blocks_in_frame + 1.
    pub raw_data_blocks: u8,
}

impl AdtsHeader {
    /// Header size in bytes (7, or 9 with CRC).
    pub fn header_len(&self) -> usize {
        if self.protection_absent { 7 } else { 9 }
    }

    /// Parse the 7-byte fixed and variable header at the start of `b`.
    ///
    /// # Errors
    /// `Truncated` when `b` is shorter than 7 bytes; `Invalid` for a wrong syncword
    /// or layer, or a frame length shorter than the header.
    pub fn parse(b: &[u8]) -> Result<AdtsHeader> {
        if b.len() < 7 {
            return Err(DecodeError::Truncated);
        }
        if b[0] != 0xff || (b[1] & 0xf0) != 0xf0 {
            return Err(DecodeError::Invalid("ADTS syncword"));
        }
        if b[1] & 0x06 != 0 {
            return Err(DecodeError::Invalid("ADTS layer"));
        }
        let h = AdtsHeader {
            id: (b[1] >> 3) & 1,
            protection_absent: b[1] & 1 != 0,
            profile: b[2] >> 6,
            sf_index: (b[2] >> 2) & 0xf,
            channel_config: ((b[2] & 1) << 2) | (b[3] >> 6),
            frame_length: (((b[3] & 3) as usize) << 11)
                | ((b[4] as usize) << 3)
                | ((b[5] as usize) >> 5),
            buffer_fullness: (((b[5] & 0x1f) as u16) << 6) | ((b[6] >> 2) as u16),
            raw_data_blocks: (b[6] & 3) + 1,
        };
        if h.frame_length < h.header_len() {
            return Err(DecodeError::Invalid("ADTS frame length"));
        }
        Ok(h)
    }
}

/// The next plausible ADTS frame in `data` at or after `from`, as its offset and header:
/// a valid header with a defined sampling frequency index that is followed by another
/// syncword, an ID3 or `TAG` tag, or the end of `data`. A final frame that runs past the
/// end of `data` is returned too, so that its decoder reports it as truncated.
pub fn find_frame(data: &[u8], from: usize) -> Option<(usize, AdtsHeader)> {
    let mut i = from;
    while i + 7 <= data.len() {
        if data[i] == 0xff
            && (data[i + 1] & 0xf6) == 0xf0
            && let Ok(h) = AdtsHeader::parse(&data[i..])
        {
            let end = i + h.frame_length;
            if h.sf_index < 13 && end <= data.len() {
                let next_ok = end + 2 > data.len()
                    || (data[end] == 0xff && (data[end + 1] & 0xf6) == 0xf0)
                    || data[end..].starts_with(b"TAG")
                    || data[end..].starts_with(b"ID3");
                if next_ok {
                    return Some((i, h));
                }
            } else if h.sf_index < 13 && end > data.len() {
                // truncated final frame: report it, the decoder sees the short buffer
                return Some((i, h));
            }
        }
        i += 1;
    }
    None
}
