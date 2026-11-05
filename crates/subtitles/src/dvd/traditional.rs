//! Cooperative traditional-SPU receiver operation composed from owned jobs.
use super::projection::project_traditional_raster;
use super::{
    ControlFacts, ControlPolicy, ControlProgram, ControlState, DvdBitmap, DvdRasterLimits,
    DvdRasterStatus, NativeCropMode, NativeDvdView, NativePalette, NativeProjectionJob, ProgramJob,
    ProgramLimits, ProgramStatus, ProjectedBitmap, ProjectionLimits, ProjectionStatus,
    native_d32_palette, raster::RasterDecode,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, BytesWriter, Error, Result, Time, TimeBase, WorkBudget,
};

#[derive(Clone, Copy, Debug)]
pub struct TraditionalLimits {
    pub program: ProgramLimits,
    pub raster: DvdRasterLimits,
    pub projection: ProjectionLimits,
    pub max_bitmaps: usize,
    /// Includes every decoded intermediate bitmap, not only the final output.
    pub max_total_pixels: usize,
}
impl Default for TraditionalLimits {
    fn default() -> Self {
        Self {
            program: ProgramLimits::default(),
            raster: DvdRasterLimits::default(),
            projection: ProjectionLimits::default(),
            max_bitmaps: 128,
            max_total_pixels: 8 * 1024 * 1024,
        }
    }
}

/// Millisecond fields of the explicitly selected receiver view. Missing source
/// start/stop commands remain missing in ControlProgram, independently of these
/// zero-initialized receiver fields. Packet PTS belongs to the caller's stamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiverDisplayTimes {
    pub start_ms: u32,
    pub end_ms: u32,
}

/// A receiver offset and its position on the caller's packet timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvdDisplayBoundary {
    pub milliseconds: u32,
    pub at: Option<Time>,
}

#[derive(Clone, Copy, Debug)]
pub struct DvdNativePresentation<'a> {
    pub bitmap: NativeDvdView<'a, 4>,
    pub show: DvdDisplayBoundary,
    pub clear: Option<DvdDisplayBoundary>,
    pub forced: bool,
    pub menu: bool,
}

#[derive(Debug)]
pub struct TraditionalSubpicture {
    program: ControlProgram,
    bitmap: Option<ProjectedBitmap<4>>,
    palette: Option<NativePalette>,
    selected_control: Option<usize>,
    forced: bool,
    menu: bool,
    times: ReceiverDisplayTimes,
}

#[derive(Default)]
pub struct TraditionalStorage {
    controls: Option<BudgetVec<ControlFacts>>,
    pixels: Option<BytesWriter>,
}

impl TraditionalSubpicture {
    /// Release the picture and retain small, reusable allocations in this pool.
    pub fn into_storage(self, pool: &Budget) -> TraditionalStorage {
        let controls = self.program.into_control_storage();
        let controls =
            (controls.capacity() <= 4 && controls.budget().same_pool(pool)).then_some(controls);
        let pixels = self.bitmap.and_then(|bitmap| {
            let pixels = bitmap.into_source().into_pixels();
            if pixels.retained_capacity() <= 4096 && pixels.belongs_to(pool) {
                pixels.try_reuse().ok()
            } else {
                None
            }
        });
        TraditionalStorage { controls, pixels }
    }

    pub fn program(&self) -> &ControlProgram {
        &self.program
    }
    pub fn bitmap(&self) -> Option<&ProjectedBitmap<4>> {
        self.bitmap.as_ref()
    }
    pub fn palette(&self) -> Option<&NativePalette> {
        self.palette.as_ref()
    }
    pub fn selected_control(&self) -> Option<usize> {
        self.selected_control
    }
    pub fn receiver_forced_flag(&self) -> bool {
        self.forced
    }
    pub fn receiver_menu(&self) -> bool {
        self.menu
    }
    pub fn receiver_display_times(&self) -> ReceiverDisplayTimes {
        self.times
    }
    /// Position the native receiver's bitmap on a caller-provided timeline.
    /// A missing stop has no clear boundary. Without PTS, absolute times stay
    /// unknown and the receiver's millisecond offsets remain available.
    pub fn native_presentation(
        &self,
        packet_pts: Option<Time>,
    ) -> Result<Option<DvdNativePresentation<'_>>> {
        self.present(|milliseconds| {
            packet_pts
                .map(|pts| pts.checked_add(Time::new(i128::from(milliseconds), 1_000)?))
                .transpose()
        })
    }

    /// Position the receiver boundaries directly from the exact packet clock.
    pub fn native_presentation_from_ticks(
        &self,
        pts: Option<i64>,
        base: TimeBase,
    ) -> Result<Option<DvdNativePresentation<'_>>> {
        self.present(|ms| pts.map(|pts| offset_ticks(pts, base, ms)).transpose())
    }

    fn present(
        &self,
        absolute: impl Fn(u32) -> Result<Option<Time>>,
    ) -> Result<Option<DvdNativePresentation<'_>>> {
        if self.program.policy() != ControlPolicy::NativeD32 {
            return Err(Error::Invalid(
                "DVD presentation requires native receiver mode",
            ));
        }
        let Some(bitmap) = self.bitmap().and_then(|bitmap| bitmap.native_view()) else {
            return Ok(None);
        };
        let boundary = |milliseconds| -> Result<DvdDisplayBoundary> {
            Ok(DvdDisplayBoundary {
                milliseconds,
                at: absolute(milliseconds)?,
            })
        };
        Ok(Some(DvdNativePresentation {
            bitmap,
            show: boundary(self.times.start_ms)?,
            clear: self
                .program
                .last_stop()
                .map(|_| boundary(self.times.end_ms))
                .transpose()?,
            forced: self.forced,
            menu: self.menu,
        }))
    }

    pub fn receiver_emits_bitmap(&self) -> bool {
        self.bitmap
            .as_ref()
            .is_some_and(|bitmap| bitmap.native_view().is_some())
    }
}

fn offset_ticks(pts: i64, base: TimeBase, milliseconds: u32) -> Result<Time> {
    let ticks = i128::from(pts) * i128::from(base.numerator());
    let denominator = i128::from(base.denominator());
    let milliseconds = i128::from(milliseconds);
    let (ticks, offset, denominator) = if denominator % 1_000 == 0 {
        (
            Some(ticks),
            milliseconds * (denominator / 1_000),
            denominator,
        )
    } else {
        (
            ticks.checked_mul(1_000),
            milliseconds * denominator,
            denominator * 1_000,
        )
    };
    if let Some(numerator) = ticks.and_then(|ticks| ticks.checked_add(offset)) {
        return Time::new(numerator, denominator as u128);
    }
    Time::from_ticks(i128::from(pts), base)?.checked_add(Time::new(milliseconds, 1_000)?)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraditionalStatus {
    Yield,
    Ready,
}

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "one bounded inline decoder stage"
)]
enum Phase {
    Program(ProgramJob),
    Select,
    Raster(RasterDecode),
    Project(NativeProjectionJob<4>),
    Ready,
    Finished,
}

/// One assembled traditional SPU. All original controls/pixels remain encoded
/// in the returned program even when a receiver replaces an earlier bitmap.
/// Validated intermediate rasters are released before the next is allocated.
///
/// Ordinary commands0..6 and END are supported. The default constructor retains
/// strict link validation and refuses opaque controls. The explicit NativeD32
/// policy also exposes receiver termination and no-event recovery while keeping
/// every raw byte, original link and truncated-command position observable.
/// Extended commands remain separate work.
/// Caller-supplied palette acceptance and cross-packet commits are not inferred.
#[derive(Debug)]
pub struct TraditionalSpuJob {
    budget: Budget,
    limits: TraditionalLimits,
    receiver_rgb: Option<[u32; 16]>,
    program: Option<ControlProgram>,
    pixel_storage: Option<BytesWriter>,
    candidate: Option<DvdBitmap>,
    palette: Option<NativePalette>,
    bitmap: Option<ProjectedBitmap<4>>,
    next_control: usize,
    selected_control: Option<usize>,
    forced_seen: bool,
    candidate_forced: bool,
    bitmaps: usize,
    total_pixels: usize,
    raster_finish_paid: bool,
    phase: Phase,
    failed: Option<Error>,
}

impl TraditionalSpuJob {
    pub fn new(
        raw: Bytes,
        initial: ControlState,
        receiver_rgb: Option<[u32; 16]>,
        budget: &Budget,
        limits: TraditionalLimits,
    ) -> Result<Self> {
        Self::with_policy(
            raw,
            initial,
            receiver_rgb,
            budget,
            limits,
            ControlPolicy::Strict,
        )
    }

    pub fn with_policy(
        raw: Bytes,
        initial: ControlState,
        receiver_rgb: Option<[u32; 16]>,
        budget: &Budget,
        limits: TraditionalLimits,
        policy: ControlPolicy,
    ) -> Result<Self> {
        Self::with_storage(
            raw,
            initial,
            receiver_rgb,
            budget,
            limits,
            policy,
            TraditionalStorage::default(),
        )
    }

    /// Reuse small allocations from a released picture in the same pool.
    pub fn with_storage(
        raw: Bytes,
        initial: ControlState,
        receiver_rgb: Option<[u32; 16]>,
        budget: &Budget,
        limits: TraditionalLimits,
        policy: ControlPolicy,
        storage: TraditionalStorage,
    ) -> Result<Self> {
        let mut job = Self {
            budget: budget.clone(),
            limits,
            receiver_rgb,
            program: None,
            pixel_storage: None,
            candidate: None,
            palette: None,
            bitmap: None,
            next_control: 0,
            selected_control: None,
            forced_seen: false,
            candidate_forced: false,
            bitmaps: 0,
            total_pixels: 0,
            raster_finish_paid: false,
            phase: Phase::Finished,
            failed: None,
        };
        job.start_next(raw, initial, policy, storage)?;
        Ok(job)
    }

    /// Start another SPU after taking the previous output. The pool, limits
    /// and receiver palette stay fixed. The caller supplies the initial control
    /// state for the new packet and may return storage from a released picture.
    pub fn start_next(
        &mut self,
        raw: Bytes,
        initial: ControlState,
        policy: ControlPolicy,
        storage: TraditionalStorage,
    ) -> Result<()> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if !matches!(self.phase, Phase::Finished) {
            return Err(Error::Invalid("DVD subpicture still active"));
        }
        let budget = &self.budget;
        if storage
            .pixels
            .as_ref()
            .is_some_and(|pixels| !pixels.belongs_to(budget))
        {
            return Err(Error::ForeignPool);
        }
        let records = storage.controls.unwrap_or_else(|| BudgetVec::new(budget));
        let program_job = ProgramJob::with_control_storage(
            raw,
            budget,
            self.limits.program,
            initial,
            policy,
            records,
        )?;
        self.pixel_storage = storage.pixels;
        self.next_control = 0;
        self.selected_control = None;
        self.forced_seen = false;
        self.candidate_forced = false;
        self.bitmaps = 0;
        self.total_pixels = 0;
        self.raster_finish_paid = false;
        self.phase = Phase::Program(program_job);
        Ok(())
    }

    pub fn bitmaps_started(&self) -> usize {
        self.bitmaps
    }
    pub fn pixels_planned(&self) -> usize {
        self.total_pixels
    }

    fn poison(&mut self, error: Error) -> Error {
        self.phase = Phase::Finished;
        self.program = None;
        self.pixel_storage = None;
        self.candidate = None;
        self.palette = None;
        self.bitmap = None;
        self.failed = Some(error);
        error
    }
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<TraditionalStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if matches!(self.phase, Phase::Ready) {
            return Ok(TraditionalStatus::Ready);
        }
        if matches!(self.phase, Phase::Finished) {
            return Err(self.poison(Error::Invalid("DVD subpicture already finished")));
        }
        match self.inner(work, cancel) {
            Ok(status) => Ok(status),
            Err(error @ Error::Backpressure { .. }) => Err(error),
            Err(error) => Err(self.poison(error)),
        }
    }

    fn inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<TraditionalStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match &mut self.phase {
                Phase::Program(job) => {
                    if job.poll(work, cancel)? == ProgramStatus::Yield {
                        return Ok(TraditionalStatus::Yield);
                    }
                    let program = job.try_finish(cancel)?;
                    let truncated = program.truncated_control().is_some();
                    self.program = Some(program);
                    self.phase = if truncated {
                        Phase::Ready
                    } else {
                        Phase::Select
                    };
                }
                Phase::Select => {
                    let program = self
                        .program
                        .as_ref()
                        .ok_or(Error::Invalid("DVD control program absent"))?;
                    if self.next_control == program.controls().len() {
                        if let Some(candidate) = self.candidate.take() {
                            let mode = if program.has_forced_start() {
                                NativeCropMode::Menu
                            } else {
                                NativeCropMode::Ordinary
                            };
                            let palette = self
                                .palette
                                .as_ref()
                                .ok_or(Error::Invalid("DVD candidate palette absent"))?;
                            let palette = *palette.argb();
                            self.phase = match project_traditional_raster(
                                candidate,
                                palette,
                                mode,
                                self.limits.projection,
                                work,
                            ) {
                                Ok(bitmap) => {
                                    self.bitmap = Some(bitmap);
                                    Phase::Ready
                                }
                                Err(candidate) => Phase::Project(NativeProjectionJob::new(
                                    candidate,
                                    palette,
                                    mode,
                                    &self.budget,
                                    self.limits.projection,
                                )?),
                            };
                        } else {
                            self.phase = Phase::Ready;
                        }
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(TraditionalStatus::Yield);
                    }
                    let index = self.next_control;
                    let fact = &program.controls()[index];
                    self.next_control += 1;
                    self.forced_seen |= fact.forced_starts != 0;
                    if let Some(opaque) = fact.opaque {
                        if program.policy() == ControlPolicy::Strict {
                            return Err(Error::Unsupported("DVD receiver opaque control"));
                        }
                        if matches!(opaque.opcode, 0x83..=0x86) {
                            return Err(Error::Unsupported("DVD extended receiver control"));
                        }
                    }
                    let (Some(area), Some(fields)) = (fact.local_area, fact.local_fields) else {
                        continue;
                    };
                    let (width, height) = area.dimensions()?;
                    // The native receiver does not start a one-row raster.
                    // Source fields remain available for an explicit local_raster.
                    if height == 1 {
                        continue;
                    }
                    let count = usize::from(width)
                        .checked_mul(usize::from(height))
                        .ok_or(Error::Overflow)?;
                    let total = self
                        .total_pixels
                        .checked_add(count)
                        .ok_or(Error::Overflow)?;
                    if self.bitmaps == self.limits.max_bitmaps {
                        return Err(Error::Limit {
                            requested: self.bitmaps.checked_add(1).ok_or(Error::Overflow)?,
                            maximum: self.limits.max_bitmaps,
                        });
                    }
                    if total > self.limits.max_total_pixels {
                        return Err(Error::Limit {
                            requested: total,
                            maximum: self.limits.max_total_pixels,
                        });
                    }
                    self.candidate = None;
                    self.palette = Some(native_d32_palette(
                        self.receiver_rgb.as_ref(),
                        fact.state_after.palette_word.unwrap_or(0),
                        fact.state_after.alpha_word.unwrap_or(0),
                    ));
                    self.phase = Phase::Raster(RasterDecode::new(
                        area,
                        fields,
                        (width, height),
                        &self.budget,
                        self.limits.raster,
                        self.pixel_storage.take(),
                        program.first_control_offset(),
                    )?);
                    self.selected_control = Some(index);
                    self.candidate_forced = self.forced_seen;
                    self.total_pixels = total;
                    self.bitmaps += 1;
                    self.raster_finish_paid = false;
                }
                Phase::Raster(raster) => {
                    if !self.raster_finish_paid {
                        let raw = self
                            .program
                            .as_ref()
                            .ok_or(Error::Invalid("DVD control program absent"))?
                            .raw();
                        if raster.poll(raw, work, cancel)? == DvdRasterStatus::Yield {
                            return Ok(TraditionalStatus::Yield);
                        }
                    }
                    if work.remaining() == 0 {
                        return Ok(TraditionalStatus::Yield);
                    }
                    if !self.raster_finish_paid {
                        if !work.try_charge(1) {
                            return Ok(TraditionalStatus::Yield);
                        }
                        self.raster_finish_paid = true;
                    }
                    self.candidate = Some(raster.try_finish(cancel)?);
                    self.phase = Phase::Select;
                }
                Phase::Project(projection) => {
                    if projection.poll(work, cancel)? == ProjectionStatus::Yield {
                        return Ok(TraditionalStatus::Yield);
                    }
                    self.bitmap = Some(projection.try_finish(cancel)?);
                    self.phase = Phase::Ready;
                }
                Phase::Ready => return Ok(TraditionalStatus::Ready),
                Phase::Finished => return Err(Error::Invalid("DVD subpicture already finished")),
            }
        }
    }

    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<TraditionalSubpicture> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if !matches!(self.phase, Phase::Ready) {
            return Err(self.poison(Error::Invalid("DVD subpicture not ready")));
        }
        let program = self
            .program
            .take()
            .ok_or(Error::Invalid("DVD subpicture program absent"))?;
        let times = ReceiverDisplayTimes {
            start_ms: program
                .last_start()
                .map_or(0, |date| u32::from(date.0) * 1024 / 90),
            end_ms: program
                .last_stop()
                .map_or(0, |date| u32::from(date.0) * 1024 / 90),
        };
        let menu = program.has_forced_start();
        self.pixel_storage = None;
        self.phase = Phase::Finished;
        Ok(TraditionalSubpicture {
            program,
            bitmap: self.bitmap.take(),
            palette: self.palette.take(),
            selected_control: self.selected_control,
            forced: self.candidate_forced,
            menu,
            times,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_offsets_match_exact_time_arithmetic() {
        for (num, den) in [
            (1, 90_000),
            (1_001, 24_000),
            (7, 13),
            (u64::MAX, u64::MAX - 1),
        ] {
            let base = TimeBase::new(num, den).unwrap();
            for ticks in [i64::MIN, -90_001, 0, 90_001, i64::MAX] {
                for milliseconds in [0, 193, u32::MAX] {
                    let expected = Time::from_ticks(i128::from(ticks), base).and_then(|time| {
                        time.checked_add(Time::new(i128::from(milliseconds), 1_000)?)
                    });
                    assert_eq!(offset_ticks(ticks, base, milliseconds), expected);
                }
            }
        }
    }
}
