//! H.222.0 (08/2023), 2.4.3.1-5. No implementation source was used.
use safe_media_core::{Bytes, Error, Result, SourceId, Time};
use std::ops::Range;

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
    pub(crate) fn ts_byte(&self, index: usize) -> u8 {
        self.source.as_ref()[self.sync + index]
    }
    pub(crate) fn normalized_byte(&self, index: usize) -> u8 {
        if self
            .pcr_range
            .as_ref()
            .is_some_and(|range| range.contains(&index))
        {
            0
        } else {
            self.ts_byte(index)
        }
    }
}

fn pcr(data: &[u8]) -> Result<Pcr> {
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

fn bounded_end(position: usize, count: usize, end: usize) -> Result<usize> {
    let next = position.checked_add(count).ok_or(Error::Overflow)?;
    if next > end {
        Err(Error::Invalid("adaptation field length"))
    } else {
        Ok(next)
    }
}

/// Parses exactly one caller-owned source packet; returned spans share its charge.
/// TEI/scrambling remain observable source facts. The coordinator must not assemble
/// their payload as clear, undamaged PSI/PES. The M2TS prefix is deliberately opaque.
pub fn parse_transport(
    source: Bytes,
    format: PacketFormat,
    source_offset: u64,
) -> Result<TransportPacket> {
    if source.len() != format.bytes() {
        return Err(Error::Invalid("transport packet size"));
    }
    source_offset
        .checked_add(u64::try_from(source.len()).map_err(|_| Error::Overflow)?)
        .ok_or(Error::Overflow)?;
    let sync = format.prefix_bytes();
    let data = &source.as_ref()[sync..];
    if data[0] != 0x47 {
        return Err(Error::Invalid("TS sync byte"));
    }
    let pid = (u16::from(data[1] & 31) << 8) | u16::from(data[2]);
    let afc = (data[3] >> 4) & 3;
    if afc == 0 {
        return Err(Error::Invalid("reserved adaptation control"));
    }
    let mut payload_start = 4;
    let mut adaptation = None;
    let mut pcr_range = None;
    if afc & 2 != 0 {
        let length = usize::from(data[4]);
        if (afc == 2 && length != 183) || (afc == 3 && length > 182) {
            return Err(Error::Invalid("adaptation length/control"));
        }
        let end = 5 + length;
        payload_start = end;
        let flags = if length == 0 { 0 } else { data[5] };
        let mut position = if length == 0 { end } else { 6 };
        let mut clock = None;
        let mut original_clock = None;
        if flags & 0x10 != 0 {
            let next = bounded_end(position, 6, end)?;
            clock = Some(pcr(&data[position..next])?);
            pcr_range = Some(position..next);
            position = next;
        }
        if flags & 8 != 0 {
            let next = bounded_end(position, 6, end)?;
            original_clock = Some(pcr(&data[position..next])?);
            position = next;
        }
        let splice_countdown = if flags & 4 != 0 {
            let next = bounded_end(position, 1, end)?;
            let value = data[position] as i8;
            position = next;
            Some(value)
        } else {
            None
        };
        let mut private_data = None;
        let mut extension = None;
        for (flag, target) in [(2, &mut private_data), (1, &mut extension)] {
            if flags & flag != 0 {
                bounded_end(position, 1, end)?;
                let count = usize::from(data[position]);
                position += 1;
                let next = bounded_end(position, count, end)?;
                *target = Some(source.slice(sync + position..sync + next)?);
                position = next;
            }
        }
        if data[position..end].iter().any(|byte| *byte != 0xff) {
            return Err(Error::Invalid("adaptation stuffing"));
        }
        adaptation = Some(Adaptation {
            source: source.slice(sync + 4..sync + end)?,
            discontinuity: flags & 128 != 0,
            random_access_indicator: flags & 64 != 0,
            elementary_priority: flags & 32 != 0,
            pcr: clock,
            opcr: original_clock,
            splice_countdown,
            private_data,
            extension,
        });
    }
    let payload = if afc & 1 != 0 {
        Some(source.slice(sync + payload_start..source.len())?)
    } else {
        None
    };
    let payload_source_offset = source_offset
        .checked_add(u64::try_from(sync + payload_start).map_err(|_| Error::Overflow)?)
        .ok_or(Error::Overflow)?;
    Ok(TransportPacket {
        prefix: if sync == 0 {
            None
        } else {
            Some(source.slice(0..sync)?)
        },
        pid,
        payload_unit_start: data[1] & 64 != 0,
        transport_priority: data[1] & 32 != 0,
        transport_error: data[1] & 128 != 0,
        scrambling: data[3] >> 6,
        continuity_counter: data[3] & 15,
        adaptation,
        payload,
        source_offset,
        payload_source_offset,
        source_id: None,
        sync,
        pcr_range,
        source,
    })
}
