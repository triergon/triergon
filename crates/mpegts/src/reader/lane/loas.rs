//! Byte-aligned AudioSyncStream envelopes (ISO/IEC 14496-3, §1.7.2).
use triergon_core::{Error, Result};

pub(super) fn parse(header: &[u8; 3]) -> Result<((), usize)> {
    let prefix = u16::from_be_bytes([header[0], header[1]]);
    if prefix >> 5 != 0x2b7 {
        return Err(Error::Invalid("LOAS syncword"));
    }
    let body = (usize::from(prefix & 31) << 8) | usize::from(header[2]);
    if body == 0 {
        return Err(Error::Invalid("empty LOAS multiplex element"));
    }
    Ok(((), header.len() + body))
}
