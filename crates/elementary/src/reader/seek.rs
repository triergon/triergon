//! Seeking: plan, jump, and land on the unit that holds the target.
use super::{Cut, Flow, Format, Next, Phase, Reader, Wait};
use crate::stream::{SeekDetail, TRACK, Track};
use crate::syntax::{Plan, SeekBasis, SeekContext};

use triergon_core::container::{Event, SeekMethod, SeekPolicy, SeekReceipt, SeekRequest};
use triergon_core::{Error, InputRequest, Result, Rounding, Step, Time};

/// A bisecting seek stops jumping once its range is this short and reads forward.
const BISECT_SPAN: u64 = 64 << 10;

/// A seek in progress.
#[derive(Clone, Copy, Debug)]
pub(super) struct Seeking {
    pub(super) requested: Time,
    pub(super) target: u64,
    pub(super) policy: SeekPolicy,
    pub(super) plan: Plan,
    pub(super) method: SeekMethod,
    pub(super) basis: SeekBasis,
    pub(super) probes: u32,
    pub(super) scanned: u64,
    /// Offset of the last jump.
    pub(super) probe: u64,
    /// Reading forward to the frame that holds the target.
    pub(super) forward: bool,
}

impl<F: Format> Reader<F> {
    pub(super) fn set_plan(&mut self, plan: Plan) {
        if let Some(seeking) = self.seeking.as_mut() {
            seeking.plan = plan;
        }
    }

    /// Jump for the current plan and return the request for the new position.
    pub(super) fn jump(&mut self) -> Result<InputRequest> {
        let audio_start = self.audio_start.unwrap_or(0);
        let probes = self.limits.seek_probes;
        let seeking = self
            .seeking
            .as_mut()
            .ok_or(Error::Invalid("no seek in progress"))?;
        let offset = match seeking.plan {
            Plan::Bisect { low, high, .. }
                if high.saturating_sub(low.0) > BISECT_SPAN && seeking.probes < probes =>
            {
                seeking.forward = false;
                low.0 + (high - low.0) / 2
            }
            Plan::Bisect { low, .. } => {
                seeking.forward = true;
                self.next_pts = i64::try_from(low.1).map_err(|_| Error::Overflow)?;
                low.0
            }
            Plan::Estimate { offset, .. } => {
                seeking.forward = false;
                offset
            }
            Plan::Scan => {
                seeking.forward = true;
                self.next_pts = 0;
                audio_start
            }
        };
        // An estimate past the end lands on the last bytes and falls back from there.
        let offset = self
            .input
            .capabilities()
            .length
            .map_or(offset, |length| offset.min(length));
        seeking.probe = offset;
        seeking.probes += 1;
        self.phase = Phase::Sync { at: 0, scanned: 0 };
        self.input.reset(offset)
    }

    /// A sync found after a jump.
    pub(super) fn seek_found(&mut self, header: F::Header) -> Result<Next> {
        let seeking = self.seeking.ok_or(Error::Invalid("no seek in progress"))?;
        let offset = self.input.position();
        if seeking.forward {
            self.phase = Phase::Frame(Cut::found(header));
            return Ok(Flow::Ready(None));
        }
        match seeking.plan {
            Plan::Bisect { low, high, method } => {
                let sample = self
                    .format
                    .first_sample(&header)
                    .ok_or(Error::Invalid("bisecting seek without frame timestamps"))?;
                let plan = if offset < high && sample <= seeking.target {
                    Plan::Bisect {
                        low: (offset, sample),
                        high,
                        method,
                    }
                } else {
                    Plan::Bisect {
                        low,
                        high: seeking.probe,
                        method,
                    }
                };
                self.set_plan(plan);
                let request = self.jump()?;
                Ok(Flow::Wait(Wait::Step(Step::NeedInput(request))))
            }
            Plan::Estimate { basis, .. } => {
                let context = self.seek_context()?;
                let sample = self.format.estimate(offset, context, basis);
                self.next_pts = i64::try_from(sample).map_err(|_| Error::Overflow)?;
                self.phase = Phase::Frame(Cut::found(header));
                Ok(Flow::Ready(Some(Event::Seek(self.land(self.next_pts)?))))
            }
            Plan::Scan => Err(Error::Invalid("restart seek lost its direction")),
        }
    }

    pub(super) fn seek_context(&self) -> Result<SeekContext> {
        Ok(SeekContext {
            audio_start: self
                .audio_start
                .ok_or(Error::Invalid("seek before the stream is open"))?,
            audio_end: self.audio_end.or(self.input.capabilities().length),
        })
    }

    /// Finish the seek at a unit starting at `landing`.
    pub(super) fn land(&mut self, landing: i64) -> Result<SeekReceipt> {
        let seeking = self
            .seeking
            .take()
            .ok_or(Error::Invalid("no seek in progress"))?;
        let time_base = self
            .track
            .as_ref()
            .map(Track::time_base)
            .ok_or(Error::Invalid("seek before the stream is open"))?;
        let landing_time = Time::from_ticks(i128::from(landing), time_base)?;
        let discard_until = match seeking.policy {
            SeekPolicy::PlaybackPreroll => seeking.requested.max(landing_time),
            SeekPolicy::PreviousRandomAccess => landing_time,
        };
        let receipt = SeekReceipt {
            requested: seeking.requested,
            landing: landing_time,
            discard_until,
            method: seeking.method,
            scanned_bytes: seeking.scanned,
            replays_from_start: false,
        };
        self.last_seek = Some(SeekDetail {
            receipt,
            basis: seeking.basis,
            probes: seeking.probes,
        });
        Ok(receipt)
    }

    /// Start a seek; the reader delivers a [`Event::Seek`] receipt before the first
    /// packet at the landing point.
    pub fn request_seek(&mut self, request: SeekRequest) -> Result<InputRequest> {
        if request.track != TRACK {
            return Err(Error::Invalid("seek track is not the stream's"));
        }
        let time_base = self
            .track
            .as_ref()
            .map(Track::time_base)
            .ok_or(Error::Invalid("seek before the stream is open"))?;
        let context = self.seek_context()?;
        let target = request
            .target
            .to_i64_ticks(time_base, Rounding::Floor)?
            .max(0) as u64;
        // An estimated landing for playback aims early enough for the preroll and one
        // frame of error; exact plans read forward to the target either way.
        let aim = match (request.policy, self.reference) {
            (SeekPolicy::PlaybackPreroll, Some(reference)) => target.saturating_sub(
                u64::from(F::PREROLL + 1) * u64::from(self.format.samples(&reference)),
            ),
            _ => target,
        };
        let plan = if self.input.capabilities().random_access {
            self.format.plan_seek(aim, context)
        } else {
            Plan::Scan
        };
        let (method, basis) = match plan {
            Plan::Bisect { method, .. } => (method, SeekBasis::Exact),
            Plan::Estimate { basis, .. } => (
                if basis == SeekBasis::Toc {
                    SeekMethod::Index
                } else {
                    SeekMethod::BoundedScan
                },
                basis,
            ),
            Plan::Scan => (SeekMethod::Restart, SeekBasis::Exact),
        };
        self.seeking = Some(Seeking {
            requested: request.target,
            target,
            policy: request.policy,
            plan,
            method,
            basis,
            probes: 0,
            scanned: 0,
            probe: 0,
            forward: false,
        });
        self.jump()
    }
}
