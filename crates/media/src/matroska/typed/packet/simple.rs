//! Small ordinary SimpleBlock transaction. Header grammar, track validation,
//! packet time/fields, source owners and publication use the original helpers.
//! Non-plain cases promote their already validated facts to the full batch.
use super::*;
use std::mem::size_of;

#[derive(Debug)]
enum SimplePhase {
    Header,
    Track,
    StartLace,
    Reserve,
    StartFrame,
    Store,
    CompleteFrames,
    Publish,
    Ready,
    Done,
    Promote,
}
/// This job owns a complete admitted leaf: it cannot request more input.
/// Keep private progress scalar; the public Reader still returns ParseStep.
#[derive(Debug)]
pub(crate) enum SimpleProgress {
    Yield,
    Ready,
    Finished,
    Promote,
    Backpressure { requested: usize, available: usize },
}

#[derive(Debug)]
pub(crate) struct SimplePacketJob {
    source: SharedBytes,
    configuration: Option<Configuration>,
    block_bytes: usize,
    phase: SimplePhase,
    cluster_ticks: u64,
    scale: u64,
    number: u64,
    offset: i16,
    flags: u8,
    payload: Range<usize>,
    track: usize,
    staged: Option<Packet>,
    ready: Option<SharedBytes>,
    failed: bool,
}
impl SimplePacketJob {
    pub(crate) fn new(
        source: SharedBytes,
        cluster_ticks: u64,
        scale: u64,
        configuration: Configuration,
        limits: Limits,
        pool: &Budget,
    ) -> Result<Self> {
        validate_batch_input(&source, &configuration, limits, scale, pool)?;
        Ok(Self::prepared(
            source,
            cluster_ticks,
            scale,
            Some(configuration),
            limits,
        ))
    }
    /// The containing Reader retains the same immutable configuration while
    /// this plain job is live. Seek/reset replace the job before replacing it.
    pub(crate) fn new_borrowed(
        source: SharedBytes,
        cluster_ticks: u64,
        scale: u64,
        configuration: &Configuration,
        limits: Limits,
        pool: &Budget,
    ) -> Result<Self> {
        validate_batch_input(&source, configuration, limits, scale, pool)?;
        Ok(Self::prepared(source, cluster_ticks, scale, None, limits))
    }
    fn prepared(
        source: SharedBytes,
        cluster_ticks: u64,
        scale: u64,
        configuration: Option<Configuration>,
        limits: Limits,
    ) -> Self {
        Self {
            source,
            configuration,
            block_bytes: limits.block_bytes,
            phase: SimplePhase::Header,
            cluster_ticks,
            scale,
            number: 0,
            offset: 0,
            flags: 0,
            payload: 0..0,
            track: 0,
            staged: None,
            ready: None,
            failed: false,
        }
    }
    fn configuration<'a>(
        &'a self,
        borrowed: Option<&'a Configuration>,
    ) -> Result<&'a Configuration> {
        self.configuration
            .as_ref()
            .or(borrowed)
            .required("missing packet configuration")
    }
    pub(crate) fn own_configuration(
        &mut self,
        configuration: Option<&Configuration>,
    ) -> Result<()> {
        if self.configuration.is_none() {
            self.configuration = Some(
                configuration
                    .required("missing packet configuration")?
                    .clone(),
            );
        }
        Ok(())
    }
    pub(crate) fn needs_batch(&self) -> bool {
        matches!(self.phase, SimplePhase::Promote)
    }
    /// Caller has observed Promote after the original Header/Track admissions.
    /// Infallible assembly only; no source parsing, allocation or charged work.
    pub(crate) fn take_batch(&mut self, limits: Limits) -> PacketBatchJob {
        let len = self.source.len();
        PacketBatchJob {
            source: std::mem::take(&mut self.source),
            configuration: self.configuration.take(),
            limits,
            simple: true,
            phase: Phase::StartLace,
            job: BatchWork::Idle,
            group: None,
            configuration_change: None,
            block: Some(0..len),
            duration: None,
            reference: false,
            discard: 0,
            cluster_ticks: self.cluster_ticks,
            scale: self.scale,
            number: self.number,
            payload: std::mem::replace(&mut self.payload, 0..0),
            offset: self.offset,
            flags: self.flags,
            track: self.track,
            staged: Staged::Single(self.staged.take()),
            frame: 0,
            publish: 0,
            restore: None,
            ready: self.ready.take(),
            decoded_capacity: 0,
            failed: self.failed,
        }
    }
    #[cfg(test)]
    pub(crate) fn step_compact(
        &mut self,
        prefix: Option<i64>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<SimpleProgress> {
        self.step_compact_borrowing(None, prefix, work, cancel)
    }
    pub(crate) fn step_compact_borrowing(
        &mut self,
        borrowed: Option<&Configuration>,
        prefix: Option<i64>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<SimpleProgress> {
        if self.failed {
            return Err(Error::Invalid(
                "packet batch requires replacement after error",
            ));
        }
        let result = self.advance(borrowed, prefix, work, cancel);
        match result {
            Err(Error::Backpressure {
                requested,
                available,
            }) => Ok(SimpleProgress::Backpressure {
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
        borrowed: Option<&Configuration>,
        prefix: Option<i64>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<SimpleProgress> {
        // A completed phase falls directly into its successor. Each original
        // bounded operation retains its own cancellation and work gate.
        if matches!(self.phase, SimplePhase::Header) {
            check(cancel)?;
            if !work.try_charge(1) {
                return Ok(SimpleProgress::Yield);
            }
            let parsed = block_prefix(&self.source)?;
            self.number = parsed.number;
            self.offset = parsed.offset;
            self.flags = parsed.flags;
            self.payload = 0_usize
                .checked_add(parsed.payload_start)
                .ok_or(Error::Overflow)?..self.source.len();
            self.phase = SimplePhase::Track;
        }
        while matches!(self.phase, SimplePhase::Track) {
            check(cancel)?;
            if !work.try_charge(1) {
                return Ok(SimpleProgress::Yield);
            }
            let Some(track) = block_track(self.configuration(borrowed)?, self.track, self.number)?
            else {
                self.track += 1;
                continue;
            };
            validate_track_lacing(self.flags, track)?;
            if (self.flags >> 1) & 3 != 0
                || track.packet_decoder.is_some()
                || !track.packet_prefix.is_empty()
            {
                self.phase = SimplePhase::Promote;
                // This is an internal handoff, consumed without exposing
                // another Yield or Reader health/cancellation boundary.
                return Ok(SimpleProgress::Promote);
            }
            self.phase = SimplePhase::StartLace;
        }
        if matches!(self.phase, SimplePhase::StartLace) {
            check(cancel)?;
            if !work.try_charge(1) {
                return Ok(SimpleProgress::Yield);
            }
            self.phase = SimplePhase::Reserve;
        }
        if matches!(self.phase, SimplePhase::Reserve) {
            check(cancel)?;
            if !work.try_charge(1) {
                return Ok(SimpleProgress::Yield);
            }
            self.phase = SimplePhase::StartFrame;
        }
        if matches!(self.phase, SimplePhase::StartFrame) {
            check(cancel)?;
            if !work.try_charge(1) {
                return Ok(SimpleProgress::Yield);
            }
            self.ready = Some(std::mem::take(&mut self.source).into_slice(self.payload.clone())?);
            self.phase = SimplePhase::Store;
        }
        if matches!(self.phase, SimplePhase::Store) {
            check(cancel)?;
            if !work.try_charge(1) {
                return Ok(SimpleProgress::Yield);
            }
            let track = &self.configuration(borrowed)?.tracks()[self.track];
            let _storage = self.ready.as_ref().required("missing restored frame")?;
            validate_decoded_capacity(0, self.block_bytes)?;
            let fields = FrameParameters {
                cluster_ticks: self.cluster_ticks,
                offset: self.offset,
                scale: self.scale,
                duration_ticks: None,
                frame: 0,
                frames: 1,
            }
            .fields(self.track, track, self.flags & 0x80 != 0, 0)?;
            if self.staged.is_some() {
                return Err(Error::Invalid("single packet already staged"));
            }
            let storage = self.ready.take().required("missing restored frame")?;
            let range = 0..storage.len();
            self.staged = Some(fields.finish(PacketData { storage, range }));
            self.phase = SimplePhase::CompleteFrames;
        }
        if matches!(self.phase, SimplePhase::CompleteFrames) {
            check(cancel)?;
            self.source = SharedBytes::default();
            self.phase = SimplePhase::Publish;
        }
        if matches!(self.phase, SimplePhase::Publish) {
            check(cancel)?;
            if !work.try_charge(1) {
                return Ok(SimpleProgress::Yield);
            }
            if let Some(lower) = prefix {
                let packet = self.staged.as_ref().required("missing staged packet")?;
                let kind = self.configuration(borrowed)?.tracks()[packet.track].kind;
                if !keep_recovery_packet(kind, packet.pts_ns, lower) {
                    self.staged = None;
                    self.phase = SimplePhase::Done;
                    check(cancel)?;
                    return Ok(SimpleProgress::Finished);
                }
            }
            self.phase = SimplePhase::Ready;
            return Ok(SimpleProgress::Ready);
        }
        // These also preserve cancellation on repeated ready/done/handoff polls.
        check(cancel)?;
        match self.phase {
            SimplePhase::Ready => Ok(SimpleProgress::Ready),
            SimplePhase::Done => Ok(SimpleProgress::Finished),
            SimplePhase::Promote => Ok(SimpleProgress::Promote),
            _ => Err(Error::Invalid("unfinished SimpleBlock phase")),
        }
    }
    #[cfg(test)]
    pub(crate) fn step_ready(
        &mut self,
        prefix: Option<i64>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<()>> {
        Ok(match self.step_compact(prefix, work, cancel)? {
            SimpleProgress::Yield | SimpleProgress::Promote => ParseStep::Yield,
            SimpleProgress::Ready => ParseStep::Event(()),
            SimpleProgress::Finished => ParseStep::Finished,
            SimpleProgress::Backpressure {
                requested,
                available,
            } => ParseStep::Backpressure {
                requested,
                available,
            },
        })
    }
    fn take_packet_ready(&mut self) -> Result<crate::matroska::Packet> {
        if self.failed {
            return Err(Error::Invalid(
                "packet batch requires replacement after error",
            ));
        }
        if !matches!(self.phase, SimplePhase::Ready) {
            self.failed = true;
            return Err(Error::Invalid("packet is not ready"));
        }
        let Some(packet) = self.staged.take() else {
            self.failed = true;
            return Err(Error::Invalid("missing staged packet"));
        };
        self.phase = SimplePhase::Done;
        Ok(packet)
    }
    #[inline(always)]
    pub(crate) fn take_ready(&mut self) -> Result<TypedPacket> {
        let packet = self.take_packet_ready()?;
        let Some(configuration) = self.configuration.take() else {
            self.failed = true;
            return Err(Error::Invalid("missing packet configuration"));
        };
        Ok(TypedPacket {
            packet,
            configuration,
            group: None,
        })
    }
    /// Typed callers still receive a charged immutable snapshot, even when a
    /// preceding legacy call left this job pending under resource pressure.
    #[inline(always)]
    pub(crate) fn take_ready_borrowing(
        &mut self,
        borrowed: Option<&Configuration>,
    ) -> Result<TypedPacket> {
        if self.configuration.is_some() {
            return self.take_ready();
        }
        let packet = self.take_packet_ready()?;
        let Some(borrowed) = borrowed else {
            self.failed = true;
            return Err(Error::Invalid("missing packet configuration"));
        };
        let configuration = borrowed.clone();
        Ok(TypedPacket {
            packet,
            configuration,
            group: None,
        })
    }
    /// Plain SimpleBlocks have no group state or additions. The same staged
    /// packet already contains the validated timing/source facts and owner.
    #[inline(always)]
    pub(crate) fn take_legacy_ready(&mut self) -> Result<crate::matroska::Packet> {
        let packet = self.take_packet_ready()?;
        let configuration = self.configuration.take();
        drop(configuration);
        Ok(packet)
    }
}
// The absent full-batch restore/job/group/config-change fields contribute zero
// slots on this plain path. The retained owner order is exactly unchanged.
super::super::usage::storage!(SimplePacketJob, |s| [
    s.source,
    s.configuration,
    super::super::usage::Packets(std::slice::from_ref(&s.staged)),
    s.ready
]);

const _: () = assert!(size_of::<SimplePacketJob>() <= 768);
#[cfg(test)]
#[test]
fn simple_direct_layout() {
    eprintln!(
        "SIMPLE_DIRECT_LAYOUT small={} batch={} reader={}",
        size_of::<SimplePacketJob>(),
        size_of::<PacketBatchJob>(),
        size_of::<crate::typed::Reader>()
    );
    assert!(size_of::<SimplePacketJob>() <= 768);
    assert!(size_of::<PacketBatchJob>() <= 3408);
    assert!(size_of::<crate::typed::Reader>() <= 10048);
}

#[cfg(test)]
#[test]
fn simple_progress_is_bounded_without_input_owner_or_request() {
    assert!(size_of::<SimpleProgress>() <= 24);
    eprintln!(
        "SIMPLE_PROGRESS_LAYOUT progress={} result={}",
        size_of::<SimpleProgress>(),
        size_of::<Result<SimpleProgress>>()
    );
}
