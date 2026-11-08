//! The header bits of the SILK layer (RFC 6716 sections 4.2.3 and 4.2.4).

use super::tables::{LBRR_FLAGS_40MS, LBRR_FLAGS_60MS};
use crate::range::RangeDecoder;

/// Voice activity and LBRR presence of each SILK frame, by channel (mid, side).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Header {
    pub(super) vad: [[bool; 3]; 2],
    pub(super) lbrr: [[bool; 3]; 2],
}

impl Header {
    /// Parse the VAD and LBRR flags of `channels` channels of `frames` SILK frames each.
    pub(super) fn parse(rc: &mut RangeDecoder<'_>, channels: usize, frames: usize) -> Header {
        let mut header = Header::default();
        let mut has_lbrr = [false; 2];
        for channel in 0..channels {
            for vad in &mut header.vad[channel][..frames] {
                *vad = rc.bit_logp(1);
            }
            has_lbrr[channel] = rc.bit_logp(1);
        }
        for channel in 0..channels {
            if !has_lbrr[channel] {
                continue;
            }
            // One flag per 20 ms frame, packed from the LSB; an all-zero set is not coded.
            let flags = match frames {
                1 => 1,
                2 => rc.icdf(&LBRR_FLAGS_40MS, 8) + 1,
                _ => rc.icdf(&LBRR_FLAGS_60MS, 8) + 1,
            };
            for (frame, lbrr) in header.lbrr[channel][..frames].iter_mut().enumerate() {
                *lbrr = flags >> frame & 1 != 0;
            }
        }
        header
    }
}
