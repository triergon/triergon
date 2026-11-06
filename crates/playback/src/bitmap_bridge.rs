//! Owned packet handoff to existing bitmap jobs. Source owners, unknown clocks,
//! typed tails, strict errors and explicitly selected native views stay distinct.
use crate::Result;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, Reservation, Rounding, SourceId, TimeBase, WorkBudget,
};
use triergon_subtitles::{compat, dvd, pgs};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitmapScope {
    pub source: SourceId,
    pub track: u64,
    pub generation: u64,
    pub configuration: u64,
}
pub struct BitmapInput<M> {
    pub scope: BitmapScope,
    pub sequence: u64,
    pub raw: Bytes,
    pub clock: pgs::PacketClock,
    pub metadata: M,
}
struct Data<M> {
    input: Option<BitmapInput<M>>,
    charge: Reservation,
}
pub struct BitmapOwner<M>(Arc<Data<M>>);
impl<M> Clone for BitmapOwner<M> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<M> BitmapOwner<M> {
    pub fn input(&self) -> &BitmapInput<M> {
        self.0.input.as_ref().expect("published bitmap packet")
    }

    fn into_storage(mut self, pool: &Budget) -> Option<Arc<Data<M>>> {
        if self.0.charge.bytes() > 4096 || !self.input().raw.belongs_to(pool) {
            return None;
        }
        let data = Arc::get_mut(&mut self.0)?;
        data.input = None;
        Some(self.0)
    }
}
pub struct BitmapRejection<M> {
    pub input: BitmapInput<M>,
    pub error: Error,
}
fn check<M>(pool: &Budget, scope: BitmapScope, input: &BitmapInput<M>) -> Result<()> {
    if input.scope.source != scope.source {
        return Err(Error::SourceMismatch);
    }
    if input.scope.generation != scope.generation {
        return Err(Error::StaleGeneration {
            expected: scope.generation,
            received: input.scope.generation,
        });
    }
    if input.scope.track != scope.track || input.scope.configuration != scope.configuration {
        return Err(Error::Invalid("bitmap source track/configuration fence"));
    }
    if !input.raw.belongs_to(pool) {
        return Err(Error::ForeignPool);
    }
    Ok(())
}

fn packet_queue_start_ns(clock: pgs::PacketClock) -> Result<i64> {
    // The packet ABI reserves the signed minimum as its missing sentinel.
    let known = |tick: &i64| *tick != i64::MIN;
    let start = match clock.pts.filter(known).or_else(|| clock.dts.filter(known)) {
        Some(tick) => clock
            .timebase
            .rescale(
                tick,
                TimeBase::new(1, 1_000_000_000)?,
                Rounding::NearestAway,
            )
            .unwrap_or(i64::MIN),
        None => 0,
    };
    Ok(start)
}

fn queue_canvas_dimensions(width: u32, height: u32) -> bool {
    (160..=8192).contains(&width) && (160..=8192).contains(&height)
}
#[expect(
    clippy::result_large_err,
    reason = "returns exact caller metadata and packet on admission failure"
)]
fn owner<M>(
    pool: &Budget,
    scope: BitmapScope,
    input: BitmapInput<M>,
) -> std::result::Result<BitmapOwner<M>, BitmapRejection<M>> {
    if let Err(error) = check(pool, scope, &input) {
        return Err(BitmapRejection { input, error });
    }
    allocate_owner(pool, input)
}

#[expect(
    clippy::result_large_err,
    reason = "returns exact caller metadata and packet on allocation failure"
)]
fn allocate_owner<M>(
    pool: &Budget,
    input: BitmapInput<M>,
) -> std::result::Result<BitmapOwner<M>, BitmapRejection<M>> {
    // Pinned std ArcInner is repr(C): two atomic counters, then aligned data.
    // Include the padding imposed by caller metadata alignment before reserving.
    let layout = match std::alloc::Layout::new::<[std::sync::atomic::AtomicUsize; 2]>()
        .extend(std::alloc::Layout::new::<Data<M>>())
    {
        Ok((layout, _)) => layout.pad_to_align(),
        Err(_) => {
            return Err(BitmapRejection {
                input,
                error: Error::Overflow,
            });
        }
    };
    let charge = match pool.reserve(layout.size()) {
        Ok(x) => x,
        Err(error) => return Err(BitmapRejection { input, error }),
    };
    Ok(BitmapOwner(Arc::new(Data {
        input: Some(input),
        charge,
    })))
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PgsMode {
    StrictSource,
    ExplicitNativeD32,
    /// Select the pinned receiver's first public event and retained decoder context.
    NativeCompatibility,
}
#[expect(
    clippy::large_enum_variant,
    reason = "bounded inline decoder state; no uncharged parser box"
)]
enum PgsDecoder {
    Source(pgs::DisplayDecoder),
    Native(compat::pgs::RecoveryDecoder),
    Compatibility(compat::pgs::ReceiverDecoder),
}
pub struct PgsBridge<M> {
    pool: Budget,
    scope: BitmapScope,
    decoder: PgsDecoder,
    pending: Option<BitmapOwner<M>>,
    pushed: bool,
    finished: bool,
    maximum_packet: usize,
}
pub struct PgsSourceOutput<M> {
    pub packet: BitmapOwner<M>,
    pub display: pgs::DisplaySet,
}
pub struct PgsNativeOutput<M> {
    pub packet: BitmapOwner<M>,
    pub observations: pgs::RecoveredPacket,
}
pub struct PgsCompatibilityOutput<M> {
    pub packet: BitmapOwner<M>,
    pub observations: compat::pgs::ReceiverPacket,
}

#[derive(Clone, Copy, Debug)]
pub enum PgsQueueAction<'a> {
    Clear { at_ns: i64 },
    Show(PgsQueuePicture<'a>),
}

#[derive(Clone, Copy, Debug)]
pub struct PgsQueuePicture<'a> {
    pub start_ns: i64,
    pub canvas: Option<[u16; 2]>,
    rectangles: &'a [pgs::RecoveredRect],
}

impl<'a> PgsQueuePicture<'a> {
    pub fn rectangles(self) -> impl Iterator<Item = &'a pgs::RecoveredRect> {
        self.rectangles.iter().filter(|r| {
            r.width > 0
                && r.height > 0
                && r.width <= 8192
                && r.height <= 8192
                && r.stride >= r.width
                && r.pixels().len()
                    >= usize::from(r.stride) * (usize::from(r.height) - 1) + usize::from(r.width)
        })
    }
}

impl<M> PgsCompatibilityOutput<M> {
    /// Project the receiver's first native event to the packet-clock bitmap
    /// queue. A zero-rectangle event clears the track. An absent event or an
    /// event whose rectangles cannot be displayed leaves the queue unchanged.
    pub fn packet_clock_action(&self) -> Result<Option<PgsQueueAction<'_>>> {
        let Some(event) = self.observations.receiver_output() else {
            return Ok(None);
        };
        let start_ns = packet_queue_start_ns(self.packet.input().clock)?;
        let rectangles = event.rectangles();
        if rectangles.is_empty() {
            return Ok(Some(PgsQueueAction::Clear { at_ns: start_ns }));
        }
        let [width, height] = self.observations.context_canvas;
        let canvas =
            queue_canvas_dimensions(u32::from(width), u32::from(height)).then_some([width, height]);
        let picture = PgsQueuePicture {
            start_ns,
            canvas,
            rectangles: &rectangles[..rectangles.len().min(8)],
        };
        Ok(picture
            .rectangles()
            .next()
            .map(|_| PgsQueueAction::Show(picture)))
    }
}

pub enum PgsStep<M> {
    Yield,
    NeedPacket,
    Source(PgsSourceOutput<M>),
    Native(PgsNativeOutput<M>),
    Compatibility(PgsCompatibilityOutput<M>),
    Finished,
}
impl<M> PgsBridge<M> {
    pub fn new(
        pool: &Budget,
        scope: BitmapScope,
        mode: PgsMode,
        limits: pgs::DisplayLimits,
        config: pgs::DisplayConfig,
    ) -> Result<Self> {
        let decoder = match mode {
            PgsMode::StrictSource => {
                PgsDecoder::Source(pgs::DisplayDecoder::new(pool, limits, config)?)
            }
            PgsMode::ExplicitNativeD32 => {
                PgsDecoder::Native(compat::pgs::RecoveryDecoder::new(pool, limits, config)?)
            }
            PgsMode::NativeCompatibility => {
                PgsDecoder::Compatibility(compat::pgs::ReceiverDecoder::new(pool, limits, config)?)
            }
        };
        Ok(Self {
            pool: pool.clone(),
            scope,
            decoder,
            pending: None,
            pushed: false,
            finished: false,
            maximum_packet: limits.max_packet_bytes,
        })
    }
    #[expect(
        clippy::result_large_err,
        reason = "returns exact packet owner on refusal"
    )]
    pub fn provide(
        &mut self,
        input: BitmapInput<M>,
    ) -> std::result::Result<(), BitmapRejection<M>> {
        if self.pending.is_some() || self.finished {
            return Err(BitmapRejection {
                input,
                error: Error::Invalid("bitmap packet pending or finished"),
            });
        }
        if input.raw.len() > self.maximum_packet {
            let requested = input.raw.len();
            return Err(BitmapRejection {
                input,
                error: Error::Limit {
                    requested,
                    maximum: self.maximum_packet,
                },
            });
        }
        self.pending = Some(owner(&self.pool, self.scope, input)?);
        self.pushed = false;
        Ok(())
    }
    pub fn finish(&mut self) {
        self.finished = true;
    }
    pub fn reset(
        &mut self,
        scope: BitmapScope,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<bool> {
        if c.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if scope == self.scope {
            return Err(Error::Invalid(
                "bitmap reset needs a new source generation/configuration fence",
            ));
        }
        if !w.try_charge(1) {
            return Ok(false);
        }
        match &mut self.decoder {
            PgsDecoder::Source(d) => d.reset(c)?,
            PgsDecoder::Native(d) => d.reset(c)?,
            PgsDecoder::Compatibility(d) => d.reset(c)?,
        }
        self.pending = None;
        self.pushed = false;
        self.finished = false;
        self.scope = scope;
        Ok(true)
    }
    pub fn step(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<PgsStep<M>> {
        if c.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if w.remaining() == 0 {
            return Ok(PgsStep::Yield);
        }
        if let Some(input) = self.pending.as_ref() {
            if !self.pushed {
                if !w.try_charge(1) {
                    return Ok(PgsStep::Yield);
                }
                let p = input.input();
                match &mut self.decoder {
                    PgsDecoder::Source(d) => d
                        .push(p.raw.clone(), p.clock, pgs::Envelope::Packet, c)
                        .map_err(|e| e.error)?,
                    PgsDecoder::Native(d) => {
                        let mut raw = Some(p.raw.clone());
                        d.push(&mut raw, p.clock, c)?;
                    }
                    PgsDecoder::Compatibility(d) => {
                        let mut raw = Some(p.raw.clone());
                        d.push(&mut raw, p.clock, c)?;
                    }
                }
                self.pushed = true;
                return Ok(PgsStep::Yield);
            }
        } else if self.finished {
            if !self.pushed {
                if !w.try_charge(1) {
                    return Ok(PgsStep::Yield);
                }
                match &mut self.decoder {
                    PgsDecoder::Source(d) => d.finish_input(c)?,
                    PgsDecoder::Native(d) => d.finish_input(c)?,
                    PgsDecoder::Compatibility(d) => d.finish_input(c)?,
                }
                self.pushed = true;
            }
        } else {
            return Ok(PgsStep::NeedPacket);
        }
        match &mut self.decoder {
            PgsDecoder::Source(d) => match d.poll(w, c)? {
                pgs::DisplayStatus::Yield => Ok(PgsStep::Yield),
                pgs::DisplayStatus::Ended => Ok(PgsStep::Finished),
                pgs::DisplayStatus::NeedPacket => {
                    self.pending = None;
                    self.pushed = false;
                    Ok(PgsStep::NeedPacket)
                }
                pgs::DisplayStatus::OutputReady => Ok(PgsStep::Source(PgsSourceOutput {
                    packet: self
                        .pending
                        .as_ref()
                        .ok_or(Error::Invalid("PGS source packet owner"))?
                        .clone(),
                    display: d.take_output(c)?,
                })),
            },
            PgsDecoder::Native(d) => match d.poll(w, c)? {
                pgs::RecoveryStatus::Yield => Ok(PgsStep::Yield),
                pgs::RecoveryStatus::Ended => Ok(PgsStep::Finished),
                pgs::RecoveryStatus::NeedPacket => {
                    self.pending = None;
                    self.pushed = false;
                    Ok(PgsStep::NeedPacket)
                }
                pgs::RecoveryStatus::OutputReady => Ok(PgsStep::Native(PgsNativeOutput {
                    packet: self
                        .pending
                        .as_ref()
                        .ok_or(Error::Invalid("PGS native packet owner"))?
                        .clone(),
                    observations: d.take_output(c)?,
                })),
            },
            PgsDecoder::Compatibility(d) => match d.poll(w, c)? {
                pgs::RecoveryStatus::Yield => Ok(PgsStep::Yield),
                pgs::RecoveryStatus::Ended => Ok(PgsStep::Finished),
                pgs::RecoveryStatus::NeedPacket => {
                    self.pending = None;
                    self.pushed = false;
                    Ok(PgsStep::NeedPacket)
                }
                pgs::RecoveryStatus::OutputReady => {
                    Ok(PgsStep::Compatibility(PgsCompatibilityOutput {
                        packet: self
                            .pending
                            .as_ref()
                            .ok_or(Error::Invalid("PGS compatibility packet owner"))?
                            .clone(),
                        observations: d.take_output(c)?,
                    }))
                }
            },
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DvdMode {
    StrictAllUnits,
    ExplicitNativeFirstUnit,
}
#[derive(Clone, Copy, Debug)]
pub struct DvdBridgeConfig {
    pub mode: DvdMode,
    pub initial: dvd::ControlState,
    pub palette: Option<[u32; 16]>,
    pub packets: dvd::SpuPacketLimits,
    pub decoder: dvd::TraditionalLimits,
}
impl Default for DvdBridgeConfig {
    fn default() -> Self {
        Self {
            mode: DvdMode::StrictAllUnits,
            initial: dvd::ControlState::default(),
            palette: None,
            packets: dvd::SpuPacketLimits::default(),
            decoder: dvd::TraditionalLimits::default(),
        }
    }
}
pub struct DvdOutput<M> {
    pub packet: BitmapOwner<M>,
    pub source: dvd::SpuPacket,
    pub pictures: BudgetVec<dvd::TraditionalSubpicture>,
}
pub use compat::dvd::Presentation as DvdPresentation;
pub use dvd::DvdDisplayBoundary;

/// A native DVD picture ready for a packet-clock indexed subtitle queue.
#[derive(Clone, Copy, Debug)]
pub struct DvdQueuePicture<'a> {
    pub start_ns: i64,
    pub canvas: Option<dvd::Canvas>,
    pub bitmap: compat::dvd::CroppedView<'a, 4>,
}

impl<M> DvdOutput<M> {
    /// Combine the explicit first-unit receiver view with the packet's PTS.
    pub fn native_presentation(&self) -> Result<Option<DvdPresentation<'_>>> {
        let Some(picture) = self.pictures.as_slice().first() else {
            return Ok(None);
        };
        let clock = self.packet.input().clock;
        picture.receiver_presentation_from_ticks(clock.pts, clock.timebase)
    }

    /// Match the existing bitmap queue: start at packet PTS, fall back to DTS,
    /// then zero. The queue ends a picture when its track is superseded or
    /// cleared. Use `native_presentation` for DVD's explicit display offsets.
    /// A packet with no receiver picture produces no queue action.
    pub fn packet_clock_picture(
        &self,
        canvas: Option<dvd::Canvas>,
    ) -> Result<Option<DvdQueuePicture<'_>>> {
        let Some(picture) = self.pictures.as_slice().first() else {
            return Ok(None);
        };
        let Some(presentation) = picture.receiver_presentation(None)? else {
            return Ok(None);
        };
        Ok(Some(DvdQueuePicture {
            start_ns: packet_queue_start_ns(self.packet.input().clock)?,
            canvas: canvas.filter(|c| queue_canvas_dimensions(c.width, c.height)),
            bitmap: presentation.bitmap,
        }))
    }
}

struct DvdStorage<M> {
    packet: Option<Arc<Data<M>>>,
    units: Option<BudgetVec<dvd::SpuSpan>>,
    picture_storage: dvd::TraditionalStorage,
    pictures: Option<BudgetVec<dvd::TraditionalSubpicture>>,
}

impl<M> Default for DvdStorage<M> {
    fn default() -> Self {
        Self {
            packet: None,
            units: None,
            picture_storage: dvd::TraditionalStorage::default(),
            pictures: None,
        }
    }
}

// Keep the small tables used by ordinary packets. Larger packets give their
// allocation back to the shared pool as soon as their output is released.
fn reusable_dvd_table<T>(table: BudgetVec<T>, pool: &Budget) -> Option<BudgetVec<T>> {
    (table.capacity() <= 4 && table.budget().same_pool(pool)).then_some(table)
}

#[allow(clippy::large_enum_variant, reason = "one bounded inline packet stage")]
enum DvdJobPhase {
    Framing(dvd::SpuPacketJob),
    Select,
    Decode,
    Store(Option<dvd::TraditionalSubpicture>),
    Ready,
}

pub struct DvdJob<M> {
    pool: Budget,
    packet: Option<BitmapOwner<M>>,
    source: Option<dvd::SpuPacket>,
    pictures: Option<BudgetVec<dvd::TraditionalSubpicture>>,
    picture_storage: dvd::TraditionalStorage,
    at: usize,
    mode: DvdMode,
    initial: dvd::ControlState,
    palette: Option<[u32; 16]>,
    packet_limits: dvd::SpuPacketLimits,
    limits: dvd::TraditionalLimits,
    decoder: Option<dvd::TraditionalSpuJob>,
    phase: DvdJobPhase,
}
impl<M> DvdJob<M> {
    #[expect(
        clippy::result_large_err,
        reason = "returns exact source packet on admission failure"
    )]
    pub fn new(
        pool: &Budget,
        scope: BitmapScope,
        input: BitmapInput<M>,
        config: DvdBridgeConfig,
    ) -> std::result::Result<Self, BitmapRejection<M>> {
        if let Err(error) = check(pool, scope, &input) {
            return Err(BitmapRejection { input, error });
        }
        let mut job = Self::vacant(pool, config);
        job.admit(input, &mut DvdStorage::default())?;
        Ok(job)
    }

    fn vacant(pool: &Budget, config: DvdBridgeConfig) -> Self {
        Self {
            pool: pool.clone(),
            packet: None,
            source: None,
            pictures: None,
            picture_storage: dvd::TraditionalStorage::default(),
            at: 0,
            mode: config.mode,
            initial: config.initial,
            palette: config.palette,
            packet_limits: config.packets,
            limits: config.decoder,
            decoder: None,
            phase: DvdJobPhase::Ready,
        }
    }

    #[expect(
        clippy::result_large_err,
        reason = "returns exact source packet on admission failure"
    )]
    fn admit(
        &mut self,
        input: BitmapInput<M>,
        storage: &mut DvdStorage<M>,
    ) -> std::result::Result<(), BitmapRejection<M>> {
        debug_assert!(self.packet.is_none());
        let pool = &self.pool;
        let units = storage.units.take().unwrap_or_else(|| BudgetVec::new(pool));
        let source_job = match dvd::SpuPacketJob::with_unit_storage(
            input.raw.clone(),
            pool,
            self.packet_limits,
            units,
        ) {
            Ok(job) => job,
            Err(error) => return Err(BitmapRejection { input, error }),
        };
        let packet = if let Some(mut data) = storage.packet.take() {
            Arc::get_mut(&mut data)
                .expect("recycled packet allocation is exclusive")
                .input = Some(input);
            BitmapOwner(data)
        } else {
            allocate_owner(pool, input)?
        };
        self.packet = Some(packet);
        self.pictures = Some(
            storage
                .pictures
                .take()
                .unwrap_or_else(|| BudgetVec::new(pool)),
        );
        self.picture_storage = std::mem::take(&mut storage.picture_storage);
        self.at = 0;
        self.phase = DvdJobPhase::Framing(source_job);
        Ok(())
    }

    fn abandon(&mut self) {
        self.phase = DvdJobPhase::Ready;
        self.decoder = None;
        self.packet = None;
        self.source = None;
        self.pictures = None;
        self.picture_storage = dvd::TraditionalStorage::default();
    }

    pub fn poll(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<bool> {
        self.poll_stages::<false>(w, c)
    }

    fn poll_stages<const CONTINUE: bool>(
        &mut self,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<bool> {
        loop {
            if c.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if w.remaining() == 0 {
                return Ok(false);
            }
            match &mut self.phase {
                DvdJobPhase::Ready => return Ok(true),
                DvdJobPhase::Framing(job) => {
                    if job.poll(w, c)? != dvd::SpuPacketStatus::Ready {
                        return Ok(false);
                    }
                    self.source = Some(job.try_finish(c)?);
                    self.phase = DvdJobPhase::Select;
                }
                DvdJobPhase::Store(candidate) => {
                    w.take(1);
                    let picture = candidate.take().ok_or(Error::Invalid("DVD candidate"))?;
                    self.store_picture(picture)?;
                }
                DvdJobPhase::Decode => {
                    let job = self
                        .decoder
                        .as_mut()
                        .ok_or(Error::Invalid("DVD decoder absent"))?;
                    if job.poll(w, c)? != dvd::TraditionalStatus::Ready {
                        return Ok(false);
                    }
                    let picture = job.try_finish(c)?;
                    if CONTINUE && w.remaining() != 0 {
                        if c.load(Ordering::Relaxed) {
                            return Err(Error::Cancelled);
                        }
                        w.take(1);
                        self.store_picture(picture)?;
                    } else {
                        self.phase = DvdJobPhase::Store(Some(picture));
                    }
                }
                DvdJobPhase::Select => {
                    if !w.try_charge(1) {
                        return Ok(false);
                    }
                    let source = self
                        .source
                        .as_ref()
                        .ok_or(Error::Invalid("DVD source unit list"))?;
                    let count = if self.mode == DvdMode::ExplicitNativeFirstUnit {
                        source.units().len().min(1)
                    } else {
                        source.units().len()
                    };
                    if self.at == count {
                        self.phase = DvdJobPhase::Ready;
                        return Ok(true);
                    }
                    if source.units()[self.at].encoding != dvd::SpuEncoding::Traditional {
                        return Err(Error::Unsupported(
                            "HD DVD receiver requires its separate explicit context",
                        ));
                    }
                    let policy = if self.mode == DvdMode::StrictAllUnits {
                        dvd::ControlPolicy::Strict
                    } else {
                        dvd::ControlPolicy::Receiver
                    };
                    let raw = source.unit(self.at)?;
                    let storage = std::mem::take(&mut self.picture_storage);
                    if let Some(decoder) = &mut self.decoder {
                        decoder.start_next(raw, self.initial, policy, storage)?;
                    } else {
                        self.decoder = Some(dvd::TraditionalSpuJob::with_storage(
                            raw,
                            self.initial,
                            self.palette,
                            &self.pool,
                            self.limits,
                            policy,
                            storage,
                        )?);
                    }
                    self.phase = DvdJobPhase::Decode;
                }
            }
            if !CONTINUE {
                return Ok(false);
            }
        }
    }

    fn store_picture(&mut self, picture: dvd::TraditionalSubpicture) -> Result<()> {
        let pictures = self
            .pictures
            .as_mut()
            .ok_or(Error::Invalid("DVD pictures absent"))?;
        if let Err(error) = pictures.try_push(picture) {
            self.phase = DvdJobPhase::Store(Some(error.value));
            return Err(error.error);
        }
        self.at += 1;
        self.phase = DvdJobPhase::Select;
        Ok(())
    }

    pub fn finish(mut self) -> Result<DvdOutput<M>> {
        self.take_output()
    }

    fn take_output(&mut self) -> Result<DvdOutput<M>> {
        if !matches!(self.phase, DvdJobPhase::Ready) {
            return Err(Error::Invalid("DVD bridge not ready"));
        }
        self.picture_storage = dvd::TraditionalStorage::default();
        Ok(DvdOutput {
            packet: self
                .packet
                .take()
                .ok_or(Error::Invalid("DVD packet absent"))?,
            source: self
                .source
                .take()
                .ok_or(Error::Invalid("DVD source absent"))?,
            pictures: self
                .pictures
                .take()
                .ok_or(Error::Invalid("DVD pictures absent"))?,
        })
    }
}

pub enum DvdReceiverStep<M> {
    Yield,
    NeedPacket,
    Output(DvdOutput<M>),
    Finished,
}

/// The native first-unit receiver with one pending packet and a control
/// state scoped to the current source, track and generation.
pub struct DvdReceiver<M> {
    scope: BitmapScope,
    job: DvdJob<M>,
    storage: DvdStorage<M>,
    last_sequence: Option<u64>,
    finished: bool,
}

impl<M> DvdReceiver<M> {
    pub fn new(pool: &Budget, scope: BitmapScope, config: DvdBridgeConfig) -> Result<Self> {
        Self::validate_config(config)?;
        Ok(Self {
            scope,
            job: DvdJob::vacant(pool, config),
            storage: DvdStorage::default(),
            last_sequence: None,
            finished: false,
        })
    }

    fn validate_config(config: DvdBridgeConfig) -> Result<()> {
        if config.mode != DvdMode::ExplicitNativeFirstUnit {
            return Err(Error::Invalid(
                "DVD receiver requires native first-unit mode",
            ));
        }
        Ok(())
    }

    #[expect(
        clippy::result_large_err,
        reason = "returns exact source packet and metadata on admission failure"
    )]
    pub fn provide(
        &mut self,
        input: BitmapInput<M>,
    ) -> std::result::Result<(), BitmapRejection<M>> {
        if let Err(error) = check(&self.job.pool, self.scope, &input) {
            return Err(BitmapRejection { input, error });
        }
        if self.job.packet.is_some() || self.finished {
            return Err(BitmapRejection {
                input,
                error: Error::Invalid("DVD receiver packet pending or finished"),
            });
        }
        let sequence = input.sequence;
        if self.last_sequence.is_some_and(|last| sequence <= last) {
            return Err(BitmapRejection {
                input,
                error: Error::Invalid("DVD receiver packet sequence"),
            });
        }
        self.job.admit(input, &mut self.storage)?;
        self.last_sequence = Some(sequence);
        Ok(())
    }

    pub fn finish(&mut self) {
        self.finished = true;
        self.storage = DvdStorage::default();
    }

    /// Return a consumed output to reuse its small charged allocations. Borrowed
    /// pictures must be consumed before calling this method. The decoder's
    /// committed control state is independent of this optional storage handoff.
    pub fn recycle(&mut self, output: DvdOutput<M>) {
        if self.finished || output.packet.input().scope != self.scope {
            return;
        }
        let DvdOutput {
            packet,
            source,
            mut pictures,
        } = output;
        self.storage.packet = packet.into_storage(&self.job.pool);
        self.storage.units = reusable_dvd_table(source.into_unit_storage(), &self.job.pool);
        if let Some(picture) = pictures.pop() {
            self.storage.picture_storage = picture.into_storage(&self.job.pool);
        }
        pictures.clear();
        self.storage.pictures = reusable_dvd_table(pictures, &self.job.pool);
    }

    /// A new playback scope starts with its own control state and palette.
    /// Outputs already handed to the caller keep their original scope.
    pub fn reset(
        &mut self,
        scope: BitmapScope,
        config: DvdBridgeConfig,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if scope == self.scope {
            return Err(Error::Invalid(
                "DVD receiver reset needs a new bitmap scope",
            ));
        }
        Self::validate_config(config)?;
        if !work.try_charge(1) {
            return Ok(false);
        }
        self.job = DvdJob::vacant(&self.job.pool, config);
        self.storage = DvdStorage::default();
        self.scope = scope;
        self.last_sequence = None;
        self.finished = false;
        Ok(true)
    }

    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<DvdReceiverStep<M>> {
        if cancel.load(Ordering::Relaxed) {
            self.job.abandon();
            self.storage = DvdStorage::default();
            self.finished = true;
            return Err(Error::Cancelled);
        }
        if self.job.packet.is_none() {
            return Ok(if self.finished {
                DvdReceiverStep::Finished
            } else {
                DvdReceiverStep::NeedPacket
            });
        }
        match self.job.poll_stages::<true>(work, cancel) {
            Ok(false) => return Ok(DvdReceiverStep::Yield),
            Ok(true) => {}
            Err(error @ Error::Backpressure { .. }) => return Err(error),
            Err(error) => {
                self.job.abandon();
                if matches!(error, Error::Cancelled) {
                    self.finished = true;
                    self.storage = DvdStorage::default();
                }
                return Err(error);
            }
        }
        let output = match self.job.take_output() {
            Ok(output) => output,
            Err(error) => {
                self.job.abandon();
                return Err(error);
            }
        };
        if let Some(picture) = output.pictures.as_slice().first() {
            self.job.initial = picture.program().state_after();
        }
        Ok(DvdReceiverStep::Output(output))
    }
}

#[cfg(test)]
mod arc_layout_regression {
    use super::*;
    use std::mem::{align_of, size_of};
    use std::sync::atomic::AtomicUsize;

    #[repr(align(4096))]
    struct AlignedMetadata(u8);

    // Independent layout witness for pinned std ArcInner's repr(C) allocation.
    #[repr(C)]
    struct ArcStorageWitness<T> {
        _strong: AtomicUsize,
        _weak: AtomicUsize,
        _data: T,
    }

    fn fixture(pool: &Budget, raw: Bytes) -> BitmapInput<AlignedMetadata> {
        assert!(raw.belongs_to(pool));
        BitmapInput {
            scope: BitmapScope {
                source: SourceId(6001),
                track: 3,
                generation: 7,
                configuration: 2,
            },
            sequence: 11,
            raw,
            clock: pgs::PacketClock {
                pts: None,
                dts: None,
                timebase: TimeBase::new(1, 90_000).unwrap(),
            },
            metadata: AlignedMetadata(17),
        }
    }

    #[test]
    fn overaligned_owner_charge_tracks_pinned_arc_storage_and_retention() {
        let pool = Budget::new(1 << 20).unwrap();
        let raw = Bytes::copy_from_slice(&pool, b"source owner layout proof").unwrap();
        let input = fixture(&pool, raw.clone());
        let scope = input.scope;
        let before = pool.used();
        let Ok(value) = owner(&pool, scope, input) else {
            panic!("sufficient policy refused valid aligned metadata")
        };
        let required = size_of::<ArcStorageWitness<Data<AlignedMetadata>>>();
        let actual = pool.used() - before;
        println!(
            "ARC_LAYOUT word={} data_size={} data_alignment={} charged={} required={}",
            size_of::<usize>(),
            size_of::<Data<AlignedMetadata>>(),
            align_of::<Data<AlignedMetadata>>(),
            actual,
            required
        );
        assert_eq!(align_of::<Data<AlignedMetadata>>(), 4096);
        assert_eq!(
            actual, required,
            "Arc reservation must include padding before aligned metadata"
        );
        let retained = value.clone();
        drop(value);
        assert_eq!(pool.used(), before + required);
        assert_eq!(retained.input().metadata.0, 17);
        assert_eq!(retained.input().raw.as_ref(), raw.as_ref());
        drop(retained);
        assert_eq!(pool.used(), before);
        drop(raw);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }

    #[test]
    fn overaligned_owner_refuses_tight_budget_and_returns_original_input() {
        let pool = Budget::new(1 << 20).unwrap();
        let raw = Bytes::copy_from_slice(&pool, b"source owner limit proof").unwrap();
        let input = fixture(&pool, raw.clone());
        let scope = input.scope;
        let required = size_of::<ArcStorageWitness<Data<AlignedMetadata>>>();
        let held = pool.reserve(pool.available() - (required - 1)).unwrap();
        let before = pool.used();
        match owner(&pool, scope, input) {
            Ok(value) => {
                println!(
                    "ARC_LIMIT available_before={} required={} incorrectly_charged={}",
                    required - 1,
                    required,
                    pool.used() - before
                );
                drop(value);
                panic!(
                    "aligned Arc allocation must refuse when required storage exceeds available policy by one byte"
                );
            }
            Err(rejected) => {
                assert!(matches!(rejected.error, Error::Backpressure { .. }));
                assert_eq!(rejected.input.scope, scope);
                assert_eq!(rejected.input.metadata.0, 17);
                assert_eq!(rejected.input.raw.as_ref(), raw.as_ref());
                assert_eq!(pool.used(), before);
            }
        }
        drop(held);
        drop(raw);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}
