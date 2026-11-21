//! Complete, independently configured LATM seek envelopes.
use triergon_bitstream::{BitCursor, BitSource};
use triergon_core::WorkBudget;
use triergon_framing::indexed_aac_frequency;

// ISO/IEC 14496-3 §1.7.3, Tables 1.20–1.23 (2001/Cor.2:2004):
// AudioMuxElement repeats PayloadLengthInfo then PayloadMux for each subframe.
// Keep the envelope intact: this probe does not split payloads or invent clocks.
pub(super) struct Probe {
    position: usize,
    remaining: u8,
    payload: usize,
}

impl Probe {
    pub(super) fn new(multiplex: &[u8]) -> Option<Self> {
        let mut bits = BitCursor::new(multiplex);
        // In-band StreamMuxConfig, version zero, common time framing.
        if bits.read(3).ok()? != 1 {
            return None;
        }
        let remaining = bits.read(6).ok()? as u8 + 1;
        if bits.read(7).ok()? != 0 {
            return None; // One program and one layer; no configuration inheritance.
        }
        let asc = bits.read(16).ok()?;
        // AAC-LC, indexed frequency, explicit channels, 1024 samples,
        // no core coder dependency or extension configuration.
        if asc >> 11 != 2 || asc & 7 != 0 || !(1..=7).contains(&((asc >> 3) & 15)) {
            return None;
        }
        indexed_aac_frequency(((asc >> 7) & 15) as u8)?;
        if bits.read(3).ok()? != 0 {
            return None; // Only variable byte lengths (frameLengthType zero).
        }
        bits.skip(8).ok()?; // latmBufferFullness is preserved in the envelope.
        if bits.read(2).ok()? != 0 {
            return None; // No other data or CRC-bearing configuration.
        }
        Some(Self {
            position: multiplex.len() * 8 - bits.remaining(),
            remaining,
            payload: 0,
        })
    }

    // One length octet and, when complete, its payload skip per work unit.
    // The LOAS length bounds arithmetic and storage; the six-bit count bounds
    // subframes. No payload allocation or inspection is needed.
    pub(super) fn step(&mut self, multiplex: &[u8], work: &mut WorkBudget) -> Option<bool> {
        let mut bits = BitCursor::new(multiplex);
        if bits.skip(self.position).is_err() {
            return Some(false);
        }
        while self.remaining != 0 {
            if !work.try_charge(1) {
                return None;
            }
            let Ok(length) = bits.read(8) else {
                return Some(false);
            };
            self.payload += length as usize;
            if self.payload > bits.remaining() / 8 {
                return Some(false);
            }
            if length != 255 {
                if self.payload == 0 || bits.skip(self.payload * 8).is_err() {
                    return Some(false);
                }
                self.payload = 0;
                self.remaining -= 1;
            }
            self.position = multiplex.len() * 8 - bits.remaining();
        }
        // Only ByteAlign may follow the final payload. Do not accept unknown
        // groups, undeclared payloads or an extra trailing byte as a landing.
        Some(bits.remaining() < 8)
    }
}
