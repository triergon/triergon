//! Per-PID continuity, including H.222.0's whole-packet/PCR duplicate exception.
use crate::checked_pid;
use crate::transport::TransportPacket;
use triergon_core::{Error, Result};

/// How one packet relates to the previous packet on its PID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContinuityOutcome {
    /// The first packet since creation or [`Continuity::reset`].
    First,
    /// Counter continuity, even if the packet's raw discontinuity flag is true.
    Continuous,
    /// A payload-carrying packet repeating the previous one byte for byte (PCR bytes excepted);
    /// do not assemble its payload again.
    Duplicate,
    /// A further copy after a duplicate; only one duplicate is permitted.
    ExcessDuplicate,
    /// An actual counter break permitted by the packet's discontinuity flag.
    /// This does not by itself establish a program clock discontinuity.
    SignalledDiscontinuity,
    /// The counter broke without a discontinuity indicator.
    Lost {
        /// The 4-bit counter value that continuity required.
        expected: u8,
        /// The 4-bit counter value the packet carried.
        actual: u8,
    },
    /// The packet has its transport error indicator set or is scrambled; the state was reset.
    Damaged,
    /// A null packet (PID 0x1FFF), which carries no continuity.
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
    /// Continuity state for `pid` with no packet seen.
    ///
    /// Fails with `Invalid` for a PID above 0x1FFE.
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
    /// Forgets the previous packet, so the next observed packet is [`ContinuityOutcome::First`].
    pub fn reset(&mut self) {
        self.counter = None;
        self.payload = false;
        self.duplicated = false;
    }

    pub(crate) fn expects_payload_counter(&self, counter: u8) -> bool {
        self.counter
            .is_some_and(|previous| (previous + 1) & 15 == counter)
    }

    pub(crate) fn record_continuation(&mut self, packet: &TransportPacket) {
        debug_assert_eq!(packet.pid, self.pid);
        debug_assert!(self.expects_payload_counter(packet.continuity_counter));
        self.record_packet(packet, packet.ts_bytes(), true);
    }

    pub(crate) fn record_plain_continuation(&mut self, ts: &[u8], counter: u8) {
        debug_assert!(self.expects_payload_counter(counter));
        self.record(ts, None, counter, true);
    }

    pub(crate) fn record_payload_run(&mut self, first_counter: u8, last: &[u8]) {
        debug_assert!(self.expects_payload_counter(first_counter));
        self.record(last, None, last[3] & 15, true);
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
            Some(previous)
                if previous.wrapping_add(u8::from(has_payload)) & 15
                    == packet.continuity_counter =>
            {
                ContinuityOutcome::Continuous
            }
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
                if packet.discontinuity_indicator() {
                    ContinuityOutcome::SignalledDiscontinuity
                } else {
                    ContinuityOutcome::Lost {
                        expected: previous.wrapping_add(u8::from(has_payload)) & 15,
                        actual: packet.continuity_counter,
                    }
                }
            }
        };
        self.record_packet(packet, ts, has_payload);
        Ok(outcome)
    }

    fn record_packet(&mut self, packet: &TransportPacket, ts: &[u8], has_payload: bool) {
        self.record(
            ts,
            packet.pcr_range.clone(),
            packet.continuity_counter,
            has_payload,
        );
    }

    fn record(
        &mut self,
        ts: &[u8],
        pcr_range: Option<std::ops::Range<usize>>,
        counter: u8,
        has_payload: bool,
    ) {
        self.previous.copy_from_slice(ts);
        if let Some(pcr) = pcr_range {
            self.previous[pcr].fill(0);
        }
        self.counter = Some(counter);
        self.payload = has_payload;
        self.duplicated = false;
    }
}
