//! Bounded source seeks in the exact presentation clock.
mod retained;

use super::{AccessUnit, Packet, PacketTracks, Reader, ReaderEvent, Shared};
use crate::{SourceEpochs, TraceEvent, clock::SourceTimePolicy};
use retained::{Retain, RetainedOutput};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::container::{
    CodecId, Event, PacketInfo, ReaderStep, SeekMethod, SeekPolicy, SeekReceipt, SeekRequest,
    TrackId, TrackInfo,
};
use triergon_core::{Budget, BudgetVec, Error, InputRequest, Result, Step, Time, WorkBudget};
use triergon_framing::MpegLayer;

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
    index: Option<IndexBuilder>,
}

pub(super) struct SeekIndex {
    track: TrackId,
    points: BudgetVec<Landing>,
}

impl SeekIndex {
    fn previous(&self, target: Time) -> Result<Landing> {
        let points = self.points.as_slice();
        let after = points.partition_point(|point| point.time <= target);
        points[..after].last().copied().ok_or(Error::Unsupported(
            "MPEG-TS seek index has no point at or before the target",
        ))
    }
}

struct IndexBuilder {
    points: BudgetVec<Landing>,
    maximum: usize,
}

impl IndexBuilder {
    fn record(&mut self, point: Landing) -> bool {
        if let Some(last) = self.points.as_mut_slice().last_mut() {
            match point.time.cmp(&last.time) {
                std::cmp::Ordering::Less => return false,
                std::cmp::Ordering::Equal => {
                    *last = point;
                    return true;
                }
                std::cmp::Ordering::Greater => (),
            }
        }
        self.points.len() < self.maximum && self.points.try_push(point).is_ok()
    }
}

enum Phase {
    Scan(Scan),
    Replay {
        landing: Landing,
        scanned_bytes: Option<u64>,
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

pub(super) struct SeekProbe {
    pub offset: u64,
    pub epochs: SourceEpochs,
    pub ticks: Option<i128>,
    pub time: Option<Time>,
    pub random_access: bool,
    pub has_vcl: bool,
}

fn seekable_audio(codec: CodecId<'_>) -> bool {
    matches!(
        codec,
        CodecId::Aac | CodecId::Ac3 | CodecId::Eac3 | CodecId::MpegAudio
    )
}

fn packet_probe(packet: &Packet) -> Result<SeekProbe> {
    let clock = packet.clock();
    let time = clock.and_then(|clock| clock.pts.exact);
    let (offset, random_access) = match packet.access_unit() {
        AccessUnit::Video(unit) => (unit.range.start, unit.random_access),
        AccessUnit::Adts(frame) => (frame.source().offset, time.is_some()),
        // E-AC-3 access units begin with the independent syncframe and
        // include its associated dependent substreams (A/52 Annex E §2.3.1).
        AccessUnit::Ac3(frame) => (frame.facts().offset, time.is_some()),
        AccessUnit::MpegAudio(frame) => {
            // Layer III needs bit-reservoir state from earlier source frames.
            if frame.header().layer == MpegLayer::III {
                return Err(Error::Unsupported(
                    "MPEG-TS Layer III seek needs bit reservoir preroll",
                ));
            }
            (frame.elementary_range().start, time.is_some())
        }
        AccessUnit::AdtsFragment(fragment) => (fragment.source().offset, false),
        _ => {
            return Err(Error::Unsupported(
                "MPEG-TS seek requires a supported video or audio track",
            ));
        }
    };
    Ok(SeekProbe {
        offset,
        epochs: packet.epochs(),
        ticks: clock.and_then(|clock| clock.pts.ticks90),
        time,
        random_access,
        has_vcl: true,
    })
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
    fn finish_index(&mut self, pool: &Budget, track: TrackId) -> Option<Shared<SeekIndex>> {
        let index = self.index.take()?;
        let charge = Shared::<SeekIndex>::reserve(pool).ok()?;
        Some(Shared::new(
            SeekIndex {
                track,
                points: index.points,
            },
            charge,
        ))
    }

    fn observe(
        &mut self,
        probe: SeekProbe,
        target: Time,
        policy: SourceTimePolicy,
    ) -> Result<bool> {
        let SeekProbe {
            offset,
            epochs,
            ticks,
            time,
            random_access,
            ..
        } = probe;
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
            let point = Landing {
                time,
                offset,
                epochs,
            };
            if let Some(index) = &mut self.index
                && !index.record(point)
            {
                self.index = None;
            }
            if time <= target && self.landing.is_none_or(|old| time >= old.time) {
                self.landing = Some(point);
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn seek_result(
    request: SeekRequest,
    landing: Landing,
    scanned_bytes: Option<u64>,
    skipped: bool,
) -> SeekReceipt {
    SeekReceipt {
        requested: request.target,
        landing: landing.time,
        discard_until: match request.policy {
            SeekPolicy::PreviousRandomAccess => landing.time,
            SeekPolicy::PlaybackPreroll => request.target,
        },
        method: if scanned_bytes.is_some() {
            SeekMethod::BoundedScan
        } else {
            SeekMethod::Index
        },
        scanned_bytes: scanned_bytes.unwrap_or(0),
        replays_from_start: !skipped,
    }
}

impl Reader {
    pub(super) fn observe_seek_picture(&mut self, picture: SeekProbe) -> Result<()> {
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

    /// Seek a selected AVC, HEVC, ADTS AAC-LC, AC-3, E-AC-3 or MPEG audio
    /// Layer I or II track in one length-known, seekable entity. The reader lands on the last coded
    /// random-access picture or complete audio frame carrying the greatest
    /// resolved PTS at or before the target. It delivers original transport,
    /// clocks and PES provenance, including untimed frames after the landing.
    /// Short single-track suffixes can be retained as the reader scans.
    /// The caller supplies any decoder configuration needed at the landing.
    /// Requests from a previously continued entity or with unresolved clock
    /// epochs are refused. A completed index for the same track skips the
    /// landing scan while replay still reconstructs transport state.
    #[cold]
    pub fn request_seek(&mut self, request: SeekRequest) -> Result<InputRequest> {
        self.begin_seek(request, None)
    }

    /// Seek and retain an index of up to `maximum_entries` distinct landing
    /// times for later seeks to this track in the same entity. The source must
    /// remain stable between requests. A reset, close or continuation drops
    /// the index. If landing times are out of order or the index exceeds its
    /// entry or storage bound, the current seek still completes normally.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [Self::request_seek], or an invalid-limit
    /// error if `maximum_entries` is zero.
    #[cold]
    pub fn request_seek_and_index(
        &mut self,
        request: SeekRequest,
        maximum_entries: usize,
    ) -> Result<InputRequest> {
        if maximum_entries == 0 {
            return Err(Error::Invalid("MPEG-TS seek index entry limit"));
        }
        self.begin_seek(request, Some(maximum_entries))
    }

    fn begin_seek(
        &mut self,
        request: SeekRequest,
        index_entries: Option<usize>,
    ) -> Result<InputRequest> {
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
            && !seekable_audio(track.codec())
        {
            return Err(Error::Unsupported(
                "MPEG-TS seek requires a supported video or audio track",
            ));
        }
        let mut seek = BudgetVec::new(&self.pool);
        seek.try_reserve_exact(1)?;
        let index = self
            .seek_index
            .as_ref()
            .filter(|index| index_entries.is_none() && index.track == request.track)
            .cloned();
        let (phase, retain) = if let Some(index) = &index {
            (
                Phase::Replay {
                    landing: index.previous(request.target)?,
                    scanned_bytes: None,
                    skipped: false,
                },
                None,
            )
        } else {
            let single_track =
                matches!(&self.packet_tracks, PacketTracks::Selected(pids) if pids.len() == 1);
            let retain = (length <= 256 * 1024 && single_track).then(|| Retain::new(&self.pool));
            (
                Phase::Scan(Scan {
                    index: index_entries.map(|maximum| IndexBuilder {
                        points: BudgetVec::new(&self.pool),
                        maximum,
                    }),
                    ..Scan::default()
                }),
                retain,
            )
        };
        seek.try_push(Seeking {
            request,
            phase,
            full_scan: retain.is_some(),
            retain,
        })
        .map_err(|rejected| rejected.error)?;
        let input = self.reset()?;
        self.seek_index = index;
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
                self.advance::<false, false>(work, cancel, |_| ())
            } else {
                self.advance::<true, true>(work, cancel, |_| ())
            };
            let seek = &mut self.seek.as_mut_slice()[0];
            if matches!(
                step,
                Ok(Step::Backpressure(_)) | Err(Error::Backpressure { .. })
            ) {
                let dropped_index = if let Phase::Scan(scan) = &mut seek.phase {
                    scan.index.take().is_some()
                } else {
                    self.seek_index.take().is_some()
                };
                if dropped_index || seek.retain.take().is_some() {
                    self.storage = None;
                    continue;
                }
            }
            let step = step?;
            match (&mut seek.phase, step) {
                (Phase::Scan(scan), Step::Ready(Event::Packet(packet))) => {
                    if packet.track() == request.track {
                        let better = scan.observe(
                            packet_probe(&packet)?,
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
                    let landing = scan.landing.ok_or_else(|| {
                        let audio = self.tracks.as_slice().iter().any(|track| {
                            track.id() == request.track && seekable_audio(track.codec())
                        });
                        Error::Unsupported(if audio {
                            "MPEG-TS has no timestamped audio frame at or before the target"
                        } else {
                            "MPEG-TS has no random-access picture at or before the target"
                        })
                    })?;
                    if let Some(retain) = seek.retain.take() {
                        self.seek_index = scan.finish_index(&self.pool, request.track);
                        let result =
                            seek_result(request, landing, Some(end.offset), retain.skipped);
                        let tracks =
                            std::mem::replace(&mut self.tracks, BudgetVec::new(&self.pool));
                        seek.phase = Phase::Retained(retain.finish(result, tracks));
                        continue;
                    }
                    let mut replay = match Self::new(
                        &self.pool,
                        self.source,
                        self.caps,
                        self.limits,
                        self.window,
                    ) {
                        Err(Error::Backpressure { .. }) if scan.index.take().is_some() => continue,
                        replay => replay?,
                    };
                    let input = replay.request()?;
                    replay.seek_index = scan.finish_index(&self.pool, request.track);
                    self.seek.as_mut_slice()[0].phase = Phase::Replay {
                        landing,
                        scanned_bytes: Some(end.offset),
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
                    let probe = packet_probe(&packet).map_err(|_| Error::SourceMismatch)?;
                    if !probe.random_access || probe.time != Some(landing.time) {
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
