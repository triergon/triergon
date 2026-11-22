//! Packet identifiers (H.222.0 Table 2-3).
use std::ops::RangeInclusive;

/// The program association table.
pub const PAT: u16 = 0x0000;
/// PIDs a program map or an elementary stream may use: above the reserved table PIDs,
/// below the null PID.
pub const ELEMENTARY: RangeInclusive<u16> = 0x0010..=0x1ffe;
/// Null packets: stuffing that carries no data.
pub const NULL: u16 = 0x1fff;
/// The 13 bits of a 16-bit wire field that hold a PID.
pub const MASK: u16 = 0x1fff;
