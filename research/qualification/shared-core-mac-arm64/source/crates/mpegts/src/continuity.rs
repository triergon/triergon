//! Per-PID continuity, including H.222.0's whole-packet/PCR duplicate exception.
use crate::{TransportPacket, checked_pid};
use safe_media_core::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContinuityOutcome {
    First,
    Continuous,
    Duplicate,
    ExcessDuplicate,
    SignalledDiscontinuity,
    Lost { expected: u8, actual: u8 },
    Damaged,
    Null,
}

/// Inline, fixed-size state. Store collections of these in a shared BudgetVec.
#[derive(Debug)]
pub struct Continuity {
    pid: u16,
    counter: Option<u8>,
    payload: bool,
    duplicated: bool,
    previous: [u8; 188],
}

impl Continuity {
    pub fn new(pid: u16) -> Result<Self> {
        checked_pid(pid)?;
        Ok(Self {
            pid,
            counter: None,
            payload: false,
            duplicated: false,
            previous: [0; 188],
        })
    }
    pub fn reset(&mut self) {
        self.counter = None;
        self.payload = false;
        self.duplicated = false;
    }

    /// A bounded 188-byte operation. The caller charges one packet work unit before
    /// calling and checks cancellation at that boundary; no heap allocation occurs.
    /// PCR observations on Duplicate remain usable; do not re-assemble its payload.
    pub fn observe(&mut self, packet: &TransportPacket) -> Result<ContinuityOutcome> {
        if packet.is_null() {
            return Ok(ContinuityOutcome::Null);
        }
        if packet.pid != self.pid {
            return Err(Error::Invalid("continuity PID mismatch"));
        }
        if packet.transport_error || packet.scrambling != 0 {
            self.reset();
            return Ok(ContinuityOutcome::Damaged);
        }
        let has_payload = packet
            .payload
            .as_ref()
            .is_some_and(|payload| !payload.is_empty());
        let outcome = match self.counter {
            None => ContinuityOutcome::First,
            Some(_) if packet.discontinuity_indicator() => {
                ContinuityOutcome::SignalledDiscontinuity
            }
            Some(previous) => {
                let same_packet = has_payload
                    && self.payload
                    && previous == packet.continuity_counter
                    && (0..188).all(|index| self.previous[index] == packet.normalized_byte(index));
                if same_packet {
                    if self.duplicated {
                        return Ok(ContinuityOutcome::ExcessDuplicate);
                    }
                    self.duplicated = true;
                    return Ok(ContinuityOutcome::Duplicate);
                }
                let expected = (previous + u8::from(has_payload)) & 15;
                if expected == packet.continuity_counter {
                    ContinuityOutcome::Continuous
                } else {
                    ContinuityOutcome::Lost {
                        expected,
                        actual: packet.continuity_counter,
                    }
                }
            }
        };
        for (index, byte) in self.previous.iter_mut().enumerate() {
            *byte = packet.normalized_byte(index);
        }
        self.counter = Some(packet.continuity_counter);
        self.payload = has_payload;
        self.duplicated = false;
        Ok(outcome)
    }
}
