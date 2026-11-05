//! H.222.0 (08/2023), 2.4.3.1-5. No implementation source was used.
use std::ops::Range;
use triergon_core::{Bytes, Error, Result, SourceId, Time};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketFormat {
    Ts188,
    M2ts192,
}
impl PacketFormat {
    pub fn bytes(self) -> usize {
        match self {
            Self::Ts188 => 188,
            Self::M2ts192 => 192,
        }
    }
    pub fn prefix_bytes(self) -> usize {
        self.bytes() - 188
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pcr {
    pub base: u64,
    pub extension: u16,
}
impl Pcr {
    /// Raw exact clock count, not PTS/DTS and not an unwrapped rational timestamp.
    pub fn ticks_27mhz(self) -> u64 {
        self.base * 300 + u64::from(self.extension)
    }
    pub fn raw_time(self) -> Result<Time> {
        Time::new(i128::from(self.ticks_27mhz()), 27_000_000)
    }
}

#[derive(Clone, Debug)]
pub struct Adaptation {
    pub source: Bytes,
    pub discontinuity: bool,
    pub random_access_indicator: bool,
    pub elementary_priority: bool,
    pub pcr: Option<Pcr>,
    pub opcr: Option<Pcr>,
    pub splice_countdown: Option<i8>,
    pub private_data: Option<Bytes>,
    /// Preserved raw extension. Full extension interpretation remains separate.
    pub extension: Option<Bytes>,
}

#[derive(Clone, Debug)]
pub struct TransportPacket {
    /// Whole source packet (including the opaque four-byte prefix, if present).
    pub source: Bytes,
    pub source_offset: u64,
    /// Attached by the InputCursor adapter. Detached packet syntax may omit it.
    pub source_id: Option<SourceId>,
    pub prefix: Option<Bytes>,
    pub pid: u16,
    pub payload_unit_start: bool,
    pub transport_priority: bool,
    pub transport_error: bool,
    pub scrambling: u8,
    pub continuity_counter: u8,
    pub adaptation: Option<Adaptation>,
    pub payload: Option<Bytes>,
    pub payload_source_offset: u64,
    pub(crate) sync: usize,
    pub(crate) pcr_range: Option<Range<usize>>,
}

impl TransportPacket {
    pub fn discontinuity_indicator(&self) -> bool {
        self.adaptation
            .as_ref()
            .is_some_and(|field| field.discontinuity)
    }
    pub fn is_null(&self) -> bool {
        self.pid == 0x1fff
    }
    pub(crate) fn ts_bytes(&self) -> &[u8] {
        &self.source.as_ref()[self.sync..self.sync + 188]
    }
}

pub(crate) fn pcr(data: &[u8]) -> Result<Pcr> {
    if data.len() != 6 {
        return Err(Error::Truncated);
    }
    let base = (u64::from(data[0]) << 25)
        | (u64::from(data[1]) << 17)
        | (u64::from(data[2]) << 9)
        | (u64::from(data[3]) << 1)
        | u64::from(data[4] >> 7);
    let extension = (u16::from(data[4] & 1) << 8) | u16::from(data[5]);
    if data[4] & 0x7e != 0x7e || extension > 299 {
        return Err(Error::Invalid("PCR reserved bits/extension"));
    }
    Ok(Pcr { base, extension })
}

/// Strict detached syntax API. The incremental inspector is the only validator.
pub fn parse_transport(
    source: Bytes,
    format: PacketFormat,
    source_offset: u64,
) -> Result<TransportPacket> {
    let mut inspector = crate::TransportInspector::detached(source, format, source_offset, None)
        .map_err(|failure| failure.error)?;
    match inspector.poll(
        &mut triergon_core::WorkBudget::new(u64::MAX),
        &std::sync::atomic::AtomicBool::new(false),
    )? {
        crate::AssemblyStep::Output(crate::TransportInspection::Accepted(packet)) => Ok(packet),
        crate::AssemblyStep::Output(crate::TransportInspection::Rejected(rejected)) => {
            Err(rejected.error())
        }
        _ => Err(Error::Invalid("detached transport inspection incomplete")),
    }
}
