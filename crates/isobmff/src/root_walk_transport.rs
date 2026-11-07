//! Explicit source-derived reference root-operation model.
//! Demux observations and reference licensed operational facts support this
//! separately selected empirical view. No demux implementation body is copied,
//! translated or linked. Source geometry/bytes remain independently factual.
use crate::{
    BoxChildren, BoxHeader, BoxStep, BoxView, BuildStatus, DescriptionContext,
    DescriptionRecoveryTable, EntryRevisit, Limits, ModeledInitialRead, ModeledOpenState, fields,
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, Error, Result, WorkBudget};

const CAPACITY: u64 = 32768; // Literal original wrapper allocation.
const DISCARD_BOUND: u64 = 32768; // Separately observed reference forward protocol bound.
const CHILD_LIMIT: u32 = 16;

#[derive(Clone, Copy, Debug)]
pub struct RootWalkTransportProfile;

#[derive(Debug)]
pub struct RootWalkTransportModel {
    ftyp: BoxView,
    movie: BoxView,
    table: BoxView,
    physical_length: u64,
    custom_read_maximum: usize,
    media: Range<u64>,
    open: ModeledOpenState,
    windows: [Window; 64],
    root_headers: [Option<BoxHeader>; CHILD_LIMIT as usize],
    absolute_seek_min: u32,
    absolute_seek_max: u32,
}
impl RootWalkTransportModel {
    pub(crate) fn context(&self) -> DescriptionContext {
        DescriptionContext {
            physical_length: self.physical_length,
            custom_read_maximum: self.custom_read_maximum,
        }
    }
    pub fn modeled_open_state(&self) -> ModeledOpenState {
        self.open
    }
    pub fn declared_media(&self) -> Range<u64> {
        self.media.clone()
    }
    pub fn physical_length(&self) -> u64 {
        self.physical_length
    }
    pub fn source_movie(&self) -> &BoxView {
        &self.movie
    }
    pub fn source_file_type(&self) -> &BoxView {
        &self.ftyp
    }
    pub fn source_root_headers(&self) -> impl Iterator<Item = &BoxHeader> {
        self.root_headers.iter().flatten()
    }
    pub fn modeled_absolute_seek_bounds(&self) -> (u32, u32) {
        (self.absolute_seek_min, self.absolute_seek_max)
    }
    /// Diagnostic containment only; no caller chooses a candidate for output.
    pub fn includes_open_window(
        &self,
        source_position: u64,
        ptr_offset: u64,
        end_offset: u64,
    ) -> bool {
        self.windows
            .iter()
            .take(self.open.possible_windows as usize)
            .any(|window| {
                window.end == source_position
                    && window.cursor.checked_sub(window.begin) == Some(ptr_offset)
                    && window.end.checked_sub(window.begin) == Some(end_offset)
            })
    }
    pub fn models_initial_eof(&self, range: Range<u64>) -> bool {
        self.initial_read_plan(range).is_some()
    }
    /// Every possible open path must admit the guarded forward operation.
    /// Only its next positive callback at known physical end supplies EOF.
    pub fn initial_read_plan(&self, range: Range<u64>) -> Option<ModeledInitialRead> {
        let size = range.end.checked_sub(range.start)?;
        if size == 0 || size > 16 * 1024 * 1024 || range.start < self.physical_length {
            return None;
        }
        let quantum = u64::try_from(self.custom_read_maximum).ok()?.min(CAPACITY);
        let (mut least, mut most) = (u64::MAX, 0);
        for window in self
            .windows
            .iter()
            .take(self.open.possible_windows as usize)
        {
            if range.start < window.end || range.start.checked_sub(window.end)? > DISCARD_BOUND {
                return None;
            }
            let remaining = self.physical_length.checked_sub(window.end)?;
            let calls = remaining.checked_add(quantum - 1)? / quantum;
            least = least.min(calls);
            most = most.max(calls);
        }
        Some(ModeledInitialRead {
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
        context: DescriptionContext,
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
                "v4 modeled transport source/configuration binding",
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
            return Err(Error::Unsupported("v4 modeled metadata consumption bounds"));
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
    fn forward_skip(&mut self, target: u64, length: u64, maximum: usize) -> Result<bool> {
        if target < self.cursor || target > length {
            return Err(Error::Unsupported("v4 symbolic source skip bounds"));
        }
        if target > self.end && target.checked_sub(self.end).ok_or(Error::Overflow)? > DISCARD_BOUND
        {
            *self = Self {
                cursor: target,
                begin: target,
                end: target,
            };
            return Ok(true);
        }
        self.consume(target, length, maximum)?;
        Ok(false)
    }
    fn revisit_set(&mut self, target: u64) -> Result<(EntryRevisit, u64, u64, bool)> {
        if (self.begin..=self.end).contains(&target) {
            self.cursor = target;
            return Ok((EntryRevisit::Buffered, target, target, true));
        }
        let filled = self.end.checked_sub(self.begin).ok_or(Error::Overflow)?;
        let gap = self
            .begin
            .checked_sub(target)
            .ok_or(Error::Unsupported("v4 unreviewed forward entry revisit"))?;
        // The captured entry-revisit protocol uses at most one preceding
        // filled window. Retain EVERY source-consistent origin in that window;
        // do not select a half-window threshold or one observed alignment.
        if gap < filled {
            Ok((
                EntryRevisit::OneWindowCallbackSet,
                self.begin.saturating_sub(filled),
                target,
                false,
            ))
        } else {
            Ok((EntryRevisit::DirectCallback, target, target, false))
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
            "v4 modeled transport requires bounded plain source atoms",
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
        return Err(Error::Unsupported("v4 unreviewed RGB24 entry fields"));
    }
    Ok(())
}

/// Constant-depth, bounded-header validation and at most64 bounded inline window states.
/// Every source byte stays in its original immutable, pool-owned metadata view.
#[derive(Debug)]
pub struct RootWalkTransportBuilder<'a> {
    ftyp: BoxView,
    movie: BoxView,
    table: BoxView,
    entry: BoxView,
    roots: &'a [BoxHeader],
    context: DescriptionContext,
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
    revisit: EntryRevisit,
    ready: bool,
    failed: Option<Error>,
    next_anchor: u64,
    last_anchor: u64,
    buffered_revisit: bool,
    source_min: Option<u64>,
    source_max: u64,
    windows: u32,
    paths: [Window; 64],
    candidate: Option<Window>,
    tail_at: usize,
    tail_phase: u8,
    candidate_seeks: u32,
    seek_min: u32,
    seek_max: u32,
    logical_position: Option<u64>,
}
impl<'a> RootWalkTransportBuilder<'a> {
    pub fn new(
        ftyp: BoxView,
        movie: BoxView,
        roots: &'a [BoxHeader],
        table: &DescriptionRecoveryTable,
        pool: &Budget,
        context: DescriptionContext,
        _profile: RootWalkTransportProfile,
    ) -> Result<Self> {
        if roots.len() > CHILD_LIMIT as usize {
            return Err(Error::Limit {
                requested: roots.len(),
                maximum: CHILD_LIMIT as usize,
            });
        }
        if !ftyp.bytes().belongs_to(pool)
            || !movie.bytes().belongs_to(pool)
            || !table.raw().bytes().belongs_to(pool)
        {
            return Err(Error::ForeignPool);
        }
        let mut extents = roots.iter().filter(|header| header.kind == *b"mdat");
        let media = extents
            .next()
            .ok_or(Error::Unsupported("v4 missing source media"))?
            .payload_range()?;
        if extents.next().is_some() {
            return Err(Error::Unsupported("v4 multiple media extents"));
        }
        if context.physical_length > i64::MAX as u64
            || movie.header().end()?.is_none_or(|end| end > CAPACITY)
            || context.physical_length == 0
            || !matches!(context.custom_read_maximum, 1 | 2 | 3 | 4 | 7 | 64 | 65536)
            || media.start > media.end
            || media.end > context.physical_length
            || ftyp.header().kind != *b"ftyp"
            || ftyp.payload() != b"qt  \0\0\0\0qt  "
            || movie.header().kind != *b"moov"
            || table.descriptions().len() != 1
        {
            return Err(Error::Unsupported(
                "v4 bounded raw-video source-operation domain",
            ));
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
            revisit: EntryRevisit::Buffered,
            ready: false,
            failed: None,
            next_anchor: 0,
            last_anchor: 0,
            buffered_revisit: false,
            source_min: None,
            source_max: 0,
            windows: 0,
            paths: [Window::default(); 64],
            candidate: None,
            tail_at: 0,
            tail_phase: 0,
            candidate_seeks: 0,
            seek_min: u32::MAX,
            seek_max: 0,
            logical_position: None,
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
                        return Err(Error::Unsupported("v4 incomplete source root carriage"));
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
                    return Err(Error::Unsupported("v4 discontinuous source root carriage"));
                }
                self.root_end = header
                    .end()?
                    .ok_or(Error::Unsupported("v4 unknown root end"))?;
                if header.kind != *b"free" {
                    let wanted = [*b"ftyp", *b"moov", *b"mdat"]
                        .get(usize::from(self.root_kind))
                        .copied();
                    if wanted != Some(header.kind) {
                        return Err(Error::Unsupported("v4 unreviewed source root order"));
                    }
                    if (self.root_kind == 0 && header != self.ftyp.header())
                        || (self.root_kind == 1 && header != self.movie.header())
                        || (self.root_kind == 2 && header.payload_range()? != self.media)
                    {
                        return Err(Error::Unsupported("v4 metadata/root range binding"));
                    }
                    self.root_kind += 1;
                }
                self.root_at += 1;
                continue;
            }
            if !self.source_ready {
                let children = self
                    .children
                    .as_mut()
                    .ok_or(Error::Invalid("v4 missing transport source iterator"))?;
                match children.poll(work)? {
                    BoxStep::Yield => return Ok(BuildStatus::Yield),
                    BoxStep::Box(raw) => {
                        small_plain(raw.header(), self.context.physical_length)?;
                        if raw.header().kind == *b"free" && self.level != 5 {
                            continue;
                        }
                        if expected(self.level).get(self.matched) != Some(&raw.header().kind) {
                            return Err(Error::Unsupported("v4 unreviewed metadata child order"));
                        }
                        if self.level == 2
                            && raw.header().kind == *b"hdlr"
                            && fields::fourcc(raw.payload(), 8)? != *b"vide"
                        {
                            return Err(Error::Unsupported("v4 nonvisual source handler"));
                        }
                        if self.level == 3
                            && raw.header().kind == *b"dinf"
                            && raw.payload()
                                != b"\0\0\0\x1cdref\0\0\0\0\0\0\0\x01\0\0\0\x0curl \0\0\0\x01"
                        {
                            return Err(Error::Unsupported("v4 unreviewed source data reference"));
                        }
                        if self.level == 3
                            && raw.header().kind == *b"stbl"
                            && (raw.header() != self.table.header()
                                || raw.bytes().as_ref() != self.table.bytes().as_ref())
                        {
                            return Err(Error::Unsupported("v4 source table binding"));
                        }
                        if self.level == 4
                            && raw.header().kind == *b"stsd"
                            && (fields::u32_at(raw.payload(), 0)? != 0
                                || fields::u32_at(raw.payload(), 4)? != 1)
                        {
                            return Err(Error::Unsupported("v4 unreviewed source catalogue"));
                        }
                        if self.level == 5
                            && (raw.header() != self.entry.header()
                                || raw.bytes().as_ref() != self.entry.bytes().as_ref())
                        {
                            return Err(Error::Unsupported("v4 source entry binding"));
                        }
                        if raw.header().kind == next_kind(self.level) {
                            self.next = Some(raw);
                        }
                        self.matched += 1;
                    }
                    BoxStep::End => {
                        if self.matched != expected(self.level).len() {
                            return Err(Error::Unsupported("v4 incomplete transport metadata"));
                        }
                        if self.level == 5 {
                            self.source_ready = true;
                            self.children = None;
                            continue;
                        }
                        let next = self
                            .next
                            .take()
                            .ok_or(Error::Unsupported("v4 missing transport metadata child"))?;
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
                        self.movie.header().end()?.ok_or(Error::Overflow)?,
                        self.context.physical_length,
                        self.context.custom_read_maximum,
                    )?;
                    self.candidate = Some(candidate);
                    self.tail_at = 0;
                    self.tail_phase = 0;
                    self.candidate_seeks = 0;
                    self.action = 3;
                }
                3 => {
                    let movie_end = self.movie.header().end()?.ok_or(Error::Overflow)?;
                    // At most16 headers. Each skipped metadata prefix header was
                    // already separately validated and paid during the root scan.
                    while self.tail_at < self.roots.len()
                        && self.roots[self.tail_at].end()?.ok_or(Error::Overflow)? <= movie_end
                    {
                        self.tail_at += 1;
                    }
                    let header = self
                        .roots
                        .get(self.tail_at)
                        .ok_or(Error::Unsupported("v4 incomplete symbolic root walk"))?;
                    let payload = header.payload_range()?;
                    let candidate = self
                        .candidate
                        .as_mut()
                        .ok_or(Error::Invalid("v4 missing symbolic path"))?;
                    match self.tail_phase {
                        0 => {
                            candidate.consume(
                                payload.start,
                                self.context.physical_length,
                                self.context.custom_read_maximum,
                            )?;
                            self.tail_phase = 1;
                        }
                        1 => {
                            let size = payload
                                .end
                                .checked_sub(payload.start)
                                .ok_or(Error::Overflow)?;
                            let probe = if header.kind == *b"free" && size >= 8 {
                                size.min(16)
                            } else {
                                0
                            };
                            candidate.consume(
                                payload.start.checked_add(probe).ok_or(Error::Overflow)?,
                                self.context.physical_length,
                                self.context.custom_read_maximum,
                            )?;
                            self.tail_phase = 2;
                        }
                        2 => {
                            if payload.end == self.context.physical_length {
                                self.action = 4;
                            } else {
                                if candidate.forward_skip(
                                    payload.end,
                                    self.context.physical_length,
                                    self.context.custom_read_maximum,
                                )? {
                                    self.candidate_seeks = self
                                        .candidate_seeks
                                        .checked_add(1)
                                        .ok_or(Error::Overflow)?;
                                }
                                self.tail_at += 1;
                                self.tail_phase = 0;
                            }
                        }
                        _ => return Err(Error::Invalid("v4 invalid symbolic atom phase")),
                    }
                }
                4 => {
                    let candidate = self
                        .candidate
                        .take()
                        .ok_or(Error::Invalid("v4 missing completed path"))?;
                    if self.logical_position.is_some_and(|p| p != candidate.cursor) {
                        return Err(Error::Unsupported(
                            "v4 path-dependent logical operation end",
                        ));
                    }
                    self.logical_position = Some(candidate.cursor);
                    self.source_min = Some(
                        self.source_min
                            .map_or(candidate.end, |v| v.min(candidate.end)),
                    );
                    self.source_max = self.source_max.max(candidate.end);
                    let slot = usize::try_from(self.windows).map_err(|_| Error::Overflow)?;
                    *self.paths.get_mut(slot).ok_or(Error::Limit {
                        requested: slot + 1,
                        maximum: 64,
                    })? = candidate;
                    self.windows = self.windows.checked_add(1).ok_or(Error::Overflow)?;
                    self.seek_min = self.seek_min.min(self.candidate_seeks);
                    self.seek_max = self.seek_max.max(self.candidate_seeks);
                    if self.next_anchor == self.last_anchor {
                        self.ready = true;
                    } else {
                        self.next_anchor =
                            self.next_anchor.checked_add(1).ok_or(Error::Overflow)?;
                        self.action = 2;
                    }
                }
                _ => return Err(Error::Invalid("v4 invalid transport replay action")),
            }
            if self.action < 2 {
                self.action += 1;
            }
        }
    }
    pub fn finish(self) -> Result<RootWalkTransportModel> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if !self.ready {
            return Err(Error::Invalid("v4 transport builder not ready"));
        }
        Ok(RootWalkTransportModel {
            ftyp: self.ftyp,
            movie: self.movie,
            table: self.table,
            physical_length: self.context.physical_length,
            custom_read_maximum: self.context.custom_read_maximum,
            media: self.media.clone(),
            open: ModeledOpenState {
                source_position_min: self
                    .source_min
                    .ok_or(Error::Invalid("v4 empty modeled window set"))?,
                source_position_max: self.source_max,
                logical_position: self
                    .logical_position
                    .ok_or(Error::Invalid("v4 missing modeled logical end"))?,
                possible_windows: self.windows,
                entry_revisit: self.revisit,
            },
            windows: self.paths,
            root_headers: std::array::from_fn(|i| self.roots.get(i).copied()),
            absolute_seek_min: self.seek_min,
            absolute_seek_max: self.seek_max,
        })
    }
}
