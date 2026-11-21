//! Bounded source seeks in the exact presentation clock.
mod dts_hd;
mod physical;
mod retained;

use super::{AccessUnit, MpegAudioFrame, Packet, PacketTracks, Reader, ReaderEvent, Shared};
use crate::stream::TraceEvent;
use crate::{SourceEpochs, clock::SourceTimePolicy};
use retained::{Retain, RetainedOutput};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_bitstream::{BitCursor, BitSource};
use triergon_core::container::{
    CodecId, Event, PacketInfo, ReaderStep, SeekMethod, SeekPolicy, SeekReceipt, SeekRequest,
    TrackId, TrackInfo,
};
use triergon_core::{
    Budget, BudgetVec, Charged, Error, InputRequest, Result, SourceCapabilities, SourceId, Step,
    Time, WorkBudget,
};
use triergon_framing::{
    aac, dts,
    mpeg_audio::{MpegChannelMode, MpegLayer, MpegVersion},
};

/// A length-known, seekable part of the original contiguous transport stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekEntity {
    /// The source entity this part is read from.
    pub source: SourceId,
    /// The entity's length in bytes.
    pub length: u64,
}

impl SeekEntity {
    pub(super) const fn capabilities(self) -> SourceCapabilities {
        SourceCapabilities {
            length: Some(self.length),
            random_access: true,
        }
    }
}

pub(super) struct EntityPlan {
    entries: BudgetVec<SeekEntity>,
    total: u64,
    next: usize,
}

impl EntityPlan {
    fn new(
        pool: &Budget,
        entities: &[SeekEntity],
        maximum_bytes: u64,
    ) -> Result<Charged<Box<Self>>> {
        if entities.is_empty() || entities.len() > 4096 {
            return Err(Error::Policy("MPEG-TS seek entity count"));
        }
        let mut total = 0_u64;
        for (i, entity) in entities.iter().enumerate() {
            if entity.length == 0 || (i != 0 && entity.source == entities[i - 1].source) {
                return Err(Error::Invalid("MPEG-TS seek entity layout"));
            }
            total = total.checked_add(entity.length).ok_or(Error::Overflow)?;
        }
        if total > maximum_bytes {
            return Err(Error::Policy("MPEG-TS seek scan bytes"));
        }
        let mut entries = BudgetVec::new(pool);
        entries.try_extend_from_slice(entities)?;
        Charged::boxed(
            pool,
            Self {
                entries,
                total,
                next: 1,
            },
        )
    }

    pub(super) fn entries_bytes(&self) -> Result<usize> {
        self.entries
            .capacity()
            .checked_mul(size_of::<SeekEntity>())
            .ok_or(Error::Overflow)
    }

    fn same_source(&self, other: &Self) -> bool {
        self.entries.as_slice() == other.entries.as_slice()
    }

    fn first(&self) -> (SourceId, SourceCapabilities) {
        let first = self.entries.as_slice()[0];
        (first.source, first.capabilities())
    }

    pub(super) fn next(&self) -> Option<SeekEntity> {
        self.entries.as_slice().get(self.next).copied()
    }

    pub(super) fn advance(&mut self) {
        self.next += 1;
    }
}

#[derive(Clone, Copy)]
struct Landing {
    time: Time,
    offset: u64,
    epochs: SourceEpochs,
    byte_run: u64,
    pes_offset: u64,
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
    epochs: Option<SourceEpochs>,
    points: BudgetVec<Landing>,
    physical: Option<Shared<physical::Index>>,
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
    physical: Option<Charged<Box<physical::Capture>>>,
}

impl IndexBuilder {
    fn packet(&mut self, packet: &Packet) {
        if let Some(capture) = &mut self.physical {
            capture.packet(packet);
            if !capture.usable() {
                self.physical = None;
            }
        }
    }

    fn other(&mut self, event: &ReaderEvent) {
        if let Some(capture) = &mut self.physical {
            capture.other(event);
            if !capture.usable() {
                self.physical = None;
            }
        }
    }

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
    selected_epochs: Option<SourceEpochs>,
    phase: Phase,
    retain: Option<Retain>,
    full_scan: bool,
    direct: Option<physical::Direct>,
    probing: Option<Probing>,
}

struct Probing {
    packet: Packet,
    probe: SeekProbe,
    xll_header: Option<dts_hd::Header>,
}

#[derive(Clone, Copy)]
pub(super) enum PictureDelivery {
    Full,
    Scan,
    Discard,
    Landing {
        offset: u64,
        byte_run: u64,
        publish_headers: bool,
    },
}

pub(super) struct SeekProbe {
    pub offset: u64,
    pub epochs: SourceEpochs,
    pub byte_run: u64,
    pub pes_offset: u64,
    pub ticks: Option<i128>,
    pub time: Option<Time>,
    pub random_access: bool,
    pub has_vcl: bool,
}

fn seekable_audio(codec: CodecId) -> bool {
    matches!(
        codec,
        CodecId::Aac
            | CodecId::AacLatm
            | CodecId::Ac3
            | CodecId::Eac3
            | CodecId::MpegAudio
            | CodecId::Dts
    )
}

// MPEG Layer III side information begins with the reservoir back-pointer
// (RFC 5219 §3). An entry point can use only bits available from its own frame.
fn mpeg_audio_entry(frame: &MpegAudioFrame) -> Result<bool> {
    let header = frame.header();
    if header.layer != MpegLayer::III {
        return Ok(true);
    }
    let mpeg1 = header.version == MpegVersion::Mpeg1;
    let stereo = header.channel_mode != MpegChannelMode::Mono;
    let side_bytes = match (mpeg1, stereo) {
        (true, true) => 32,
        (true, false) | (false, true) => 17,
        (false, false) => 9,
    };
    let offset = if header.crc { 6 } else { 4 };
    let side = frame
        .bytes()
        .as_ref()
        .get(offset..offset + side_bytes)
        .ok_or(Error::Invalid("MPEG Layer III side information"))?;
    Ok(side[0] == 0 && (!mpeg1 || side[1] & 0x80 == 0))
}

// ISO/IEC 14496-3 §1.7.3: a LOAS AudioMuxElement can either carry its
// StreamMuxConfig or refer to the previous one. A seek entry must carry its own
// config. This profile has one AAC-LC program, layer and variable-size payload.
fn latm_entry(multiplex: &[u8]) -> Option<()> {
    let mut bits = BitCursor::new(multiplex);
    // useSameStreamMux = 0, audioMuxVersion = 0, allStreamsSameTimeFraming = 1.
    if bits.read(3).ok()? != 1 || bits.read(13).ok()? != 0 {
        return None;
    }
    let asc = bits.read(16).ok()?;
    if asc >> 11 != 2 || asc & 7 != 0 || !(1..=7).contains(&((asc >> 3) & 15)) {
        return None;
    }
    aac::indexed_frequency(((asc >> 7) & 15) as u8)?;
    if bits.read(3).ok()? != 0 {
        return None;
    }
    bits.skip(8).ok()?; // LATM buffer fullness.
    if bits.read(2).ok()? != 0 {
        return None; // Other data and CRC-bearing configurations are outside this profile.
    }
    let mut payload = 0;
    loop {
        let length = bits.read(8).ok()? as usize;
        payload += length;
        if payload > bits.remaining() / 8 {
            return None;
        }
        if length != 255 {
            break;
        }
    }
    bits.skip(payload * 8).ok()?;
    (payload != 0 && bits.remaining() < 8).then_some(())
}

fn packet_probe(packet: Packet) -> Result<Probing> {
    let clock = packet.clock();
    let time = clock.and_then(|clock| clock.pts.exact);
    let mut xll_header = None;
    let (offset, random_access) = match packet.access_unit() {
        AccessUnit::Video(unit) => (unit.range.start, unit.random_access),
        AccessUnit::Adts(frame) => (frame.source().offset, time.is_some()),
        AccessUnit::Loas(frame) => (
            frame.elementary_range().start,
            time.is_some() && latm_entry(frame.multiplex_element()).is_some(),
        ),
        // E-AC-3 access units begin with the independent syncframe and
        // include its associated dependent substreams (A/52 Annex E §2.3.1).
        AccessUnit::Ac3(frame) => (frame.facts().offset, time.is_some()),
        AccessUnit::MpegAudio(frame) => (
            frame.elementary_range().start,
            time.is_some() && mpeg_audio_entry(frame)?,
        ),
        AccessUnit::Dts(frame) => {
            let header = dts::probe_core_header(frame.bytes().as_ref())?;
            if header.frame_bytes != frame.bytes().len() {
                let descriptor = packet.track_info().stream().dvb_dts_hd()?;
                if !descriptor.is_some_and(|descriptor| {
                    descriptor.extensions[0].is_some_and(|extension| {
                        extension
                            .assets()
                            .next()
                            .is_some_and(|asset| asset.construction == 14)
                    })
                }) {
                    return Err(Error::Unsupported("MPEG-TS DTS-HD seek"));
                }
                xll_header = dts_hd::entry(&packet)?;
            }
            // DTS TS 102 114 §5.4.2, HFLAG signals prior predictor history.
            (
                frame.elementary_range().start,
                time.is_some()
                    && header.history == 0
                    && (header.frame_bytes == frame.bytes().len() || xll_header.is_some()),
            )
        }
        AccessUnit::DtsExtension(frame) => {
            xll_header = dts_hd::entry(&packet)?;
            (
                frame.elementary_range().start,
                time.is_some() && xll_header.is_some(),
            )
        }
        AccessUnit::AdtsFragment(fragment) => (fragment.source().offset, false),
        _ => {
            return Err(Error::Unsupported(
                "MPEG-TS seek requires a supported video or audio track",
            ));
        }
    };
    let pes = packet.sources()[0].pes.pes();
    let probe = SeekProbe {
        offset,
        epochs: packet.epochs(),
        byte_run: pes.byte_run_epoch,
        pes_offset: pes.elementary_offset,
        ticks: clock.and_then(|clock| clock.pts.ticks90),
        time,
        random_access,
        has_vcl: true,
    };
    Ok(Probing {
        packet,
        probe,
        xll_header,
    })
}

impl Seeking {
    pub(super) fn scan_track(&self) -> Option<TrackId> {
        matches!(self.phase, Phase::Scan(_)).then_some(self.request.track)
    }

    pub(super) fn publishes_source(&self, event: &TraceEvent) -> bool {
        matches!(self.phase, Phase::Replay { .. })
            && (matches!(event, TraceEvent::Pat(_) | TraceEvent::ProgramMap { .. })
                || (self.selected_epochs.is_some()
                    && matches!(
                        event,
                        TraceEvent::Timeline { .. } | TraceEvent::Discontinuity { .. }
                    )))
    }

    pub(super) fn picture_delivery(&self, track: TrackId) -> PictureDelivery {
        match self.phase {
            Phase::Scan(ref scan)
                if self.full_scan
                    || scan
                        .index
                        .as_ref()
                        .is_some_and(|index| index.physical.is_some()) =>
            {
                PictureDelivery::Full
            }
            Phase::Scan(_) => PictureDelivery::Scan,
            Phase::Replay { landing, .. } if track == self.request.track => {
                PictureDelivery::Landing {
                    offset: landing.offset,
                    byte_run: landing.byte_run,
                    publish_headers: self.selected_epochs.is_some(),
                }
            }
            Phase::Replay { .. } => PictureDelivery::Discard,
            Phase::Deliver(_) | Phase::Retained(_) => PictureDelivery::Full,
        }
    }
}

impl Scan {
    fn finish_index(
        &mut self,
        pool: &Budget,
        track: TrackId,
        epochs: Option<SourceEpochs>,
    ) -> Option<Shared<SeekIndex>> {
        let index = self.index.take()?;
        let charge = Shared::<SeekIndex>::reserve(pool).ok()?;
        Some(Shared::new(
            SeekIndex {
                track,
                epochs,
                points: index.points,
                physical: index
                    .physical
                    .and_then(|capture| physical::Capture::finish(capture, pool)),
            },
            charge,
        ))
    }

    fn observe(
        &mut self,
        probe: SeekProbe,
        target: Time,
        policy: SourceTimePolicy,
        selected_epochs: Option<SourceEpochs>,
    ) -> Result<bool> {
        let SeekProbe {
            offset,
            epochs,
            byte_run,
            pes_offset,
            ticks,
            time,
            random_access,
            ..
        } = probe;
        if selected_epochs.is_some_and(|selected| selected != epochs) {
            return Ok(false);
        }
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
                byte_run,
                pes_offset,
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
                .observe(
                    picture,
                    seek.request.target,
                    self.limits.time_policy,
                    seek.selected_epochs,
                )
                .map(|_| ()),
            Phase::Replay { skipped, .. } => {
                *skipped = true;
                Ok(())
            }
            Phase::Deliver(_) | Phase::Retained(_) => unreachable!("a seek probe precedes landing"),
        }
    }

    /// Seek a selected AVC, HEVC, AAC-LC in ADTS or LOAS, AC-3, E-AC-3, MPEG audio,
    /// DTS core or self-contained lossless-only DTS-HD XLL
    /// track in one length-known, seekable entity. The reader lands on the last coded
    /// random-access picture or complete audio frame carrying the greatest
    /// resolved PTS at or before the target. It delivers original transport,
    /// clocks and PES provenance, including untimed frames after the landing.
    /// Short single-track suffixes can be retained as the reader scans.
    /// The caller supplies any decoder configuration needed at the landing.
    /// Requests from a previously continued entity or with unresolved clock
    /// epochs are refused. A completed index for the same track skips the
    /// landing scan. Verified in-band Layer I/II checkpoints can also skip
    /// prefix reads; other indexed landings reconstruct state by replay.
    /// DTS-HD entries require one extension-zero asset with its own static setup,
    /// XLL sync at offset zero, no smoothing buffer or decoding delay, and a complete
    /// XLL frame with a valid common-header CRC. Other DTS-HD recovery layouts are refused.
    #[cold]
    pub fn request_seek(&mut self, request: SeekRequest) -> Result<InputRequest> {
        self.begin_seek(request, None, None, None)
    }

    /// Seek within the exact source clock and configuration pair reported by
    /// [Packet::epochs] in this entity. Only access units with this pair compete
    /// for the landing, even when other epochs have overlapping timestamps.
    /// The existing timestamp-only seek still refuses epoch changes.
    ///
    /// Scans and replays one length-known, seekable entity from its start,
    /// reconstructing transport state and original provenance. Replay publishes
    /// program tables, clock transitions and selected-epoch video header groups
    /// before the landing. The landing and complete following suffix retain
    /// decode order, source clocks and track configurations. The caller supplies
    /// any additional decoder configuration needed at the landing.
    /// Only an index built for the same explicit pair can be reused.
    /// The source must remain stable between scan and replay.
    ///
    /// # Errors
    ///
    /// Returns the source, track, clock and scan-limit errors of [Self::request_seek].
    /// A missing pair, no eligible access unit at or before the target in that
    /// pair, or an ambiguous clock wrap within it is refused during the scan.
    #[cold]
    pub fn request_seek_in_epoch(
        &mut self,
        request: SeekRequest,
        epochs: SourceEpochs,
    ) -> Result<InputRequest> {
        self.begin_seek(request, None, None, Some(epochs))
    }

    /// Seek in one explicit epoch and cache at most `maximum_entries` distinct
    /// landing times. Only requests for this track and epoch pair reuse the index.
    /// Storage exhaustion or unordered times discard the optional index.
    ///
    /// # Errors
    ///
    /// Returns the errors of [Self::request_seek_in_epoch], or an invalid limit
    /// when `maximum_entries` is zero.
    #[cold]
    pub fn request_seek_in_epoch_and_index(
        &mut self,
        request: SeekRequest,
        epochs: SourceEpochs,
        maximum_entries: usize,
    ) -> Result<InputRequest> {
        if maximum_entries == 0 {
            return Err(Error::Invalid("MPEG-TS seek index entry limit"));
        }
        self.begin_seek(request, Some(maximum_entries), None, Some(epochs))
    }

    /// Seek in an explicit clock/configuration pair over ordered contiguous
    /// entities. Replay preserves original PES coordinates across boundaries.
    ///
    /// # Errors
    ///
    /// Returns the layout errors of [Self::request_seek_in_entities] and the
    /// selection errors of [Self::request_seek_in_epoch].
    #[cold]
    pub fn request_seek_in_entities_in_epoch(
        &mut self,
        request: SeekRequest,
        entities: &[SeekEntity],
        epochs: SourceEpochs,
    ) -> Result<InputRequest> {
        self.healthy()?;
        let plan = EntityPlan::new(&self.pool, entities, self.limits.maximum_seek_scan_bytes)?;
        self.begin_seek(request, None, Some(plan), Some(epochs))
    }

    /// Cache a bounded explicit-epoch index for this exact ordered entity plan.
    /// Reuse requires identical source IDs, lengths, order, track and epoch pair.
    /// Sources must remain stable between requests. Index limits and fallback
    /// follow [Self::request_seek_in_epoch_and_index].
    ///
    /// # Errors
    ///
    /// Returns the errors of [Self::request_seek_in_entities_in_epoch] or an
    /// invalid limit when `maximum_entries` is zero.
    #[cold]
    pub fn request_seek_in_entities_in_epoch_and_index(
        &mut self,
        request: SeekRequest,
        entities: &[SeekEntity],
        epochs: SourceEpochs,
        maximum_entries: usize,
    ) -> Result<InputRequest> {
        if maximum_entries == 0 {
            return Err(Error::Invalid("MPEG-TS seek index entry limit"));
        }
        self.healthy()?;
        let plan = EntityPlan::new(&self.pool, entities, self.limits.maximum_seek_scan_bytes)?;
        self.begin_seek(request, Some(maximum_entries), Some(plan), Some(epochs))
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
        self.begin_seek(request, Some(maximum_entries), None, None)
    }

    /// Seek over consecutive parts of one logical transport stream. Supply
    /// the exact order and lengths of up to 4096 seekable entities. The reader
    /// requests each part by its source ID, preserving parser state across
    /// boundaries including a PES or access unit split across parts. Supply
    /// input by source ID as usual and signal EOF for the final entity.
    ///
    /// # Errors
    ///
    /// Refuses empty parts, adjacent repeated source IDs, scan limits and
    /// unresolved clock or configuration epochs.
    #[cold]
    pub fn request_seek_in_entities(
        &mut self,
        request: SeekRequest,
        entities: &[SeekEntity],
    ) -> Result<InputRequest> {
        self.seek_in_entity_plan(request, entities, None)
    }

    /// Build a bounded landing index over the declared contiguous entities.
    /// Later calls to [Self::request_seek_in_entities] can reuse it if the
    /// track, source IDs, lengths and order match. The sources must remain stable.
    #[cold]
    pub fn request_seek_in_entities_and_index(
        &mut self,
        request: SeekRequest,
        entities: &[SeekEntity],
        maximum_entries: usize,
    ) -> Result<InputRequest> {
        if maximum_entries == 0 {
            return Err(Error::Invalid("MPEG-TS seek index entry limit"));
        }
        self.seek_in_entity_plan(request, entities, Some(maximum_entries))
    }

    fn seek_in_entity_plan(
        &mut self,
        request: SeekRequest,
        entities: &[SeekEntity],
        maximum_entries: Option<usize>,
    ) -> Result<InputRequest> {
        self.healthy()?;
        let plan = EntityPlan::new(&self.pool, entities, self.limits.maximum_seek_scan_bytes)?;
        self.begin_seek(request, maximum_entries, Some(plan), None)
    }

    fn begin_seek(
        &mut self,
        request: SeekRequest,
        index_entries: Option<usize>,
        entities: Option<Charged<Box<EntityPlan>>>,
        selected_epochs: Option<SourceEpochs>,
    ) -> Result<InputRequest> {
        self.healthy()?;
        let length = if let Some(plan) = &entities {
            plan.total
        } else {
            self.caps
                .length
                .filter(|_| {
                    self.caps.random_access
                        && !self.continued_entity
                        && self.seek_entities.is_none()
                })
                .ok_or(Error::Unsupported(
                    "MPEG-TS seek needs one length-known seekable entity",
                ))?
        };
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
        if selected_epochs.is_none()
            && entities.is_none()
            && let Some(track) = self.tracks().iter().find(|track| track.stream().pid == pid)
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
            .filter(|index| {
                index.epochs == selected_epochs
                    && index_entries.is_none()
                    && index.track == request.track
                    && match (entities.as_deref(), self.seek_entities.as_deref()) {
                        (None, None) => true,
                        (Some(next), Some(previous)) => next.same_source(previous),
                        _ => false,
                    }
            })
            .cloned();
        let (mut phase, retain) = if let Some(index) = &index {
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
            // An indexed scan retains landing points. Replay can then stream
            // the suffix without keeping both the index and the output packets.
            let retain = (selected_epochs.is_none()
                && index_entries.is_none()
                && entities.is_none()
                && length <= 256 * 1024
                && single_track)
                .then(|| Retain::new(&self.pool));
            (
                Phase::Scan(Scan {
                    index: index_entries.map(|maximum| IndexBuilder {
                        points: BudgetVec::new(&self.pool),
                        maximum,
                        physical: (selected_epochs.is_none() && entities.is_none())
                            .then(|| {
                                physical::Capture::new(&self.pool, self.source, length, maximum)
                                    .ok()
                            })
                            .flatten(),
                    }),
                    ..Scan::default()
                }),
                retain,
            )
        };
        let direct = match (&phase, &index) {
            (Phase::Replay { landing, .. }, Some(index)) => {
                physical::Direct::new(index.physical.as_ref(), *landing, self.source, self.caps)
            }
            _ => None,
        };
        if direct.is_some()
            && let Phase::Replay {
                landing, skipped, ..
            } = &mut phase
        {
            *skipped = landing.offset != 0;
        }
        seek.try_push(Seeking {
            request,
            selected_epochs,
            phase,
            full_scan: retain.is_some(),
            retain,
            direct,
            probing: None,
        })
        .map_err(|rejected| rejected.error)?;
        let mut input = if let Some(plan) = &entities {
            let (source, caps) = plan.first();
            let mut next = Self::new(&self.pool, source, caps, self.limits, self.window)?;
            let input = next.request()?;
            std::mem::swap(&mut next.packet_tracks, &mut self.packet_tracks);
            *self = next;
            input
        } else {
            self.reset()?
        };
        self.seek_entities = entities;
        self.seek_index = index;
        self.seek = seek;
        if let Some(offset) = self.seek.as_slice()[0]
            .direct
            .as_ref()
            .map(physical::Direct::offset)
        {
            input = self.input_mut()?.request_range(offset)?;
        }
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
            if let Some(probing) = &mut seek.probing {
                if let Some(header) = &mut probing.xll_header {
                    let checked =
                        header
                            .step(probing.packet.bytes().as_ref(), work)
                            .map_err(|error| {
                                if matches!(seek.phase, Phase::Replay { .. }) {
                                    Error::SourceMismatch
                                } else {
                                    error
                                }
                            })?;
                    if !checked {
                        return Ok(Step::Yield);
                    }
                }
                let Probing { packet, probe, .. } =
                    seek.probing.take().expect("pending seek probe");
                match &mut seek.phase {
                    Phase::Scan(scan) => {
                        let better = scan.observe(
                            probe,
                            request.target,
                            self.limits.time_policy,
                            seek.selected_epochs,
                        )?;
                        if let Some(retain) = &mut seek.retain
                            && !retain.packet(packet, better)
                        {
                            seek.retain = None;
                        }
                    }
                    Phase::Replay {
                        landing,
                        scanned_bytes,
                        skipped,
                    } => {
                        if !probe.random_access || probe.time != Some(landing.time) {
                            return Err(Error::SourceMismatch);
                        }
                        let result = seek_result(request, *landing, *scanned_bytes, *skipped);
                        seek.phase = Phase::Deliver(packet);
                        seek.direct = None;
                        return Ok(Step::Ready(Event::Seek(result)));
                    }
                    _ => unreachable!("seek probe precedes delivery"),
                }
                continue;
            }
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
            let capturing = matches!(&seek.phase, Phase::Scan(scan)
                if scan.index.as_ref().is_some_and(|index| index.physical.is_some()));
            let step = if seek.retain.is_some() || capturing || seek.direct.is_some() {
                self.advance::<false, false>(work, cancel, |_| ())
            } else {
                self.advance::<true, true>(work, cancel, |_| ())
            };
            let seek = &mut self.seek.as_mut_slice()[0];
            if matches!(
                step,
                Ok(Step::Backpressure(_)) | Err(Error::Backpressure { .. })
            ) {
                if seek.direct.is_some() {
                    // The checkpoint is optional storage. If it prevents the
                    // restart from progressing, release it and replay normally.
                    let mut seeking = std::mem::replace(&mut self.seek, BudgetVec::new(&self.pool));
                    seeking.as_mut_slice()[0].direct = None;
                    if let Phase::Replay { skipped, .. } = &mut seeking.as_mut_slice()[0].phase {
                        *skipped = false;
                    }
                    let input = self.reset()?;
                    self.seek = seeking;
                    return Ok(Step::NeedInput(input));
                }
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
            let mut step = step?;
            if let Some(mut direct) = self.seek.as_mut_slice()[0].direct.take() {
                let result = direct.observe(self, &mut step);
                self.seek.as_mut_slice()[0].direct = Some(direct);
                result?;
            }
            let seek = &mut self.seek.as_mut_slice()[0];
            match (&mut seek.phase, step) {
                (Phase::Scan(scan), Step::Ready(Event::Packet(packet))) => {
                    if packet.track() == request.track
                        && seek
                            .selected_epochs
                            .is_none_or(|epochs| epochs == packet.epochs())
                    {
                        if let Some(index) = &mut scan.index {
                            index.packet(&packet);
                        }
                        seek.probing = Some(packet_probe(packet)?);
                    }
                }
                (Phase::Scan(scan), Step::Ready(Event::Other(event))) => {
                    if let Some(index) = &mut scan.index {
                        index.other(&event);
                    }
                    if let Some(retain) = &mut seek.retain
                        && !retain.other(event, !self.tracks.is_empty())
                    {
                        seek.retain = None;
                    }
                }
                (Phase::Scan(scan), Step::End(end)) => {
                    let landing = scan.landing.ok_or_else(|| {
                        if seek.selected_epochs.is_some() {
                            return Error::Unsupported(
                                "MPEG-TS selected epoch has no eligible access unit at or before the target",
                            );
                        }
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
                        self.seek_index =
                            scan.finish_index(&self.pool, request.track, seek.selected_epochs);
                        let result =
                            seek_result(request, landing, Some(end.offset), retain.skipped);
                        let tracks =
                            std::mem::replace(&mut self.tracks, BudgetVec::new(&self.pool));
                        seek.phase = Phase::Retained(retain.finish(result, tracks));
                        continue;
                    }
                    let (source, caps) = self
                        .seek_entities
                        .as_deref()
                        .map_or((self.source, self.caps), |plan| plan.first());
                    let mut replay =
                        match Self::new(&self.pool, source, caps, self.limits, self.window) {
                            Err(Error::Backpressure { .. }) if scan.index.take().is_some() => {
                                continue;
                            }
                            replay => replay?,
                        };
                    replay.seek_index =
                        scan.finish_index(&self.pool, request.track, seek.selected_epochs);
                    let direct = replay.seek_index.as_ref().and_then(|index| {
                        physical::Direct::new(index.physical.as_ref(), landing, source, caps)
                    });
                    let input = if let Some(direct) = &direct {
                        replay.input_mut()?.request_range(direct.offset())?
                    } else {
                        replay.request()?
                    };
                    let scanned = if let Some(mut plan) = self.seek_entities.take() {
                        plan.next = 1;
                        let total = plan.total;
                        replay.seek_entities = Some(plan);
                        total
                    } else {
                        end.offset
                    };
                    self.seek.as_mut_slice()[0].phase = Phase::Replay {
                        landing,
                        scanned_bytes: Some(scanned),
                        skipped: direct.is_some() && landing.offset != 0,
                    };
                    self.seek.as_mut_slice()[0].direct = direct;
                    std::mem::swap(&mut replay.packet_tracks, &mut self.packet_tracks);
                    std::mem::swap(&mut replay.seek, &mut self.seek);
                    *self = replay;
                    return Ok(Step::NeedInput(input));
                }
                (
                    Phase::Replay {
                        landing, skipped, ..
                    },
                    Step::Ready(Event::Packet(packet)),
                ) => {
                    let pes = packet.sources()[0].pes.pes();
                    if packet.track() != request.track
                        || packet.elementary_range().start != landing.offset
                        || packet.epochs() != landing.epochs
                        || pes.byte_run_epoch != landing.byte_run
                        || pes.elementary_offset != landing.pes_offset
                    {
                        *skipped = true;
                        continue;
                    }
                    if seek
                        .direct
                        .as_ref()
                        .is_some_and(|direct| !direct.verified())
                    {
                        return Err(Error::SourceMismatch);
                    }
                    seek.probing = Some(packet_probe(packet).map_err(|_| Error::SourceMismatch)?);
                }
                (Phase::Replay { .. }, Step::Ready(Event::Other(event))) => {
                    if matches!(&event, ReaderEvent::Source(source)
                        if seek.publishes_source(source.event()))
                        || matches!(&event, ReaderEvent::VideoHeaders(packet)
                            if packet.track() == request.track
                                && seek.selected_epochs == Some(packet.epochs()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PacketFormat, ReaderLimits};
    use triergon_core::{Bytes, InputWindow};

    #[test]
    fn cached_landing_index_releases_storage_after_the_seek() {
        let bytes = include_bytes!("../../tests/data/ts188-basic.ts");
        let pool = Budget::new(1 << 20).unwrap();
        let source = SourceId(1);
        let caps = SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: true,
        };
        let mut reader = Reader::new(
            &pool,
            source,
            caps,
            ReaderLimits {
                format: PacketFormat::Ts188,
                ..ReaderLimits::default()
            },
            4096,
        )
        .unwrap();
        let request = reader.request().unwrap();
        reader
            .provide(
                &InputWindow::new(
                    source,
                    request.token,
                    0,
                    Bytes::copy_from_slice(&pool, bytes).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        let mut points = BudgetVec::new(&pool);
        points.try_reserve_exact(64).unwrap();
        let charge = Shared::<SeekIndex>::reserve(&pool).unwrap();
        reader.seek_index = Some(Shared::new(
            SeekIndex {
                track: TrackId(256),
                epochs: None,
                points,
                physical: None,
            },
            charge,
        ));
        let occupied = pool.reserve(pool.limit() - pool.used()).unwrap();
        let cancel = AtomicBool::new(false);
        for _ in 0..32 {
            reader.step(&mut WorkBudget::new(4096), &cancel).unwrap();
            if reader.seek_index.is_none() {
                drop(occupied);
                assert!(reader.healthy().is_ok());
                return;
            }
        }
        panic!("transport pressure did not release the inactive index");
    }
}
