//! A model of FFmpeg's buffered reader during the open of a small QuickTime raw-video file.
//!
//! [`RawVideoTransportBuilder`] walks the file's top-level boxes and movie metadata and works
//! out the buffer states FFmpeg's reader can be in when open returns, for a read callback
//! that returns at most
//! [`custom_read_maximum`](crate::compat::mov::recovery::DescriptionContext::custom_read_maximum)
//! bytes per call. The model reads no media and never repairs a source range, clock or end
//! of file.
use super::transport_window::{CAPACITY, Revisit, Window};
use crate::{
    BuildStatus, Limits, Next,
    boxes::{BoxChildren, BoxHeader, BoxView},
    fields,
    table::recovery::{DescriptionContext, DescriptionRecoveryTable},
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, Error, Result, WorkBudget};

const CONSUMPTION_BOUNDS: &str = "v3 modeled metadata consumption bounds";
const DISCARD_BOUND: u64 = 32768; // Separately observed reference forward protocol bound.
const CHILD_LIMIT: u32 = 16;

/// Selects the raw-video transport model; pass it to [`RawVideoTransportBuilder::new`].
#[derive(Clone, Copy, Debug)]
pub struct RawVideoTransportProfile;

/// How FFmpeg's reader returns to the start of the sample entry after reading the entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryRevisit {
    /// The entry start is still in the buffer; the revisit reads nothing.
    Buffered,
    /// The entry start lies at least the buffered length before the buffer; the reader
    /// refills from the entry start.
    DirectCallback,
    /// The entry start lies less than the buffered length before the buffer. The refill
    /// can start anywhere from that length before the buffer to the entry start, and the
    /// model keeps every such start.
    OneWindowCallbackSet,
}

/// The buffer states FFmpeg's reader can be in when open returns. It is a model, not a
/// record of FFmpeg's reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeledOpenState {
    /// Least absolute file offset just past the buffered bytes, over the possible states.
    pub source_position_min: u64,
    /// Greatest absolute file offset just past the buffered bytes, over the possible states.
    pub source_position_max: u64,
    /// Absolute file offset of the next byte the reader returns; the same in every state.
    pub logical_position: u64,
    /// Number of possible buffer states, from 1 to 64.
    pub possible_windows: u32,
    /// How the reader returned to the start of the sample entry during open.
    pub entry_revisit: EntryRevisit,
}

/// How FFmpeg's reader reaches end of file for a first packet that starts at or past the
/// physical end of the source.
///
/// In every possible buffer state the reader skips forward by reading and discarding the
/// rest of the source, through callback calls that each return bytes, and the next call,
/// at the physical end, returns end of file. No packet bytes are produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeledInitialRead {
    pub(crate) possible_windows: u32,
    /// Fewest callback calls returning bytes before the call that returns end of file, over
    /// the possible buffer states.
    pub positive_callback_calls_min: u64,
    /// Most callback calls returning bytes before the call that returns end of file, over
    /// the possible buffer states.
    pub positive_callback_calls_max: u64,
    /// Absolute file offset of the call that returns end of file: the physical length of
    /// the source.
    pub terminal_callback_position: u64,
    pub(crate) terminal_callback_requested: u32,
}

/// The buffer states FFmpeg's reader can be in after opening a small QuickTime raw-video
/// file, built by [`RawVideoTransportBuilder`].
///
/// Passed to [`DescriptionRecoveryTable::description_packet_cursor_with_transport`], it
/// lets a recovered first packet past the physical end report the end of file FFmpeg
/// reaches.
#[derive(Debug)]
pub struct RawVideoTransportModel {
    ftyp: BoxView,
    movie: BoxView,
    table: BoxView,
    physical_length: u64,
    custom_read_maximum: usize,
    media: Range<u64>,
    open: ModeledOpenState,
    revisit_seed: Window,
    revisit: Revisit,
}
impl RawVideoTransportModel {
    pub(crate) fn context(&self) -> DescriptionContext {
        DescriptionContext {
            physical_length: self.physical_length,
            custom_read_maximum: self.custom_read_maximum,
        }
    }
    /// The buffer states when open returns.
    pub fn modeled_open_state(&self) -> ModeledOpenState {
        self.open
    }
    /// The `moov` box the model was built from.
    pub fn source_movie(&self) -> &BoxView {
        &self.movie
    }
    /// The `ftyp` box the model was built from.
    pub fn source_file_type(&self) -> &BoxView {
        &self.ftyp
    }
    /// Whether a buffer state FFmpeg reports after open is exactly one of the possible
    /// states. `source_position` is the absolute file offset just past the buffered bytes;
    /// `ptr_offset` and `end_offset` are the offsets of the read position and of the buffer
    /// end from the buffer start.
    pub fn includes_open_window(
        &self,
        source_position: u64,
        ptr_offset: u64,
        end_offset: u64,
    ) -> bool {
        for anchor in self.revisit.first_anchor..=self.revisit.last_anchor {
            let mut window = self.revisit.window_at(self.revisit_seed, anchor);
            if window
                .consume(self.media.start, self.context(), CONSUMPTION_BOUNDS)
                .is_err()
            {
                return false;
            }
            if window.matches(source_position, ptr_offset, end_offset) {
                return true;
            }
        }
        false
    }
    /// Whether FFmpeg reaches end of file, rather than seeking, when it reads a first packet
    /// at `range` (absolute file offsets): see [`initial_read_plan`](Self::initial_read_plan).
    /// The source range stays invalid either way.
    pub fn models_initial_eof(&self, range: Range<u64>) -> bool {
        self.initial_read_plan(range).is_some()
    }
    /// How FFmpeg's reader reaches end of file for a first packet at `range`, or `None`
    /// where the model does not apply. It applies to a packet of 1 byte to 16 MiB that
    /// starts at or past the physical end, at most 32768 bytes past the buffer end in every
    /// possible state, so that FFmpeg skips to it by reading and discarding.
    pub fn initial_read_plan(&self, range: Range<u64>) -> Option<ModeledInitialRead> {
        let size = range.end.checked_sub(range.start)?;
        if size == 0 || size > 16 * 1024 * 1024 || range.start < self.physical_length {
            return None;
        }
        let quantum = u64::try_from(self.custom_read_maximum).ok()?.min(CAPACITY);
        let mut least = u64::MAX;
        let mut most = 0;
        for anchor in self.revisit.first_anchor..=self.revisit.last_anchor {
            let mut window = self.revisit.window_at(self.revisit_seed, anchor);
            window
                .consume(self.media.start, self.context(), CONSUMPTION_BOUNDS)
                .ok()?;
            let buffered_target = window.contains(range.start);
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
                "v3 modeled transport source/configuration binding",
            ));
        }
        Ok(())
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

/// Builds a [`RawVideoTransportModel`] from a file's top-level box headers and movie
/// metadata.
///
/// It accepts only a small QuickTime raw-video file: `ftyp`, `moov` and a final `mdat`,
/// with `free` boxes between them, plain 8-byte box headers, and one video track whose
/// boxes come in a fixed order and whose one entry is a 78-byte RGB24 `raw ` entry. Any
/// other file is `Error::Unsupported`. Source boxes stay views of their original bytes,
/// and the builder holds at most 64 buffer states.
#[derive(Debug)]
pub struct RawVideoTransportBuilder<'a> {
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
    revisit: Revisit,
    ready: bool,
    failed: Option<Error>,
    next_anchor: u64,
    source_min: Option<u64>,
    source_max: u64,
    windows: u32,
}
impl<'a> RawVideoTransportBuilder<'a> {
    /// Starts a model of the file with these `ftyp` and `moov` boxes, `roots` its top-level
    /// box headers in file order and `table` its track's recovered sample table. Fails with
    /// `Error::ForeignPool` when a box's bytes do not belong to `pool`, and with
    /// `Error::Unsupported` for a source longer than 32768 bytes, a `context` read size
    /// other than 1, 2, 3, 4, 7, 64 or 65536 bytes, or a file outside the accepted set.
    pub fn new(
        ftyp: BoxView,
        movie: BoxView,
        roots: &'a [BoxHeader],
        table: &DescriptionRecoveryTable,
        pool: &Budget,
        context: DescriptionContext,
        _profile: RawVideoTransportProfile,
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
            revisit: Revisit::buffered(0),
            ready: false,
            failed: None,
            next_anchor: 0,
            source_min: None,
            source_max: 0,
            windows: 0,
        })
    }
    /// Checks further box headers and metadata boxes, then works out the buffer states.
    /// Fails with `Error::Unsupported` where the file leaves the accepted set.
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
                    Next::Yield => return Ok(BuildStatus::Yield),
                    Next::Item(raw) => {
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
                    Next::End => {
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
                    self.context,
                    CONSUMPTION_BOUNDS,
                )?,
                1 => {
                    self.revisit = self.window.revisit_set(
                        self.entry.header().start,
                        "v3 unreviewed forward entry revisit",
                    )?;
                    self.next_anchor = self.revisit.first_anchor;
                }
                2 => {
                    let mut candidate = self.revisit.window_at(self.window, self.next_anchor);
                    candidate.consume(self.media.start, self.context, CONSUMPTION_BOUNDS)?;
                    self.source_min = Some(
                        self.source_min
                            .map_or(candidate.end, |v| v.min(candidate.end)),
                    );
                    self.source_max = self.source_max.max(candidate.end);
                    self.windows = self.windows.checked_add(1).ok_or(Error::Overflow)?;
                    if self.next_anchor == self.revisit.last_anchor {
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
    /// Returns the model after [`poll`](Self::poll) returned `Ready`; `Error::Invalid`
    /// before then.
    pub fn finish(self) -> Result<RawVideoTransportModel> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if !self.ready {
            return Err(Error::Invalid("v3 transport builder not ready"));
        }
        Ok(RawVideoTransportModel {
            ftyp: self.ftyp,
            movie: self.movie,
            table: self.table,
            physical_length: self.context.physical_length,
            custom_read_maximum: self.context.custom_read_maximum,
            media: self.media.clone(),
            open: ModeledOpenState {
                source_position_min: self
                    .source_min
                    .ok_or(Error::Invalid("v3 empty modeled window set"))?,
                source_position_max: self.source_max,
                logical_position: self.media.start,
                possible_windows: self.windows,
                entry_revisit: self.revisit.kind,
            },
            revisit_seed: self.window,
            revisit: self.revisit,
        })
    }
}
