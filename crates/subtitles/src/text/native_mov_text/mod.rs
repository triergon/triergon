//! Explicit, bounded D32 native mov_text packet-to-ASS consumer.
//!
//! This is a separate receiver view. Raw Input, source timing and configuration
//! are retained; neither Extractor nor the strict SemanticJob is changed.
mod config;
mod packet;
mod write;

use super::{Format, Input, ProvideError, unicode::cancelled};
use config::Configuration;
use packet::PacketJob;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, Result, TimeBase, WorkBudget};

pub const NATIVE_MOV_TEXT: &str = "NativeD32MovTextAss1";

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// AVCodec packet timebase. None reproduces an unset native context clock;
    /// no clock is inferred from Source Timing or a previous packet.
    pub packet_timebase: Option<TimeBase>,
    pub frame_width: u32,
    pub frame_height: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub maximum_input_bytes: usize,
    pub maximum_config_bytes: usize,
    pub maximum_fonts: usize,
    pub maximum_styles: usize,
    pub maximum_boxes: usize,
    pub maximum_ass_bytes: usize,
    pub maximum_work: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            maximum_input_bytes: 4 * 1024 * 1024,
            maximum_config_bytes: 1024 * 1024,
            maximum_fonts: 65535,
            maximum_styles: 65535,
            maximum_boxes: 4096,
            maximum_ass_bytes: 4 * 1024 * 1024,
            maximum_work: 64 * 1024 * 1024,
        }
    }
}

/// Native packet clocks are explicit and separate from the original exact
/// Source Timing. Unknown and known zero duration remain distinct here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Clock {
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub duration: Option<i64>,
}

/// Complete emitted AVSubtitle/ASS facts for this decoder's one ASS rectangle.
/// Its geometry/flags are zero, rectangle type is ASS (3), format is text (1).
#[derive(Debug)]
pub struct Subtitle {
    pub pts_microseconds: Option<i64>,
    pub start_display_ms: u32,
    pub end_display_ms: u32,
    pub ass: Bytes,
}
impl Subtitle {
    pub const fn format(&self) -> u16 {
        1
    }
    pub const fn rectangle_type(&self) -> u8 {
        3
    }
    pub const fn geometry(&self) -> [i32; 4] {
        [0; 4]
    }
    pub const fn flags(&self) -> u32 {
        0
    }
    pub const fn palette_entries(&self) -> u32 {
        0
    }
}

/// A completed native API call, including clear/no-output and data refusal.
/// Resource, generation and cancellation errors use core Error instead.
#[derive(Debug)]
pub struct Output {
    pub input: Input,
    pub clock: Clock,
    /// Native return value: complete physical size, zero clear/empty result, or
    /// AVERROR_INVALIDDATA (-1094995529). No observer no-progress loop is hidden.
    pub returned: i32,
    pub subtitle: Option<Subtitle>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pending,
    NeedOutput,
    NeedInput,
    OutputReady,
    Finished,
}

/// Fresh native-compatible context with a bounded incremental open phase.
/// Extradata must be the caller's AVCodec extradata, not an assumed SampleEntry.
/// All caller-owned inputs rejected by provide are returned intact.
#[derive(Debug)]
pub struct NativeMovText {
    pool: Budget,
    limits: Limits,
    options: Options,
    generation: u64,
    configuration: Configuration,
    job: Option<PacketJob>,
    read_order: i32,
    work: Funding,
    finished: bool,
    failed: Option<Error>,
}

impl NativeMovText {
    pub fn new(
        pool: &Budget,
        extradata: Option<&Bytes>,
        options: Options,
        limits: Limits,
        generation: u64,
    ) -> Result<Self> {
        if options.frame_width > i32::MAX as u32 || options.frame_height > i32::MAX as u32 {
            return Err(Error::Overflow);
        }
        if let Some(base) = options.packet_timebase {
            i32::try_from(base.numerator()).map_err(|_| Error::Overflow)?;
            i32::try_from(base.denominator()).map_err(|_| Error::Overflow)?;
        }
        Ok(Self {
            configuration: Configuration::new(pool, extradata, options, limits)?,
            pool: pool.clone(),
            limits,
            options,
            generation,
            job: None,
            read_order: 0,
            work: Funding::new(limits.maximum_work),
            finished: false,
            failed: None,
        })
    }

    pub fn subtitle_header(&self) -> Option<&Bytes> {
        self.configuration.header.as_ref()
    }
    pub fn extradata(&self) -> Option<&Bytes> {
        self.configuration.raw.as_ref()
    }
    pub fn header_uses_fallback(&self) -> Option<bool> {
        self.subtitle_header().map(|_| self.configuration.fallback)
    }
    pub fn work_spent(&self) -> usize {
        self.work.spent
    }

    #[expect(
        clippy::result_large_err,
        reason = "Return the exact existing Input on rejection without allocating; ProvideError has an existing physical size guard."
    )]
    pub fn provide(&mut self, input: Input, clock: Clock) -> std::result::Result<(), ProvideError> {
        if let Err(error) = self.check(&input) {
            return Err(ProvideError { error, input });
        }
        self.job = Some(PacketJob::new(input, clock, &self.pool));
        Ok(())
    }
    fn check(&self, input: &Input) -> Result<()> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.finished || self.job.is_some() {
            return Err(Error::Invalid("native mov_text input state"));
        }
        if input.format != Format::Tx3g {
            return Err(Error::Unsupported("native mov_text requires tx3g packet"));
        }
        if input.generation != self.generation {
            return Err(Error::StaleGeneration {
                expected: self.generation,
                received: input.generation,
            });
        }
        for (owner, maximum) in [
            (Some(&input.payload), self.limits.maximum_input_bytes),
            (
                input.codec_private.as_ref(),
                self.limits.maximum_config_bytes,
            ),
            (input.codec_id.as_ref(), 256),
            (input.charset_label.as_ref(), 256),
            (
                input.addition.as_ref().map(|a| &a.bytes),
                self.limits.maximum_input_bytes,
            ),
        ]
        .into_iter()
        .filter_map(|(v, m)| v.map(|b| (b, m)))
        {
            if !owner.belongs_to(&self.pool) {
                return Err(Error::ForeignPool);
            }
            if owner.len() > maximum {
                return Err(Error::Limit {
                    requested: owner.len(),
                    maximum,
                });
            }
        }
        i32::try_from(input.payload.len()).map_err(|_| Error::Overflow)?;
        Ok(())
    }

    pub fn poll(&mut self, caller: &mut WorkBudget, cancel: &AtomicBool) -> Result<Status> {
        cancelled(cancel)?;
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.job.as_ref().is_some_and(PacketJob::ready) {
            return Ok(Status::OutputReady);
        }
        if caller.remaining() == 0 {
            return Ok(Status::Pending);
        }
        let result = self.inner(caller, cancel);
        match result {
            Err(Error::Backpressure { .. }) => Ok(Status::NeedOutput),
            Err(Error::Cancelled) => Err(Error::Cancelled),
            Err(error) => {
                self.job = None;
                self.failed = Some(error);
                Err(error)
            }
            value => value,
        }
    }
    fn inner(&mut self, caller: &mut WorkBudget, cancel: &AtomicBool) -> Result<Status> {
        if !self
            .configuration
            .poll(&self.pool, self.limits, &mut self.work, caller, cancel)?
        {
            return Ok(Status::Pending);
        }
        let Some(job) = self.job.as_mut() else {
            return Ok(if self.finished {
                Status::Finished
            } else {
                Status::NeedInput
            });
        };
        if job.poll(
            &self.configuration,
            self.options,
            self.limits,
            self.read_order,
            &mut self.work,
            caller,
            cancel,
        )? {
            if job.advances_read_order() {
                self.read_order = self.read_order.checked_add(1).ok_or(Error::Overflow)?;
            }
            Ok(Status::OutputReady)
        } else {
            Ok(Status::Pending)
        }
    }
    pub fn take_output(&mut self, cancel: &AtomicBool) -> Result<Output> {
        cancelled(cancel)?;
        if !self.job.as_ref().is_some_and(PacketJob::ready) {
            return Err(Error::Invalid("native mov_text output not ready"));
        }
        self.job
            .take()
            .ok_or(Error::Invalid("native mov_text missing job"))?
            .take_output(self.options)
    }
    pub fn finish_input(&mut self, cancel: &AtomicBool) -> Result<()> {
        cancelled(cancel)?;
        self.finished = true;
        Ok(())
    }
    /// Native public flush resets read order; previously returned owners survive.
    pub fn reset(&mut self, generation: u64, cancel: &AtomicBool) -> Result<()> {
        cancelled(cancel)?;
        self.job = None;
        self.generation = generation;
        self.read_order = 0;
        self.finished = false;
        self.failed = None;
        self.work = Funding::new(self.limits.maximum_work);
        Ok(())
    }
}

/// Funding stays attached to the current atomic transition until it commits.
/// A funded allocation that encounters pressure is not charged again on retry.
#[derive(Debug)]
struct Funding {
    credit: usize,
    spent: usize,
    maximum: usize,
}
impl Funding {
    fn new(maximum: usize) -> Self {
        Self {
            credit: 0,
            spent: 0,
            maximum,
        }
    }
    fn pay(&mut self, cost: usize, caller: &mut WorkBudget) -> Result<bool> {
        let available = usize::try_from(caller.remaining()).unwrap_or(usize::MAX);
        let paid = cost.saturating_sub(self.credit).min(available);
        let total = self.spent.checked_add(paid).ok_or(Error::Overflow)?;
        if total > self.maximum {
            return Err(Error::Limit {
                requested: total,
                maximum: self.maximum,
            });
        }
        if !caller.try_charge(paid as u64) {
            return Err(Error::Invalid("native mov_text funding"));
        }
        self.spent = total;
        self.credit += paid;
        Ok(self.credit == cost)
    }
    fn commit(&mut self) {
        self.credit = 0;
    }
}
