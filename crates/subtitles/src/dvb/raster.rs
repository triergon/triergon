use super::ReceiverView;
use super::bits::{Bits, Feed, Stop, cancelled, take_work};
use super::coverage::Coverage;
use super::model::*;
use super::pixel::{Code, Symbol};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

const CHUNK: usize = 1024;

#[derive(Clone, Copy, Debug)]
struct Run {
    index: u8,
    non_modifying: bool,
    length: u16,
    written: u16,
    announced: bool,
}

#[derive(Clone, Copy, Debug)]
enum Stage {
    Tag,
    Map { tag: u8, at: u8 },
    String(Code),
    Run { code: Code, run: Run },
    Align,
}

/// One private target and its source-coordinate projection. The input/work
/// context is the separate Feed type; neither context erases typed invariants.
struct RasterTarget<'a> {
    pixels: &'a mut [u8],
    spec: RasterSpec,
    geometry: StoredGeometry,
    decoded_pixels: &'a mut usize,
    bounds: &'a mut Option<ClipRect>,
    coverage: Option<&'a mut Coverage>,
    receiver_view: ReceiverView,
}

#[derive(Debug)]
struct Field {
    spec: FieldSpec,
    remaining: usize,
    bits: Bits,
    stage: Stage,
    maps: MapTables,
    // Logical source position includes every decoded code.
    column: u32,
    // The receiver view's suppressed codes omit destination advance only.
    destination_column: u32,
    line: u32,
    line_open: bool,
}

impl Field {
    fn new(spec: FieldSpec) -> Self {
        Self {
            spec,
            remaining: spec.byte_length,
            bits: Bits::default(),
            stage: Stage::Tag,
            maps: spec.maps,
            column: 0,
            destination_column: 0,
            line: spec.parity.line(),
            line_open: false,
        }
    }

    fn source_y(&self, spec: RasterSpec) -> Result<u32> {
        spec.object_origin
            .y
            .checked_add(self.line)
            .ok_or(Error::Overflow)
    }

    fn validate_run(&self, spec: RasterSpec, length: u16) -> Result<()> {
        let end = spec
            .object_origin
            .x
            .checked_add(self.column)
            .and_then(|x| x.checked_add(u32::from(length)))
            .ok_or(Error::Overflow)?;
        if length == 0 || end > spec.region.width {
            return Err(Error::Invalid("DVB run exceeds source row"));
        }
        if self.source_y(spec)? >= spec.region.height {
            return Err(Error::Invalid("DVB run exceeds source height"));
        }
        Ok(())
    }

    fn write_run(
        &self,
        spec: RasterSpec,
        geometry: StoredGeometry,
        pixels: &mut [u8],
        run: Run,
        count: usize,
    ) -> Result<()> {
        // Source bounds were checked for the complete run before any writes.
        // Clipping and nonmodifying holes never hide an invalid source run.
        if run.non_modifying {
            return Ok(());
        }
        let y = i64::from(self.source_y(spec)?);
        let clip = geometry.stored;
        let bottom = clip
            .y
            .checked_add(i64::from(clip.height))
            .ok_or(Error::Overflow)?;
        if y < clip.y || y >= bottom || clip.width == 0 {
            return Ok(());
        }
        let x = spec
            .object_origin
            .x
            .checked_add(self.destination_column)
            .and_then(|x| x.checked_add(u32::from(run.written)))
            .ok_or(Error::Overflow)?;
        let end = i64::from(x)
            .checked_add(i64::try_from(count).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        let right = clip
            .x
            .checked_add(i64::from(clip.width))
            .ok_or(Error::Overflow)?;
        let left = i64::from(x).max(clip.x);
        let end = end.min(right);
        if left >= end {
            return Ok(());
        }
        let row = usize::try_from(y - clip.y).map_err(|_| Error::Overflow)?;
        let column = usize::try_from(left - clip.x).map_err(|_| Error::Overflow)?;
        let count = usize::try_from(end - left).map_err(|_| Error::Overflow)?;
        let start = row
            .checked_mul(geometry.stride)
            .and_then(|v| v.checked_add(column))
            .ok_or(Error::Overflow)?;
        let end = start.checked_add(count).ok_or(Error::Overflow)?;
        pixels
            .get_mut(start..end)
            .ok_or(Error::Invalid("DVB stored plane bounds"))?
            .fill(run.index);
        Ok(())
    }

    fn drive(
        &mut self,
        input: &mut Feed<'_>,
        target: RasterTarget<'_>,
    ) -> std::result::Result<Status, Stop> {
        let RasterTarget {
            pixels,
            spec,
            geometry,
            decoded_pixels,
            bounds,
            mut coverage,
            receiver_view,
        } = target;
        loop {
            cancelled(input.cancel).map_err(Stop::Error)?;
            if matches!(self.stage, Stage::Tag) && input.remaining == 0 && self.bits.aligned_idle()
            {
                if self.line_open {
                    return Err(Stop::Error(Error::Invalid("DVB missing object line end")));
                }
                return Ok(Status::FieldComplete);
            }
            if input.work.remaining() == 0 {
                return Ok(Status::Yield);
            }
            match self.stage {
                Stage::Tag => {
                    let tag = self.bits.read(8, input)?;
                    self.stage = match tag {
                        0x10..=0x12 => {
                            let depth = match tag {
                                0x10 => Depth::Two,
                                0x11 => Depth::Four,
                                _ => Depth::Eight,
                            };
                            if depth.bits() > spec.depth.bits() {
                                return Err(Stop::Error(Error::Unsupported(
                                    "DVB receiver depth reduction",
                                )));
                            }
                            self.line_open = true;
                            Stage::String(Code::new(depth))
                        }
                        0x20..=0x22 => Stage::Map { tag, at: 0 },
                        0xf0 => {
                            if self.source_y(spec).map_err(Stop::Error)? >= spec.region.height {
                                return Err(Stop::Error(Error::Invalid(
                                    "DVB line exceeds source height",
                                )));
                            }
                            self.column = 0;
                            self.destination_column = 0;
                            self.line = self
                                .line
                                .checked_add(2)
                                .ok_or(Stop::Error(Error::Overflow))?;
                            self.line_open = false;
                            Stage::Tag
                        }
                        _ => {
                            return Err(Stop::Error(Error::Unsupported(
                                "DVB pixel-data sub-block",
                            )));
                        }
                    };
                }
                Stage::Map { tag, at } => {
                    let value = self.bits.read(8, input)?;
                    match tag {
                        0x20 => {
                            let base = usize::from(at) * 2;
                            self.maps.two_to_four[base] = value >> 4;
                            self.maps.two_to_four[base + 1] = value & 15;
                        }
                        0x21 => self.maps.two_to_eight[usize::from(at)] = value,
                        0x22 => self.maps.four_to_eight[usize::from(at)] = value,
                        _ => return Err(Stop::Error(Error::Invalid("DVB map state"))),
                    }
                    let length = match tag {
                        0x20 => 2,
                        0x21 => 4,
                        _ => 16,
                    };
                    self.stage = if at + 1 == length {
                        Stage::Tag
                    } else {
                        Stage::Map { tag, at: at + 1 }
                    };
                }
                Stage::String(mut code) => {
                    let value = self.bits.read(code.width(), input)?;
                    match code.accept(value).map_err(Stop::Error)? {
                        None => self.stage = Stage::String(code),
                        Some(Symbol::End) => self.stage = Stage::Align,
                        Some(Symbol::Run { length, index }) => {
                            self.validate_run(spec, length).map_err(Stop::Error)?;
                            let x = spec
                                .object_origin
                                .x
                                .checked_add(self.column)
                                .ok_or(Stop::Error(Error::Overflow))?;
                            let y = self.source_y(spec).map_err(Stop::Error)?;
                            let right = i64::from(x) + i64::from(length);
                            let bottom = i64::from(y) + 1;
                            let (left, top, right, bottom) = match *bounds {
                                Some(old) => (
                                    old.x.min(i64::from(x)),
                                    old.y.min(i64::from(y)),
                                    (old.x + i64::from(old.width)).max(right),
                                    (old.y + i64::from(old.height)).max(bottom),
                                ),
                                None => (i64::from(x), i64::from(y), right, bottom),
                            };
                            *bounds = Some(ClipRect {
                                x: left,
                                y: top,
                                width: u32::try_from(right - left)
                                    .map_err(|_| Stop::Error(Error::Overflow))?,
                                height: u32::try_from(bottom - top)
                                    .map_err(|_| Stop::Error(Error::Overflow))?,
                            });
                            let mapped_index = self
                                .maps
                                .map(code.depth, spec.depth, index)
                                .map_err(Stop::Error)?;
                            // Source retains its mapped-CLUT1 policy. The
                            // observed receiver-view tests encoded
                            // code1, before mapping to the output index.
                            let non_modifying = spec.non_modifying
                                && match receiver_view {
                                    ReceiverView::Source => mapped_index == 1,
                                    ReceiverView::Receiver => index == 1,
                                };
                            self.stage = Stage::Run {
                                code,
                                run: Run {
                                    index: mapped_index,
                                    non_modifying,
                                    length,
                                    written: 0,
                                    announced: false,
                                },
                            };
                        }
                    }
                }
                Stage::Run { code, mut run } => {
                    if !run.announced {
                        if take_work(input.work, input.total, input.limit, 1, input.cancel)
                            .map_err(Stop::Error)?
                            == 0
                        {
                            return Ok(Status::Yield);
                        }
                        run.announced = true;
                        self.stage = Stage::Run { code, run };
                    }
                    let remaining = usize::from(run.length - run.written);
                    let count = take_work(
                        input.work,
                        input.total,
                        input.limit,
                        remaining.min(CHUNK),
                        input.cancel,
                    )
                    .map_err(Stop::Error)?;
                    if count == 0 {
                        return Ok(Status::Yield);
                    }
                    if let Some(coverage) = coverage.as_deref_mut() {
                        let x = spec
                            .object_origin
                            .x
                            .checked_add(self.column)
                            .and_then(|v| v.checked_add(u32::from(run.written)))
                            .ok_or(Stop::Error(Error::Overflow))?;
                        let y = self.source_y(spec).map_err(Stop::Error)?;
                        let first = u64::from(y)
                            .checked_mul(u64::from(spec.region.width))
                            .and_then(|v| v.checked_add(u64::from(x)))
                            .ok_or(Stop::Error(Error::Overflow))?;
                        coverage
                            .claim(
                                usize::try_from(first).map_err(|_| Stop::Error(Error::Overflow))?,
                                count,
                            )
                            .map_err(Stop::Error)?;
                    }
                    self.write_run(spec, geometry, pixels, run, count)
                        .map_err(Stop::Error)?;
                    run.written = run
                        .written
                        .checked_add(
                            u16::try_from(count).map_err(|_| Stop::Error(Error::Overflow))?,
                        )
                        .ok_or(Stop::Error(Error::Overflow))?;
                    *decoded_pixels = decoded_pixels
                        .checked_add(count)
                        .ok_or(Stop::Error(Error::Overflow))?;
                    self.stage = if run.written == run.length {
                        self.column = self
                            .column
                            .checked_add(u32::from(run.length))
                            .ok_or(Stop::Error(Error::Overflow))?;
                        if receiver_view == ReceiverView::Source || !run.non_modifying {
                            self.destination_column = self
                                .destination_column
                                .checked_add(u32::from(run.length))
                                .ok_or(Stop::Error(Error::Overflow))?;
                        }
                        Stage::String(code)
                    } else {
                        Stage::Run { code, run }
                    };
                }
                Stage::Align => {
                    self.bits.align_zero().map_err(Stop::Error)?;
                    self.stage = Stage::Tag;
                }
            }
        }
    }

    fn advance(
        &mut self,
        mut input: Feed<'_>,
        target: RasterTarget<'_>,
    ) -> std::result::Result<Progress, Failure> {
        let status = self.drive(&mut input, target);
        self.remaining = input.remaining;
        let consumed = input.consumed;
        let status = match status {
            Ok(status) => status,
            Err(Stop::NeedInput) => Status::NeedInput,
            Err(Stop::Yield) => Status::Yield,
            Err(Stop::Error(error)) => {
                return Err(Failure {
                    consumed,
                    field_offset: self.spec.byte_length - self.remaining,
                    error,
                });
            }
        };
        Ok(Progress { consumed, status })
    }
}

/// A private cropped region transaction for one or more interlaced objects.
/// The enclosing format owns page/codec clocks and compound source provenance.
///
/// Constructor reservations precede allocation; initialization and pixel stores
/// are work-yielding. Syntax failure/cancellation poisons and frees the private
/// plane. A failed freeze returns its builder internally and is retryable.
#[derive(Debug)]
pub struct Raster {
    budget: Budget,
    spec: RasterSpec,
    limits: RasterLimits,
    geometry: StoredGeometry,
    area: usize,
    pixels: Option<BudgetVec<u8>>,
    initial: Option<InitialPlane>,
    source: SourceFacts,
    active: Option<Field>,
    completed: u8,
    last_maps: Option<MapTables>,
    failed: Option<Error>,
    finished: bool,
    total_work: usize,
    decoded_pixels: usize,
    bounds: Option<ClipRect>,
    coverage: Option<Coverage>,
    receiver_view: ReceiverView,
}

fn geometry(spec: RasterSpec, limits: RasterLimits) -> Result<(StoredGeometry, usize)> {
    if spec.region.width == 0 || spec.region.height == 0 {
        return Err(Error::Invalid("DVB zero region size"));
    }
    for axis in [spec.region.width, spec.region.height] {
        if axis > limits.max_axis {
            return Err(Error::Limit {
                requested: usize::try_from(axis).map_err(|_| Error::Overflow)?,
                maximum: usize::try_from(limits.max_axis).map_err(|_| Error::Overflow)?,
            });
        }
    }
    let source_area = u64::from(spec.region.width)
        .checked_mul(u64::from(spec.region.height))
        .ok_or(Error::Overflow)?;
    let source_area = usize::try_from(source_area).map_err(|_| Error::Overflow)?;
    if source_area > limits.max_pixels {
        return Err(Error::Limit {
            requested: source_area,
            maximum: limits.max_pixels,
        });
    }
    if spec.object_origin.x >= spec.region.width || spec.object_origin.y >= spec.region.height {
        return Err(Error::Invalid("DVB object origin outside region"));
    }
    let bounds = ClipRect {
        x: 0,
        y: 0,
        width: spec.region.width,
        height: spec.region.height,
    };
    let clip = spec.clip.unwrap_or(bounds);
    let x_end = clip
        .x
        .checked_add(i64::from(clip.width))
        .ok_or(Error::Overflow)?;
    let y_end = clip
        .y
        .checked_add(i64::from(clip.height))
        .ok_or(Error::Overflow)?;
    let x = clip.x.clamp(0, i64::from(bounds.width));
    let y = clip.y.clamp(0, i64::from(bounds.height));
    let right = x_end.clamp(0, i64::from(bounds.width)).max(x);
    let bottom = y_end.clamp(0, i64::from(bounds.height)).max(y);
    let stored = ClipRect {
        x,
        y,
        width: u32::try_from(right - x).map_err(|_| Error::Overflow)?,
        height: u32::try_from(bottom - y).map_err(|_| Error::Overflow)?,
    };
    let stride = usize::try_from(stored.width).map_err(|_| Error::Overflow)?;
    let area = stride
        .checked_mul(usize::try_from(stored.height).map_err(|_| Error::Overflow)?)
        .ok_or(Error::Overflow)?;
    Ok((
        StoredGeometry {
            region: spec.region,
            object_origin: spec.object_origin,
            requested_clip: spec.clip,
            stored,
            stride,
        },
        area,
    ))
}

impl Raster {
    pub fn new(
        budget: &Budget,
        spec: RasterSpec,
        limits: RasterLimits,
        initial: InitialPlane,
        source: SourceFacts,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        cancelled(cancel)?;
        let (geometry, area) = geometry(spec, limits)?;
        source.palette.validate(budget, spec.depth)?;
        if source
            .object
            .as_ref()
            .is_some_and(|bytes| !bytes.belongs_to(budget))
        {
            return Err(Error::Invalid("DVB object source uses another budget"));
        }
        match &initial {
            InitialPlane::Fill(index) if *index > spec.depth.maximum() => {
                return Err(Error::Invalid("DVB initial fill index"));
            }
            InitialPlane::Copy(bytes) => {
                if bytes.len() != area {
                    return Err(Error::Invalid("DVB initial plane shape"));
                }
                if !bytes.belongs_to(budget) {
                    return Err(Error::Invalid("DVB initial plane uses another budget"));
                }
            }
            InitialPlane::Fill(_) => {}
        }
        let mut pixels = BudgetVec::new(budget);
        pixels.try_reserve_exact(area)?;
        Ok(Self {
            budget: budget.clone(),
            spec,
            limits,
            geometry,
            area,
            pixels: Some(pixels),
            initial: Some(initial),
            source,
            active: None,
            completed: 0,
            last_maps: None,
            failed: None,
            finished: false,
            total_work: 0,
            decoded_pixels: 0,
            bounds: None,
            coverage: None,
            receiver_view: ReceiverView::Source,
        })
    }

    /// Select the enclosing receiver policy before any initialization or fields.
    pub(super) fn new_with_receiver_view(
        budget: &Budget,
        spec: RasterSpec,
        limits: RasterLimits,
        initial: InitialPlane,
        source: SourceFacts,
        receiver_view: ReceiverView,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let mut raster = Self::new(budget, spec, limits, initial, source, cancel)?;
        raster.receiver_view = receiver_view;
        Ok(raster)
    }

    /// Enable exact overlap validation for this private region's object list.
    /// The format owner calls this before the first field. A new Raster starts a
    /// new display composition, so historical pixels are never marked occupied.
    pub(super) fn require_disjoint_objects(&mut self) -> Result<()> {
        if self.active.is_some() || self.completed != 0 || self.finished {
            return Err(Error::Invalid("DVB coverage must precede fields"));
        }
        if self.coverage.is_none() {
            self.coverage = Some(Coverage::new(&self.budget, self.spec.region)?);
        }
        Ok(())
    }

    /// Continue rasterizing another object into the same private region target.
    /// No immutable output may have been published and both preceding fields
    /// must be complete. The enclosing DVB state owns compound-object provenance.
    pub fn start_object(
        &mut self,
        origin: Point,
        non_modifying: bool,
        source: SourceFacts,
        cancel: &AtomicBool,
    ) -> Result<()> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        cancelled(cancel)?;
        if self.finished || self.active.is_some() || self.completed != 3 {
            return Err(Error::Invalid(
                "DVB preceding object incomplete or published",
            ));
        }
        if origin.x >= self.spec.region.width || origin.y >= self.spec.region.height {
            return Err(Error::Invalid("DVB object origin outside region"));
        }
        source.palette.validate(&self.budget, self.spec.depth)?;
        if source
            .object
            .as_ref()
            .is_some_and(|b| !b.belongs_to(&self.budget))
        {
            return Err(Error::Invalid("DVB object source uses another budget"));
        }
        self.spec.object_origin = origin;
        self.spec.non_modifying = non_modifying;
        self.geometry.object_origin = origin;
        self.source = source;
        self.completed = 0;
        self.last_maps = None;
        self.bounds = None;
        Ok(())
    }

    pub fn start_field(&mut self, field: FieldSpec) -> Result<()> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.finished {
            return Err(Error::Invalid("DVB raster already finished"));
        }
        if self.active.is_some() {
            return Err(Error::Invalid("DVB field already active"));
        }
        if self.completed & field.parity.bit() != 0 {
            return Err(Error::Invalid("DVB duplicate field parity"));
        }
        let maximum = self.limits.max_field_bytes.min(65535);
        if field.byte_length > maximum {
            return Err(Error::Limit {
                requested: field.byte_length,
                maximum,
            });
        }
        field.maps.validate()?;
        self.active = Some(Field::new(field));
        Ok(())
    }

    fn poison(&mut self, error: Error) {
        self.failed = Some(error);
        self.active = None;
        self.pixels = None;
        self.initial = None;
        self.coverage = None;
    }

    fn prepare(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        let pixels = self
            .pixels
            .as_mut()
            .ok_or(Error::Invalid("DVB private plane absent"))?;
        while pixels.len() < self.area {
            let count = take_work(
                work,
                &mut self.total_work,
                self.limits.max_work,
                (self.area - pixels.len()).min(CHUNK),
                cancel,
            )?;
            if count == 0 {
                return Ok(false);
            }
            let start = pixels.len();
            match self
                .initial
                .as_ref()
                .ok_or(Error::Invalid("DVB initial plane absent"))?
            {
                InitialPlane::Fill(index) => {
                    let chunk = [*index; CHUNK];
                    pixels.try_extend_from_slice(&chunk[..count])?;
                }
                InitialPlane::Copy(bytes) => {
                    let end = start.checked_add(count).ok_or(Error::Overflow)?;
                    let values = bytes
                        .as_ref()
                        .get(start..end)
                        .ok_or(Error::Invalid("DVB initial plane shape"))?;
                    if values
                        .iter()
                        .any(|&index| index > self.spec.depth.maximum())
                    {
                        return Err(Error::Invalid("DVB initial plane index"));
                    }
                    pixels.try_extend_from_slice(values)?;
                }
            }
        }
        self.initial = None;
        if let Some(coverage) = self.coverage.as_mut()
            && !coverage.prepare(work, &mut self.total_work, self.limits.max_work, cancel)?
        {
            return Ok(false);
        }
        Ok(true)
    }

    /// `final_input` means this supplied tail is known final for this field. It is
    /// not an EOF/transport signal. Only `consumed` bytes may be discarded; the
    /// remaining caller slice must be supplied again after Yield/NeedInput.
    pub fn step(
        &mut self,
        input: &[u8],
        final_input: bool,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> std::result::Result<Progress, Failure> {
        let offset = self
            .active
            .as_ref()
            .map_or(0, |field| field.spec.byte_length - field.remaining);
        if let Some(error) = self.failed {
            return Err(Failure {
                consumed: 0,
                field_offset: offset,
                error,
            });
        }
        let preflight = cancelled(cancel).and_then(|()| {
            if self.finished {
                Err(Error::Invalid("DVB raster already finished"))
            } else if self.active.is_none() {
                Err(Error::Invalid("DVB no active field"))
            } else {
                Ok(())
            }
        });
        if let Err(error) = preflight {
            self.poison(error);
            return Err(Failure {
                consumed: 0,
                field_offset: offset,
                error,
            });
        }
        if work.remaining() == 0 {
            return Ok(Progress {
                consumed: 0,
                status: Status::Yield,
            });
        }
        match self.prepare(work, cancel) {
            Ok(false) => {
                return Ok(Progress {
                    consumed: 0,
                    status: Status::Yield,
                });
            }
            Err(error) => {
                self.poison(error);
                return Err(Failure {
                    consumed: 0,
                    field_offset: offset,
                    error,
                });
            }
            Ok(true) => {}
        }
        let Some(mut field) = self.active.take() else {
            let error = Error::Invalid("DVB field state absent");
            self.poison(error);
            return Err(Failure {
                consumed: 0,
                field_offset: offset,
                error,
            });
        };
        let Some(pixels) = self.pixels.as_mut() else {
            let error = Error::Invalid("DVB private plane absent");
            self.poison(error);
            return Err(Failure {
                consumed: 0,
                field_offset: offset,
                error,
            });
        };
        let input = Feed {
            bytes: input,
            consumed: 0,
            remaining: field.remaining,
            final_input,
            work,
            total: &mut self.total_work,
            limit: self.limits.max_work,
            cancel,
        };
        let target = RasterTarget {
            pixels: pixels.as_mut_slice(),
            spec: self.spec,
            geometry: self.geometry,
            decoded_pixels: &mut self.decoded_pixels,
            bounds: &mut self.bounds,
            coverage: self.coverage.as_mut(),
            receiver_view: self.receiver_view,
        };
        let result = field.advance(input, target);
        match result {
            Ok(progress) if progress.status == Status::FieldComplete => {
                self.completed |= field.spec.parity.bit();
                self.last_maps = Some(field.maps);
                Ok(progress)
            }
            Ok(progress) => {
                self.active = Some(field);
                Ok(progress)
            }
            Err(failure) => {
                self.poison(failure.error);
                Err(failure)
            }
        }
    }

    pub fn field_maps(&self) -> Option<MapTables> {
        self.last_maps
    }
    /// Source-space enclosing rectangle for the current object's decoded codes,
    /// including clipped and nonmodifying pixels. Read after both fields complete.
    pub fn object_bounds(&self) -> Option<ClipRect> {
        self.bounds
    }

    pub fn decoded_pixels(&self) -> usize {
        self.decoded_pixels
    }
    pub fn total_work(&self) -> usize {
        self.total_work
    }

    /// Both fields must be explicitly completed. For an ODS with absent bottom
    /// data, replay the bounded top field with Bottom parity; no bytes are copied.
    /// Backpressure here preserves the private builder for an identical retry.
    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<IndexedPlane> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if let Err(error) = cancelled(cancel) {
            self.poison(error);
            return Err(error);
        }
        if self.finished {
            return Err(Error::Invalid("DVB raster already finished"));
        }
        if self.active.is_some() || self.completed != 3 || self.initial.is_some() {
            return Err(Error::Invalid("DVB fields incomplete"));
        }
        let pixels = self
            .pixels
            .take()
            .ok_or(Error::Invalid("DVB private plane absent"))?;
        match pixels.freeze() {
            Ok(pixels) => {
                self.coverage = None;
                self.finished = true;
                Ok(IndexedPlane {
                    geometry: self.geometry,
                    depth: self.spec.depth,
                    pixels,
                    source: self.source.clone(),
                })
            }
            Err(failure) => {
                self.pixels = Some(failure.buffer);
                Err(failure.error)
            }
        }
    }
}
