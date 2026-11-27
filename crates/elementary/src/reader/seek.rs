//! Seeking: plan, jump, and land on the unit that holds the target.
use super::{Cut, Flow, Format, Next, Phase, Reader, Wait};
use crate::stream::{SeekDetail, TRACK, Track};
use crate::syntax::{Gap, Plan, SeekBasis, SeekContext};

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
    pub(super) progress: Progress,
    pub(super) method: SeekMethod,
    pub(super) basis: SeekBasis,
    pub(super) probes: u32,
    pub(super) scanned: u64,
    /// Offset of the last jump.
    pub(super) probe: u64,
}

/// What to do with the first confirmed frame at or after the input position.
#[derive(Clone, Copy, Debug)]
pub(super) enum Progress {
    Bisect { low: (u64, u64), high: u64 },
    Estimate,
    Forward,
    VerifyEstimate { offset: u64, sample: u64 },
}

impl<F: Format> Reader<F> {
    /// Advance the seek's input position and remember what to do with the next sync.
    pub(super) fn jump(&mut self, mut seeking: Seeking, plan: Plan) -> Result<InputRequest> {
        let (offset, progress) = match plan {
            Plan::Bisect { low, high, .. }
                if high.saturating_sub(low.0) > BISECT_SPAN
                    && seeking.probes < self.limits.seek_probes =>
            {
                (low.0 + (high - low.0) / 2, Progress::Bisect { low, high })
            }
            Plan::Bisect { low, .. } => {
                self.next_pts = i64::try_from(low.1).map_err(|_| Error::Overflow)?;
                (low.0, Progress::Forward)
            }
            Plan::Estimate { offset, .. } => (offset, Progress::Estimate),
            Plan::Scan => (
                self.seek_context()?.audio_start,
                match seeking.progress {
                    verify @ Progress::VerifyEstimate { .. } => verify,
                    _ => Progress::Forward,
                },
            ),
        };
        // An estimate past the end lands on the last bytes and falls back from there.
        let offset = self
            .input
            .capabilities()
            .length
            .map_or(offset, |length| offset.min(length));
        let request = self.input.reset(offset)?;
        if matches!(plan, Plan::Scan) {
            self.next_pts = 0;
        }
        seeking.progress = progress;
        seeking.probe = offset;
        seeking.probes += 1;
        self.seeking = Some(seeking);
        self.phase = Phase::Sync { at: 0, scanned: 0 };
        Ok(request)
    }

    /// A sync found after a jump.
    pub(super) fn seek_found(&mut self, seeking: Seeking, header: F::Header) -> Result<Next> {
        let offset = self.input.position();
        match seeking.progress {
            Progress::Forward | Progress::VerifyEstimate { .. } => {
                self.phase = Phase::Frame(Cut::found(header));
                Ok(Flow::Ready(None))
            }
            Progress::Bisect { low, high } => {
                let sample = self
                    .format
                    .first_sample(&header)
                    .ok_or(Error::Invalid("bisecting seek without frame timestamps"))?;
                let (low, high) = if offset < high && sample <= seeking.target {
                    ((offset, sample), high)
                } else {
                    (low, seeking.probe)
                };
                let request = self.jump(
                    seeking,
                    Plan::Bisect {
                        low,
                        high,
                        method: seeking.method,
                    },
                )?;
                Ok(Flow::Wait(Wait::Step(Step::NeedInput(request))))
            }
            Progress::Estimate => {
                let basis = seeking.basis;
                let context = self.seek_context()?;
                let mut sample = self.format.estimate(offset, context, basis);
                let mut cut = Cut::found(header);
                if sample > seeking.target && F::GAP == Gap::PrependToNext && offset > seeking.probe
                {
                    if basis == SeekBasis::Bitrate {
                        // Confirm whether the bitrate jump reached a packet boundary
                        // or the middle of a frame by reading from the first frame.
                        let mut seeking = seeking;
                        seeking.progress = Progress::VerifyEstimate {
                            offset: seeking.probe,
                            sample: self.format.estimate(seeking.probe, context, basis),
                        };
                        let request = self.jump(seeking, Plan::Scan)?;
                        return Ok(Flow::Wait(Wait::Step(Step::NeedInput(request))));
                    }
                    if let Ok(lead) = usize::try_from(offset - seeking.probe) {
                        sample = self.format.estimate(seeking.probe, context, basis);
                        self.input.reset(seeking.probe)?;
                        cut = Cut::at(lead);
                    }
                }
                self.next_pts = i64::try_from(sample).map_err(|_| Error::Overflow)?;
                self.phase = Phase::Frame(cut);
                Ok(Flow::Ready(Some(Event::Seek(
                    self.land(seeking, self.next_pts)?,
                ))))
            }
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
    pub(super) fn land(&mut self, mut seeking: Seeking, landing: i64) -> Result<SeekReceipt> {
        if let Progress::VerifyEstimate { offset, sample } = seeking.progress
            && (self.input.position() != offset || u64::try_from(landing) != Ok(sample))
        {
            seeking.basis = SeekBasis::Exact;
            seeking.method = SeekMethod::Restart;
        }
        let time_base = self
            .stream()
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
            replays_from_start: self.audio_start == Some(self.input.position()),
        };
        self.last_seek = Some(SeekDetail {
            receipt,
            basis: seeking.basis,
            probes: seeking.probes,
        });
        self.seeking = None;
        Ok(receipt)
    }

    /// Plan a seek and make its first jump.
    pub(super) fn start_seek(&mut self, request: SeekRequest) -> Result<InputRequest> {
        if request.track != TRACK {
            return Err(Error::Invalid("seek track is not the stream's"));
        }
        let time_base = self
            .stream()
            .map(Track::time_base)
            .ok_or(Error::Invalid("seek before the stream is open"))?;
        let context = self.seek_context()?;
        // A seek reads the first sequence again.
        self.new_sequence = false;
        let target = request
            .target
            .to_i64_ticks(time_base, Rounding::Floor)?
            .max(0) as u64;
        // An estimated landing for playback aims early enough for the preroll and one
        // frame of error; exact plans read forward to the target either way.
        let aim = match (request.policy, self.opened.as_ref()) {
            (SeekPolicy::PlaybackPreroll, Some((_, reference))) => target.saturating_sub(
                u64::from(F::PREROLL + 1) * u64::from(self.format.samples(reference)),
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
        self.jump(
            Seeking {
                requested: request.target,
                target,
                policy: request.policy,
                progress: Progress::Forward,
                method,
                basis,
                probes: 0,
                scanned: 0,
                probe: 0,
            },
            plan,
        )
    }
}
