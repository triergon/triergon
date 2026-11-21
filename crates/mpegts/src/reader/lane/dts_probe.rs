//! Opt-in envelope evidence, never a replacement for source signalling.
use super::Track;
use crate::Pes;
use triergon_core::WorkBudget;
use triergon_framing::probe_dts_core_header;

pub(super) fn eligible(track: &Track) -> bool {
    let map = track.program_map();
    map.program_descriptors.is_empty()
        && track.stream().stream_type == 0x06
        && track.stream().descriptor_bytes.is_empty()
        && map
            .streams
            .as_slice()
            .iter()
            .filter(|stream| stream.stream_type == 0x06 && stream.descriptor_bytes.is_empty())
            .count()
            == 1
}

/// One initial PES only: at most two 16-KiB frames, no sync search or allocation.
/// PES ownership stays in the caller's existing mailbox across yields.
#[derive(Default)]
pub(super) struct Probe {
    header: [u8; 15],
    used: usize,
    remaining: usize,
    frames: usize,
    span: usize,
    offset: usize,
    inspected: usize,
    format: Option<(u8, u8, u8)>,
    result: Option<bool>,
}

impl Probe {
    pub(super) fn step(&mut self, pes: &Pes, work: &mut WorkBudget) -> Option<bool> {
        if let Some(result) = self.result {
            return Some(result);
        }
        if pes.header.stream_id != 0xbd || pes.elementary_offset != 0 {
            self.result = Some(false);
            return self.result;
        }
        loop {
            if !work.try_charge(1) {
                return None;
            }
            if self.inspected == 32768 {
                self.result = Some(false);
                return self.result;
            }
            let Some(span) = pes.spans().get(self.span) else {
                self.result = Some(false);
                return self.result;
            };
            let bytes: &[u8] = span.as_ref();
            let Some(&byte) = bytes.get(self.offset) else {
                self.span += 1;
                self.offset = 0;
                continue;
            };
            self.offset += 1;
            self.inspected += 1;
            if self.remaining != 0 {
                self.remaining -= 1;
                if self.remaining == 0 {
                    self.frames += 1;
                    self.used = 0;
                    if self.frames == 2 {
                        self.result = Some(true);
                        return self.result;
                    }
                }
                continue;
            }
            self.header[self.used] = byte;
            self.used += 1;
            if self.used == self.header.len() {
                let Ok(header) = probe_dts_core_header(&self.header) else {
                    self.result = Some(false);
                    return self.result;
                };
                let format = (header.sfreq, header.amode, header.nblks);
                if header.frame_bytes <= self.header.len()
                    || header.frame_bytes > 16384
                    || self.format.is_some_and(|old| old != format)
                {
                    self.result = Some(false);
                    return self.result;
                }
                self.format = Some(format);
                self.remaining = header.frame_bytes - self.header.len();
            }
        }
    }
}
