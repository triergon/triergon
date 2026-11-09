//! Owned packet handoff to existing bitmap jobs. Source owners, unknown clocks,
//! typed tails, strict errors and explicitly selected native views stay distinct.
use crate::Result;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, Reservation, Rounding, SourceId, Time, TimeBase, WorkBudget,
};
use triergon_subtitles::{compat, pgs};
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
    /// The packet's duration in ticks of `clock.timebase`, where known.
    pub duration: Option<i64>,
    pub metadata: M,
}
struct Data<M> {
    input: Option<BitmapInput<M>>,
    /// Keeps the owner's allocation charged to the pool while it lives.
    _charge: Reservation,
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
        _charge: charge,
    })))
}
pub struct PgsBridge<M> {
    pool: Budget,
    scope: BitmapScope,
    decoder: compat::pgs::ReceiverDecoder,
    pending: Option<BitmapOwner<M>>,
    pushed: bool,
    finished: bool,
    maximum_packet: usize,
}
/// One decoded packet with the receiver's output for it.
pub struct PgsOutput<M> {
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

impl<M> PgsOutput<M> {
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
    Output(PgsOutput<M>),
    Finished,
}
impl<M> PgsBridge<M> {
    pub fn new(
        pool: &Budget,
        scope: BitmapScope,
        limits: pgs::DisplayLimits,
        config: pgs::DisplayConfig,
    ) -> Result<Self> {
        let decoder = compat::pgs::ReceiverDecoder::new(pool, limits, config)?;
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
        self.decoder.reset(c)?;
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
                let mut raw = Some(p.raw.clone());
                self.decoder.push(&mut raw, p.clock, c)?;
                self.pushed = true;
                return Ok(PgsStep::Yield);
            }
        } else if self.finished {
            if !self.pushed {
                if !w.try_charge(1) {
                    return Ok(PgsStep::Yield);
                }
                self.decoder.finish_input(c)?;
                self.pushed = true;
            }
        } else {
            return Ok(PgsStep::NeedPacket);
        }
        match self.decoder.poll(w, c)? {
            pgs::RecoveryStatus::Yield => Ok(PgsStep::Yield),
            pgs::RecoveryStatus::Ended => Ok(PgsStep::Finished),
            pgs::RecoveryStatus::NeedPacket => {
                self.pending = None;
                self.pushed = false;
                Ok(PgsStep::NeedPacket)
            }
            pgs::RecoveryStatus::OutputReady => Ok(PgsStep::Output(PgsOutput {
                packet: self
                    .pending
                    .as_ref()
                    .ok_or(Error::Invalid("PGS packet owner"))?
                    .clone(),
                observations: self.decoder.take_output(c)?,
            })),
        }
    }
}
/// The settings and bounds of a [`DvdReceiver`]'s decoder.
#[derive(Clone, Debug, Default)]
pub struct DvdBridgeConfig {
    pub settings: compat::dvd::Settings,
    pub limits: compat::dvd::Limits,
}

/// One presented DVD subpicture, owned: its display offsets after the packet
/// clock, its rectangle, colours (4, or 256 for eight-bit pixels) and
/// `width * height` colour indices, row by row.
#[derive(Debug)]
pub struct DvdPicture {
    pub start_ms: u32,
    /// The stop offset, else the packet's duration as FFmpeg's decode API
    /// ends a subtitle without a stop; 0 when neither is known.
    pub end_ms: u32,
    pub forced: bool,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    palette: BudgetVec<u32>,
    pub pixels: Bytes,
}

impl DvdPicture {
    pub fn palette(&self) -> &[u32] {
        self.palette.as_slice()
    }

    /// One row of colour indices.
    pub fn row(&self, y: u16) -> Option<&[u8]> {
        let width = usize::from(self.width);
        (y < self.height).then(|| &self.pixels[usize::from(y) * width..][..width])
    }

    fn copy(pool: &Budget, subtitle: compat::dvd::Subtitle<'_>) -> Result<Self> {
        let b = subtitle.bitmap;
        let (width, height) = (usize::from(b.width), usize::from(b.height));
        let mut rows = BudgetVec::new(pool);
        rows.try_reserve_exact(width * height)?;
        for y in 0..height {
            rows.try_extend_from_slice(&b.pixels[y * b.stride..][..width])?;
        }
        let pixels = rows.freeze().map_err(|failure| failure.error)?;
        let mut palette = BudgetVec::new(pool);
        palette.try_extend_from_slice(b.palette)?;
        Ok(Self {
            start_ms: subtitle.start_ms,
            end_ms: subtitle.end_ms,
            forced: subtitle.forced,
            x: b.x,
            y: b.y,
            width: b.width,
            height: b.height,
            palette,
            pixels,
        })
    }
}

/// A display offset and its position on the packet's timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvdDisplayBoundary {
    pub milliseconds: u32,
    pub at: Option<Time>,
}

/// A picture placed on its packet's timeline.
#[derive(Clone, Copy, Debug)]
pub struct DvdPresentation<'a> {
    pub picture: &'a DvdPicture,
    pub show: DvdDisplayBoundary,
    /// `None` for a subpicture without a stop.
    pub clear: Option<DvdDisplayBoundary>,
}

/// A DVD picture ready for a packet-clock indexed subtitle queue.
#[derive(Clone, Copy, Debug)]
pub struct DvdQueuePicture<'a> {
    pub start_ns: i64,
    pub canvas: Option<[u16; 2]>,
    pub picture: &'a DvdPicture,
}

/// One decoded packet and what it presents.
pub struct DvdOutput<M> {
    pub packet: BitmapOwner<M>,
    pub picture: Option<DvdPicture>,
}

impl<M> DvdOutput<M> {
    /// The picture's display boundaries on the packet's PTS. Without a PTS,
    /// absolute times stay unknown and the offsets remain.
    pub fn presentation(&self) -> Result<Option<DvdPresentation<'_>>> {
        let Some(picture) = &self.picture else {
            return Ok(None);
        };
        let clock = self.packet.input().clock;
        let boundary = |milliseconds: u32| -> Result<DvdDisplayBoundary> {
            let at = clock
                .pts
                .map(|pts| {
                    Time::from_ticks(i128::from(pts), clock.timebase)?
                        .checked_add(Time::new(i128::from(milliseconds), 1_000)?)
                })
                .transpose()?;
            Ok(DvdDisplayBoundary { milliseconds, at })
        };
        Ok(Some(DvdPresentation {
            picture,
            show: boundary(picture.start_ms)?,
            clear: (picture.end_ms != 0)
                .then(|| boundary(picture.end_ms))
                .transpose()?,
        }))
    }

    /// Match the existing bitmap queue: start at packet PTS, fall back to DTS,
    /// then zero. The queue ends a picture when its track is superseded or
    /// cleared. Use `presentation` for DVD's explicit display offsets.
    pub fn packet_clock_picture(
        &self,
        canvas: Option<[u16; 2]>,
    ) -> Result<Option<DvdQueuePicture<'_>>> {
        let Some(picture) = &self.picture else {
            return Ok(None);
        };
        Ok(Some(DvdQueuePicture {
            start_ns: packet_queue_start_ns(self.packet.input().clock)?,
            canvas: canvas.filter(|&[w, h]| queue_canvas_dimensions(u32::from(w), u32::from(h))),
            picture,
        }))
    }
}

pub enum DvdReceiverStep<M> {
    Yield,
    NeedPacket,
    Output(DvdOutput<M>),
    Finished,
}

/// FFmpeg's DVD receiver ([`compat::dvd::Decoder`]) over one track's packets,
/// in order, with one pending packet. Units are reassembled across packets
/// and the colour state carries over between them within the current source,
/// track and generation.
pub struct DvdReceiver<M> {
    pool: Budget,
    scope: BitmapScope,
    decoder: compat::dvd::Decoder,
    packet: Option<BitmapOwner<M>>,
    /// The last step yielded, so the next one resumes the decoder.
    yielded: bool,
    last_sequence: Option<u64>,
    finished: bool,
}

impl<M> DvdReceiver<M> {
    pub fn new(pool: &Budget, scope: BitmapScope, config: DvdBridgeConfig) -> Result<Self> {
        Ok(Self {
            pool: pool.clone(),
            scope,
            decoder: compat::dvd::Decoder::new(pool, config.settings, config.limits),
            packet: None,
            yielded: false,
            last_sequence: None,
            finished: false,
        })
    }

    #[expect(
        clippy::result_large_err,
        reason = "returns exact source packet and metadata on admission failure"
    )]
    pub fn provide(
        &mut self,
        input: BitmapInput<M>,
    ) -> std::result::Result<(), BitmapRejection<M>> {
        if let Err(error) = check(&self.pool, self.scope, &input) {
            return Err(BitmapRejection { input, error });
        }
        if self.packet.is_some() || self.finished {
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
        self.packet = Some(owner(&self.pool, self.scope, input)?);
        self.last_sequence = Some(sequence);
        Ok(())
    }

    pub fn finish(&mut self) {
        self.finished = true;
    }

    /// A new playback scope starts with its own decoder state and settings.
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
        if !work.try_charge(1) {
            return Ok(false);
        }
        *self = Self::new(&self.pool, scope, config)?;
        Ok(true)
    }

    /// Decode the pending packet. Running out of work or memory keeps it
    /// pending; any other error drops it.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<DvdReceiverStep<M>> {
        if cancel.load(Ordering::Relaxed) {
            self.decoder.abandon();
            self.yielded = false;
            self.packet = None;
            self.finished = true;
            return Err(Error::Cancelled);
        }
        let Some(packet) = &self.packet else {
            return Ok(if self.finished {
                DvdReceiverStep::Finished
            } else {
                DvdReceiverStep::NeedPacket
            });
        };
        let status = if self.yielded {
            self.decoder.resume(work)
        } else {
            self.decoder.decode(&packet.input().raw, work)
        };
        self.yielded = matches!(status, Ok(compat::dvd::Status::Yield));
        let picture = match status {
            Ok(compat::dvd::Status::Yield) => return Ok(DvdReceiverStep::Yield),
            Ok(compat::dvd::Status::Nothing) => None,
            Ok(compat::dvd::Status::Subtitle(subtitle)) => match DvdPicture::copy(
                &self.pool,
                match packet_milliseconds(packet.input()) {
                    Some(ms) => subtitle.ending_with_packet(ms),
                    None => subtitle,
                },
            ) {
                Ok(picture) => Some(picture),
                Err(error) => {
                    self.decoder.rewind()?;
                    return Err(error);
                }
            },
            Err(error @ Error::Backpressure { .. }) => return Err(error),
            Err(error) => {
                self.packet = None;
                return Err(error);
            }
        };
        let packet = self
            .packet
            .take()
            .ok_or(Error::Invalid("DVD packet absent"))?;
        Ok(DvdReceiverStep::Output(DvdOutput { packet, picture }))
    }
}

/// A positive packet duration in milliseconds, rounded as FFmpeg rescales it.
fn packet_milliseconds<M>(input: &BitmapInput<M>) -> Option<u32> {
    let duration = input.duration.filter(|&d| d > 0)?;
    let ms = input
        .clock
        .timebase
        .rescale(
            duration,
            TimeBase::new(1, 1_000).ok()?,
            Rounding::NearestAway,
        )
        .ok()?;
    Some(u32::try_from(ms).unwrap_or(u32::MAX))
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
            duration: None,
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
