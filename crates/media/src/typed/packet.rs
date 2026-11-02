//! Atomic typed packet-batch publication; source leaf storage is already bounded.
use super::{
    configuration::{Configuration, StateChangeJob, StateCopyJob},
    lace::{Frames, LaceJob},
    mapping::TrackExtensionsJob,
    side_data::{BlockGroupSource, GroupJob, GroupLocation},
    syntax::check,
};
use crate::matroska::typed::syntax::Required;
use crate::zlib::typed_restore::{RestoreJob, RestorePlan};
use crate::{
    Error, Limits, Result, ebml,
    incremental::ParseStep,
    matroska::{Packet, PacketData},
    storage::SharedBytes,
    timing::BlockTiming,
};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, BudgetVec, Time, WorkBudget};

#[derive(Clone, Debug)]
pub struct TypedPacket {
    pub packet: Packet,
    pub configuration: Configuration,
    /// Shared BlockGroup wire facts; all laces share one complete source owner.
    pub group: Option<BlockGroupSource>,
}
impl TypedPacket {
    /// A view in the shared core's exact seconds. No second arithmetic/timebase
    /// implementation; the pinned format clock remains the compatibility source.
    pub fn source_pts(&self) -> Result<Option<Time>> {
        self.packet
            .timing
            .source()?
            .pts
            .map(|value| {
                Ok(Time::new(
                    value.numerator(),
                    u128::from(value.denominator()) * 1_000_000_000,
                )?)
            })
            .transpose()
    }
}
#[derive(Debug)]
enum Phase {
    Group,
    Extensions,
    StateCopy,
    StateChange,
    Header,
    Track,
    StartLace,
    Lace,
    Reserve,
    StartFrame,
    Frame,
    Store,
    Publish,
    Done,
}
/// The ordinary one-frame batch needs no pointer-vector allocation. Laced
/// batches keep the existing precharged arena and validate every frame first.
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
    /// Resolve every fallible capacity requirement before the ready-frame owner
    /// is moved. Normal batches already reserved the complete lace count.
    #[inline(always)]
    fn prepare(&mut self) -> Result<()> {
        match self {
            Self::Single(slot) => {
                if slot.is_some() {
                    return Err(Error::Invalid("single packet already staged"));
                }
            }
            Self::Laced(packets) => {
                if packets.len() == packets.capacity() {
                    packets.try_reserve_exact(1)?;
                }
            }
        }
        Ok(())
    }
    /// Keep ownership at the caller on any rejection. No allocation can occur
    /// after prepare succeeds, but the core owned-error value also makes the
    /// commit retry-safe if that invariant ever changes.
    #[inline(always)]
    fn push(&mut self, packet: &mut Option<Packet>) -> Result<()> {
        if packet.is_none() {
            return Err(Error::Invalid("missing staged packet"));
        }
        match self {
            Self::Single(slot) => {
                if slot.is_some() {
                    return Err(Error::Invalid("single packet already staged"));
                }
                *slot = packet.take();
                Ok(())
            }
            Self::Laced(packets) => match packets.try_push(packet.take()) {
                Ok(()) => Ok(()),
                Err(error) => {
                    *packet = error.value;
                    Err(Error::from(error.error))
                }
            },
        }
    }
    fn take(&mut self, index: usize) -> Result<Packet> {
        let slot = match self {
            Self::Single(slot) if index == 0 => slot,
            Self::Single(_) => return Err(Error::Invalid("missing staged packet")),
            Self::Laced(packets) => packets
                .as_mut_slice()
                .get_mut(index)
                .required("missing staged packet")?,
        };
        slot.take().required("missing staged packet")
    }
}
/// Only one of these progress jobs is live at a time. Lace stays live while
/// a frame is restored, so RestoreJob deliberately remains a separate field.
/// This union needs no new allocation and preserves the original pool minima.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "one exclusive inline workspace preserves allocation-free common paths and original pool minima; packet/lace fixed resource ceilings are tested"
)]
enum BatchWork {
    Idle,
    Group(GroupJob),
    Extensions(TrackExtensionsJob),
    Lace(LaceJob),
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
    job: BatchWork,
    group: Option<BlockGroupSource>,
    configuration_change: Option<Configuration>,
    block: Option<Range<usize>>,
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
    ready: Option<SharedBytes>,
    decoded_capacity: usize,
    failed: bool,
}
impl PacketBatchJob {
    #[inline(always)]
    pub fn new(
        source: SharedBytes,
        simple: bool,
        cluster_ticks: u64,
        scale: u64,
        configuration: Configuration,
        limits: Limits,
        pool: &Budget,
    ) -> Result<Self> {
        if !source.belongs_to(pool) || !configuration.belongs_to(pool) {
            return Err(triergon_core::Error::ForeignPool.into());
        }
        if source.len() > limits.block_bytes {
            return Err(Error::Limit("element bytes"));
        }
        if scale == 0 {
            return Err(Error::Invalid("zero TimestampScale"));
        }
        let len = source.len();
        let job = if simple {
            BatchWork::Idle
        } else {
            BatchWork::Group(GroupJob::new(source.clone(), u64::MAX, None, limits, pool)?)
        };
        Ok(Self {
            source,
            configuration: Some(configuration),
            limits,
            simple,
            phase: if simple { Phase::Header } else { Phase::Group },
            job,
            group: None,
            configuration_change: None,
            block: simple.then_some(0..len),
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
            ready: None,
            decoded_capacity: 0,
            failed: false,
        })
    }
    #[inline(always)]
    pub(crate) fn set_location(&mut self, location: GroupLocation) -> Result<()> {
        if !self.simple {
            let BatchWork::Group(job) = &mut self.job else {
                return Err(Error::Invalid("missing group source job"));
            };
            job.set_location(location)?;
        }
        Ok(())
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
    fn frame_count(&self) -> Result<usize> {
        if (self.flags >> 1) & 3 == 0 {
            return Ok(1);
        }
        let BatchWork::Lace(job) = &self.job else {
            return Err(Error::Invalid("missing frame plan"));
        };
        job.frames().map(Frames::len).required("missing frame plan")
    }
    fn frame_range(&self, index: usize) -> Result<Range<usize>> {
        if (self.flags >> 1) & 3 == 0 {
            return (index == 0)
                .then(|| self.payload.clone())
                .required("missing frame range");
        }
        let BatchWork::Lace(job) = &self.job else {
            return Err(Error::Invalid("missing frame range"));
        };
        job.frames()
            .and_then(|frames| frames.get(index))
            .required("missing frame range")
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<TypedPacket>> {
        self.step_selected(None, work, cancel)
    }
    pub(crate) fn step_recovery_prefix(
        &mut self,
        lower_ns: i64,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<TypedPacket>> {
        self.step_selected(Some(lower_ns), work, cancel)
    }
    fn step_selected(
        &mut self,
        prefix_ns: Option<i64>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<TypedPacket>> {
        if self.failed {
            return Err(Error::Invalid(
                "packet batch requires replacement after error",
            ));
        }
        let result = self.advance(prefix_ns, work, cancel);
        match result {
            Err(Error::Backpressure {
                requested,
                available,
            }) => Ok(ParseStep::Backpressure {
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
        prefix_ns: Option<i64>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<TypedPacket>> {
        loop {
            check(cancel)?;
            match self.phase {
                Phase::Group => {
                    let BatchWork::Group(job) = &mut self.job else {
                        return Err(Error::Invalid("missing group source job"));
                    };
                    match job.step(work, cancel)? {
                        ParseStep::Event(group) => {
                            self.block = Some(group.block());
                            self.duration = group.duration_ticks();
                            self.reference = !group.references().is_empty();
                            self.discard = group.discard_padding_ns().unwrap_or(0);
                            self.group = Some(group);
                            self.job = BatchWork::Idle;
                            self.phase = Phase::Header;
                        }
                        other => return super::pending(other),
                    }
                }
                Phase::Extensions => {
                    let BatchWork::Extensions(job) = &mut self.job else {
                        return Err(Error::Invalid("missing track extension job"));
                    };
                    match job.step(work, cancel)? {
                        ParseStep::Event(extensions) => {
                            if self
                                .group
                                .as_ref()
                                .required("missing source group")?
                                .max_addition_id()
                                > extensions.max_block_addition_id()
                            {
                                return Err(Error::Invalid("BlockAddID exceeds track maximum"));
                            }
                            self.job = BatchWork::Idle;
                            self.phase = Phase::StartLace;
                        }
                        other => return super::pending(other),
                    }
                }
                Phase::StateCopy => {
                    let BatchWork::StateCopy(job) = &mut self.job else {
                        return Err(Error::Invalid("missing state copy job"));
                    };
                    match job.step(work, cancel)? {
                        ParseStep::Event(state) => {
                            self.job = BatchWork::StateChange(StateChangeJob::new(
                                self.live_configuration()?.clone(),
                                self.track,
                                Some(state),
                                self.live_configuration()?.budget(),
                            )?);
                            self.phase = Phase::StateChange;
                        }
                        other => return super::pending(other),
                    }
                }
                Phase::StateChange => {
                    let BatchWork::StateChange(job) = &mut self.job else {
                        return Err(Error::Invalid("missing state change job"));
                    };
                    match job.step(work, cancel)? {
                        ParseStep::Event(configuration) => {
                            self.configuration = Some(configuration.clone());
                            self.configuration_change = Some(configuration);
                            self.job = BatchWork::Idle;
                            self.phase = Phase::Publish;
                            // All laces, checksums and configuration allocations
                            // succeeded before this existing transaction handoff.
                            return Ok(ParseStep::Yield);
                        }
                        other => return super::pending(other),
                    }
                }
                Phase::Header => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let block = self.block.as_ref().required("BlockGroup without Block")?;
                    let b = &self.source[block.clone()];
                    let (number, width, _) = ebml::vint(b, false)?;
                    let header = b.get(width..width + 3).ok_or(Error::Truncated)?;
                    self.number = number;
                    self.offset = i16::from_be_bytes([header[0], header[1]]);
                    self.flags = header[2];
                    self.payload =
                        block.start.checked_add(width + 3).ok_or(Error::Overflow)?..block.end;
                    self.phase = Phase::Track;
                }
                Phase::Track => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let Some(track) = self.live_configuration()?.tracks().get(self.track) else {
                        return Err(Error::Invalid("unregistered block track"));
                    };
                    if track.number != self.number {
                        self.track += 1;
                        continue;
                    }
                    if (self.flags >> 1) & 3 != 0 && !track.lacing {
                        return Err(Error::Invalid("lacing forbidden by track"));
                    }
                    if self
                        .group
                        .as_ref()
                        .is_some_and(|group| group.max_addition_id() != 0)
                    {
                        self.job = BatchWork::Extensions(
                            self.live_configuration()?
                                .track_extensions_job(self.track, self.limits)?,
                        );
                        self.phase = Phase::Extensions;
                    } else {
                        self.phase = Phase::StartLace;
                    }
                }
                Phase::StartLace => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let mode = (self.flags >> 1) & 3;
                    if mode == 0 {
                        self.phase = Phase::Reserve;
                    } else {
                        self.job = BatchWork::Lace(LaceJob::new(
                            self.source.clone(),
                            self.payload.clone(),
                            mode,
                        )?);
                        self.phase = Phase::Lace;
                    }
                }
                Phase::Lace => {
                    let BatchWork::Lace(job) = &mut self.job else {
                        return Err(Error::Invalid("missing lace job"));
                    };
                    match job.step_descriptors(work, cancel)? {
                        ParseStep::Event(()) => self.phase = Phase::Reserve,
                        other => return super::pending(other),
                    }
                }
                Phase::Reserve => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let frames = self.frame_count()?;
                    if frames != 1 {
                        let mut staged = BudgetVec::new(self.live_configuration()?.budget());
                        staged.try_reserve_exact(frames)?;
                        self.staged = Staged::Laced(staged);
                    }
                    self.phase = Phase::StartFrame;
                }
                Phase::StartFrame => {
                    let frames = self.frame_count()?;
                    if self.frame == frames {
                        self.source = SharedBytes::default();
                        self.job = BatchWork::Idle;
                        if let Some(group) = self
                            .group
                            .as_ref()
                            .filter(|group| group.codec_state().is_some())
                        {
                            self.job = BatchWork::StateCopy(StateCopyJob::new(
                                group,
                                self.limits.block_bytes,
                                self.live_configuration()?.budget(),
                            )?);
                            self.phase = Phase::StateCopy;
                        } else {
                            self.phase = Phase::Publish;
                        }
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let track = &self.live_configuration()?.tracks()[self.track];
                    let laced = frames != 1;
                    if !laced && track.packet_decoder.is_none() && track.packet_prefix.is_empty() {
                        // Exactly RestorePlan::identity's immutable source view.
                        // The same one-unit StartFrame admission and subsequent
                        // loop cancellation/Store work gate remain in force.
                        let range = self.frame_range(self.frame)?;
                        self.ready = Some(std::mem::take(&mut self.source).into_slice(range)?);
                        self.phase = Phase::Store;
                        continue;
                    }
                    let plan = if let Some(decoder) = &track.packet_decoder {
                        RestorePlan::from_decoder(decoder)
                            .borrow_single_stored(!laced && decoder.is_zlib_only())
                    } else {
                        RestorePlan::header(track.packet_prefix.clone())
                    };
                    let limit = if laced {
                        self.limits
                            .block_bytes
                            .checked_sub(self.decoded_capacity)
                            .ok_or(Error::Limit("decoded laced block bytes"))?
                    } else {
                        self.limits.block_bytes
                    };
                    let range = self.frame_range(self.frame)?;
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
                        self.ready = Some(bytes);
                        self.restore = None;
                        self.phase = Phase::Store;
                    }
                    other => return super::pending(other),
                },
                Phase::Store => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let frames = self.frame_count()?;
                    let track = &self.live_configuration()?.tracks()[self.track];
                    let storage = self.ready.as_ref().required("missing restored frame")?;
                    let capacity = if frames != 1 {
                        self.decoded_capacity
                            .checked_add(storage.retained_capacity())
                            .ok_or(Error::Overflow)?
                    } else {
                        0
                    };
                    if capacity > self.limits.block_bytes {
                        return Err(Error::Limit("decoded laced block capacity"));
                    }
                    let timing = BlockTiming {
                        timestamp_ticks: i128::from(self.cluster_ticks) + i128::from(self.offset),
                        scale_ns: self.scale,
                        codec_delay_ns: track.codec_delay_ns,
                        default_duration_ns: track.default_duration_ns,
                        duration_ticks: self.duration,
                        frame: self.frame as u16,
                        frames: frames as u16,
                    };
                    let fields = super::super::PacketFields::new(
                        self.track,
                        timing,
                        if self.simple {
                            self.flags & 0x80 != 0
                        } else {
                            !self.reference
                        },
                        self.discard,
                    )?;
                    self.staged.prepare()?;
                    // Timing, limits and capacity are now validated. Consume
                    // the existing handle without an Arc clone/drop pair.
                    let storage = self.ready.take().required("missing restored frame")?;
                    let range = 0..storage.len();
                    let mut packet = Some(fields.finish(PacketData { storage, range }));
                    if let Err(error) = self.staged.push(&mut packet) {
                        // A core rejection returns the exact uncommitted value;
                        // keep the same restored owner pending for retry.
                        self.ready = packet.map(|packet| packet.data.storage);
                        return Err(error);
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
                    let packet = self.staged.take(self.publish)?;
                    self.publish += 1;
                    if let Some(lower) = prefix_ns {
                        let kind = self.live_configuration()?.tracks()[packet.track].kind;
                        // Consumer recovery selection at the real replay input
                        // boundary. The pinned public-API observations retain
                        // all subtitle packets in this prefix, including known
                        // ends before the cutoff. This is not a visibility test:
                        // None and every source timestamp/duration stay intact.
                        let keep = kind == 17 || packet.pts_ns.is_none_or(|pts| pts >= lower);
                        if !keep {
                            continue;
                        }
                    }
                    let last = self.publish == self.staged.len();
                    let configuration = if last {
                        self.configuration
                            .take()
                            .required("missing packet configuration")?
                    } else {
                        self.live_configuration()?.clone()
                    };
                    let group = if last {
                        self.group.take()
                    } else {
                        self.group.clone()
                    };
                    return Ok(ParseStep::Event(TypedPacket {
                        packet,
                        configuration,
                        group,
                    }));
                }
                Phase::Done => return Ok(ParseStep::Finished),
            }
        }
    }
}

super::usage::storage!(PacketBatchJob, |s| [
    s.source,
    s.configuration,
    s.staged,
    s.restore,
    s.ready,
    s.job,
    s.group,
    s.configuration_change
]);

impl super::usage::Storage for Staged {
    fn storage_slots(&self) -> Result<usize> {
        use super::usage::{Packets, slots};
        match self {
            Self::Single(packet) => Packets(std::slice::from_ref(packet)).storage_slots(),
            Self::Laced(packets) => slots(&[packets, &Packets(packets.as_slice())]),
        }
    }
    fn storage_owner(&self, index: usize) -> Result<Option<super::usage::Owner>> {
        use super::usage::{Packets, owner};
        match self {
            Self::Single(packet) => Packets(std::slice::from_ref(packet)).storage_owner(index),
            Self::Laced(packets) => owner(&[packets, &Packets(packets.as_slice())], index),
        }
    }
}

// An inline union owns only its active variant. Every nested unique allocation
// and shared retained owner still joins the same bounded deduplication proof.
impl super::usage::Storage for BatchWork {
    fn storage_slots(&self) -> Result<usize> {
        match self {
            Self::Idle => Ok(0),
            Self::Group(job) => super::usage::Storage::storage_slots(job),
            Self::Extensions(job) => super::usage::Storage::storage_slots(job),
            Self::Lace(job) => super::usage::Storage::storage_slots(job),
            Self::StateCopy(job) => super::usage::Storage::storage_slots(job),
            Self::StateChange(job) => super::usage::Storage::storage_slots(job),
        }
    }
    fn storage_owner(&self, index: usize) -> Result<Option<super::usage::Owner>> {
        match self {
            Self::Idle => Ok(None),
            Self::Group(job) => super::usage::Storage::storage_owner(job, index),
            Self::Extensions(job) => super::usage::Storage::storage_owner(job, index),
            Self::Lace(job) => super::usage::Storage::storage_owner(job, index),
            Self::StateCopy(job) => super::usage::Storage::storage_owner(job, index),
            Self::StateChange(job) => super::usage::Storage::storage_owner(job, index),
        }
    }
}
