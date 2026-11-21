//! tx3g samples converted to the ASS events and header FFmpeg's `mov_text`
//! decoder emits, re-exported as [`crate::compat::mov_text`].
//!
//! Each output keeps its input, clock and the decoder's extradata unchanged.
//! The specification view of the same samples is
//! [`tx3g_semantics`](crate::text::tx3g_semantics).
mod config;
mod packet;
mod write;

pub use super::decoded::{Clock, Subtitle};
use super::{Format, Input, unicode::cancelled};
use config::Configuration;
use packet::PacketJob;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, Refused, Result, TimeBase, WorkBudget};

/// The decoder options that change its output.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// The packet time base; without one a subtitle has no PTS and its
    /// display ends at 0. No time base is taken from the input's timing or
    /// an earlier packet.
    pub packet_timebase: Option<TimeBase>,
    /// Frame width in pixels, the header's `PlayResX`; at most `i32::MAX`.
    /// The header has 384 × 288 when either dimension is 0, or when the
    /// extradata is shorter than 40 bytes or than its font table.
    pub frame_width: u32,
    /// Frame height in pixels, the header's `PlayResY`; at most `i32::MAX`.
    pub frame_height: u32,
}

/// Resource bounds of the decoder; a value beyond one is `Error::Limit`.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Payload bytes, and block addition bytes, of one input; 4 MiB by
    /// default.
    pub maximum_input_bytes: usize,
    /// Extradata bytes, and an input's `codec_private` bytes; 1 MiB by
    /// default.
    pub maximum_config_bytes: usize,
    /// Font table entries in the extradata; 65535 by default.
    pub maximum_fonts: usize,
    /// Style records in one `styl` box; 65535 by default.
    pub maximum_styles: usize,
    /// Modifier boxes read in one sample; 4096 by default.
    pub maximum_boxes: usize,
    /// Bytes of one ASS event; 4 MiB by default.
    pub maximum_ass_bytes: usize,
    /// Work units spent from opening or the last reset, across packets;
    /// 64 × 1024 × 1024 by default.
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

/// One decoded packet as FFmpeg's decode call returns it: a subtitle, none,
/// or a refusal of the packet. Resource, generation and cancellation
/// failures are errors instead.
#[derive(Debug)]
pub struct Output {
    /// The packet, unchanged.
    pub input: Input,
    /// The clock the packet was provided with.
    pub clock: Clock,
    /// What the decode call returns: the packet size in bytes when it gives
    /// a subtitle, 0 for an empty packet or a bare zero text length, or
    /// AVERROR_INVALIDDATA (-1094995529) for a packet it refuses.
    pub returned: i32,
    /// The subtitle; present exactly when `returned` is positive.
    pub subtitle: Option<Subtitle>,
}

/// What [`Decoder::poll`] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// The work budget ran out; poll again.
    Pending,
    /// The pool could not hold the output; poll again once memory is free.
    NeedOutput,
    /// No packet is being decoded; provide the next.
    NeedInput,
    /// The packet is decoded; take it with
    /// [`take_output`](Decoder::take_output).
    OutputReady,
    /// Input is finished and no packet is pending.
    Finished,
}

/// FFmpeg's `mov_text` decoder over tx3g samples. The extradata is the codec
/// extradata FFmpeg's demuxer hands the decoder, not a sample entry; the
/// first polls read it into the subtitle header. A refused input is handed
/// back unchanged.
#[derive(Debug)]
pub struct Decoder {
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

impl Decoder {
    /// Opens the decoder with `extradata`, accepting inputs of `generation`.
    /// `Error::Overflow` when a frame dimension or a time base term exceeds
    /// `i32::MAX`; `Error::ForeignPool` for extradata from another pool and
    /// `Error::Limit` for more than `limits.maximum_config_bytes` of it.
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

    /// The ASS header FFmpeg's decoder writes for the extradata; `None`
    /// until [`poll`](Self::poll) has read the extradata.
    pub fn subtitle_header(&self) -> Option<&Bytes> {
        self.configuration.header.as_ref()
    }
    /// The extradata the decoder was opened with, unchanged.
    pub fn extradata(&self) -> Option<&Bytes> {
        self.configuration.raw.as_ref()
    }
    /// Work units spent since opening or the last reset, at most
    /// [`Limits::maximum_work`].
    pub fn work_spent(&self) -> usize {
        self.work.spent
    }

    /// Starts decoding `input` at `clock`. It is handed back unchanged with
    /// the sticky error after a failure, `Error::Invalid` after
    /// [`finish_input`](Self::finish_input) or while a packet is pending,
    /// `Error::Unsupported` unless it is [`Format::Tx3g`],
    /// `Error::StaleGeneration` for another generation, `Error::ForeignPool`
    /// for bytes of another pool, `Error::Limit` for bytes beyond their limit
    /// (256 for the codec identifier and charset label) and
    /// `Error::Overflow` for a payload beyond `i32::MAX` bytes.
    #[expect(
        clippy::result_large_err,
        reason = "A refused input goes back to the caller inline, without an allocation; its size is guarded in the text module."
    )]
    pub fn provide(
        &mut self,
        input: Input,
        clock: Clock,
    ) -> std::result::Result<(), Refused<Input>> {
        if let Err(error) = self.check(&input) {
            return Err(Refused {
                error,
                value: input,
            });
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

    /// Reads the extradata, then decodes the pending packet, spending
    /// `caller`. `Error::Cancelled` is resumable; any other error is
    /// returned by every later call until [`reset`](Self::reset).
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
    /// The decoded packet, once [`poll`](Self::poll) reported
    /// [`Status::OutputReady`]; `Error::Invalid` before then.
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
    /// Ends input: once no packet is pending, [`poll`](Self::poll) returns
    /// [`Status::Finished`].
    pub fn finish_input(&mut self, cancel: &AtomicBool) -> Result<()> {
        cancelled(cancel)?;
        self.finished = true;
        Ok(())
    }
    /// Flushes the decoder as FFmpeg's flush does: drops the pending packet
    /// and any sticky error, restarts the events' read order and the work
    /// count, reopens input and accepts inputs of `generation`. The header
    /// and outputs already taken stay valid.
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
