//! Atomic typed packet-batch publication; source leaf storage is already bounded.
use super::{
    PacketFields,
    block_group::{BlockGroupSource, GroupJob, GroupLocation},
    lace::LaceJob,
};
use crate::configuration::{Configuration, StateChangeJob, StateCopyJob};
use crate::step::{Required, check};
use crate::track::extensions::TrackExtensionsJob;
use crate::zlib::restore::{COPY_CHUNK_BYTES, RestoreJob, RestorePlan};
use crate::{
    Error, Limits, Result, ebml,
    packet::{Packet, PacketData},
    step::ParseStep,
    storage::SharedBytes,
    timing::BlockTiming,
    track::Track,
};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, BudgetVec, Bytes, Time, WorkBudget};

/// Shared Block/SimpleBlock syntax. A caller may supply just the actual prefix;
/// the same failure is retained until full-body parsing by the scan controller.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BlockPrefix {
    pub number: u64,
    pub offset: i16,
    pub flags: u8,
    pub payload_start: usize,
}
pub(crate) fn block_prefix(bytes: &[u8]) -> Result<BlockPrefix> {
    if let [track @ 0x80..=0xff, high, low, flags, ..] = bytes {
        return Ok(BlockPrefix {
            number: u64::from(*track & 0x7f),
            offset: i16::from_be_bytes([*high, *low]),
            flags: *flags,
            payload_start: 4,
        });
    }
    let (number, width, _) = ebml::vint(bytes, false)?;
    let header = bytes.get(width..width + 3).ok_or(Error::Truncated)?;
    Ok(BlockPrefix {
        number,
        offset: i16::from_be_bytes([header[0], header[1]]),
        flags: header[2],
        payload_start: width + 3,
    })
}
pub(crate) fn block_track(
    configuration: &Configuration,
    at: usize,
    number: u64,
) -> Result<Option<&Track>> {
    let Some(track) = configuration.tracks().get(at) else {
        return Err(Error::Invalid("unregistered block track"));
    };
    Ok((track.number == number).then_some(track))
}

/// Format-private eligibility; source restoration stays in the packet module.
pub(crate) fn scan_track_is_plain(track: &Track) -> bool {
    track.packet_decoder.is_none() && track.packet_prefix.is_empty()
}

/// Source parameters only; all timestamp/duration validation is in the
/// PacketFields constructor, including its nullable rounded PTS view.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameParameters {
    pub cluster_ticks: u64,
    pub offset: i16,
    pub scale: u64,
    pub duration_ticks: Option<u64>,
    pub frame: u16,
    pub frames: u16,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct ScanPacketFields {
    pub track: usize,
    pub pts_ns: Option<i64>,
    pub keyframe: bool,
}
impl FrameParameters {
    fn timing(self, track: &Track) -> BlockTiming {
        BlockTiming {
            timestamp_ticks: i128::from(self.cluster_ticks) + i128::from(self.offset),
            scale_ns: self.scale,
            codec_delay_ns: track.codec_delay_ns,
            default_duration_ns: track.default_duration_ns,
            duration_ticks: self.duration_ticks,
            frame: self.frame,
            frames: self.frames,
        }
    }
    fn fields(
        self,
        index: usize,
        track: &Track,
        keyframe: bool,
        discard: i64,
    ) -> Result<PacketFields> {
        PacketFields::new(index, self.timing(track), keyframe, discard)
    }
    pub(crate) fn scan_fields(
        self,
        index: usize,
        track: &Track,
        keyframe: bool,
    ) -> Result<ScanPacketFields> {
        if self.frames == 1 && self.frame == 0 && self.duration_ticks.is_none() {
            // A single frame starts at the integral block timestamp. Its
            // optional u64 default duration is already representable in ns.
            let pts_ns =
                i64::try_from(self.timing(track).source_base_ns()?).map_err(|_| Error::Overflow)?;
            return Ok(ScanPacketFields {
                track: index,
                pts_ns: Some(pts_ns),
                keyframe,
            });
        }
        let fields = self.fields(index, track, keyframe, 0)?;
        Ok(ScanPacketFields {
            track: fields.track,
            pts_ns: fields.pts_ns,
            keyframe: fields.keyframe,
        })
    }
}

mod simple;
pub(crate) use simple::{SimplePacketJob, SimpleProgress};

fn validate_batch_input(
    source: &SharedBytes,
    configuration: &Configuration,
    limits: Limits,
    scale: u64,
    pool: &Budget,
) -> Result<()> {
    if !source.belongs_to(pool) || !configuration.belongs_to(pool) {
        return Err(Error::ForeignPool);
    }
    crate::step::check_byte_limit(source.len(), limits.block_bytes)?;
    if scale == 0 {
        return Err(Error::Invalid("zero TimestampScale"));
    }
    Ok(())
}
fn validate_track_lacing(flags: u8, track: &Track) -> Result<()> {
    if (flags >> 1) & 3 != 0 && !track.lacing {
        return Err(Error::Invalid("lacing forbidden by track"));
    }
    Ok(())
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct RecoveryCutoff {
    pub(crate) lower_ns: i64,
    pub(crate) block_start_ticks: Option<i64>,
}

fn keep_recovery_packet(kind: u64, packet: &Packet, cutoff: RecoveryCutoff) -> Result<bool> {
    let lower = cutoff.lower_ns;
    if kind == Track::AUDIO
        && let Some(cutoff_ticks) = cutoff.block_start_ticks
    {
        let timing = packet.timing;
        return Ok(crate::compat::clock::block_start_ticks(
            timing.timestamp_ticks,
            timing.codec_delay_ns,
            timing.scale_ns,
        )? >= i128::from(cutoff_ticks));
    }
    let Some(pts) = packet.pts_ns else {
        return Ok(true);
    };
    if kind != Track::AUDIO {
        return Ok(kind == Track::SUBTITLE || pts >= lower);
    }
    let mut timing = packet.timing;
    let zero_block_duration = timing.duration_ticks == Some(0);
    if zero_block_duration {
        timing.duration_ticks = None;
    }
    let Some(duration) = timing
        .duration_ticks
        .or(timing.default_duration_ns)
        .filter(|&value| value != 0)
    else {
        // The Block timestamp locates only the first frame in a lace
        // (RFC 9559 §10.3.5). Keep untimed laces and their anchor together.
        return Ok(pts >= lower || timing.frames > 1);
    };
    if packet.discard_padding_ns < 0 && timing.frames > 1 {
        // Leading DiscardPadding covers the whole Block (RFC 9559 §5.1.3.5.7).
        // Keep the earlier frames of an overlapping lace so the decoder has
        // their sample counts when applying the trim.
        let start = timing.source_base_ns()?;
        let multiplier = if timing.duration_ticks.is_some() {
            timing.scale_ns
        } else {
            u64::from(timing.frames)
        };
        let end = i128::from(duration)
            .checked_mul(i128::from(multiplier))
            .and_then(|duration| start.checked_add(duration))
            .ok_or(Error::Overflow)?;
        return Ok(end > i128::from(lower));
    }
    if zero_block_duration {
        return recovery_overlaps(timing, lower);
    }
    let duration = packet
        .duration_ns
        .required("missing timed audio duration")?;
    let rounded_delta = i128::from(pts) + i128::from(duration) - i128::from(lower);
    // Rounding the start and duration can move their sum by up to 1 ns.
    if !(0..=1).contains(&rounded_delta) {
        return Ok(rounded_delta > 0);
    }
    recovery_overlaps(timing, lower)
}

fn recovery_overlaps(timing: BlockTiming, lower: i64) -> Result<bool> {
    let source = timing.source()?;
    let pts = source.pts.required("missing timed audio start")?;
    let duration = source.duration.required("missing timed audio duration")?;
    let end = Time::new(pts.numerator(), u128::from(pts.denominator()))?.checked_add(Time::new(
        duration.numerator(),
        u128::from(duration.denominator()),
    )?)?;
    Ok(end > Time::new(i128::from(lower), 1)?)
}

/// A packet with the track configuration it was read under and the side data of
/// its BlockGroup.
#[derive(Clone, Debug)]
pub struct TypedPacket {
    /// The frame, its timing and payload.
    pub packet: Packet,
    /// The configuration in effect for this packet, CodecState changes included.
    pub configuration: Configuration,
    /// Shared BlockGroup wire facts; all laces share one complete source owner.
    pub group: Option<BlockGroupSource>,
}
impl TypedPacket {
    /// The exact presentation time from [`BlockTiming::source`] as a [`Time`] in
    /// seconds; `None` when the frame has no derivable PTS. Fails as that does.
    pub fn source_pts(&self) -> Result<Option<Time>> {
        self.packet
            .timing
            .source()?
            .pts
            .map(|value| {
                Time::new(
                    value.numerator(),
                    u128::from(value.denominator()) * 1_000_000_000,
                )
            })
            .transpose()
    }
}
/// Where the block's own steps stand: its header, its track, its lace and then each
/// frame, before the packets are handed out.
#[derive(Clone, Copy, Debug)]
enum Phase {
    Header,
    Track,
    StartLace,
    Lace,
    Reserve,
    StartFrame,
    Frame,
    Store,
    Publish,
    Ready,
    Done,
}
/// The ordinary one-frame batch needs no pointer-vector allocation. Laced
/// batches keep a precharged arena and validate every frame first.
#[derive(Debug)]
enum Staged {
    Single(Option<Packet>),
    Laced(BudgetVec<Option<Packet>>),
}
impl Staged {
    fn len(&self) -> usize {
        match self {
            Self::Single(_) => 1,
            Self::Laced(packets) => packets.len(),
        }
    }
    /// Keep the restored bytes on a capacity rejection so the frame can be retried.
    #[inline(always)]
    fn push(
        &mut self,
        packet: Packet,
    ) -> std::result::Result<(), triergon_core::Refused<SharedBytes>> {
        match self {
            Self::Single(slot) => {
                if slot.is_some() {
                    return Err(triergon_core::Refused {
                        value: packet.data.storage,
                        error: Error::Invalid("single packet already staged"),
                    });
                }
                *slot = Some(packet);
                Ok(())
            }
            Self::Laced(packets) => {
                packets
                    .try_push(Some(packet))
                    .map_err(|rejected| triergon_core::Refused {
                        value: rejected
                            .value
                            .map_or_else(SharedBytes::default, |packet| packet.data.storage),
                        error: rejected.error,
                    })
            }
        }
    }

    fn peek(&self, index: usize) -> Result<&Packet> {
        let slots = match self {
            Self::Single(slot) => std::slice::from_ref(slot),
            Self::Laced(packets) => packets.as_slice(),
        };
        slots
            .get(index)
            .and_then(Option::as_ref)
            .required("missing staged packet")
    }

    fn take(&mut self, index: usize) -> Result<Packet> {
        let slots = match self {
            Self::Single(slot) => std::slice::from_mut(slot),
            Self::Laced(packets) => packets.as_mut_slice(),
        };
        slots
            .get_mut(index)
            .and_then(Option::take)
            .required("missing staged packet")
    }
}
/// A job that must finish before the block's own steps continue: the BlockGroup's
/// children before the Block header, the track's extensions before the lace, and a
/// CodecState's copy and the configuration it changes before the packets are handed
/// out. It is one inline workspace, with no allocation of its own.
// An explicit tag byte: with the tag in a niche of the group job the frame loop ran
// up to 0.12% more instructions.
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "variant sizes vary by target and one inline workspace preserves the packet budget"
)]
#[repr(u8)]
enum SideJob {
    /// No side job: the block's own steps run.
    Idle,
    Group(GroupJob),
    Extensions(TrackExtensionsJob),
    StateCopy(StateCopyJob),
    StateChange(StateChangeJob),
}
#[derive(Debug)]
pub(crate) struct PacketBatchJob {
    source: SharedBytes,
    configuration: Option<Configuration>,
    limits: Limits,
    simple: bool,
    phase: Phase,
    side: SideJob,
    // `None` for a block of one frame; `frames` is set once the lace is read.
    lace: Option<LaceJob>,
    frames: usize,
    group: Option<BlockGroupSource>,
    configuration_change: Option<Configuration>,
    duration: Option<u64>,
    reference: bool,
    discard: i64,
    cluster_ticks: u64,
    scale: u64,
    number: u64,
    payload: Range<usize>,
    offset: i16,
    flags: u8,
    track: usize,
    staged: Staged,
    frame: usize,
    publish: usize,
    restore: Option<RestoreJob>,
    ready: SharedBytes,
    decoded_capacity: usize,
    failed: bool,
}
impl PacketBatchJob {
    /// A batch over one SimpleBlock (`simple`) or BlockGroup payload; `location` is
    /// where a BlockGroup lies in its source, when its side data is to say so.
    #[expect(
        clippy::too_many_arguments,
        reason = "one block's bytes, kind, place, clock, configuration and bounds"
    )]
    #[inline(always)]
    pub(crate) fn new(
        source: SharedBytes,
        simple: bool,
        location: Option<GroupLocation>,
        cluster_ticks: u64,
        scale: u64,
        configuration: Configuration,
        limits: Limits,
        pool: &Budget,
    ) -> Result<Self> {
        validate_batch_input(&source, &configuration, limits, scale, pool)?;
        let side = if simple {
            SideJob::Idle
        } else {
            SideJob::Group(GroupJob::new(
                source.clone(),
                u64::MAX,
                location,
                limits,
                pool,
            )?)
        };
        Ok(Self {
            source,
            configuration: Some(configuration),
            limits,
            simple,
            phase: Phase::Header,
            side,
            lace: None,
            frames: 0,
            group: None,
            configuration_change: None,
            duration: None,
            reference: false,
            discard: 0,
            cluster_ticks,
            scale,
            number: 0,
            payload: 0..0,
            offset: 0,
            flags: 0,
            track: 0,
            staged: Staged::Single(None),
            frame: 0,
            publish: 0,
            restore: None,
            ready: SharedBytes::default(),
            decoded_capacity: 0,
            failed: false,
        })
    }
    #[inline(always)]
    pub(crate) fn take_configuration_change(&mut self) -> Option<Configuration> {
        self.configuration_change.take()
    }
    #[inline(always)]
    fn live_configuration(&self) -> Result<&Configuration> {
        self.configuration
            .as_ref()
            .required("missing packet configuration")
    }
    fn frame_range(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Range<usize>>> {
        match &mut self.lace {
            None => Ok(Some(self.payload.clone())),
            Some(lace) => lace.frame_range(self.frame, work, cancel),
        }
    }
    pub(crate) fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<TypedPacket>> {
        match self.step_ready(None, work, cancel)? {
            ParseStep::Event(()) => Ok(ParseStep::Event(self.take_ready()?)),
            other => crate::step::pending(other),
        }
    }
    /// The validated packet stays in Staged; return progress without moving a
    /// TypedPacket through the batch parser's Result/ParseStep return slot.
    pub(crate) fn step_ready(
        &mut self,
        prefix: Option<RecoveryCutoff>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<()>> {
        if self.failed {
            return Err(Error::Invalid(
                "packet batch requires replacement after error",
            ));
        }
        let result = self.advance(prefix, work, cancel);
        match result {
            Err(Error::Backpressure(triergon_core::Backpressure {
                requested,
                available,
            })) => Ok(ParseStep::Backpressure {
                requested,
                available,
            }),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
            other => other,
        }
    }
    fn advance(
        &mut self,
        prefix: Option<RecoveryCutoff>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<()>> {
        'side: loop {
            if !matches!(self.side, SideJob::Idle) {
                check(cancel)?;
                match &mut self.side {
                    SideJob::Idle => {}
                    SideJob::Group(job) => match job.step(work, cancel)? {
                        ParseStep::Event(group) => {
                            self.duration = group.duration_ticks();
                            self.reference = !group.references().is_empty();
                            self.discard = group.discard_padding_ns().unwrap_or(0);
                            self.group = Some(group);
                            self.side = SideJob::Idle;
                        }
                        other => return crate::step::pending(other),
                    },
                    SideJob::Extensions(job) => match job.step_job(work, cancel)? {
                        ParseStep::Event(extensions) => {
                            // Extensions are read for a group that carries additions.
                            if let Some(group) = &self.group {
                                self.live_configuration()?.validate_additions(
                                    self.track,
                                    group,
                                    &extensions,
                                )?;
                            }
                            self.side = SideJob::Idle;
                        }
                        other => return crate::step::pending(other),
                    },
                    SideJob::StateCopy(job) => match job.step(work, cancel)? {
                        ParseStep::Event(state) => {
                            self.side = SideJob::StateChange(StateChangeJob::new(
                                self.live_configuration()?.clone(),
                                self.track,
                                Some(state),
                                self.live_configuration()?.budget(),
                            )?);
                        }
                        other => return crate::step::pending(other),
                    },
                    SideJob::StateChange(job) => match job.step(work, cancel)? {
                        ParseStep::Event(configuration) => {
                            self.configuration = Some(configuration.clone());
                            self.configuration_change = Some(configuration);
                            self.side = SideJob::Idle;
                            // All laces, checksums and configuration allocations
                            // succeeded before this transaction handoff.
                            return Ok(ParseStep::Yield);
                        }
                        other => return crate::step::pending(other),
                    },
                }
                continue;
            }
            // The block's own steps: the frame loop dispatches on `phase` alone and
            // leaves only when a step starts a side job.
            loop {
                check(cancel)?;
                match self.phase {
                    Phase::Header => {
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        let block = match &self.group {
                            Some(group) => group.block(),
                            None => 0..self.source.len(),
                        };
                        let b = &self.source[block.clone()];
                        let prefix = block_prefix(b)?;
                        self.number = prefix.number;
                        self.offset = prefix.offset;
                        self.flags = prefix.flags;
                        self.payload = block
                            .start
                            .checked_add(prefix.payload_start)
                            .ok_or(Error::Overflow)?
                            ..block.end;
                        self.phase = Phase::Track;
                    }
                    Phase::Track => {
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        let Some(track) =
                            block_track(self.live_configuration()?, self.track, self.number)?
                        else {
                            self.track += 1;
                            continue;
                        };
                        validate_track_lacing(self.flags, track)?;
                        if self
                            .group
                            .as_ref()
                            .is_some_and(|group| group.max_addition_id() != 0)
                        {
                            self.side = SideJob::Extensions(
                                self.live_configuration()?
                                    .track_extensions_job(self.track, self.limits)?,
                            );
                            self.phase = Phase::StartLace;
                            continue 'side;
                        }
                        self.phase = Phase::StartLace;
                    }
                    Phase::StartLace => {
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        let mode = (self.flags >> 1) & 3;
                        if mode == 0 {
                            self.frames = 1;
                            self.phase = Phase::Reserve;
                        } else {
                            self.lace = Some(LaceJob::new(
                                self.source.clone(),
                                self.payload.clone(),
                                mode,
                            )?);
                            self.phase = Phase::Lace;
                        }
                    }
                    Phase::Lace => match &mut self.lace {
                        Some(lace) => match lace.step_descriptors(work, cancel)? {
                            ParseStep::Event(frames) => {
                                self.frames = frames;
                                self.phase = Phase::Reserve;
                            }
                            other => return crate::step::pending(other),
                        },
                        None => self.phase = Phase::Reserve,
                    },
                    Phase::Reserve => {
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        let frames = self.frames;
                        if frames != 1 {
                            let mut staged = BudgetVec::new(self.live_configuration()?.budget());
                            staged.try_reserve_exact(frames)?;
                            self.staged = Staged::Laced(staged);
                        }
                        self.phase = Phase::StartFrame;
                    }
                    Phase::StartFrame => {
                        let frames = self.frames;
                        if self.frame == frames {
                            self.source = SharedBytes::default();
                            self.lace = None;
                            if let Some(group) = self
                                .group
                                .as_ref()
                                .filter(|group| group.codec_state().is_some())
                            {
                                self.side = SideJob::StateCopy(StateCopyJob::new(
                                    group,
                                    self.limits.block_bytes,
                                    self.live_configuration()?.budget(),
                                )?);
                                self.phase = Phase::Publish;
                                continue 'side;
                            }
                            self.phase = Phase::Publish;
                            continue;
                        }
                        let Some(range) = self.frame_range(work, cancel)? else {
                            return Ok(ParseStep::Yield);
                        };
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        let track = &self.live_configuration()?.tracks()[self.track];
                        let laced = frames != 1;
                        let plain = scan_track_is_plain(track);
                        if !laced && plain {
                            // Exactly RestorePlan::identity's immutable source view.
                            // The same one-unit StartFrame admission and subsequent
                            // loop cancellation/Store work gate remain in force.
                            self.ready = std::mem::take(&mut self.source).into_slice(range)?;
                            self.phase = Phase::Store;
                            continue;
                        }
                        let limit = if laced {
                            self.limits
                                .block_bytes
                                .checked_sub(self.decoded_capacity)
                                .ok_or(Error::Policy("decoded laced block bytes"))?
                        } else {
                            self.limits.block_bytes
                        };
                        let size = range.len();
                        if plain
                            && size <= limit.min(COPY_CHUNK_BYTES)
                            && work.remaining() >= size as u64 + 3
                            && let Some(input) = self.source.get(range.clone())
                        {
                            check(cancel)?;
                            if let Ok(bytes) =
                                Bytes::copy_from_slice(self.live_configuration()?.budget(), input)
                            {
                                // Charge transform admission, reserve, bytes and freeze.
                                // Each lace still owns exactly its restored payload.
                                work.take(size as u64 + 3);
                                self.ready = SharedBytes::from_core(bytes);
                                self.phase = Phase::Store;
                                continue;
                            }
                        }
                        let plan = if let Some(decoder) = &track.packet_decoder {
                            RestorePlan::from_decoder(decoder)
                                .borrow_single_stored(!laced && decoder.is_zlib_only())
                        } else {
                            RestorePlan::header(track.packet_prefix.clone())
                        };
                        self.restore = Some(RestoreJob::new(
                            self.source.slice(range)?,
                            plan,
                            self.limits,
                            limit,
                            self.live_configuration()?.budget(),
                        )?);
                        self.phase = Phase::Frame;
                    }
                    Phase::Frame => match self
                        .restore
                        .as_mut()
                        .required("missing frame restore")?
                        .step(work, cancel)?
                    {
                        ParseStep::Event(bytes) => {
                            self.ready = bytes;
                            self.restore = None;
                            self.phase = Phase::Store;
                        }
                        other => return crate::step::pending(other),
                    },
                    Phase::Store => {
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        let frames = self.frames;
                        let track = &self.live_configuration()?.tracks()[self.track];
                        let capacity = if frames != 1 {
                            self.decoded_capacity
                                .checked_add(self.ready.retained_capacity())
                                .ok_or(Error::Overflow)?
                        } else {
                            0
                        };
                        crate::step::check_byte_limit(capacity, self.limits.block_bytes)?;
                        let fields = FrameParameters {
                            cluster_ticks: self.cluster_ticks,
                            offset: self.offset,
                            scale: self.scale,
                            duration_ticks: self.duration,
                            frame: self.frame as u16,
                            frames: frames as u16,
                        }
                        .fields(
                            self.track,
                            track,
                            if self.simple {
                                self.flags & 0x80 != 0
                            } else {
                                !self.reference
                            },
                            self.discard,
                        )?;
                        let storage = std::mem::take(&mut self.ready);
                        let range = 0..storage.len();
                        let packet = fields.finish(PacketData { storage, range });
                        if let Err(rejected) = self.staged.push(packet) {
                            self.ready = rejected.value;
                            return Err(rejected.error);
                        }
                        self.decoded_capacity = capacity;
                        self.frame += 1;
                        self.phase = Phase::StartFrame;
                    }
                    Phase::Publish => {
                        if self.publish == self.staged.len() {
                            self.phase = Phase::Done;
                            continue;
                        }
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        if let Some(cutoff) = prefix {
                            let packet = self.staged.peek(self.publish)?;
                            let kind = self.live_configuration()?.tracks()[packet.track].kind;
                            // Consumer recovery selection at the real replay input
                            // boundary. Every subtitle packet in this prefix is kept,
                            // including known ends before the cutoff. This is not a visibility test:
                            // None and every source timestamp/duration stay intact.
                            let keep = keep_recovery_packet(kind, packet, cutoff)?;
                            if !keep {
                                drop(self.staged.take(self.publish)?);
                                self.publish += 1;
                                continue;
                            }
                        }
                        self.phase = Phase::Ready;
                        return Ok(ParseStep::Event(()));
                    }
                    Phase::Ready => return Ok(ParseStep::Event(())),
                    Phase::Done => return Ok(ParseStep::Finished),
                }
            }
        }
    }
    /// One caller-side extraction after step_ready's original publication work
    /// and validation. No new owner, allocation or cancellation boundary.
    #[inline(always)]
    pub(crate) fn take_ready(&mut self) -> Result<TypedPacket> {
        if self.failed {
            return Err(Error::Invalid(
                "packet batch requires replacement after error",
            ));
        }
        if !matches!(self.phase, Phase::Ready) {
            self.failed = true;
            return Err(Error::Invalid("packet is not ready"));
        }
        let packet = match self.staged.take(self.publish) {
            Ok(packet) => packet,
            Err(error) => {
                self.failed = true;
                return Err(error);
            }
        };
        self.publish += 1;
        let last = self.publish == self.staged.len();
        let configuration = if last {
            let Some(configuration) = self.configuration.take() else {
                self.failed = true;
                return Err(Error::Invalid("missing packet configuration"));
            };
            configuration
        } else {
            let Some(configuration) = self.configuration.as_ref() else {
                self.failed = true;
                return Err(Error::Invalid("missing packet configuration"));
            };
            configuration.clone()
        };
        let group = if last {
            self.group.take()
        } else {
            self.group.clone()
        };
        self.phase = Phase::Publish;
        Ok(TypedPacket {
            packet,
            configuration,
            group,
        })
    }
}

// At most one of the side job and the lace is live: the same slots as one job.
crate::storage::usage::storage!(PacketBatchJob, |s| [
    s.source,
    s.configuration,
    s.staged,
    s.restore,
    matches!(s.phase, Phase::Store).then_some(&s.ready),
    s.side,
    s.lace,
    s.group,
    s.configuration_change
]);

impl crate::storage::usage::Storage for Staged {
    fn storage_slots(&self) -> Result<usize> {
        use crate::storage::usage::{Packets, slots};
        match self {
            Self::Single(packet) => Packets(std::slice::from_ref(packet)).storage_slots(),
            Self::Laced(packets) => slots(&[packets, &Packets(packets.as_slice())]),
        }
    }
    fn storage_owner(&self, index: usize) -> Result<Option<crate::storage::usage::Owner>> {
        use crate::storage::usage::{Packets, owner};
        match self {
            Self::Single(packet) => Packets(std::slice::from_ref(packet)).storage_owner(index),
            Self::Laced(packets) => owner(&[packets, &Packets(packets.as_slice())], index),
        }
    }
}

// An inline union owns only its active variant. Every nested unique allocation
// and shared retained owner still joins the same bounded deduplication proof.
impl crate::storage::usage::Storage for SideJob {
    fn storage_slots(&self) -> Result<usize> {
        match self {
            Self::Idle => Ok(0),
            Self::Group(job) => crate::storage::usage::Storage::storage_slots(job),
            Self::Extensions(job) => crate::storage::usage::Storage::storage_slots(job),
            Self::StateCopy(job) => crate::storage::usage::Storage::storage_slots(job),
            Self::StateChange(job) => crate::storage::usage::Storage::storage_slots(job),
        }
    }
    fn storage_owner(&self, index: usize) -> Result<Option<crate::storage::usage::Owner>> {
        match self {
            Self::Idle => Ok(None),
            Self::Group(job) => crate::storage::usage::Storage::storage_owner(job, index),
            Self::Extensions(job) => crate::storage::usage::Storage::storage_owner(job, index),
            Self::StateCopy(job) => crate::storage::usage::Storage::storage_owner(job, index),
            Self::StateChange(job) => crate::storage::usage::Storage::storage_owner(job, index),
        }
    }
}

#[cfg(test)]
mod tests;
