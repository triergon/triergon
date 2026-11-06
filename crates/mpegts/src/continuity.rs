//! Per-PID continuity, including H.222.0's whole-packet/PCR duplicate exception.
use crate::{TransportPacket, checked_pid};
use triergon_core::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContinuityOutcome {
    First,
    /// Counter continuity, even if the packet's raw discontinuity flag is true.
    Continuous,
    Duplicate,
    ExcessDuplicate,
    /// An actual counter break permitted by the packet's discontinuity flag.
    /// This does not by itself establish a program clock discontinuity.
    SignalledDiscontinuity,
    Lost {
        expected: u8,
        actual: u8,
    },
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

    pub(crate) fn expects_payload_counter(&self, counter: u8) -> bool {
        self.counter
            .is_some_and(|previous| (previous + 1) & 15 == counter)
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
        let ts = packet.ts_bytes();
        let outcome = match self.counter {
            None => ContinuityOutcome::First,
            Some(previous) => {
                // H.222.0 2.4.3.3/5: a flagged packet can still be the second
                // copy of a duplicate pair. Only an actual counter break is a
                // counter discontinuity point; the raw flag remains observable.
                let same_packet = has_payload
                    && self.payload
                    && previous == packet.continuity_counter
                    && match packet.pcr_range.as_ref() {
                        Some(pcr) => {
                            self.previous[..pcr.start] == ts[..pcr.start]
                                && self.previous[pcr.clone()].iter().all(|&byte| byte == 0)
                                && self.previous[pcr.end..] == ts[pcr.end..]
                        }
                        None => self.previous == ts,
                    };
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
                } else if packet.discontinuity_indicator() {
                    ContinuityOutcome::SignalledDiscontinuity
                } else {
                    ContinuityOutcome::Lost {
                        expected,
                        actual: packet.continuity_counter,
                    }
                }
            }
        };
        self.previous.copy_from_slice(ts);
        if let Some(pcr) = packet.pcr_range.as_ref() {
            self.previous[pcr.clone()].fill(0);
        }
        self.counter = Some(packet.continuity_counter);
        self.payload = has_payload;
        self.duplicated = false;
        Ok(outcome)
    }
}
