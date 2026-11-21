//! H.222.0 (08/2023), 2.4.3.1-5. No implementation source was used.
use crate::compat::pes::PcrPolicy;
use std::ops::Range;
use triergon_core::{Bytes, Error, Result, SourceId, Time};

/// A carrier with nonzero transport scrambling control.
pub(crate) const SCRAMBLED_PAYLOAD: &str = "scrambled TS payload";

/// A carrier with its transport error indicator set.
pub(crate) const TRANSPORT_ERROR_INDICATOR: &str = "TS transport error indicator";

/// A PCR whose reserved bits are not all set or whose extension exceeds 299.
pub(crate) const PCR_ENCODING: &str = "PCR reserved bits/extension";

/// Transport packet size on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketFormat {
    /// Plain 188-byte transport packets.
    Ts188,
    /// 192-byte M2TS packets: an opaque 4-byte prefix before each 188-byte transport packet.
    M2ts192,
}
impl PacketFormat {
    /// Bytes per packet on the wire, prefix included: 188 or 192.
    pub fn bytes(self) -> usize {
        match self {
            Self::Ts188 => 188,
            Self::M2ts192 => 192,
        }
    }
    /// Bytes before the sync byte of each packet: 0 for TS, 4 for M2TS.
    pub fn prefix_bytes(self) -> usize {
        self.bytes() - 188
    }
    /// The format whose sync bytes (0x47) the start of an input repeats at
    /// three successive packets, as far as `head` reaches: 188-byte packets
    /// first, then 192-byte packets after their 4-byte prefix.
    pub fn detect(head: &[u8]) -> Option<Self> {
        [Self::Ts188, Self::M2ts192].into_iter().find(|format| {
            let sync = format.prefix_bytes();
            head.len() > sync
                && (0..3).all(|n| {
                    head.get(sync + n * format.bytes())
                        .is_none_or(|&b| b == 0x47)
                })
        })
    }
}

/// A program clock reference (PCR or OPCR) as encoded in an adaptation field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pcr {
    /// 33-bit base in 90 kHz units.
    pub base: u64,
    /// 9-bit extension in 27 MHz units, 0 to 299.
    pub extension: u16,
}
impl Pcr {
    /// Raw exact clock count, not PTS/DTS and not an unwrapped rational timestamp.
    pub fn ticks_27mhz(self) -> u64 {
        self.base * 300 + u64::from(self.extension)
    }
    /// The clock count as an exact time of 27 MHz ticks; never fails, as the rate is fixed and
    /// nonzero.
    pub fn raw_time(self) -> Result<Time> {
        Time::new(i128::from(self.ticks_27mhz()), 27_000_000)
    }
}

/// A decoded adaptation field of one accepted transport packet.
#[derive(Clone, Debug)]
pub struct Adaptation {
    /// Adaptation field bytes, from the adaptation field length byte to the end of the field.
    pub source: Bytes,
    /// The discontinuity indicator.
    pub discontinuity: bool,
    /// The random access indicator.
    pub random_access_indicator: bool,
    /// The elementary stream priority indicator.
    pub elementary_priority: bool,
    /// The program clock reference, when the PCR flag is set.
    pub pcr: Option<Pcr>,
    /// The original program clock reference, when the OPCR flag is set.
    pub opcr: Option<Pcr>,
    /// The signed splice countdown, when the splicing point flag is set.
    pub splice_countdown: Option<i8>,
    /// The transport private data bytes, without their length byte, when present.
    pub private_data: Option<Bytes>,
    /// Preserved raw extension. Full extension interpretation remains separate.
    pub extension: Option<Bytes>,
}

impl Adaptation {
    /// The six encoded reserved bits, when this field carries a PCR.
    pub fn pcr_reserved_bits(&self) -> Option<u8> {
        self.pcr?;
        self.source.as_ref().get(6).map(|byte| (byte >> 1) & 0x3f)
    }
}

/// One accepted transport packet: its header fields, adaptation field and payload, sliced from the
/// source bytes.
#[derive(Clone, Debug)]
pub struct TransportPacket {
    /// Whole source packet (including the opaque four-byte prefix, if present).
    pub source: Bytes,
    /// Byte offset of the packet's first byte (prefix included) in the source entity.
    pub source_offset: u64,
    /// Attached by the InputCursor adapter. Detached packet syntax may omit it.
    pub source_id: Option<SourceId>,
    /// The opaque 4-byte M2TS prefix; `None` for 188-byte packets.
    pub prefix: Option<Bytes>,
    /// The 13-bit packet identifier.
    pub pid: u16,
    /// The payload unit start indicator.
    pub payload_unit_start: bool,
    /// The transport priority bit.
    pub transport_priority: bool,
    /// The transport error indicator.
    pub transport_error: bool,
    /// The 2-bit transport scrambling control; 0 means not scrambled.
    pub scrambling: u8,
    /// The 4-bit continuity counter.
    pub continuity_counter: u8,
    /// The adaptation field, when the adaptation field control signals one.
    pub adaptation: Option<Adaptation>,
    /// The payload bytes after the header and adaptation field, when the adaptation field control
    /// signals a payload.
    pub payload: Option<Bytes>,
    /// Byte offset of the payload's first byte in the source entity.
    pub payload_source_offset: u64,
    pub(crate) sync: usize,
    pub(crate) pcr_range: Option<Range<usize>>,
}

impl TransportPacket {
    /// Whether the adaptation field sets the discontinuity indicator; `false` without one.
    pub fn discontinuity_indicator(&self) -> bool {
        self.adaptation
            .as_ref()
            .is_some_and(|field| field.discontinuity)
    }
    /// Whether this is a null packet (PID 0x1FFF).
    pub fn is_null(&self) -> bool {
        self.pid == 0x1fff
    }
    pub(crate) fn ts_bytes(&self) -> &[u8] {
        &self.source.as_ref()[self.sync..self.sync + 188]
    }
}

pub(crate) fn pcr(data: &[u8]) -> Result<Pcr> {
    pcr_with_policy(data, PcrPolicy::Strict)
}

pub(crate) fn pcr_with_policy(data: &[u8], policy: PcrPolicy) -> Result<Pcr> {
    if data.len() != 6 {
        return Err(Error::Truncated);
    }
    let base = (u64::from(data[0]) << 25)
        | (u64::from(data[1]) << 17)
        | (u64::from(data[2]) << 9)
        | (u64::from(data[3]) << 1)
        | u64::from(data[4] >> 7);
    let extension = (u16::from(data[4] & 1) << 8) | u16::from(data[5]);
    if extension > 299 || (data[4] & 0x7e != 0x7e && policy == PcrPolicy::Strict) {
        return Err(Error::Invalid(PCR_ENCODING));
    }
    Ok(Pcr { base, extension })
}

/// Strict detached syntax API. The incremental inspector is the only validator.
pub fn parse_transport(
    source: Bytes,
    format: PacketFormat,
    source_offset: u64,
) -> Result<TransportPacket> {
    let mut inspector =
        crate::transport::TransportInspector::detached(source, format, source_offset, None)
            .map_err(|failure| failure.error)?;
    match inspector.poll(
        &mut triergon_core::WorkBudget::new(u64::MAX),
        &std::sync::atomic::AtomicBool::new(false),
    )? {
        crate::AssemblyStep::Output(crate::transport::TransportInspection::Accepted(packet)) => {
            Ok(packet)
        }
        crate::AssemblyStep::Output(crate::transport::TransportInspection::Rejected(rejected)) => {
            Err(rejected.error())
        }
        _ => Err(Error::Invalid("detached transport inspection incomplete")),
    }
}
