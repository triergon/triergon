//! The two 16-bit field shapes of PAT and PMT sections (H.222.0 §2.4.4): a PID below three
//! reserved bits, and a 12-bit length below four. The reserved bits are ignored.
use crate::transport::pid::MASK;

/// The 13-bit PID of a two-byte field.
#[inline]
pub(crate) fn pid(field: [u8; 2]) -> u16 {
    u16::from_be_bytes(field) & MASK
}

/// The 12-bit length of a two-byte field, in bytes.
#[inline]
pub(crate) fn length(field: [u8; 2]) -> usize {
    usize::from(u16::from_be_bytes(field) & 0x0fff)
}
