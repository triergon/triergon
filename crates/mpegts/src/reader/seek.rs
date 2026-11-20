//! Bounded source seeks in the exact presentation clock.
mod retained;

use super::{AccessUnit, Packet, PacketTracks, Reader, ReaderEvent};
use crate::{SourceEpochs, TraceEvent, clock::SourceTimePolicy};
use retained::{Retain, RetainedOutput};
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
    Retained(RetainedOutput),
}

pub(super) struct Seeking {
    request: SeekRequest,
    phase: Phase,
    retain: Option<Retain>,
    full_scan: bool,
}

#[derive(Clone, Copy)]
pub(super) enum PictureDelivery {
    Full,
    Scan,
    Discard,
    Landing(u64),
}

pub(super) struct PictureProbe {
    pub offset: u64,
    pub epochs: SourceEpochs,
    pub ticks: Option<i128>,
    pub time: Option<Time>,
    pub random_access: bool,
    pub has_vcl: bool,
}

impl Seeking {
    pub(super) fn scan_track(&self) -> Option<TrackId> {
        matches!(self.phase, Phase::Scan(_)).then_some(self.request.track)
    }

    pub(super) fn publishes_tables(&self) -> bool {
        matches!(self.phase, Phase::Replay { .. })
    }

    pub(super) fn picture_delivery(&self, track: TrackId) -> PictureDelivery {
        match self.phase {
            Phase::Scan(_) if self.full_scan => PictureDelivery::Full,
            Phase::Scan(_) => PictureDelivery::Scan,
            Phase::Replay { landing, .. } if track == self.request.track => {
                PictureDelivery::Landing(landing.offset)
            }
            Phase::Replay { .. } => PictureDelivery::Discard,
            Phase::Deliver(_) | Phase::Retained(_) => PictureDelivery::Full,
        }
    }
}

impl Scan {
    fn observe(
        &mut self,
        picture: PictureProbe,
        target: Time,
        policy: SourceTimePolicy,
    ) -> Result<bool> {
        let PictureProbe {
            offset,
            epochs,
            ticks,
            time,
            random_access,
            ..
        } = picture;
        if *self.epochs.get_or_insert(epochs) != epochs {
            return Err(Error::Unsupported("MPEG-TS seek across source epochs"));
        }
        if let Some(ticks) = ticks
            && let Some(previous) = self.previous.replace(ticks)
            && policy == SourceTimePolicy::RelativeEpochZero
            && ticks.abs_diff(previous) >= (crate::clock::PERIOD90 / 2) as u128
        {
            return Err(Error::Unsupported(
                "MPEG-TS seek across an ambiguous clock wrap",
            ));
        }
        if random_access {
            let time = time.ok_or(Error::Unsupported(
                "MPEG-TS seek point without a resolved PTS",
            ))?;
            if time <= target && self.landing.is_none_or(|old| time >= old.time) {
                self.landing = Some(Landing {
                    time,
                    offset,
                    epochs,
                });
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn seek_result(
    request: SeekRequest,
    landing: Landing,
    scanned_bytes: u64,
    skipped: bool,
) -> SeekReceipt {
    SeekReceipt {
        requested: request.target,
        landing: landing.time,
        discard_until: match request.policy {
            SeekPolicy::PreviousRandomAccess => landing.time,
            SeekPolicy::PlaybackPreroll => request.target,
        },
        method: SeekMethod::BoundedScan,
        scanned_bytes,
        replays_from_start: !skipped,
    }
}

impl Reader {
    pub(super) fn observe_seek_picture(&mut self, picture: PictureProbe) -> Result<()> {
        if !picture.has_vcl {
            return Ok(());
        }
        let seek = &mut self.seek.as_mut_slice()[0];
        match &mut seek.phase {
            Phase::Scan(scan) => scan
                .observe(picture, seek.request.target, self.limits.time_policy)
                .map(|_| ()),
            Phase::Replay { skipped, .. } => {
                *skipped = true;
                Ok(())
            }
            Phase::Deliver(_) | Phase::Retained(_) => unreachable!("a seek probe precedes landing"),
        }
    }

    /// Seek a selected AVC or HEVC track in one length-known, seekable entity.
    /// The reader scans for the previous coded random-access picture, then
    /// delivers original transport, clock and PES provenance from the landing.
    /// Short single-track suffixes can be retained as the reader scans.
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
        let full_scan = length <= 256 * 1024
            && matches!(&self.packet_tracks, PacketTracks::Selected(pids) if pids.len() == 1);
        seek.try_push(Seeking {
            request,
            phase: Phase::Scan(Scan::default()),
            retain: full_scan.then(|| Retain::new(&self.pool)),
            full_scan,
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
            let seek = &mut self.seek.as_mut_slice()[0];
            let request = seek.request;
            if let Phase::Retained(output) = &mut seek.phase {
                work.try_charge(1);
                if let Some(event) = output.next(&mut self.tracks) {
                    return Ok(Step::Ready(event));
                }
                self.seek = BudgetVec::new(&self.pool);
                return Ok(Step::End(
                    self.end.ok_or(Error::Invalid("retained seek end"))?,
                ));
            }
            if matches!(seek.phase, Phase::Deliver(_)) {
                work.try_charge(1);
                let Phase::Deliver(packet) = self.seek.pop().expect("pending packet").phase else {
                    unreachable!("seek delivery");
                };
                self.seek = BudgetVec::new(&self.pool);
                return Ok(Step::Ready(Event::Packet(packet)));
            }
            let step = if seek.retain.is_some() {
                match self.advance::<false, false>(work, cancel, |_| ()) {
                    Ok(Step::Backpressure(_)) | Err(Error::Backpressure { .. }) => {
                        self.seek.as_mut_slice()[0].retain = None;
                        self.storage = None;
                        continue;
                    }
                    step => step?,
                }
            } else {
                self.advance::<true, true>(work, cancel, |_| ())?
            };
            let seek = &mut self.seek.as_mut_slice()[0];
            match (&mut seek.phase, step) {
                (Phase::Scan(scan), Step::Ready(Event::Packet(packet))) => {
                    if packet.track() == request.track {
                        let AccessUnit::Video(unit) = packet.access_unit() else {
                            return Err(Error::Unsupported("MPEG-TS seek requires AVC or HEVC"));
                        };
                        let clock = packet.clock();
                        let better = scan.observe(
                            PictureProbe {
                                offset: unit.range.start,
                                epochs: packet.epochs(),
                                ticks: clock.and_then(|clock| clock.pts.ticks90),
                                time: clock.and_then(|clock| clock.pts.exact),
                                random_access: unit.random_access,
                                has_vcl: true,
                            },
                            request.target,
                            self.limits.time_policy,
                        )?;
                        if let Some(retain) = &mut seek.retain
                            && !retain.packet(packet, better)
                        {
                            seek.retain = None;
                        }
                    }
                }
                (Phase::Scan(_), Step::Ready(Event::Other(event))) => {
                    if let Some(retain) = &mut seek.retain
                        && !retain.other(event, !self.tracks.is_empty())
                    {
                        seek.retain = None;
                    }
                }
                (Phase::Scan(scan), Step::End(end)) => {
                    let landing = scan.landing.ok_or(Error::Unsupported(
                        "MPEG-TS has no random-access picture at or before the target",
                    ))?;
                    if let Some(retain) = seek.retain.take() {
                        let result = seek_result(request, landing, end.offset, retain.skipped);
                        let tracks =
                            std::mem::replace(&mut self.tracks, BudgetVec::new(&self.pool));
                        seek.phase = Phase::Retained(retain.finish(result, tracks));
                        continue;
                    }
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
                    let receipt = seek_result(request, *landing, *scanned_bytes, *skipped);
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
