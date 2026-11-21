//! The program clock: PCR discontinuities and quarantine.
use super::TransportStream;
use super::event::{FaultKind, TraceEvent};
use triergon_core::{Error, Result};

impl TransportStream {
    pub(super) fn clock_transition(&mut self) -> Result<Option<TraceEvent>> {
        let packet = &self
            .held
            .as_ref()
            .ok_or(Error::Invalid("TS packet missing"))?
            .packet;
        if self.pcr_pid != Some(packet.pid) {
            return Ok(None);
        }
        let signalled = packet.discontinuity_indicator();
        let pid = packet.pid;
        let has_pcr = packet
            .adaptation
            .as_ref()
            .is_some_and(|field| field.pcr.is_some());
        if !signalled && !self.clock_signal && !self.clock_pending && !self.clock_quarantined {
            // The ordinary PCR carrier: the second sample after a new time base.
            if has_pcr && self.clock_samples.is_some() {
                self.clock_samples = Some(2);
            }
            return Ok(None);
        }
        self.clock_boundary(pid, signalled, has_pcr)
    }

    /// The clock stage of a carrier that raises, holds or drops the discontinuity
    /// indicator, or of any carrier while the clock is quarantined.
    #[cold]
    fn clock_boundary(
        &mut self,
        pid: u16,
        signalled: bool,
        has_pcr: bool,
    ) -> Result<Option<TraceEvent>> {
        if self.clock_quarantined {
            self.clock_signal = signalled;
            return Ok(None);
        }
        if self.clock_pending && !signalled {
            self.clock_signal = false;
            return Ok(Some(
                self.quarantine_clock(pid, "PCR discontinuity signal dropped"),
            ));
        }
        if signalled && !self.clock_signal {
            self.clock_pending = true;
        }
        self.clock_signal = signalled;
        if !has_pcr {
            return Ok(None);
        }
        if self.clock_pending {
            if self.clock_samples == Some(1) {
                return Ok(Some(self.quarantine_clock(
                    pid,
                    "clock discontinuity before two PCR samples",
                )));
            }
            self.clock_epoch = self.clock_epoch.checked_add(1).ok_or(Error::Overflow)?;
            self.clock_pending = false;
            self.clock_samples = Some(1);
            for route in self.routes.as_mut_slice() {
                route.epochs.clock = self.clock_epoch;
                route.assembler.discard(route.epochs);
            }
            return Ok(Some(TraceEvent::Timeline {
                program: self.program,
                clock_epoch: self.clock_epoch,
            }));
        }
        if self.clock_samples.is_some() {
            self.clock_samples = Some(2);
        }
        Ok(None)
    }
    fn quarantine_clock(&mut self, pid: u16, reason: &'static str) -> TraceEvent {
        self.clock_pending = false;
        self.clock_quarantined = true;
        self.diagnostic(FaultKind::Transport, Some(pid), Error::Invalid(reason))
    }
}
