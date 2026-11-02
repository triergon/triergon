//! VP9 VPCodecConfigurationRecord facts from WebM's authoritative v1.0 binding.
//! The FullBox envelope is supplied explicitly by the container owner; this
//! module does not guess whether record bytes include box headers or v0 syntax.
use crate::{BuildStep, record_input};
use safe_media_core::{Budget, Bytes, Error, InputCursor, Result, SourceId, Step, WorkBudget};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vp9ConfigContext {
    pub version: u8,
    pub flags: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vp9ConfigLimits {
    pub maximum_record_bytes: usize,
    pub maximum_input_window: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vp9ConfigFields {
    raw: [u8; 8],
}

impl Vp9ConfigFields {
    pub const fn raw_bytes(self) -> [u8; 8] {
        self.raw
    }
    pub const fn profile(self) -> u8 {
        self.raw[0]
    }
    pub const fn level(self) -> u8 {
        self.raw[1]
    }
    pub const fn bit_depth(self) -> u8 {
        self.raw[2] >> 4
    }
    pub const fn chroma_subsampling(self) -> u8 {
        (self.raw[2] >> 1) & 7
    }
    pub const fn video_full_range(self) -> bool {
        self.raw[2] & 1 != 0
    }
    pub const fn colour_primaries(self) -> u8 {
        self.raw[3]
    }
    pub const fn transfer_characteristics(self) -> u8 {
        self.raw[4]
    }
    pub const fn matrix_coefficients(self) -> u8 {
        self.raw[5]
    }
    pub const fn initialization_size(self) -> u16 {
        u16::from_be_bytes([self.raw[6], self.raw[7]])
    }
}

/// Bits: 1 nonzero FullBox flags; 2 nonempty VP9 initialization; 4 profile
/// outside 0..3; 8 bit depth outside 8/10/12; 16 chroma outside 0..3;
/// 32 RGB matrix without 4:4:4. No full profile/level/CICP/sample validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vp9ConfigSyntaxIssues(u8);

impl Vp9ConfigSyntaxIssues {
    pub const fn bits(self) -> u8 {
        self.0
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

#[derive(Clone, Debug)]
pub struct Vp9Configuration {
    raw: Bytes,
    context: Vp9ConfigContext,
    fields: Vp9ConfigFields,
    source: SourceId,
    generation: u64,
}

impl Vp9Configuration {
    pub fn raw_record(&self) -> &Bytes {
        &self.raw
    }
    pub const fn context(&self) -> Vp9ConfigContext {
        self.context
    }
    pub const fn fields(&self) -> Vp9ConfigFields {
        self.fields
    }
    pub const fn source(&self) -> SourceId {
        self.source
    }
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    pub fn initialization_bytes(&self) -> Result<Bytes> {
        self.raw.slice(8..self.raw.len())
    }
    pub fn syntax_issues(&self) -> Vp9ConfigSyntaxIssues {
        let fields = self.fields;
        Vp9ConfigSyntaxIssues(
            u8::from(self.context.flags != 0)
                | (u8::from(fields.initialization_size() != 0) << 1)
                | (u8::from(fields.profile() > 3) << 2)
                | (u8::from(!matches!(fields.bit_depth(), 8 | 10 | 12)) << 3)
                | (u8::from(fields.chroma_subsampling() > 3) << 4)
                | (u8::from(fields.matrix_coefficients() == 0 && fields.chroma_subsampling() != 3)
                    << 5),
        )
    }
}

/// Reads exactly the eight-byte fixed record prefix with bounded work, checks
/// its declared initialization length, and retains the opaque initialization.
/// Nonempty VP9 initialization violates the binding; it is reported as a source
/// issue rather than silently removed. No source-sized copy/allocation occurs.
pub struct Vp9ConfigParser {
    source: Option<Bytes>,
    cursor: Option<InputCursor>,
    context: Vp9ConfigContext,
    identity: SourceId,
    generation: u64,
    header: [u8; 8],
    used: usize,
    complete: bool,
    failure: Option<Error>,
}

impl Vp9ConfigParser {
    pub fn new(
        pool: &Budget,
        source: SourceId,
        record: Bytes,
        context: Vp9ConfigContext,
        limits: Vp9ConfigLimits,
    ) -> Result<Self> {
        record_input::check_record(pool, &record, 8, limits.maximum_record_bytes)?;
        if context.version != 1 {
            return Err(Error::Unsupported("VP9 configuration FullBox version"));
        }
        if context.flags > 0x00ff_ffff {
            return Err(Error::Invalid("VP9 FullBox flags exceed 24 bits"));
        }
        let cursor = record_input::cursor(pool, source, &record, limits.maximum_input_window)?;
        let generation = cursor.generation();
        Ok(Self {
            source: Some(record),
            cursor: Some(cursor),
            context,
            identity: source,
            generation,
            header: [0; 8],
            used: 0,
            complete: false,
            failure: None,
        })
    }

    fn stop(&mut self, error: Error) {
        self.failure = Some(error);
        self.source = None;
        self.cursor = None;
    }

    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<Vp9Configuration>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            self.stop(Error::Cancelled);
            return Err(Error::Cancelled);
        }
        let result = self.step_inner(work, cancel);
        if let Err(error) = result {
            self.stop(error);
        }
        result
    }

    fn step_inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<Vp9Configuration>> {
        if self.complete {
            return Ok(BuildStep::Complete);
        }
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.used == self.header.len() {
                if !work.try_charge(1) {
                    return Ok(BuildStep::Yield);
                }
                let fields = Vp9ConfigFields { raw: self.header };
                let expected = 8_usize
                    .checked_add(usize::from(fields.initialization_size()))
                    .ok_or(Error::Overflow)?;
                let source = self
                    .source
                    .as_ref()
                    .ok_or(Error::Invalid("missing VP9 record source"))?;
                if source.len() < expected {
                    return Err(Error::Truncated);
                }
                if source.len() > expected {
                    return Err(Error::Invalid(
                        "trailing bytes after VP9 configuration record",
                    ));
                }
                let raw = self
                    .source
                    .take()
                    .ok_or(Error::Invalid("missing VP9 record source"))?;
                self.cursor = None;
                self.complete = true;
                return Ok(BuildStep::Ready(Vp9Configuration {
                    raw,
                    context: self.context,
                    fields,
                    source: self.identity,
                    generation: self.generation,
                }));
            }
            let cursor = self
                .cursor
                .as_mut()
                .ok_or(Error::Invalid("missing VP9 record cursor"))?;
            match cursor.read_into(&mut self.header[self.used..], work, cancel)? {
                Step::Ready(count) => {
                    self.used = self.used.checked_add(count).ok_or(Error::Overflow)?
                }
                Step::Yield => return Ok(BuildStep::Yield),
                Step::End(_) => return Err(Error::Truncated),
                Step::NeedInput(request) => {
                    let source = self
                        .source
                        .as_ref()
                        .ok_or(Error::Invalid("missing VP9 record source"))?;
                    match record_input::window(source, &request)? {
                        Some(window) => cursor.provide(&window)?,
                        None => cursor.finish(request.source, request.token)?,
                    }
                }
            }
        }
    }
}
