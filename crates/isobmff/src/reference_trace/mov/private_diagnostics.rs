//! FFmpeg's log lines for the child boxes of a raw-video sample entry.
//!
//! [`PrivateDiagnosticJob`] walks the children of a track's `raw ` entry and reports, in
//! order, the messages FFmpeg logs for them and the open return code when a child fails
//! the open. Where FFmpeg reads past a child's end, the reported bytes are a slice of the
//! enclosing `stbl`; a short child is reported, never taken for a valid configuration box.
use crate::{
    BuildStatus, Limits, Next,
    boxes::{BoxChildren, BoxView},
    movie::EntryLayout,
    table::SampleTable,
};
use std::{
    fmt,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};
/// A message FFmpeg logs for a child box of a raw-video entry; `Display` writes its text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticKind {
    /// An `avcC`, `hvcC` or `av1C` child after configuration bytes longer than one byte
    /// are installed; FFmpeg ignores it.
    IgnoredAdditionalConfiguration,
    /// A `vpcC` child whose version, carried here, is not 1.
    UnsupportedVpVersion(u8),
    /// A `vpcC` child shorter than 5 bytes; it fails the open.
    EmptyVpConfiguration,
    /// A `ddts` child whose `ChannelLayout`, carried here, is above 255.
    UnsupportedDtsLayout(u16),
    /// A `ddts` child whose sampling frequency, read as signed and carried here, is not
    /// positive; it fails the open.
    InvalidDtsRate(i32),
    /// FFmpeg's read of an `esds` or `ddts` child runs past the end of the child.
    AttemptedChildOverread {
        /// Code of the child: `esds` or `ddts`.
        atom: [u8; 4],
        /// Bytes read past the end of the child.
        bytes: u64,
    },
}
impl fmt::Display for DiagnosticKind {
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
/// One message FFmpeg logs while walking a raw-video entry, with the child box it is about.
#[derive(Clone, Debug)]
pub struct ReceiverDiagnostic {
    /// Which message FFmpeg logs, with its values.
    pub kind: DiagnosticKind,
    /// The child box the message is about, as it is in the source.
    pub raw: BoxView,
    /// 1-based position of that child among the entry's child boxes.
    pub child_ordinal: u32,
    /// For an `esds` or `ddts` child, the bytes of the enclosing `stbl` that FFmpeg reads,
    /// from the child's payload start; they can extend past the child. `None` otherwise.
    pub inspected_parent_span: Option<Bytes>,
    /// Whether the child is shorter than the bytes FFmpeg reads from it.
    pub source_child_truncated: bool,
}
/// The messages FFmpeg logs for the child boxes of a raw-video entry, and its open result.
#[derive(Debug)]
pub struct PrivateDiagnostics {
    /// The `stbl` box holding the entry.
    pub source_table: BoxView,
    /// The messages, in the order FFmpeg logs them.
    pub events: BudgetVec<ReceiverDiagnostic>,
    /// FFmpeg's open return code when a child fails the open (`-1094995529`,
    /// `AVERROR_INVALIDDATA`); `None` when none does. Later children are not inspected.
    pub demux_open_result: Option<i32>,
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
                        return Err(Error::Unsupported(
                            "private diagnostic esds DecoderConfigDescriptor not admitted",
                        ));
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
/// Walks the child boxes of a track's single raw-video entry and collects the messages
/// FFmpeg logs for them.
#[derive(Debug)]
pub struct PrivateDiagnosticJob {
    pool: Budget,
    parent: BoxView,
    children: Option<BoxChildren>,
    pending: Option<BoxView>,
    esds: Option<EsdsDemand>,
    output: BudgetVec<ReceiverDiagnostic>,
    emitting: [Option<ReceiverDiagnostic>; 2],
    demux_size: usize,
    demux_failure: Option<i32>,
    ordinal: u32,
    done: bool,
    finished: bool,
    failed: Option<Error>,
}
impl PrivateDiagnosticJob {
    /// Starts a walk over the children of `table`'s one entry. Fails with
    /// `Error::ForeignPool` when the table's bytes do not belong to `pool`, and with
    /// `Error::Unsupported` unless the table has exactly one description, of format `raw `.
    pub fn new(table: &SampleTable, pool: &Budget, limits: Limits) -> Result<Self> {
        if !table.raw().bytes().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        if table.descriptions().len() != 1 || table.descriptions()[0].format != *b"raw " {
            return Err(Error::Unsupported("private diagnostic raw-visual context"));
        }
        Ok(Self {
            pool: pool.clone(),
            parent: table.raw().clone(),
            children: Some(table.descriptions()[0].extensions(EntryLayout::Visual, limits)?),
            pending: None,
            esds: None,
            output: BudgetVec::new(pool),
            emitting: [None, None],
            demux_size: 0,
            demux_failure: None,
            ordinal: 0,
            done: false,
            finished: false,
            failed: None,
        })
    }
    fn event(
        &self,
        raw: &BoxView,
        kind: DiagnosticKind,
        span: Option<Bytes>,
        truncated: bool,
    ) -> ReceiverDiagnostic {
        ReceiverDiagnostic {
            kind,
            raw: raw.clone(),
            child_ordinal: self.ordinal,
            inspected_parent_span: span,
            source_child_truncated: truncated,
        }
    }
    /// Walks further children. Fails with `Error::Unsupported` for a child whose effect is
    /// not modelled: a `dOps` shorter than 11 bytes, of nonzero version or after other
    /// configuration, a `vpcC` of version 1, or an `esds` whose ES descriptor is followed
    /// by a decoder configuration descriptor.
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
                return Err(Error::Invalid("private diagnostics already taken"));
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
                            DiagnosticKind::AttemptedChildOverread {
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
                    Next::Yield => return Ok(BuildStatus::Yield),
                    Next::End => {
                        self.children = None;
                        self.done = true;
                        continue;
                    }
                    Next::Item(raw) => {
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
            if body.is_empty() || self.demux_failure.is_some() {
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
                        if self.demux_size > 1 {
                            self.emitting[0] = Some(self.event(
                                &raw,
                                DiagnosticKind::IgnoredAdditionalConfiguration,
                                None,
                                false,
                            ));
                        } else {
                            self.demux_size = body.len();
                        }
                    }
                }
                k if k == *b"dOps" => {
                    if body.len() < 11 || body[0] != 0 || self.demux_size != 0 {
                        return Err(Error::Unsupported("private diagnostic Opus context"));
                    }
                    self.demux_size = 19;
                }
                k if k == *b"vpcC" => {
                    if body.len() < 5 {
                        self.emitting[0] = Some(self.event(
                            &raw,
                            DiagnosticKind::EmptyVpConfiguration,
                            None,
                            true,
                        ));
                        self.demux_failure = Some(-1_094_995_529);
                    } else if body[0] != 1 {
                        self.emitting[0] = Some(self.event(
                            &raw,
                            DiagnosticKind::UnsupportedVpVersion(body[0]),
                            None,
                            false,
                        ));
                    } else {
                        return Err(Error::Unsupported(
                            "private diagnostic vpcC version 1 effects not admitted",
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
                            DiagnosticKind::InvalidDtsRate(rate),
                            Some(span),
                            body.len() < 20,
                        ));
                        self.demux_failure = Some(-1_094_995_529);
                        continue;
                    }
                    let layout = u16::from_be_bytes([b[17], b[18]]);
                    let mut slot = 0;
                    if layout > 255 {
                        self.emitting[slot] = Some(self.event(
                            &raw,
                            DiagnosticKind::UnsupportedDtsLayout(layout),
                            Some(span.clone()),
                            body.len() < 20,
                        ));
                        slot += 1;
                    }
                    if body.len() < 20 {
                        self.emitting[slot] = Some(self.event(
                            &raw,
                            DiagnosticKind::AttemptedChildOverread {
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
    /// Takes the result after [`poll`](Self::poll) returned `Ready`. Fails with
    /// `Error::Invalid` before then or when called again, and with `Error::Cancelled` when
    /// `cancel` is set.
    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<PrivateDiagnostics> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        if cancel.load(Ordering::Relaxed) {
            self.failed = Some(Error::Cancelled);
            self.output.clear();
            return Err(Error::Cancelled);
        }
        if !self.done || self.finished || self.emitting.iter().any(Option::is_some) {
            return Err(Error::Invalid("private diagnostics not ready"));
        }
        self.finished = true;
        let events = std::mem::replace(&mut self.output, BudgetVec::new(&self.pool));
        Ok(PrivateDiagnostics {
            source_table: self.parent.clone(),
            events,
            demux_open_result: self.demux_failure,
        })
    }
}
