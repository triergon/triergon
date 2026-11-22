//! Complete, independently configured LATM seek envelopes.
use triergon_core::WorkBudget;
use triergon_framing::indexed_aac_frequency;

// ISO/IEC 14496-3 §1.7.3, Tables 1.20–1.23 (2001/Cor.2:2004):
// AudioMuxElement repeats PayloadLengthInfo then PayloadMux for each subframe.
// Keep the envelope intact so the original payload and clock survive seeking.
pub(super) struct Probe {
    position: usize,
    remaining: u8,
    payload: usize,
}

impl Probe {
    pub(super) fn new(multiplex: &[u8]) -> Option<Self> {
        let &[mux, layout, asc_hi, asc_lo, length_type, flags] = multiplex.first_chunk::<6>()?;
        // In-band StreamMuxConfig, version zero, common time framing, one
        // program and one layer, and no configuration inheritance.
        if mux >> 5 != 1 || layout & 0x7f != 0 {
            return None;
        }
        let subframes = ((mux & 0x1f) << 1) | (layout >> 7);
        let asc = u16::from_be_bytes([asc_hi, asc_lo]);
        // AAC-LC, indexed frequency, explicit channels, 1024 samples,
        // no core coder dependency or extension configuration.
        if asc >> 11 != 2 || asc & 7 != 0 || !(1..=7).contains(&((asc >> 3) & 15)) {
            return None;
        }
        indexed_aac_frequency(((asc >> 7) & 15) as u8)?;
        // Variable byte lengths, with neither other data nor a CRC.
        if length_type >> 5 != 0 || flags & 0x18 != 0 {
            return None;
        }
        Some(Self {
            position: 5,
            remaining: subframes + 1,
            payload: 0,
        })
    }

    // The fixed configuration consumes 45 bits. Lengths and payloads are
    // whole octets, so each next length is always bits 3–10 of two bytes.
    // Charge one unit per length octet and its payload skip, preserving the
    // cursor only when the caller must replenish its work budget.
    pub(super) fn step(&mut self, multiplex: &[u8], work: &mut WorkBudget) -> Option<bool> {
        let Some(mut tail) = multiplex.get(self.position..) else {
            return Some(false);
        };
        let mut remaining = self.remaining;
        let mut payload = self.payload;
        while remaining != 0 {
            if !work.try_charge(1) {
                self.position = multiplex.len() - tail.len();
                self.remaining = remaining;
                self.payload = payload;
                return None;
            }
            let Some(&pair) = tail.first_chunk::<2>() else {
                return Some(false);
            };
            let length = (u16::from_be_bytes(pair) >> 3) as u8;
            tail = &tail[1..];
            payload += usize::from(length);
            // Each byte skipped must leave the three alignment bits.
            if payload >= tail.len() {
                return Some(false);
            }
            if length != 255 {
                if payload == 0 {
                    return Some(false);
                }
                tail = &tail[payload..];
                payload = 0;
                remaining -= 1;
            }
        }
        // Only the three ByteAlign bits may follow the final payload.
        Some(tail.len() == 1)
    }
}
