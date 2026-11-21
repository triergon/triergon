//! The VP9 `VPCodecConfigurationRecord` of WebM's authoritative v1.0 binding
//! (the `vpcC` box). The caller supplies the `FullBox` version and flags
//! explicitly, as [`Vp9ConfigContext`]; this module does not guess whether
//! record bytes include box headers or v0 syntax.
use crate::{BuildStep, record_input};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, InputCursor, Result, SourceId, Step, WorkBudget};

/// The `FullBox` header of the `vpcC` box a record comes from, as the caller
/// read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vp9ConfigContext {
    /// The box `version`; the parser accepts only 1.
    pub version: u8,
    /// The box `flags`, 24 bits. The binding has them zero; another value is
    /// kept and reported as a syntax issue.
    pub flags: u32,
}

/// Bounds on a `vpcC` record. Each must be non-zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vp9ConfigLimits {
    /// The most bytes of the whole record.
    pub maximum_record_bytes: usize,
    /// The most bytes of the record the parser takes at once.
    pub maximum_input_window: usize,
}

/// The eight fixed bytes of a `VPCodecConfigurationRecord`, each field as
/// coded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vp9ConfigFields {
    raw: [u8; 8],
}

impl Vp9ConfigFields {
    /// `profile`; VP9 has profiles 0 to 3.
    pub const fn profile(self) -> u8 {
        self.raw[0]
    }
    /// `level`: ten times the level number, such as 41 for level 4.1.
    pub const fn level(self) -> u8 {
        self.raw[1]
    }
    /// `bitDepth`: bits per sample; VP9 has 8, 10 and 12.
    pub const fn bit_depth(self) -> u8 {
        self.raw[2] >> 4
    }
    /// `chromaSubsampling`, 0 to 7: 0 and 1 are 4:2:0 (vertically sited and
    /// colocated with luma), 2 is 4:2:2, 3 is 4:4:4, the rest are reserved.
    pub const fn chroma_subsampling(self) -> u8 {
        (self.raw[2] >> 1) & 7
    }
    /// `videoFullRangeFlag`: full range rather than legal range.
    pub const fn video_full_range(self) -> bool {
        self.raw[2] & 1 != 0
    }
    /// `colourPrimaries`, a code point of ISO/IEC 23001-8.
    pub const fn colour_primaries(self) -> u8 {
        self.raw[3]
    }
    /// `transferCharacteristics`, a code point of ISO/IEC 23001-8.
    pub const fn transfer_characteristics(self) -> u8 {
        self.raw[4]
    }
    /// `matrixCoefficients`, a code point of ISO/IEC 23001-8; 0 is RGB.
    pub const fn matrix_coefficients(self) -> u8 {
        self.raw[5]
    }
    /// `codecInitializationDataSize`: bytes of initialization data after the
    /// fixed fields. The binding has it zero for VP9.
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
    /// The issue bits listed on the type; 0 when none was found.
    pub const fn bits(self) -> u8 {
        self.0
    }
    /// True when none of the listed issues was found.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// A parsed `vpcC` record with the `FullBox` header it came with, sharing the
/// record's bytes.
#[derive(Clone, Debug)]
pub struct Vp9Configuration {
    raw: Bytes,
    context: Vp9ConfigContext,
    fields: Vp9ConfigFields,
    source: SourceId,
    generation: u64,
}

impl Vp9Configuration {
    /// The whole record as it was given to the parser.
    pub fn raw_record(&self) -> &Bytes {
        &self.raw
    }
    /// The `FullBox` header the parser was given.
    pub const fn context(&self) -> Vp9ConfigContext {
        self.context
    }
    /// The eight fixed bytes before `codecInitializationData`.
    pub const fn fields(&self) -> Vp9ConfigFields {
        self.fields
    }
    /// The source the record was given to the parser as.
    pub const fn source(&self) -> SourceId {
        self.source
    }
    /// The input generation the record was read under.
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    /// `codecInitializationData` as coded: the record after its eight fixed
    /// bytes, sharing its allocation. Empty in a record that follows the
    /// binding.
    ///
    /// # Errors
    ///
    /// None for a record a parser returned, which holds its eight fixed bytes.
    pub fn initialization_bytes(&self) -> Result<Bytes> {
        self.raw.slice(8..self.raw.len())
    }
    /// The departures from the binding found in the `FullBox` flags and the
    /// fixed fields.
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
/// Nonempty VP9 initialization violates the binding; it is reported as a syntax
/// issue rather than silently removed. No source-sized copy/allocation occurs.
#[derive(Debug)]
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
    /// A parser for `record`, the payload of a `vpcC` box after its `FullBox`
    /// header, which `context` carries. `source` names the record in the
    /// configuration.
    ///
    /// # Errors
    ///
    /// When a limit is zero; the record is charged to another pool than
    /// `pool`, is longer than `maximum_record_bytes` or is shorter than its
    /// eight fixed bytes; the box version is not 1; or the flags exceed 24
    /// bits.
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

    /// Reads the fixed fields, one work unit per byte and one for the length
    /// check, and returns the configuration.
    ///
    /// # Errors
    ///
    /// Cancellation, or a record shorter or longer than its eight fixed bytes
    /// and the `codecInitializationDataSize` they declare. Each is final: every
    /// later call returns the same error.
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
                    self.used = self.used.checked_add(count).ok_or(Error::Overflow)?;
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
