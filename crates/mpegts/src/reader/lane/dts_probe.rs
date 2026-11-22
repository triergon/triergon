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
            let Some(span) = pes.spans().get(self.span) else {
                if !work.try_charge(1) {
                    return None;
                }
                self.result = Some(false);
                return self.result;
            };
            let bytes: &[u8] = span.as_ref();
            let bytes = &bytes[self.offset..];
            if bytes.is_empty() {
                if !work.try_charge(1) {
                    return None;
                }
                self.span += 1;
                self.offset = 0;
                continue;
            }
            if self.remaining != 0 {
                // The span already proves the extent. Charge its bytes without
                // loading the body, which is opaque to the envelope check.
                let count = work.take(self.remaining.min(bytes.len()) as u64) as usize;
                if count == 0 {
                    return None;
                }
                self.offset += count;
                self.remaining -= count;
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
            let count = work.take((self.header.len() - self.used).min(bytes.len()) as u64) as usize;
            if count == 0 {
                return None;
            }
            self.header[self.used..self.used + count].copy_from_slice(&bytes[..count]);
            self.offset += count;
            self.used += count;
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
