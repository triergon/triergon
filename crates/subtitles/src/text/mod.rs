//! Bounded reading of demuxed text samples into cues that keep every input
//! byte and exact times.
//!
//! An [`Extractor`] reads each sample by its [`Format`]: [`Format::Tx3g`],
//! [`Format::Utf8Packet`], [`Format::MatroskaWebVtt`] or
//! [`Format::OpaqueAss`]. Their rules and default limits are documented in
//! this module's README.md. These operations follow the specifications, not
//! FFmpeg; the FFmpeg-compatible readers and conversions are in
//! [`crate::compat::document`], [`crate::compat::ass`],
//! [`crate::compat::charset`] and [`crate::compat::mov_text`].

pub(crate) mod ass_decoder;
pub mod charset;
pub(crate) mod decoded;
pub(crate) mod demuxer;
pub(crate) mod mov_text;
pub(crate) mod receiver_charset;
pub mod tx3g_config;
pub mod tx3g_semantics;

mod emit;
mod model;
mod sample;
mod session;
mod unicode;
pub use self::model::*;

use self::session::{Action, Session};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error, Refused, Result, WorkBudget};

// Failure must return the exact input even when no output budget is available.
// The Result envelope permits one extra 16-byte alignment unit for its tag.
const _: () = assert!(size_of::<Refused<Input>>() <= 432);
const _: () = assert!(size_of::<std::result::Result<(), Refused<Input>>>() <= 448);

/// What [`Extractor::step`] produced.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "Exclusive delivery states move charged output inline, preserving allocation-free UTF-8 publication; guarded at 672 bytes."
)]
pub enum Status {
    /// No input is pending; provide the next.
    NeedInput,
    /// The work budget ran out; step again.
    Yield,
    /// The pending input's cue; the extractor takes the next input.
    Cue(Cue),
    /// The pool could not hold the output; step again once memory is free.
    NeedOutput,
    /// Input is finished and none is pending.
    Finished,
}

const _: () = assert!(size_of::<Status>() <= 672);
const _: () = assert!(size_of::<Result<Status>>() <= 688);

/// Reads one demuxed sample per [`provide`](Self::provide) into a [`Cue`]:
/// its text range, strict character decoding, tx3g modifier boxes and WebVTT
/// fields. Transport assembly, charset selection and container discovery
/// stay with the caller.
#[derive(Debug)]
pub struct Extractor {
    pool: Budget,
    iso_tx3g: bool,
    limits: Limits,
    generation: u64,
    pending: Option<Input>,
    session: Option<Session>,
    failed: Option<Error>,
    finished: bool,
}

impl Extractor {
    /// [`new`](Self::new), with tx3g modifier boxes also read in their ISO
    /// forms: size 0 (to the end of the sample), size 1 (a 64-bit size) and
    /// `uuid` boxes.
    pub fn tx3g_iso_boxes(pool: &Budget, limits: Limits, generation: u64) -> Self {
        let mut result = Self::new(pool, limits, generation);
        result.iso_tx3g = true;
        result
    }

    /// An extractor charging `pool`, bounded by `limits`, that accepts
    /// inputs of `generation`. A tx3g modifier box of size 0 or 1 is
    /// `Error::Unsupported`.
    pub fn new(pool: &Budget, limits: Limits, generation: u64) -> Self {
        Self {
            pool: pool.clone(),
            iso_tx3g: false,
            limits,
            generation,
            pending: None,
            session: None,
            failed: None,
            finished: false,
        }
    }

    /// Takes the next input. It is handed back unchanged with
    /// `Error::Invalid` after [`finish_input`](Self::finish_input) or while
    /// an input is pending, `Error::StaleGeneration` for another generation,
    /// `Error::ForeignPool` for bytes of another pool and `Error::Limit` for
    /// bytes beyond their limit.
    #[expect(
        clippy::result_large_err,
        reason = "A refused input goes back to the caller inline, without an allocation; its size is guarded above."
    )]
    pub fn provide(&mut self, input: Input) -> std::result::Result<(), Refused<Input>> {
        match self.check_input(&input) {
            Ok(()) => {
                self.session = Some(Session::new(&input, &self.pool, self.iso_tx3g));
                self.pending = Some(input);
                Ok(())
            }
            Err(error) => Err(Refused {
                error,
                value: input,
            }),
        }
    }

    fn check_input(&self, input: &Input) -> Result<()> {
        if self.finished {
            return Err(Error::Invalid("text input after finish"));
        }
        if self.pending.is_some() {
            return Err(Error::Invalid("text input already pending"));
        }
        if input.generation != self.generation {
            return Err(Error::StaleGeneration {
                expected: self.generation,
                received: input.generation,
            });
        }
        for (bytes, maximum) in [
            (Some(&input.payload), self.limits.max_input_bytes),
            (input.codec_private.as_ref(), self.limits.max_config_bytes),
            (input.codec_id.as_ref(), self.limits.max_label_bytes),
            (input.charset_label.as_ref(), self.limits.max_label_bytes),
            (
                input.addition.as_ref().map(|addition| &addition.bytes),
                self.limits.max_addition_bytes,
            ),
        ]
        .into_iter()
        .filter_map(|(bytes, maximum)| bytes.map(|bytes| (bytes, maximum)))
        {
            if !bytes.belongs_to(&self.pool) {
                return Err(Error::ForeignPool);
            }
            if bytes.len() > maximum {
                return Err(Error::Limit {
                    requested: bytes.len(),
                    maximum,
                });
            }
        }
        Ok(())
    }

    /// Ends input: once no input is pending, [`step`](Self::step) returns
    /// [`Status::Finished`].
    pub fn finish_input(&mut self) {
        self.finished = true;
    }

    /// Drops the pending input and any sticky error, reopens input and
    /// accepts inputs of `generation`. Cues already returned stay valid.
    pub fn reset(&mut self, generation: u64) {
        self.session = None;
        self.pending = None;
        self.failed = None;
        self.generation = generation;
        self.finished = false;
    }

    /// Reads the pending input, spending `work`. `Error::Cancelled` is
    /// resumable; any other error is returned by every later call until
    /// [`reset`](Self::reset).
    pub fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Status> {
        unicode::cancelled(cancel)?;
        if let Some(error) = self.failed {
            return Err(error);
        }
        let Some(input) = self.pending.as_ref() else {
            return Ok(if self.finished {
                Status::Finished
            } else {
                Status::NeedInput
            });
        };
        if work.remaining() == 0 {
            return Ok(Status::Yield);
        }
        let result = self
            .session
            .as_mut()
            .ok_or(Error::Invalid("missing text session"))?
            .step(input, &self.pool, self.limits, work, cancel);
        match result {
            Ok(Action::Yield) => Ok(Status::Yield),
            Ok(Action::Cue(cue)) => {
                self.session = None;
                self.pending = None;
                Ok(Status::Cue(cue))
            }
            Ok(Action::Complete) => {
                self.session = None;
                self.pending = None;
                Ok(if self.finished {
                    Status::Finished
                } else {
                    Status::NeedInput
                })
            }
            Err(Error::Backpressure { .. }) => Ok(Status::NeedOutput),
            Err(Error::Cancelled) => Err(Error::Cancelled),
            Err(error) => {
                // Prefix outputs retain their owners. A fatal operation error
                // frees private construction and remains explicit until reset.
                self.session = None;
                self.failed = Some(error);
                Err(error)
            }
        }
    }
}
