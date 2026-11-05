//! Bounded text source ingestion with original bytes and exact cue times.
//!
//! Named profiles: SubRipStrict1, WebVttSourceStrict1, Tx3gCarriage1,
//! Utf8Packet1, MatroskaWebVttCarriage1 and OpaqueAss1. Their limits and
//! source/semantic boundaries are documented in this module's README.md.
//! These profiles do not establish full native compatibility or M05 acceptance.
#![forbid(unsafe_code)]

pub mod carriage;
pub mod charset;
pub mod native_charset;
pub mod native_mov_text;
pub mod tx3g_config;
pub mod tx3g_semantics;

mod clock;
mod document;
mod emit;
mod line;
mod model;
mod sample;
mod session;
mod unicode;
pub use self::model::*;

use self::session::{Action, Session};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error, Result, WorkBudget};

#[derive(Debug)]
pub struct ProvideError {
    pub error: Error,
    pub input: Input,
}

// Failure must return the exact input even when no output budget is available.
// The Result envelope permits one extra 16-byte alignment unit for its tag.
const _: () = assert!(std::mem::size_of::<ProvideError>() <= 432);
const _: () = assert!(std::mem::size_of::<std::result::Result<(), ProvideError>>() <= 448);

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "Exclusive delivery states move charged output inline, preserving allocation-free UTF-8 publication; guarded at 672 bytes."
)]
pub enum Status {
    NeedInput,
    Yield,
    Event(Event),
    NeedOutput,
    Finished,
}

const _: () = assert!(std::mem::size_of::<Status>() <= 672);
const _: () = assert!(std::mem::size_of::<Result<Status>>() <= 688);

/// One complete immutable document or demuxed sample per provide operation.
/// Transport assembly, charset selection and container discovery stay upstream.
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
    /// Additive ISO-box tx3g profile; original constructor stays unchanged.
    pub fn tx3g_iso_boxes(pool: &Budget, limits: Limits, generation: u64) -> Self {
        let mut result = Self::new(pool, limits, generation);
        result.iso_tx3g = true;
        result
    }

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

    #[expect(
        clippy::result_large_err,
        reason = "Rejection returns the exact source owners inline without allocating on failure; ProvideError is guarded at 432 bytes."
    )]
    pub fn provide(&mut self, input: Input) -> std::result::Result<(), ProvideError> {
        match self.check_input(&input) {
            Ok(()) => {
                self.session = Some(Session::new(&input, &self.pool, self.iso_tx3g));
                self.pending = Some(input);
                Ok(())
            }
            Err(error) => Err(ProvideError { error, input }),
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

    pub fn finish_input(&mut self) {
        self.finished = true;
    }

    pub fn reset(&mut self, generation: u64) {
        self.session = None;
        self.pending = None;
        self.failed = None;
        self.generation = generation;
        self.finished = false;
    }

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
            Ok(Action::Event { event, last }) => {
                if last {
                    self.session = None;
                    self.pending = None;
                }
                Ok(Status::Event(event))
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
