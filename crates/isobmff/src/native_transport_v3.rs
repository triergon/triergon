//! Separately selected, source-derived model of the pinned D32 transport slice.
//! This is an empirical native view, never a source EOF/clock/range repair.
//! Own model from passive public callback controls; no native bodies consulted.
use crate::{
    BoxChildren, BoxHeader, BoxStep, BoxView, BuildStatus, Limits, Mov9RecoveryV3Context,
    Mov9RunRecoveryV3Table, fields,
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, Error, Result, WorkBudget};

const CAPACITY: u64 = 32768; // Literal original wrapper allocation.
const DISCARD_BOUND: u64 = 32768; // Separately observed pinned forward protocol bound.
const CHILD_LIMIT: u32 = 16;

#[derive(Clone, Copy, Debug)]
pub struct PinnedMovD32SmallRawVideoTransportV3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mov9V3EntryRevisit {
    Buffered,
    DirectCallback,
    OneWindowCallbackSet,
}

/// Modeled state, not a claim that the source operation observed native I/O.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mov9V3ModeledOpenState {
    pub source_position_min: u64,
    pub source_position_max: u64,
    pub logical_position: u64,
    pub possible_windows: u32,
    pub entry_revisit: Mov9V3EntryRevisit,
}

/// A symbolic callback operation plan, not an observed receipt. Positive reads
/// discard source bytes without emitting a packet; the final positive-size
/// source callback at its known end returns EOF under the original adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mov9V3ModeledInitialRead {
    pub possible_windows: u32,
    pub positive_callback_calls_min: u64,
    pub positive_callback_calls_max: u64,
    pub terminal_callback_position: u64,
    pub terminal_callback_requested: u32,
}

#[derive(Debug)]
pub struct Mov9V3TransportModel {
    ftyp: BoxView,
    movie: BoxView,
    table: BoxView,
    physical_length: u64,
    custom_read_maximum: usize,
    media: Range<u64>,
    open: Mov9V3ModeledOpenState,
    revisit_seed: Window,
    first_anchor: u64,
    last_anchor: u64,
    buffered_revisit: bool,
}
impl Mov9V3TransportModel {
    pub(crate) fn context(&self) -> Mov9RecoveryV3Context {
        Mov9RecoveryV3Context {
            physical_length: self.physical_length,
            custom_read_maximum: self.custom_read_maximum,
        }
    }
    pub fn modeled_open_state(&self) -> Mov9V3ModeledOpenState {
        self.open
    }
    pub fn source_movie(&self) -> &BoxView {
        &self.movie
    }
    pub fn source_file_type(&self) -> &BoxView {
        &self.ftyp
    }
    /// Data/functional guard: the observed public window must be an exact
    /// member of the conservative set, never a guessed selected alignment.
    pub fn includes_open_window(
        &self,
        source_position: u64,
        ptr_offset: u64,
        end_offset: u64,
    ) -> bool {
        for anchor in self.first_anchor..=self.last_anchor {
            let mut window = if self.buffered_revisit {
                self.revisit_seed
            } else {
                Window {
                    cursor: anchor,
                    begin: anchor,
                    end: anchor,
                }
            };
            if window
                .consume(
                    self.media.start,
                    self.physical_length,
                    self.custom_read_maximum,
                )
                .is_err()
            {
                return false;
            }
            if window.end == source_position
                && window.cursor.checked_sub(window.begin) == Some(ptr_offset)
                && window.end.checked_sub(window.begin) == Some(end_offset)
            {
                return true;
            }
        }
        false
    }
    /// A positive first packet read beyond physical source can follow the
    /// independently guarded forward-discard protocol. Source range invalidity
    /// remains invalid; an unreviewed seek path is never converted into EOF.
    pub fn models_initial_native_eof(&self, range: Range<u64>) -> bool {
        self.initial_native_read_plan(range).is_some()
    }
    pub fn initial_native_read_plan(&self, range: Range<u64>) -> Option<Mov9V3ModeledInitialRead> {
        let size = range.end.checked_sub(range.start)?;
        if size == 0 || size > 16 * 1024 * 1024 || range.start < self.physical_length {
            return None;
        }
        let quantum = u64::try_from(self.custom_read_maximum).ok()?.min(CAPACITY);
        let mut least = u64::MAX;
        let mut most = 0;
        for anchor in self.first_anchor..=self.last_anchor {
            let mut window = if self.buffered_revisit {
                self.revisit_seed
            } else {
                Window {
                    cursor: anchor,
                    begin: anchor,
                    end: anchor,
                }
            };
            window
                .consume(
                    self.media.start,
                    self.physical_length,
                    self.custom_read_maximum,
                )
                .ok()?;
            let buffered_target = (window.begin..=window.end).contains(&range.start);
            // This bound selects only the guarded read/discard operation path.
            // Crossing it stays an unreviewed seek, never a modeled EOF/error.
            if !buffered_target
                && (range.start < window.end
                    || range.start.checked_sub(window.end)? > DISCARD_BOUND)
            {
                return None;
            }
            // Symbolically execute the immutable callback's positive read run.
            // EOF is produced by the NEXT positive-size callback at source end,
            // not by a quantum/offset test. No packet bytes are emitted here.
            let remaining = self.physical_length.checked_sub(window.end)?;
            let positive = if remaining == 0 {
                0
            } else {
                remaining.checked_add(quantum - 1)? / quantum
            };
            let at_terminal = window.end.checked_add(remaining)?;
            if at_terminal != self.physical_length {
                return None;
            }
            least = least.min(positive);
            most = most.max(positive);
        }
        Some(Mov9V3ModeledInitialRead {
            possible_windows: self.open.possible_windows,
            positive_callback_calls_min: least,
            positive_callback_calls_max: most,
            terminal_callback_position: self.physical_length,
            terminal_callback_requested: CAPACITY as u32,
        })
    }
    pub(crate) fn require_binding(
        &self,
        raw: &BoxView,
        pool: &Budget,
        context: Mov9RecoveryV3Context,
        media: &[Range<u64>],
    ) -> Result<()> {
        if !self.table.bytes().belongs_to(pool) || !raw.bytes().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        if self.table.header() != raw.header()
            || self.table.bytes().as_ref() != raw.bytes().as_ref()
            || context.physical_length != self.physical_length
            || context.custom_read_maximum != self.custom_read_maximum
            || media != [self.media.clone()]
        {
            return Err(Error::Unsupported(
                "v3 modeled transport source/configuration binding",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Default, Debug)]
struct Window {
    cursor: u64,
    begin: u64,
    end: u64,
}
impl Window {
    fn consume(&mut self, target: u64, length: u64, maximum: usize) -> Result<()> {
        if target < self.cursor || target > length {
            return Err(Error::Unsupported("v3 modeled metadata consumption bounds"));
        }
        if target > self.end {
            let quantum = u64::try_from(maximum)
                .map_err(|_| Error::Overflow)?
                .min(CAPACITY);
            let needed = target.checked_sub(self.end).ok_or(Error::Overflow)?;
            let fills = needed.checked_add(quantum - 1).ok_or(Error::Overflow)? / quantum;
            self.begin = self
                .end
                .checked_add((fills - 1).checked_mul(quantum).ok_or(Error::Overflow)?)
                .ok_or(Error::Overflow)?;
            self.end = self
                .end
                .checked_add(fills.checked_mul(quantum).ok_or(Error::Overflow)?)
                .ok_or(Error::Overflow)?
                .min(length);
        }
        self.cursor = target;
        Ok(())
    }
    fn revisit_set(&mut self, target: u64) -> Result<(Mov9V3EntryRevisit, u64, u64, bool)> {
        if (self.begin..=self.end).contains(&target) {
            self.cursor = target;
            return Ok((Mov9V3EntryRevisit::Buffered, target, target, true));
        }
        let filled = self.end.checked_sub(self.begin).ok_or(Error::Overflow)?;
        let gap = self
            .begin
            .checked_sub(target)
            .ok_or(Error::Unsupported("v3 unreviewed forward entry revisit"))?;
        // The captured entry-revisit protocol uses at most one preceding
        // filled window. Retain EVERY source-consistent origin in that window;
        // do not select a half-window threshold or one observed alignment.
        if gap < filled {
            Ok((
                Mov9V3EntryRevisit::OneWindowCallbackSet,
                self.begin.saturating_sub(filled),
                target,
                false,
            ))
        } else {
            Ok((Mov9V3EntryRevisit::DirectCallback, target, target, false))
        }
    }
}

const LEVEL_0: [[u8; 4]; 2] = [*b"mvhd", *b"trak"];
const LEVEL_1: [[u8; 4]; 2] = [*b"tkhd", *b"mdia"];
const LEVEL_2: [[u8; 4]; 3] = [*b"mdhd", *b"hdlr", *b"minf"];
const LEVEL_3: [[u8; 4]; 3] = [*b"vmhd", *b"dinf", *b"stbl"];
const LEVEL_4: [[u8; 4]; 6] = [*b"stsd", *b"stts", *b"stsc", *b"stsz", *b"stco", *b"stss"];
const LEVEL_5: [[u8; 4]; 1] = [*b"raw "];

fn expected(level: u8) -> &'static [[u8; 4]] {
    match level {
        0 => &LEVEL_0,
        1 => &LEVEL_1,
        2 => &LEVEL_2,
        3 => &LEVEL_3,
        4 => &LEVEL_4,
        5 => &LEVEL_5,
        _ => &[],
    }
}
fn next_kind(level: u8) -> [u8; 4] {
    match level {
        0 => *b"trak",
        1 => *b"mdia",
        2 => *b"minf",
        3 => *b"stbl",
        4 => *b"stsd",
        _ => *b"raw ",
    }
}
fn small_plain(header: &BoxHeader, length: u64) -> Result<()> {
    if header.header_size != 8
        || header.size.is_none()
        || header.end()?.is_none_or(|end| end > length)
    {
        return Err(Error::Unsupported(
            "v3 modeled transport requires bounded plain source atoms",
        ));
    }
    Ok(())
}
fn visual_entry(raw: &BoxView) -> Result<()> {
    let b = raw.payload();
    if raw.header().kind != *b"raw "
        || b.len() != 78
        || b[..6] != [0; 6]
        || fields::u16_at(b, 6)? != 1
        || b[8..24] != [0; 16]
        || fields::u16_at(b, 24)? != 2
        || fields::u16_at(b, 26)? != 2
        || fields::u32_at(b, 28)? != 0x0048_0000
        || fields::u32_at(b, 32)? != 0x0048_0000
        || fields::u32_at(b, 36)? != 0
        || fields::u16_at(b, 40)? != 1
        || b[42..74] != [0; 32]
        || fields::u16_at(b, 74)? != 24
        || fields::u16_at(b, 76)? != u16::MAX
    {
        return Err(Error::Unsupported("v3 unreviewed RGB24 entry fields"));
    }
    Ok(())
}

/// Constant-depth, bounded-header validation and at most64 charged window states.
/// Every source byte stays in its original immutable, pool-owned metadata view.
#[derive(Debug)]
pub struct Mov9V3TransportBuilder<'a> {
    ftyp: BoxView,
    movie: BoxView,
    table: BoxView,
    entry: BoxView,
    roots: &'a [BoxHeader],
    context: Mov9RecoveryV3Context,
    media: Range<u64>,
    root_at: usize,
    root_end: u64,
    root_kind: u8,
    level: u8,
    matched: usize,
    children: Option<BoxChildren>,
    next: Option<BoxView>,
    root_ready: bool,
    source_ready: bool,
    action: u8,
    window: Window,
    revisit: Mov9V3EntryRevisit,
    ready: bool,
    failed: Option<Error>,
    first_anchor: u64,
    next_anchor: u64,
    last_anchor: u64,
    buffered_revisit: bool,
    source_min: Option<u64>,
    source_max: u64,
    windows: u32,
}
impl<'a> Mov9V3TransportBuilder<'a> {
    pub fn new(
        ftyp: BoxView,
        movie: BoxView,
        roots: &'a [BoxHeader],
        table: &Mov9RunRecoveryV3Table,
        pool: &Budget,
        context: Mov9RecoveryV3Context,
        _profile: PinnedMovD32SmallRawVideoTransportV3,
    ) -> Result<Self> {
        if !ftyp.bytes().belongs_to(pool)
            || !movie.bytes().belongs_to(pool)
            || !table.raw().bytes().belongs_to(pool)
        {
            return Err(Error::ForeignPool);
        }
        let media = roots
            .last()
            .filter(|header| header.kind == *b"mdat")
            .ok_or(Error::Unsupported("v3 media is not terminal"))?
            .payload_range()?;
        if context.physical_length > CAPACITY
            || context.physical_length == 0
            || !matches!(context.custom_read_maximum, 1 | 2 | 3 | 4 | 7 | 64 | 65536)
            || media.start > media.end
            || media.end != context.physical_length
            || ftyp.header().kind != *b"ftyp"
            || ftyp.payload() != b"qt  \0\0\0\0qt  "
            || movie.header().kind != *b"moov"
            || roots.len() > CHILD_LIMIT as usize
            || table.descriptions().len() != 1
        {
            return Err(Error::Unsupported("v3 small raw-video transport domain"));
        }
        let entry = table.descriptions()[0].raw().clone();
        visual_entry(&entry)?;
        Ok(Self {
            ftyp,
            movie,
            table: table.raw().clone(),
            entry,
            roots,
            context,
            media,
            root_at: 0,
            root_end: 0,
            root_kind: 0,
            level: 0,
            matched: 0,
            children: None,
            next: None,
            root_ready: false,
            source_ready: false,
            action: 0,
            window: Window::default(),
            revisit: Mov9V3EntryRevisit::Buffered,
            ready: false,
            failed: None,
            first_anchor: 0,
            next_anchor: 0,
            last_anchor: 0,
            buffered_revisit: false,
            source_min: None,
            source_max: 0,
            windows: 0,
        })
    }
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(error) = result
            && !matches!(error, Error::Backpressure { .. })
        {
            self.failed = Some(error);
            self.ready = false;
        }
        result
    }
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.ready {
                return Ok(BuildStatus::Ready);
            }
            if !self.root_ready {
                if self.root_at == self.roots.len() {
                    if self.root_kind != 3 || self.root_end != self.context.physical_length {
                        return Err(Error::Unsupported("v3 incomplete source root carriage"));
                    }
                    self.root_ready = true;
                    self.children = Some(self.movie.children(
                        0,
                        Limits {
                            max_boxes: CHILD_LIMIT,
                            ..Limits::default()
                        },
                    )?);
                    continue;
                }
                if !work.try_charge(1) {
                    return Ok(BuildStatus::Yield);
                }
                let header = &self.roots[self.root_at];
                small_plain(header, self.context.physical_length)?;
                if header.start != self.root_end {
                    return Err(Error::Unsupported("v3 discontinuous source root carriage"));
                }
                self.root_end = header
                    .end()?
                    .ok_or(Error::Unsupported("v3 unknown root end"))?;
                if header.kind != *b"free" {
                    let wanted = [*b"ftyp", *b"moov", *b"mdat"]
                        .get(usize::from(self.root_kind))
                        .copied();
                    if wanted != Some(header.kind) {
                        return Err(Error::Unsupported("v3 unreviewed source root order"));
                    }
                    if (self.root_kind == 0 && header != self.ftyp.header())
                        || (self.root_kind == 1 && header != self.movie.header())
                        || (self.root_kind == 2 && header.payload_range()? != self.media)
                    {
                        return Err(Error::Unsupported("v3 metadata/root range binding"));
                    }
                    self.root_kind += 1;
                } else if self.root_kind == 3 {
                    return Err(Error::Unsupported("v3 media is not terminal"));
                }
                self.root_at += 1;
                continue;
            }
            if !self.source_ready {
                let children = self
                    .children
                    .as_mut()
                    .ok_or(Error::Invalid("v3 missing transport source iterator"))?;
                match children.poll(work)? {
                    BoxStep::Yield => return Ok(BuildStatus::Yield),
                    BoxStep::Box(raw) => {
                        small_plain(raw.header(), self.context.physical_length)?;
                        if raw.header().kind == *b"free" && self.level != 5 {
                            continue;
                        }
                        if expected(self.level).get(self.matched) != Some(&raw.header().kind) {
                            return Err(Error::Unsupported("v3 unreviewed metadata child order"));
                        }
                        if self.level == 2
                            && raw.header().kind == *b"hdlr"
                            && fields::fourcc(raw.payload(), 8)? != *b"vide"
                        {
                            return Err(Error::Unsupported("v3 nonvisual source handler"));
                        }
                        if self.level == 3
                            && raw.header().kind == *b"dinf"
                            && raw.payload()
                                != b"\0\0\0\x1cdref\0\0\0\0\0\0\0\x01\0\0\0\x0curl \0\0\0\x01"
                        {
                            return Err(Error::Unsupported("v3 unreviewed source data reference"));
                        }
                        if self.level == 3
                            && raw.header().kind == *b"stbl"
                            && (raw.header() != self.table.header()
                                || raw.bytes().as_ref() != self.table.bytes().as_ref())
                        {
                            return Err(Error::Unsupported("v3 source table binding"));
                        }
                        if self.level == 4
                            && raw.header().kind == *b"stsd"
                            && (fields::u32_at(raw.payload(), 0)? != 0
                                || fields::u32_at(raw.payload(), 4)? != 1)
                        {
                            return Err(Error::Unsupported("v3 unreviewed source catalogue"));
                        }
                        if self.level == 5
                            && (raw.header() != self.entry.header()
                                || raw.bytes().as_ref() != self.entry.bytes().as_ref())
                        {
                            return Err(Error::Unsupported("v3 source entry binding"));
                        }
                        if raw.header().kind == next_kind(self.level) {
                            self.next = Some(raw);
                        }
                        self.matched += 1;
                    }
                    BoxStep::End => {
                        if self.matched != expected(self.level).len() {
                            return Err(Error::Unsupported("v3 incomplete transport metadata"));
                        }
                        if self.level == 5 {
                            self.source_ready = true;
                            self.children = None;
                            continue;
                        }
                        let next = self
                            .next
                            .take()
                            .ok_or(Error::Unsupported("v3 missing transport metadata child"))?;
                        self.level += 1;
                        self.matched = 0;
                        self.children = Some(next.children(
                            if self.level == 5 { 8 } else { 0 },
                            Limits {
                                max_boxes: CHILD_LIMIT,
                                ..Limits::default()
                            },
                        )?);
                    }
                }
                continue;
            }
            if !work.try_charge(1) {
                return Ok(BuildStatus::Yield);
            }
            match self.action {
                0 => self.window.consume(
                    self.entry
                        .header()
                        .end()?
                        .ok_or(Error::Overflow)?
                        .checked_sub(2)
                        .ok_or(Error::Overflow)?,
                    self.context.physical_length,
                    self.context.custom_read_maximum,
                )?,
                1 => {
                    let (kind, first, last, buffered) =
                        self.window.revisit_set(self.entry.header().start)?;
                    self.revisit = kind;
                    self.first_anchor = first;
                    self.next_anchor = first;
                    self.last_anchor = last;
                    self.buffered_revisit = buffered;
                }
                2 => {
                    let mut candidate = if self.buffered_revisit {
                        self.window
                    } else {
                        Window {
                            cursor: self.next_anchor,
                            begin: self.next_anchor,
                            end: self.next_anchor,
                        }
                    };
                    candidate.consume(
                        self.media.start,
                        self.context.physical_length,
                        self.context.custom_read_maximum,
                    )?;
                    self.source_min = Some(
                        self.source_min
                            .map_or(candidate.end, |v| v.min(candidate.end)),
                    );
                    self.source_max = self.source_max.max(candidate.end);
                    self.windows = self.windows.checked_add(1).ok_or(Error::Overflow)?;
                    if self.next_anchor == self.last_anchor {
                        self.ready = true;
                    } else {
                        self.next_anchor =
                            self.next_anchor.checked_add(1).ok_or(Error::Overflow)?;
                    }
                }
                _ => return Err(Error::Invalid("v3 invalid transport replay action")),
            }
            if self.action < 2 {
                self.action += 1;
            }
        }
    }
    pub fn finish(self) -> Result<Mov9V3TransportModel> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if !self.ready {
            return Err(Error::Invalid("v3 transport builder not ready"));
        }
        Ok(Mov9V3TransportModel {
            ftyp: self.ftyp,
            movie: self.movie,
            table: self.table,
            physical_length: self.context.physical_length,
            custom_read_maximum: self.context.custom_read_maximum,
            media: self.media.clone(),
            open: Mov9V3ModeledOpenState {
                source_position_min: self
                    .source_min
                    .ok_or(Error::Invalid("v3 empty modeled window set"))?,
                source_position_max: self.source_max,
                logical_position: self.media.start,
                possible_windows: self.windows,
                entry_revisit: self.revisit,
            },
            revisit_seed: self.window,
            first_anchor: self.first_anchor,
            last_anchor: self.last_anchor,
            buffered_revisit: self.buffered_revisit,
        })
    }
}
