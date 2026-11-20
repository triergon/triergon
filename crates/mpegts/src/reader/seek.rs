//! Bounded source seeks in the exact presentation clock.
use super::{Packet, Reader, ReaderEvent};
use crate::{
    SourceEpochs, TraceEvent,
    clock::{SourceTimePolicy, SourceTimestamp},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::container::{
    CodecId, Event, PacketInfo, RandomAccess, ReaderStep, SeekMethod, SeekPolicy, SeekReceipt,
    SeekRequest, TrackId, TrackInfo,
};
use triergon_core::{BudgetVec, Error, InputRequest, Result, Step, Time, WorkBudget};

#[derive(Clone, Copy)]
struct Landing {
    time: Time,
    offset: u64,
    epochs: SourceEpochs,
}

#[derive(Default)]
struct Scan {
    landing: Option<Landing>,
    epochs: Option<SourceEpochs>,
    previous: Option<i128>,
}

enum Phase {
    Scan(Scan),
    Replay {
        landing: Landing,
        scanned_bytes: u64,
        skipped: bool,
    },
    Deliver(Packet),
}

pub(super) struct Seeking {
    request: SeekRequest,
    phase: Phase,
}

#[derive(Clone, Copy)]
pub(super) enum PictureDelivery {
    Full,
    Probe,
    Landing(u64),
}

pub(super) struct PictureProbe {
    pub offset: u64,
    pub epochs: SourceEpochs,
    pub stamp: Option<SourceTimestamp>,
    pub random_access: bool,
    pub has_vcl: bool,
}

impl Seeking {
    pub(super) fn scan_track(&self) -> Option<TrackId> {
        matches!(self.phase, Phase::Scan(_)).then_some(self.request.track)
    }

    pub(super) fn picture_delivery(&self, track: TrackId) -> PictureDelivery {
        match self.phase {
            Phase::Scan(_) => PictureDelivery::Probe,
            Phase::Replay { landing, .. } if track == self.request.track => {
                PictureDelivery::Landing(landing.offset)
            }
            Phase::Replay { .. } => PictureDelivery::Probe,
            Phase::Deliver(_) => PictureDelivery::Full,
        }
    }
}

impl Scan {
    fn observe(
        &mut self,
        picture: PictureProbe,
        target: Time,
        policy: SourceTimePolicy,
    ) -> Result<()> {
        let PictureProbe {
            offset,
            epochs,
            stamp,
            random_access,
            ..
        } = picture;
        if *self.epochs.get_or_insert(epochs) != epochs {
            return Err(Error::Unsupported("MPEG-TS seek across source epochs"));
        }
        if let Some(ticks) = stamp.and_then(|pts| pts.ticks90)
            && let Some(previous) = self.previous.replace(ticks)
            && policy == SourceTimePolicy::RelativeEpochZero
            && ticks.abs_diff(previous) >= (crate::clock::PERIOD90 / 2) as u128
        {
            return Err(Error::Unsupported(
                "MPEG-TS seek across an ambiguous clock wrap",
            ));
        }
        if random_access {
            let time = stamp.and_then(|pts| pts.exact).ok_or(Error::Unsupported(
                "MPEG-TS seek point without a resolved PTS",
            ))?;
            if time <= target && self.landing.is_none_or(|old| time >= old.time) {
                self.landing = Some(Landing {
                    time,
                    offset,
                    epochs,
                });
            }
        }
        Ok(())
    }
}

impl Reader {
    pub(super) fn observe_seek_picture(&mut self, picture: PictureProbe) -> Result<()> {
        if !picture.has_vcl {
            return Ok(());
        }
        let seek = &mut self.seek.as_mut_slice()[0];
        match &mut seek.phase {
            Phase::Scan(scan) => {
                scan.observe(picture, seek.request.target, self.limits.time_policy)
            }
            Phase::Replay { skipped, .. } => {
                *skipped = true;
                Ok(())
            }
            Phase::Deliver(_) => unreachable!("a seek probe precedes landing"),
        }
    }

    /// Seek a selected AVC or HEVC track in one length-known, seekable entity.
    /// The reader scans for the previous coded random-access picture, then
    /// replays from the start to preserve transport, clock and PES provenance.
    /// The caller supplies any decoder configuration needed at the landing.
    /// Requests from a previously continued entity or with unresolved clock
    /// epochs are refused.
    #[cold]
    pub fn request_seek(&mut self, request: SeekRequest) -> Result<InputRequest> {
        self.healthy()?;
        let length = self
            .caps
            .length
            .filter(|_| self.caps.random_access && !self.continued_entity)
            .ok_or(Error::Unsupported(
                "MPEG-TS seek needs one length-known seekable entity",
            ))?;
        if length > self.limits.maximum_seek_scan_bytes || self.limits.maximum_seek_scan_bytes == 0
        {
            return Err(Error::Policy("MPEG-TS seek scan bytes"));
        }
        if self.limits.time_policy == SourceTimePolicy::Unanchored {
            return Err(Error::Unsupported("MPEG-TS seek needs a resolved clock"));
        }
        let pid = u16::try_from(request.track.0)
            .ok()
            .filter(|pid| crate::ELEMENTARY_PID_RANGE.contains(pid) && self.selected_track(*pid))
            .ok_or(Error::Invalid("MPEG-TS seek track is not selected"))?;
        if let Some(track) = self.tracks().iter().find(|track| track.stream().pid == pid)
            && !matches!(track.codec(), CodecId::H264 | CodecId::Hevc)
        {
            return Err(Error::Unsupported("MPEG-TS seek requires AVC or HEVC"));
        }
        let mut seek = BudgetVec::new(&self.pool);
        seek.try_reserve_exact(1)?;
        seek.try_push(Seeking {
            request,
            phase: Phase::Scan(Scan::default()),
        })
        .map_err(|rejected| rejected.error)?;
        let input = self.reset()?;
        self.seek = seek;
        Ok(input)
    }

    #[cold]
    #[inline(never)]
    pub(super) fn advance_seek(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ReaderStep<Self>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if work.remaining() == 0 {
                return Ok(Step::Yield);
            }
            let seek = &self.seek.as_slice()[0];
            let request = seek.request;
            if matches!(seek.phase, Phase::Deliver(_)) {
                work.try_charge(1);
                let Phase::Deliver(packet) = self.seek.pop().expect("pending packet").phase else {
                    unreachable!("seek delivery");
                };
                self.seek = BudgetVec::new(&self.pool);
                return Ok(Step::Ready(Event::Packet(packet)));
            }
            let step = self.advance::<true>(work, cancel, |_| ())?;
            match (&mut self.seek.as_mut_slice()[0].phase, step) {
                (Phase::Scan(_), Step::Ready(Event::Packet(packet))) => {
                    if packet.track() == request.track {
                        return Err(Error::Unsupported("MPEG-TS seek requires AVC or HEVC"));
                    }
                }
                (Phase::Scan(_), Step::Ready(Event::Other(_))) => {}
                (Phase::Scan(scan), Step::End(end)) => {
                    let landing = scan.landing.ok_or(Error::Unsupported(
                        "MPEG-TS has no random-access picture at or before the target",
                    ))?;
                    let mut replay =
                        Self::new(&self.pool, self.source, self.caps, self.limits, self.window)?;
                    let input = replay.request()?;
                    self.seek.as_mut_slice()[0].phase = Phase::Replay {
                        landing,
                        scanned_bytes: end.offset,
                        skipped: false,
                    };
                    std::mem::swap(&mut replay.packet_tracks, &mut self.packet_tracks);
                    std::mem::swap(&mut replay.seek, &mut self.seek);
                    *self = replay;
                    return Ok(Step::NeedInput(input));
                }
                (
                    Phase::Replay {
                        landing,
                        scanned_bytes,
                        skipped,
                    },
                    Step::Ready(Event::Packet(packet)),
                ) => {
                    if packet.track() != request.track
                        || packet.elementary_range().start != landing.offset
                        || packet.epochs() != landing.epochs
                    {
                        *skipped = true;
                        continue;
                    }
                    if packet.random_access() != RandomAccess::Sync
                        || packet.clock().and_then(|clock| clock.pts.exact) != Some(landing.time)
                    {
                        return Err(Error::SourceMismatch);
                    }
                    let receipt = SeekReceipt {
                        requested: request.target,
                        landing: landing.time,
                        discard_until: match request.policy {
                            SeekPolicy::PreviousRandomAccess => landing.time,
                            SeekPolicy::PlaybackPreroll => request.target,
                        },
                        method: SeekMethod::BoundedScan,
                        scanned_bytes: *scanned_bytes,
                        replays_from_start: !*skipped,
                    };
                    self.seek.as_mut_slice()[0].phase = Phase::Deliver(packet);
                    return Ok(Step::Ready(Event::Seek(receipt)));
                }
                (Phase::Replay { .. }, Step::Ready(Event::Other(event))) => {
                    if matches!(&event, ReaderEvent::Source(source)
                        if matches!(source.event(), TraceEvent::Pat(_) | TraceEvent::ProgramMap { .. }))
                    {
                        return Ok(Step::Ready(Event::Other(event)));
                    }
                }
                (Phase::Replay { .. }, Step::End(_)) => return Err(Error::SourceMismatch),
                (_, progress @ (Step::NeedInput(_) | Step::Yield | Step::Backpressure(_))) => {
                    return Ok(progress);
                }
                _ => unreachable!("reader step during seek"),
            }
        }
    }
}
