//! The PES packets the demuxer hands a decoder, for any stream type.
//!
//! A packet starts at a payload unit start and ends at its declared length, at
//! the next unit start or at the end of the input, whichever comes first. The
//! header is read as far as its timestamps; marker bits and the remaining
//! optional fields are not checked. Continuity counters are not checked either:
//! a repeated or missing transport packet changes the data as it arrives.
use super::DemuxBoundary;
use crate::pes::HEADER_MAX;
use crate::{ParseLimits, TransportPacket, checked_pid, limit};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result};

/// How far after the last PCR the demuxer lets a teletext PTS lie, in 90 kHz
/// ticks.
const PCR_LEAD: u64 = 12_654;

/// One PES packet as the demuxer emits it: its timestamps, the program clock
/// when it started and its data bytes, which are never empty.
#[derive(Debug)]
pub struct CutPes {
    pub stream_id: u8,
    pub pts_raw33: Option<u64>,
    pub dts_raw33: Option<u64>,
    /// The base of the last PCR on the program's PCR PID before the packet's
    /// unit start, if any.
    pub pcr_base: Option<u64>,
    /// [`DemuxBoundary::FiniteLength`] when the declared length was reached.
    pub boundary: DemuxBoundary,
    pub data: Bytes,
}

impl CutPes {
    /// The PTS the demuxer gives a teletext packet ([`pcr_bounded_pts`]).
    pub fn pcr_bounded_pts(&self) -> Option<u64> {
        pcr_bounded_pts(self.pts_raw33, self.pcr_base)
    }
}

/// The PTS the demuxer gives a teletext packet whose raw PTS is `pts_raw33`
/// and whose unit start followed the PCR base `pcr_base`: none before the
/// program's first PCR, otherwise the PTS kept within 12,654 ticks (about
/// 141 ms) after that PCR.
#[must_use]
pub fn pcr_bounded_pts(pts_raw33: Option<u64>, pcr_base: Option<u64>) -> Option<u64> {
    let pcr = pcr_base?;
    Some(pts_raw33?.clamp(pcr, pcr + PCR_LEAD))
}

#[derive(Debug)]
enum State {
    /// Waiting for a unit start.
    Idle,
    /// Collecting header bytes into `PesCutter::header` up to `target`.
    Header { target: usize },
    /// Collecting data: `remaining` bytes for a declared length.
    Data {
        head: Head,
        remaining: Option<usize>,
        data: BudgetVec<u8>,
    },
}

#[derive(Clone, Copy, Debug)]
struct Head {
    stream_id: u8,
    pts_raw33: Option<u64>,
    dts_raw33: Option<u64>,
    pcr_base: Option<u64>,
}

/// Cuts the transport packets of one PID into [`CutPes`] packets.
///
/// Push every transport packet of the program in source order (packets of
/// other PIDs only update the program clock), then take the packets the push
/// completed with [`Self::pop`]; [`Self::finish`] ends the input.
#[derive(Debug)]
pub struct PesCutter {
    pid: u16,
    pcr_pid: Option<u16>,
    pcr_base: Option<u64>,
    budget: Budget,
    limits: ParseLimits,
    state: State,
    header: [u8; HEADER_MAX],
    used: usize,
    /// At most the packet a unit start ends and the one it starts.
    ready: [Option<CutPes>; 2],
    ended: bool,
}

impl PesCutter {
    /// Data is allocated from `budget`; a packet longer than
    /// `limits.pes_bytes` is an error.
    pub fn new(pid: u16, budget: Budget, limits: ParseLimits) -> Result<Self> {
        checked_pid(pid)?;
        Ok(Self {
            pid,
            pcr_pid: None,
            pcr_base: None,
            budget,
            limits: limits.validate()?,
            state: State::Idle,
            header: [0; HEADER_MAX],
            used: 0,
            ready: [None, None],
            ended: false,
        })
    }

    pub fn pid(&self) -> u16 {
        self.pid
    }

    /// The program's PCR PID, as its current PMT names it.
    pub fn set_pcr_pid(&mut self, pid: Option<u16>) {
        self.pcr_pid = pid;
    }

    /// One transport packet. Take the completed packets before the next push.
    pub fn push(&mut self, packet: &TransportPacket) -> Result<()> {
        if self.ended {
            return Err(Error::Invalid("PES packet after the end"));
        }
        if self.ready.iter().any(Option::is_some) {
            return Err(Error::Invalid("PES output not taken"));
        }
        if Some(packet.pid) == self.pcr_pid
            && let Some(pcr) = packet.adaptation.as_ref().and_then(|a| a.pcr)
        {
            self.pcr_base = Some(pcr.base);
        }
        if packet.pid != self.pid {
            return Ok(());
        }
        let Some(payload) = packet.payload.as_ref() else {
            return Ok(());
        };
        if packet.payload_unit_start {
            self.end(DemuxBoundary::NextPesStart)?;
            self.used = 0;
            self.state = State::Header { target: 6 };
        }
        self.consume(payload.as_ref())
    }

    /// The next packet the last push or [`Self::finish`] completed.
    pub fn pop(&mut self) -> Option<CutPes> {
        self.ready.iter_mut().find_map(Option::take)
    }

    /// The end of the input: completes the packet in progress.
    pub fn finish(&mut self) -> Result<()> {
        self.end(DemuxBoundary::LogicalEof)?;
        self.ended = true;
        Ok(())
    }

    fn consume(&mut self, mut bytes: &[u8]) -> Result<()> {
        loop {
            match &mut self.state {
                State::Idle => return Ok(()),
                State::Header { target } => {
                    let target = *target;
                    let count = (target - self.used).min(bytes.len());
                    self.header[self.used..self.used + count].copy_from_slice(&bytes[..count]);
                    self.used += count;
                    bytes = &bytes[count..];
                    if self.used < target {
                        return Ok(());
                    }
                    self.state = self.after_header()?;
                }
                State::Data {
                    remaining, data, ..
                } => {
                    let count = remaining.map_or(bytes.len(), |r| r.min(bytes.len()));
                    let length = data.len() + count;
                    if length > self.limits.pes_bytes {
                        return Err(limit(length, self.limits.pes_bytes));
                    }
                    if length > data.capacity() {
                        let wanted = remaining.map_or(2 * data.capacity(), |r| data.len() + r);
                        let target = wanted.clamp(length, self.limits.pes_bytes);
                        data.try_reserve_exact(target - data.len())?;
                    }
                    data.try_extend_from_slice(&bytes[..count])?;
                    if let Some(remaining) = remaining {
                        *remaining -= count;
                        if *remaining == 0 {
                            // Bytes after the declared length are not PES data.
                            return self.end(DemuxBoundary::FiniteLength);
                        }
                    }
                    return Ok(());
                }
            }
        }
    }

    /// The next state once `self.used` header bytes are in.
    fn after_header(&self) -> Result<State> {
        let header = &self.header[..self.used];
        if self.used == 6 {
            if header[..3] != [0, 0, 1] {
                return Ok(State::Idle);
            }
            if !has_optional_header(header[3]) {
                return self.data(header, None, None);
            }
            return Ok(State::Header { target: 9 });
        }
        let target = 9 + usize::from(header[8]);
        if self.used < target {
            return Ok(State::Header { target });
        }
        let flags = header[7] >> 6;
        let pts = (flags & 2 != 0)
            .then(|| header.get(9..14))
            .flatten()
            .map(timestamp);
        let dts = (flags == 3)
            .then(|| header.get(14..19))
            .flatten()
            .map(timestamp);
        self.data(header, pts, dts)
    }

    fn data(&self, header: &[u8], pts: Option<u64>, dts: Option<u64>) -> Result<State> {
        let declared = usize::from(u16::from_be_bytes([header[4], header[5]]));
        let remaining = (declared != 0).then(|| (6 + declared).saturating_sub(header.len()));
        if remaining == Some(0) {
            return Ok(State::Idle);
        }
        Ok(State::Data {
            head: Head {
                stream_id: header[3],
                pts_raw33: pts,
                dts_raw33: dts,
                pcr_base: self.pcr_base,
            },
            remaining,
            data: BudgetVec::new(&self.budget),
        })
    }

    /// Ends the packet in progress at `boundary`; one with data becomes ready.
    fn end(&mut self, boundary: DemuxBoundary) -> Result<()> {
        let State::Data { head, data, .. } = std::mem::replace(&mut self.state, State::Idle) else {
            return Ok(());
        };
        if data.is_empty() {
            return Ok(());
        }
        let data = data.freeze().map_err(|failure| failure.error)?;
        let slot = self
            .ready
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(Error::Invalid("PES output not taken"))?;
        *slot = Some(CutPes {
            stream_id: head.stream_id,
            pts_raw33: head.pts_raw33,
            dts_raw33: head.dts_raw33,
            pcr_base: head.pcr_base,
            boundary,
            data,
        });
        Ok(())
    }
}

/// Whether PES packets of `stream_id` carry the optional header with the
/// timestamps (H.222.0 2.4.3.7): all but the program stream map, padding,
/// private stream 2, ECM, EMM, DSM-CC, H.222.1 type E and the program stream
/// directory.
fn has_optional_header(stream_id: u8) -> bool {
    !matches!(
        stream_id,
        0xbc | 0xbe | 0xbf | 0xf0 | 0xf1 | 0xf2 | 0xf8 | 0xff
    )
}

/// A 33-bit PTS or DTS field, without its marker bits.
fn timestamp(field: &[u8]) -> u64 {
    (u64::from(field[0] >> 1) & 7) << 30
        | u64::from(field[1]) << 22
        | u64::from(field[2] >> 1) << 15
        | u64::from(field[3]) << 7
        | u64::from(field[4] >> 1)
}
