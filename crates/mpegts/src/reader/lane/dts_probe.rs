//! Identifies canonical DTS core frames in a descriptor-free private PES.
use super::Track;
use crate::pes::Pes;
use crate::psi::stream_type::PES_PRIVATE_DATA;
use triergon_core::WorkBudget;
use triergon_framing::dts::probe_core_header;

pub(super) fn eligible(track: &Track) -> bool {
    let map = track.program_map();
    map.program_descriptors.is_empty()
        && track.stream().stream_type == PES_PRIVATE_DATA
        && track.stream().descriptor_bytes.is_empty()
        && map
            .streams
            .as_slice()
            .iter()
            .filter(|stream| {
                stream.stream_type == PES_PRIVATE_DATA && stream.descriptor_bytes.is_empty()
            })
            .count()
            == 1
}

const HEADER_BYTES: usize = 15;
const MAX_FRAME_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Format {
    frequency: u8,
    channel_mode: u8,
    blocks: u8,
}

#[derive(Clone, Copy)]
pub(super) enum Verdict {
    Proved,
    Refused,
}

#[derive(Clone, Copy)]
enum AfterBody {
    NextHeader(Format),
    Admit,
}

enum Phase {
    Header {
        bytes: [u8; HEADER_BYTES],
        used: usize,
        first: Option<Format>,
    },
    Body {
        remaining: usize,
        next: AfterBody,
    },
    Finished(Verdict),
}

impl Phase {
    fn header(first: Option<Format>) -> Self {
        Self::Header {
            bytes: [0; HEADER_BYTES],
            used: 0,
            first,
        }
    }
}

/// One initial PES only: at most two 16 KiB frames, with no sync search or allocation.
/// The caller retains the PES across yields.
pub(super) struct Probe {
    span: usize,
    offset: usize,
    phase: Phase,
}

impl Default for Probe {
    fn default() -> Self {
        Self {
            span: 0,
            offset: 0,
            phase: Phase::header(None),
        }
    }
}

impl Probe {
    pub(super) fn step(&mut self, pes: &Pes, work: &mut WorkBudget) -> Option<Verdict> {
        if let Phase::Finished(verdict) = self.phase {
            return Some(verdict);
        }
        if pes.header.stream_id != 0xbd || pes.elementary_offset != 0 {
            return self.finish(Verdict::Refused);
        }
        loop {
            let Some(span) = pes.spans().get(self.span) else {
                return if work.try_charge(1) {
                    self.finish(Verdict::Refused)
                } else {
                    None
                };
            };
            let payload = &span.as_ref()[self.offset..];
            if payload.is_empty() {
                if !work.try_charge(1) {
                    return None;
                }
                self.span += 1;
                self.offset = 0;
                continue;
            }
            match &mut self.phase {
                Phase::Header { bytes, used, first } => {
                    let count =
                        work.take((HEADER_BYTES - *used).min(payload.len()) as u64) as usize;
                    if count == 0 {
                        return None;
                    }
                    bytes[*used..*used + count].copy_from_slice(&payload[..count]);
                    self.offset += count;
                    *used += count;
                    if *used != HEADER_BYTES {
                        continue;
                    }
                    let Ok(header) = probe_core_header(bytes) else {
                        return self.finish(Verdict::Refused);
                    };
                    let format = Format {
                        frequency: header.sfreq,
                        channel_mode: header.amode,
                        blocks: header.nblks,
                    };
                    if !(HEADER_BYTES + 1..=MAX_FRAME_BYTES).contains(&header.frame_bytes) {
                        return self.finish(Verdict::Refused);
                    }
                    let next = match first {
                        None => AfterBody::NextHeader(format),
                        Some(previous) if *previous == format => AfterBody::Admit,
                        Some(_) => return self.finish(Verdict::Refused),
                    };
                    self.phase = Phase::Body {
                        remaining: header.frame_bytes - HEADER_BYTES,
                        next,
                    };
                }
                Phase::Body { remaining, next } => {
                    // Charge the validated span without reading the opaque frame body.
                    let count = work.take((*remaining).min(payload.len()) as u64) as usize;
                    if count == 0 {
                        return None;
                    }
                    self.offset += count;
                    *remaining -= count;
                    if *remaining != 0 {
                        continue;
                    }
                    self.phase = match *next {
                        AfterBody::NextHeader(first) => Phase::header(Some(first)),
                        AfterBody::Admit => return self.finish(Verdict::Proved),
                    };
                }
                Phase::Finished(verdict) => return Some(*verdict),
            }
        }
    }

    fn finish(&mut self, verdict: Verdict) -> Option<Verdict> {
        self.phase = Phase::Finished(verdict);
        Some(verdict)
    }
}
