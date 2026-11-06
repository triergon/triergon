//! Source-owned causal diagnostics for the selected pinned receiver. Process
//! addresses are never invented. Reference read-demand spans borrow the REAL
//! enclosing source table; short children never become validated codec boxes.
use crate::{BoxChildren, BoxStep, BoxView, BuildStatus, EntryLayout, Limits, SampleTable};
use std::{
    fmt,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mov9DiagnosticKindV7 {
    IgnoredAdditionalConfiguration,
    UnsupportedVpVersion(u8),
    EmptyVpConfiguration,
    UnsupportedDtsLayout(u16),
    InvalidDtsRate(i32),
    AttemptedChildOverread { atom: [u8; 4], bytes: u64 },
}
impl fmt::Display for Mov9DiagnosticKindV7 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::IgnoredAdditionalConfiguration => write!(f, "ignoring multiple glbl"),
            Self::UnsupportedVpVersion(v) => {
                write!(f, "Unsupported VP Codec Configuration box version {v}")
            }
            Self::EmptyVpConfiguration => write!(f, "Empty VP Codec Configuration box"),
            Self::UnsupportedDtsLayout(_) => write!(f, "Unsupported DTS audio channel layout"),
            Self::InvalidDtsRate(v) => write!(f, "Invalid sample rate {v}"),
            Self::AttemptedChildOverread { atom, bytes } => {
                let name = if atom == *b"ddts" { "ddts" } else { "esds" };
                write!(f, "overread end of atom '{name}' by {bytes} bytes")
            }
        }
    }
}
#[derive(Clone, Debug)]
pub struct Mov9NativeDiagnosticV7 {
    pub kind: Mov9DiagnosticKindV7,
    pub raw: BoxView,
    pub child_ordinal: u32,
    pub inspected_parent_span: Option<Bytes>,
    pub source_child_truncated: bool,
}
#[derive(Debug)]
pub struct Mov9PrivateDiagnosticsV7 {
    pub source_table: BoxView,
    pub events: BudgetVec<Mov9NativeDiagnosticV7>,
    pub native_open_result: Option<i32>,
}
#[derive(Debug, Clone, Copy)]
enum EsdsPhase {
    Full(u8),
    Tag(bool),
    Length { first: bool, tag: u8, count: u8 },
    Id { left: u8, es: bool },
    Flags,
    Depends(u8),
    UrlLength,
    Url(u8),
    Ocr(u8),
    Done,
}
#[derive(Debug)]
struct EsdsDemand {
    span: Bytes,
    at: usize,
    start: usize,
    phase: EsdsPhase,
    flags: u8,
}
impl EsdsDemand {
    fn new(parent: &BoxView, child: &BoxView) -> Result<Self> {
        let start = usize::try_from(
            child
                .header()
                .payload_range()?
                .start
                .checked_sub(parent.header().start)
                .ok_or(Error::Overflow)?,
        )
        .map_err(|_| Error::Overflow)?;
        // A real parent view bounds every access, including attempted cross-child
        // reads. No EOF zero fill or read into an unowned neighbour is possible.
        Ok(Self {
            span: parent.bytes().clone(),
            at: start,
            start,
            phase: EsdsPhase::Full(4),
            flags: 0,
        })
    }
    fn following_optional(&self) -> EsdsPhase {
        if self.flags & 64 != 0 {
            EsdsPhase::UrlLength
        } else if self.flags & 32 != 0 {
            EsdsPhase::Ocr(2)
        } else {
            EsdsPhase::Tag(false)
        }
    }
    fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if matches!(self.phase, EsdsPhase::Done) {
                return Ok(true);
            }
            if !work.try_charge(1) {
                return Ok(false);
            }
            let byte = *self.span.as_ref().get(self.at).ok_or(Error::Truncated)?;
            self.at = self.at.checked_add(1).ok_or(Error::Overflow)?;
            self.phase = match self.phase {
                EsdsPhase::Full(n) => {
                    if n == 1 {
                        EsdsPhase::Tag(true)
                    } else {
                        EsdsPhase::Full(n - 1)
                    }
                }
                EsdsPhase::Tag(first) => EsdsPhase::Length {
                    first,
                    tag: byte,
                    count: 0,
                },
                EsdsPhase::Length { first, tag, count } => {
                    let n = count + 1;
                    if byte & 128 != 0 && n < 4 {
                        EsdsPhase::Length {
                            first,
                            tag,
                            count: n,
                        }
                    } else if first {
                        EsdsPhase::Id {
                            left: 2,
                            es: tag == 3,
                        }
                    } else if tag == 4 {
                        return Err(Error::Unsupported("V7 raw ESDS codec effect not admitted"));
                    } else {
                        EsdsPhase::Done
                    }
                }
                EsdsPhase::Id { left, es } => {
                    if left > 1 {
                        EsdsPhase::Id { left: left - 1, es }
                    } else if es {
                        EsdsPhase::Flags
                    } else {
                        EsdsPhase::Tag(false)
                    }
                }
                EsdsPhase::Flags => {
                    self.flags = byte;
                    if byte & 128 != 0 {
                        EsdsPhase::Depends(2)
                    } else {
                        self.following_optional()
                    }
                }
                EsdsPhase::Depends(n) => {
                    if n > 1 {
                        EsdsPhase::Depends(n - 1)
                    } else {
                        self.following_optional()
                    }
                }
                EsdsPhase::UrlLength => {
                    self.flags &= !64;
                    if byte == 0 {
                        self.following_optional()
                    } else {
                        EsdsPhase::Url(byte)
                    }
                }
                EsdsPhase::Url(n) => {
                    if n > 1 {
                        EsdsPhase::Url(n - 1)
                    } else {
                        self.following_optional()
                    }
                }
                EsdsPhase::Ocr(n) => {
                    if n > 1 {
                        EsdsPhase::Ocr(n - 1)
                    } else {
                        EsdsPhase::Tag(false)
                    }
                }
                EsdsPhase::Done => return Err(Error::Invalid("ESDS diagnostic state invariant")),
            };
        }
    }
}
#[derive(Debug)]
pub struct Mov9PrivateDiagnosticJobV7 {
    pool: Budget,
    parent: BoxView,
    children: Option<BoxChildren>,
    pending: Option<BoxView>,
    esds: Option<EsdsDemand>,
    output: BudgetVec<Mov9NativeDiagnosticV7>,
    emitting: [Option<Mov9NativeDiagnosticV7>; 2],
    native_size: usize,
    native_failure: Option<i32>,
    ordinal: u32,
    done: bool,
    finished: bool,
    failed: Option<Error>,
}
impl Mov9PrivateDiagnosticJobV7 {
    pub fn new(table: &SampleTable, pool: &Budget, limits: Limits) -> Result<Self> {
        if !table.raw().bytes().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        if table.descriptions().len() != 1 || table.descriptions()[0].format != *b"raw " {
            return Err(Error::Unsupported("V7 diagnostic raw-visual context"));
        }
        Ok(Self {
            pool: pool.clone(),
            parent: table.raw().clone(),
            children: Some(table.descriptions()[0].extensions(EntryLayout::Visual, limits)?),
            pending: None,
            esds: None,
            output: BudgetVec::new(pool),
            emitting: [None, None],
            native_size: 0,
            native_failure: None,
            ordinal: 0,
            done: false,
            finished: false,
            failed: None,
        })
    }
    fn event(
        &self,
        raw: &BoxView,
        kind: Mov9DiagnosticKindV7,
        span: Option<Bytes>,
        truncated: bool,
    ) -> Mov9NativeDiagnosticV7 {
        Mov9NativeDiagnosticV7 {
            kind,
            raw: raw.clone(),
            child_ordinal: self.ordinal,
            inspected_parent_span: span,
            source_child_truncated: truncated,
        }
    }
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        let value = self.inner(work, cancel);
        if let Err(e) = value
            && !matches!(e, Error::Backpressure { .. })
        {
            self.failed = Some(e);
            self.pending = None;
            self.esds = None;
            self.emitting = [None, None];
            self.children = None;
        }
        value
    }
    fn inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.finished {
                return Err(Error::Invalid("V7 diagnostics already taken"));
            }
            if let Some(i) = self.emitting.iter().position(Option::is_some) {
                if !work.try_charge(1) {
                    return Ok(BuildStatus::Yield);
                }
                let event = self.emitting[i]
                    .take()
                    .ok_or(Error::Invalid("diagnostic emission invariant"))?;
                if let Err(error) = self.output.try_push(event) {
                    self.emitting[i] = Some(error.value);
                    return Err(error.error);
                }
                continue;
            }
            if self.done {
                return Ok(BuildStatus::Ready);
            }
            if let Some(reader) = &mut self.esds {
                if !reader.poll(work, cancel)? {
                    return Ok(BuildStatus::Yield);
                }
                let reader = self
                    .esds
                    .take()
                    .ok_or(Error::Invalid("diagnostic reader invariant"))?;
                let raw = self
                    .pending
                    .take()
                    .ok_or(Error::Invalid("diagnostic ESDS source invariant"))?;
                let consumed = reader.at.checked_sub(reader.start).ok_or(Error::Overflow)?;
                if consumed > raw.payload().len() {
                    let span = reader.span.slice(reader.start..reader.at)?;
                    self.emitting[0] = Some(
                        self.event(
                            &raw,
                            Mov9DiagnosticKindV7::AttemptedChildOverread {
                                atom: *b"esds",
                                bytes: u64::try_from(consumed - raw.payload().len())
                                    .map_err(|_| Error::Overflow)?,
                            },
                            Some(span),
                            true,
                        ),
                    );
                }
                continue;
            }
            if self.pending.is_none() {
                let Some(children) = &mut self.children else {
                    return Err(Error::Invalid("diagnostic child cursor invariant"));
                };
                match children.poll(work)? {
                    BoxStep::Yield => return Ok(BuildStatus::Yield),
                    BoxStep::End => {
                        self.children = None;
                        self.done = true;
                        continue;
                    }
                    BoxStep::Box(raw) => {
                        self.ordinal = self.ordinal.checked_add(1).ok_or(Error::Overflow)?;
                        self.pending = Some(raw);
                    }
                }
            }
            if !work.try_charge(1) {
                return Ok(BuildStatus::Yield);
            }
            let raw = self
                .pending
                .take()
                .ok_or(Error::Invalid("diagnostic pending invariant"))?;
            let body = raw.payload();
            if body.is_empty() || self.native_failure.is_some() {
                continue;
            }
            match raw.header().kind {
                k if k == *b"avcC" || k == *b"hvcC" || k == *b"av1C" => {
                    let field_only = body.len() >= 10
                        && &body[4..8] == b"fiel"
                        && usize::try_from(u32::from_be_bytes([
                            body[0], body[1], body[2], body[3],
                        ]))
                        .ok()
                            == Some(body.len());
                    if !field_only {
                        if self.native_size > 1 {
                            self.emitting[0] = Some(self.event(
                                &raw,
                                Mov9DiagnosticKindV7::IgnoredAdditionalConfiguration,
                                None,
                                false,
                            ));
                        } else {
                            self.native_size = body.len();
                        }
                    }
                }
                k if k == *b"dOps" => {
                    if body.len() < 11 || body[0] != 0 || self.native_size != 0 {
                        return Err(Error::Unsupported("V7 diagnostic Opus context"));
                    }
                    self.native_size = 19;
                }
                k if k == *b"vpcC" => {
                    if body.len() < 5 {
                        self.emitting[0] = Some(self.event(
                            &raw,
                            Mov9DiagnosticKindV7::EmptyVpConfiguration,
                            None,
                            true,
                        ));
                        self.native_failure = Some(-1_094_995_529);
                    } else if body[0] != 1 {
                        self.emitting[0] = Some(self.event(
                            &raw,
                            Mov9DiagnosticKindV7::UnsupportedVpVersion(body[0]),
                            None,
                            false,
                        ));
                    } else {
                        return Err(Error::Unsupported(
                            "V7 VP version1 diagnostic/native effects unadmitted",
                        ));
                    }
                }
                k if k == *b"esds" => {
                    self.esds = Some(EsdsDemand::new(&self.parent, &raw)?);
                    self.pending = Some(raw);
                }
                k if k == *b"ddts" => {
                    let at = usize::try_from(
                        raw.header()
                            .payload_range()?
                            .start
                            .checked_sub(self.parent.header().start)
                            .ok_or(Error::Overflow)?,
                    )
                    .map_err(|_| Error::Overflow)?;
                    let end = at.checked_add(20).ok_or(Error::Overflow)?;
                    let span = self.parent.bytes().slice(at..end)?;
                    let b = span.as_ref();
                    let rate = i32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                    if rate <= 0 {
                        self.emitting[0] = Some(self.event(
                            &raw,
                            Mov9DiagnosticKindV7::InvalidDtsRate(rate),
                            Some(span),
                            body.len() < 20,
                        ));
                        self.native_failure = Some(-1_094_995_529);
                        continue;
                    }
                    let layout = u16::from_be_bytes([b[17], b[18]]);
                    let mut slot = 0;
                    if layout > 255 {
                        self.emitting[slot] = Some(self.event(
                            &raw,
                            Mov9DiagnosticKindV7::UnsupportedDtsLayout(layout),
                            Some(span.clone()),
                            body.len() < 20,
                        ));
                        slot += 1;
                    }
                    if body.len() < 20 {
                        self.emitting[slot] = Some(self.event(
                            &raw,
                            Mov9DiagnosticKindV7::AttemptedChildOverread {
                                atom: *b"ddts",
                                bytes: (20 - body.len()) as u64,
                            },
                            Some(span),
                            true,
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<Mov9PrivateDiagnosticsV7> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        if cancel.load(Ordering::Relaxed) {
            self.failed = Some(Error::Cancelled);
            self.output.clear();
            return Err(Error::Cancelled);
        }
        if !self.done || self.finished || self.emitting.iter().any(Option::is_some) {
            return Err(Error::Invalid("V7 diagnostics not ready"));
        }
        self.finished = true;
        let events = std::mem::replace(&mut self.output, BudgetVec::new(&self.pool));
        Ok(Mov9PrivateDiagnosticsV7 {
            source_table: self.parent.clone(),
            events,
            native_open_result: self.native_failure,
        })
    }
}
