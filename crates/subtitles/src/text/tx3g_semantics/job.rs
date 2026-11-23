use super::super::{
    Encoding, Extractor, Format, Input, Status,
    charset::{Charset, CharsetJob, CharsetStatus},
    tx3g_config::{ConfigJob, ConfigLayout, ConfigStatus},
    unicode::{Cursor, Poll, cancelled},
};
use super::parse::{Item, Reader};
use super::*;
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, Error, Result, WorkBudget};

/// Resolves one tx3g sample and its optional sample description into
/// [`Semantics`]. It runs the tx3g extractor and the description parser itself,
/// so it takes no caller-built `Cue` or `Configuration`.
#[derive(Debug)]
pub struct SemanticJob {
    pool: Budget,
    limits: SemanticLimits,
    state: State,
}

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "The state is stepped in place; the resolution stays inline rather than in an allocation the pool does not charge."
)]
enum State {
    /// Reading the sample; its description, if any, is parsed after it.
    Source {
        extractor: Extractor,
        description: Option<ConfigJob>,
        context: ValidationContext,
    },
    /// The sample is read; resolving its records.
    Resolving(Resolution),
    /// The semantics were taken.
    Taken,
    /// The error that ended the job; its storage is released.
    Failed(Error),
}

#[derive(Debug)]
struct Resolution {
    found: Found,
    phase: Phase,
}

/// What is resolved so far: the semantics being built, handed over whole once
/// ready.
#[derive(Debug)]
struct Found {
    semantics: Semantics,
    /// Records committed, against [`SemanticLimits::maximum_records`].
    records: usize,
    /// Where the next karaoke entry starts: the end of the one before it.
    next_tick: u32,
}

#[derive(Debug)]
enum Phase {
    /// Parsing the sample description.
    Description(ConfigJob),
    /// Converting the description's font names, from the font at `at`.
    Fonts { at: usize, font: FontStep },
    /// Counting the text's characters.
    Count(Cursor),
    /// Reading the description's `disp` boxes, from the box at `at`.
    DescriptionBoxes { at: usize, open: Option<Reader> },
    /// Reading the sample's modifier boxes, from the box at `at`, and
    /// committing their records one by one.
    Boxes {
        at: usize,
        open: Option<Reader>,
        record: Option<Record>,
    },
    /// Checking static highlights and links against karaoke.
    Conflicts(Conflicts),
    /// Cutting the text into runs.
    Runs(Runs),
    /// The semantics, ready to take.
    Ready,
}

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "The step is advanced in place; the conversion stays inline rather than in an allocation the pool does not charge."
)]
enum FontStep {
    /// The next name is not opened yet.
    Open,
    /// Converting the name to UTF-8.
    Convert(CharsetJob),
    /// The font, to enter in the font table; whether its unit is paid.
    Commit { font: SemanticFont, paid: bool },
}

/// A record read but not yet committed, and whether its unit is paid.
#[derive(Debug)]
struct Record {
    item: Item,
    paid: bool,
}

/// The records whose ranges the scan checks against karaoke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Checked {
    Highlights,
    Links,
}

#[derive(Debug)]
struct Conflicts {
    checked: Checked,
    /// The record being checked.
    at: usize,
    /// The karaoke entry it is checked against.
    event: usize,
}

/// What a step of the conflict scan did.
enum Scan {
    /// The work ran out.
    Pending,
    /// It checked a pair or moved on.
    Stepped,
    /// Every record is checked.
    Done,
}

#[derive(Debug)]
struct Runs {
    cursor: Cursor,
    /// The next run's first character.
    position: usize,
    /// The record of each kind (style, highlight, link, blink) the sweep is
    /// at.
    sweep: [usize; 4],
    /// Characters the cursor has read, and the source and UTF-8 offsets it
    /// has reached.
    character: usize,
    source: usize,
    utf8: usize,
    run: RunStep,
}

#[derive(Debug)]
enum RunStep {
    /// The next run is not cut yet.
    Prepare,
    /// Reading the run's characters to find its byte ranges.
    Text(ResolvedRun),
    /// The run, to append; whether its unit is paid.
    Commit { run: ResolvedRun, paid: bool },
}

/// What preparing the next run did.
enum Prepared {
    /// The work ran out.
    Pending,
    /// The sweep passed a record that ends before the run.
    Swept,
    /// The run, its byte ranges still open.
    Run(ResolvedRun),
}

impl SemanticJob {
    /// Starts on `input`, a tx3g sample whose `codec_private` holds the sample
    /// description in `layout`, if one is given. `track_timescale`, in ticks
    /// per second, converts modifier times to [`Time`]; `generation` must be
    /// the input's.
    ///
    /// Fails with `Error::Unsupported` for a format other than `Format::Tx3g`,
    /// `Error::Invalid` for a zero timescale or a layout without
    /// `codec_private`, and as the tx3g extractor and
    /// `ConfigJob::new_unicode_fonts` refuse the input (`Error::StaleGeneration`,
    /// `Error::ForeignPool`, `Error::Limit`). The input is left as it was.
    pub fn new(
        input: &Input,
        layout: Option<ConfigLayout>,
        track_timescale: Option<u32>,
        pool: &Budget,
        limits: SemanticLimits,
        generation: u64,
    ) -> Result<Self> {
        if input.format != Format::Tx3g {
            return Err(Error::Unsupported("tx3g semantic input format"));
        }
        if track_timescale == Some(0) {
            return Err(Error::Invalid("zero tx3g track timescale"));
        }
        let mut extractor = Extractor::tx3g_iso_boxes(pool, limits.text, generation);
        extractor.provide(input.clone()).map_err(|e| e.error)?;
        let description = match layout {
            Some(layout) => Some(ConfigJob::new_unicode_fonts(
                input
                    .codec_private
                    .as_ref()
                    .ok_or(Error::Invalid("tx3g description layout without source"))?,
                layout,
                pool,
                limits.config,
            )?),
            None => None,
        };
        let duration_known = input.timing.duration().is_some();
        Ok(Self {
            pool: pool.clone(),
            limits,
            state: State::Source {
                extractor,
                description,
                context: ValidationContext {
                    description_parsed: false,
                    description_bytes_absent: input.codec_private.is_none(),
                    explicit_track_timescale: track_timescale,
                    sample_duration_known: duration_known,
                    duration_bounds_checked: track_timescale.is_some() && duration_known,
                    conditional_combinations_checked: true,
                },
            },
        })
    }

    /// Advances by at most `work` units, charged per byte, character, record
    /// and run handled.
    ///
    /// A malformed sample or description fails with `Error::Invalid`,
    /// `Error::Truncated` or `Error::Unsupported`, a modifier time past the
    /// known sample duration with `Error::Invalid`, and too many records or
    /// runs with `Error::Limit`. Such an error releases the job's storage and
    /// every later call returns it; cancellation does not, and the job
    /// resumes. After [`Self::take_output`] the call fails with
    /// `Error::Invalid`.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<SemanticStatus> {
        cancelled(cancel)?;
        if let State::Failed(error) = self.state {
            return Err(error);
        }
        match self.inner(work, cancel) {
            Err(Error::Backpressure { .. }) => Ok(SemanticStatus::NeedOutput),
            Err(error) if error != Error::Cancelled => {
                self.state = State::Failed(error);
                Err(error)
            }
            result => result,
        }
    }

    fn inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<SemanticStatus> {
        loop {
            cancelled(cancel)?;
            let resolution = match &mut self.state {
                State::Resolving(Resolution {
                    phase: Phase::Ready,
                    ..
                }) => return Ok(SemanticStatus::Ready),
                State::Taken => return Err(Error::Invalid("tx3g semantics already taken")),
                State::Failed(error) => return Err(*error),
                _ if work.remaining() == 0 => return Ok(SemanticStatus::Pending),
                State::Source {
                    extractor,
                    description,
                    context,
                } => {
                    match extractor.step(work, cancel)? {
                        Status::Yield => return Ok(SemanticStatus::Pending),
                        Status::NeedOutput => return Ok(SemanticStatus::NeedOutput),
                        Status::Cue(cue) => {
                            let phase = match description.take() {
                                Some(job) => Phase::Description(job),
                                None => Phase::Fonts {
                                    at: 0,
                                    font: FontStep::Open,
                                },
                            };
                            self.state = State::Resolving(Resolution {
                                found: Found::new(cue, *context, &self.pool),
                                phase,
                            });
                        }
                        _ => return Err(Error::Invalid("unexpected tx3g source state")),
                    }
                    continue;
                }
                State::Resolving(resolution) => resolution,
            };
            if let Some(status) = resolution.step(&self.pool, self.limits, work, cancel)? {
                return Ok(status);
            }
        }
    }

    /// Hands over the semantics once [`Self::poll`] returned
    /// [`SemanticStatus::Ready`]; `Error::Invalid` before that and on a second
    /// call, and the job's error after a failure.
    pub fn take_output(&mut self, cancel: &AtomicBool) -> Result<Semantics> {
        cancelled(cancel)?;
        match std::mem::replace(&mut self.state, State::Taken) {
            State::Resolving(Resolution {
                found,
                phase: Phase::Ready,
            }) => Ok(found.semantics),
            state => {
                let error = match state {
                    State::Failed(error) => error,
                    _ => Error::Invalid("tx3g semantics not ready"),
                };
                self.state = state;
                Err(error)
            }
        }
    }
}

impl Resolution {
    /// One step of the phase; the status to return, or `None` to go on.
    fn step(
        &mut self,
        pool: &Budget,
        limits: SemanticLimits,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<SemanticStatus>> {
        const PENDING: Option<SemanticStatus> = Some(SemanticStatus::Pending);
        let found = &mut self.found;
        match &mut self.phase {
            Phase::Description(job) => {
                if job.poll(work, cancel)? == ConfigStatus::Pending {
                    return Ok(PENDING);
                }
                let description = job.take_output(cancel)?;
                let semantics = &mut found.semantics;
                semantics.globals.display =
                    Some(DisplayIntent::new(description.defaults.display_flags));
                semantics.context.description_parsed = true;
                semantics.description = Some(description);
                self.phase = Phase::Fonts {
                    at: 0,
                    font: FontStep::Open,
                };
            }
            Phase::Fonts { at, font } => {
                let description = found.semantics.description.as_ref();
                let Some((d, record)) =
                    description.and_then(|d| Some((d, d.fonts.as_slice().get(*at)?)))
                else {
                    if let Some(d) = description {
                        found.reference_font(d.defaults.style.font_id)?;
                    }
                    let (text, encoding) = found.text()?;
                    self.phase = Phase::Count(Cursor::new(text, encoding));
                    return Ok(None);
                };
                match font {
                    FontStep::Open => {
                        if !work.try_charge(1) {
                            return Ok(PENDING);
                        }
                        let bytes = d.raw.slice(record.name_range.clone())?;
                        let charset = if bytes.as_ref().starts_with(&[0xfe, 0xff]) {
                            Charset::Utf16Bom
                        } else {
                            Charset::Utf8
                        };
                        *font = FontStep::Convert(CharsetJob::new(
                            &bytes,
                            None,
                            charset,
                            pool,
                            limits.text,
                        )?);
                    }
                    FontStep::Convert(job) => {
                        if job.poll(work, cancel)? == CharsetStatus::Pending {
                            return Ok(PENDING);
                        }
                        let name = job.take_output(cancel)?;
                        *font = FontStep::Commit {
                            font: SemanticFont {
                                id: record.id,
                                raw_name: record.name_range.clone(),
                                name_utf8: name.utf8,
                                utf16_bom: name.charset == Charset::Utf16Bom,
                            },
                            paid: false,
                        };
                    }
                    FontStep::Commit {
                        font: pending,
                        paid,
                    } => {
                        if found.commit_font(pending, paid, work)? {
                            *at += 1;
                            *font = FontStep::Open;
                        }
                    }
                }
            }
            Phase::Count(cursor) => {
                let payload = found.semantics.source.input.payload.as_ref();
                match cursor.poll(payload, work, cancel)? {
                    Poll::Yield => return Ok(PENDING),
                    Poll::Ready(_) => {
                        let characters = &mut found.semantics.characters;
                        *characters = characters.checked_add(1).ok_or(Error::Overflow)?;
                    }
                    Poll::End => {
                        self.phase = Phase::DescriptionBoxes { at: 0, open: None };
                    }
                }
            }
            Phase::DescriptionBoxes { at, open } => {
                let Some(d) = found
                    .semantics
                    .description
                    .as_ref()
                    .filter(|d| *at < d.boxes.len())
                else {
                    self.phase = Phase::Boxes {
                        at: 0,
                        open: None,
                        record: None,
                    };
                    return Ok(None);
                };
                let reader = if let Some(reader) = open {
                    reader
                } else {
                    if !work.try_charge(1) {
                        return Ok(PENDING);
                    }
                    let modifier = &d.boxes.as_slice()[*at];
                    // Of the description's boxes only `disp` is read.
                    let reader = if modifier.kind == *b"disp" {
                        Reader::new(d.raw.as_ref(), modifier, *at)?
                    } else {
                        None
                    };
                    let Some(reader) = reader else {
                        *at += 1;
                        return Ok(None);
                    };
                    open.insert(reader)
                };
                match reader.poll(d.raw.as_ref(), work, cancel)? {
                    Poll::Yield => return Ok(PENDING),
                    Poll::Ready(Item::Disparity(_, value)) => {
                        let disparity = &mut found.semantics.globals.description_disparity;
                        if disparity.is_some() {
                            return Err(Error::Invalid("duplicate tx3g description disparity"));
                        }
                        *disparity = Some(value);
                    }
                    Poll::Ready(_) => {
                        return Err(Error::Invalid("unexpected description semantic field"));
                    }
                    Poll::End => {
                        *open = None;
                        *at += 1;
                    }
                }
            }
            Phase::Boxes { at, open, record } => {
                if let Some(pending) = record {
                    if !pending.paid {
                        if !work.try_charge(1) {
                            return Ok(PENDING);
                        }
                        pending.paid = true;
                    }
                    if found.records >= limits.maximum_records {
                        return Err(Error::Limit {
                            requested: found.records.checked_add(1).ok_or(Error::Overflow)?,
                            maximum: limits.maximum_records,
                        });
                    }
                    found.commit(&pending.item)?;
                    *record = None;
                    return Ok(None);
                }
                let cue = &found.semantics.source;
                let Some(modifier) = cue.modifiers.as_slice().get(*at) else {
                    found.finish_globals();
                    self.phase = Phase::Conflicts(Conflicts {
                        checked: Checked::Highlights,
                        at: 0,
                        event: 0,
                    });
                    return Ok(None);
                };
                let payload = cue.input.payload.as_ref();
                let reader = if let Some(reader) = open {
                    reader
                } else {
                    if !work.try_charge(1) {
                        return Ok(PENDING);
                    }
                    // A box no record is read from has no effect.
                    let Some(reader) = Reader::new(payload, modifier, *at)? else {
                        *at += 1;
                        return Ok(None);
                    };
                    open.insert(reader)
                };
                match reader.poll(payload, work, cancel)? {
                    Poll::Yield => return Ok(PENDING),
                    Poll::Ready(item) => *record = Some(Record { item, paid: false }),
                    Poll::End => {
                        *open = None;
                        *at += 1;
                    }
                }
            }
            Phase::Conflicts(conflicts) => match conflicts.step(found, work)? {
                Scan::Pending => return Ok(PENDING),
                Scan::Stepped => {}
                Scan::Done => {
                    let (text, encoding) = found.text()?;
                    self.phase = Phase::Runs(Runs {
                        position: 0,
                        sweep: [0; 4],
                        character: 0,
                        source: text.start,
                        utf8: 0,
                        cursor: Cursor::new(text, encoding),
                        run: RunStep::Prepare,
                    });
                }
            },
            Phase::Runs(runs) => match &mut runs.run {
                RunStep::Prepare => {
                    if runs.position == found.semantics.characters {
                        self.phase = Phase::Ready;
                        return Ok(None);
                    }
                    match runs.prepare(found, work)? {
                        Prepared::Pending => return Ok(PENDING),
                        Prepared::Swept => {}
                        Prepared::Run(run) => runs.run = RunStep::Text(run),
                    }
                }
                RunStep::Text(run) => {
                    if runs.character == run.characters.end {
                        run.source_bytes.end = runs.source;
                        run.utf8_bytes.end = runs.utf8;
                        runs.run = RunStep::Commit {
                            run: run.clone(),
                            paid: false,
                        };
                        return Ok(None);
                    }
                    let payload = found.semantics.source.input.payload.as_ref();
                    match runs.cursor.poll(payload, work, cancel)? {
                        Poll::Yield => return Ok(PENDING),
                        Poll::End => return Err(Error::Truncated),
                        Poll::Ready(scalar) => {
                            runs.character += 1;
                            runs.source = scalar.end;
                            runs.utf8 = scalar.decoded_end;
                        }
                    }
                }
                RunStep::Commit { run, paid } => {
                    if !*paid {
                        if !work.try_charge(1) {
                            return Ok(PENDING);
                        }
                        *paid = true;
                    }
                    let resolved = &mut found.semantics.runs;
                    if resolved.len() >= limits.maximum_runs {
                        return Err(Error::Limit {
                            requested: resolved.len() + 1,
                            maximum: limits.maximum_runs,
                        });
                    }
                    resolved.try_push(run.clone()).map_err(|e| e.error)?;
                    runs.position = run.characters.end;
                    runs.run = RunStep::Prepare;
                }
            },
            Phase::Ready => return Ok(Some(SemanticStatus::Ready)),
        }
        Ok(None)
    }
}

impl Found {
    fn new(cue: Cue, context: ValidationContext, pool: &Budget) -> Self {
        Self {
            semantics: Semantics {
                source: cue,
                description: None,
                fonts: BudgetVec::new(pool),
                styles: BudgetVec::new(pool),
                highlights: BudgetVec::new(pool),
                hyperlinks: BudgetVec::new(pool),
                blink: BudgetVec::new(pool),
                karaoke_header: None,
                karaoke: BudgetVec::new(pool),
                possible_karaoke_end: None,
                globals: GlobalEffects::default(),
                runs: BudgetVec::new(pool),
                characters: 0,
                font_index: FontIndex::new(pool),
                context,
            },
            records: 0,
            next_tick: 0,
        }
    }

    /// The text's characters in the payload, the UTF-16 byte-order mark left
    /// out, and their encoding.
    fn text(&self) -> Result<(Range<usize>, Encoding)> {
        let cue = &self.semantics.source;
        let mut range = cue.text_range.clone();
        if cue.effective_encoding == Encoding::Utf16Be {
            range.start = range.start.checked_add(2).ok_or(Error::Overflow)?;
        }
        Ok((range, cue.effective_encoding))
    }

    fn span(&self, start: u16, end: u16, sentinel: bool) -> Result<CharacterSpan> {
        let characters = self.semantics.characters;
        let start = usize::from(start);
        let raw_end = usize::from(end);
        let special = sentinel && raw_end == characters.checked_add(1).ok_or(Error::Overflow)?;
        if start > raw_end || start > characters || (raw_end > characters && !special) {
            return Err(Error::Invalid("tx3g character range"));
        }
        Ok(CharacterSpan {
            declared_start: u16::try_from(start).map_err(|_| Error::Overflow)?,
            declared_end: end,
            characters: start..if special { characters } else { raw_end },
            end_of_text_sentinel: special,
        })
    }

    fn ordered(previous: Option<&CharacterSpan>, next: &CharacterSpan) -> Result<()> {
        if previous.is_some_and(|p| p.declared_end > next.declared_start) {
            Err(Error::Invalid("tx3g overlapping or unordered ranges"))
        } else {
            Ok(())
        }
    }

    fn relative(&self, ticks: u32) -> Result<Option<Time>> {
        self.semantics
            .context
            .explicit_track_timescale
            .map(|scale| Time::new(i128::from(ticks), u128::from(scale)))
            .transpose()
    }

    fn time_bound(&self, ticks: u32) -> Result<()> {
        if let (Some(relative), Some(duration)) =
            (self.relative(ticks)?, self.semantics.source.duration)
            && relative > duration
        {
            return Err(Error::Invalid("tx3g modifier time exceeds sample duration"));
        }
        Ok(())
    }

    fn reference_font(&self, id: u16) -> Result<Option<usize>> {
        let index = self.semantics.font_index.get(id);
        if self.semantics.description.is_some() && index.is_none() {
            return Err(Error::Invalid(
                "tx3g font reference absent from description",
            ));
        }
        Ok(index)
    }

    fn commit(&mut self, item: &Item) -> Result<()> {
        macro_rules! unique {
            ($field:ident,$value:expr,$source:expr) => {{
                let slot = &mut self.semantics.globals.$field;
                if slot.is_some() {
                    return Err(Error::Invalid("duplicate global tx3g modifier"));
                }
                *slot = Some(SampleValue {
                    value: $value,
                    source: $source.clone(),
                });
            }};
        }
        match item {
            Item::Style(source, record) => {
                let span = self.span(record.start, record.end, false)?;
                let styles = &self.semantics.styles;
                Self::ordered(styles.as_slice().last().map(|x| &x.span), &span)?;
                self.reference_font(record.font_id)?;
                self.semantics
                    .styles
                    .try_push(StyleSpan {
                        source: source.clone(),
                        span,
                        style: (*record).into(),
                    })
                    .map_err(|e| e.error)?;
            }
            Item::Highlight(source, start, end) => {
                let span = self.span(*start, *end, true)?;
                let highlights = &mut self.semantics.highlights;
                Self::ordered(highlights.as_slice().last().map(|x| &x.span), &span)?;
                highlights
                    .try_push(SpanEffect {
                        source: source.clone(),
                        span,
                    })
                    .map_err(|e| e.error)?;
            }
            Item::Blink(source, start, end) => {
                let span = self.span(*start, *end, false)?;
                let blink = &mut self.semantics.blink;
                Self::ordered(blink.as_slice().last().map(|x| &x.span), &span)?;
                blink
                    .try_push(SpanEffect {
                        source: source.clone(),
                        span,
                    })
                    .map_err(|e| e.error)?;
            }
            Item::Hyperlink(source, start, end, url, alternate) => {
                let span = self.span(*start, *end, false)?;
                let links = &mut self.semantics.hyperlinks;
                Self::ordered(links.as_slice().last().map(|x| &x.span), &span)?;
                links
                    .try_push(Hyperlink {
                        source: source.clone(),
                        span,
                        url: url.clone(),
                        alternate: alternate.clone(),
                    })
                    .map_err(|e| e.error)?;
            }
            Item::KaraokeHeader(source, start_tick, entries) => {
                if self.semantics.karaoke_header.is_some() {
                    return Err(Error::Invalid("duplicate tx3g karaoke box"));
                }
                self.time_bound(*start_tick)?;
                self.semantics.karaoke_header = Some(KaraokeHeader {
                    source: source.clone(),
                    start_tick: *start_tick,
                    entries: *entries,
                });
                self.next_tick = *start_tick;
            }
            Item::Karaoke(source, end_tick, start, end) => {
                let span = self.span(*start, *end, true)?;
                Self::ordered(
                    self.semantics.karaoke.as_slice().last().map(|x| &x.span),
                    &span,
                )?;
                if *end_tick < self.next_tick {
                    return Err(Error::Invalid("unordered tx3g karaoke times"));
                }
                self.time_bound(*end_tick)?;
                let possible_end = (*end_tick > self.next_tick).then_some(span.characters.end);
                let event = KaraokeEvent {
                    source: source.clone(),
                    span,
                    start_tick: self.next_tick,
                    end_tick: *end_tick,
                    start_time: self.relative(self.next_tick)?,
                    end_time: self.relative(*end_tick)?,
                };
                self.semantics
                    .karaoke
                    .try_push(event)
                    .map_err(|e| e.error)?;
                if let Some(end) = possible_end {
                    let possible = &mut self.semantics.possible_karaoke_end;
                    *possible = Some(possible.map_or(end, |old| old.max(end)));
                }
                self.next_tick = *end_tick;
            }
            Item::HighlightColour(source, rgba) => unique!(highlight_colour, *rgba, source),
            Item::ScrollDelay(source, ticks) => {
                self.time_bound(*ticks)?;
                unique!(scroll_delay, *ticks, source);
            }
            Item::TextBox(source, rect) => unique!(text_box, *rect, source),
            Item::Wrap(source, mode) => unique!(wrap, *mode, source),
            Item::Disparity(source, shift) => unique!(disparity, *shift, source),
        }
        self.records = self.records.checked_add(1).ok_or(Error::Overflow)?;
        Ok(())
    }

    /// Enters `font` in the font table once its unit is paid; `false` when
    /// `work` runs out first. A refused allocation leaves the font to commit
    /// again.
    fn commit_font(
        &mut self,
        font: &SemanticFont,
        paid: &mut bool,
        work: &mut WorkBudget,
    ) -> Result<bool> {
        if !*paid {
            if !work.try_charge(1) {
                return Ok(false);
            }
            *paid = true;
        }
        let semantics = &mut self.semantics;
        if semantics.font_index.get(font.id).is_some() {
            return Err(Error::Invalid("duplicate tx3g font identifier"));
        }
        let page = semantics.font_index.page(font.id)?;
        let entry = u16::try_from(
            semantics
                .fonts
                .len()
                .checked_add(1)
                .ok_or(Error::Overflow)?,
        )
        .map_err(|_| Error::Overflow)?;
        semantics
            .fonts
            .try_push(font.clone())
            .map_err(|e| e.error)?;
        semantics.font_index.set(page, font.id, entry);
        Ok(true)
    }

    fn finish_globals(&mut self) {
        let semantics = &mut self.semantics;
        let globals = &mut semantics.globals;
        globals.resolved_text_box = globals
            .text_box
            .as_ref()
            .map(|x| DerivedValue {
                value: x.value,
                origin: ValueOrigin::Sample,
            })
            .or_else(|| {
                semantics.description.as_ref().map(|x| DerivedValue {
                    value: x.defaults.text_box,
                    origin: ValueOrigin::Description,
                })
            });
        globals.resolved_disparity = globals
            .disparity
            .as_ref()
            .map(|x| DerivedValue {
                value: x.value,
                origin: ValueOrigin::Sample,
            })
            .or(globals.description_disparity.map(|x| DerivedValue {
                value: x,
                origin: ValueOrigin::Description,
            }))
            .or_else(|| {
                semantics
                    .context
                    .description_parsed
                    .then_some(DerivedValue {
                        value: 0,
                        origin: ValueOrigin::SpecifiedAbsentDefault,
                    })
            });
    }
}

impl Conflicts {
    /// Checks the next highlight or link against the next karaoke entry: a
    /// static range that a positive-duration entry, or the continuous
    /// karaoke prefix, overlaps is an error.
    fn step(&mut self, found: &mut Found, work: &mut WorkBudget) -> Result<Scan> {
        let semantics = &mut found.semantics;
        let ranges = match self.checked {
            Checked::Highlights => semantics
                .highlights
                .as_slice()
                .get(self.at)
                .map(|x| &x.span),
            Checked::Links => semantics
                .hyperlinks
                .as_slice()
                .get(self.at)
                .map(|x| &x.span),
        };
        let Some(range) = ranges.map(|span| span.characters.clone()) else {
            if self.checked == Checked::Links {
                return Ok(Scan::Done);
            }
            *self = Self {
                checked: Checked::Links,
                at: 0,
                event: 0,
            };
            return Ok(Scan::Stepped);
        };
        if !work.try_charge(1) {
            return Ok(Scan::Pending);
        }
        if range.is_empty() {
            self.at += 1;
            return Ok(Scan::Stepped);
        }
        let continuous = semantics.globals.display.map(|x| x.continuous_karaoke);
        if continuous != Some(false) && range.start < semantics.possible_karaoke_end.unwrap_or(0) {
            if continuous == Some(true) {
                return Err(Error::Invalid(
                    "tx3g dynamic highlight conflicts with static highlight or link",
                ));
            }
            semantics.context.conditional_combinations_checked = false;
        }
        let Some(event) = semantics.karaoke.as_slice().get(self.event) else {
            self.at += 1;
            return Ok(Scan::Stepped);
        };
        // Every record has already passed source range/time ordering checks.
        // An empty time interval never applies a cross-kind effect; retain the
        // record and advance the charged scan without inventing a clock.
        if event.start_tick == event.end_tick {
            self.event += 1;
            return Ok(Scan::Stepped);
        }
        let other = &event.span.characters;
        if !other.is_empty() && range.start < other.end && other.start < range.end {
            return Err(Error::Invalid(
                "tx3g dynamic highlight conflicts with static highlight or link",
            ));
        }
        if other.end <= range.start || other.is_empty() {
            self.event += 1;
        } else {
            self.at += 1;
        }
        Ok(Scan::Stepped)
    }
}

impl Runs {
    /// Cuts the next run at the first record boundary after its start,
    /// passing the records that end before it one unit at a time.
    fn prepare(&mut self, found: &Found, work: &mut WorkBudget) -> Result<Prepared> {
        let semantics = &found.semantics;
        let position = self.position;
        let mut end = semantics.characters;
        let mut active = [None; 4];
        for (kind, slot) in active.iter_mut().enumerate() {
            let at = self.sweep[kind];
            let span = match kind {
                0 => semantics.styles.as_slice().get(at).map(|x| &x.span),
                1 => semantics.highlights.as_slice().get(at).map(|x| &x.span),
                2 => semantics.hyperlinks.as_slice().get(at).map(|x| &x.span),
                _ => semantics.blink.as_slice().get(at).map(|x| &x.span),
            };
            let Some(range) = span.map(|span| &span.characters) else {
                continue;
            };
            if range.is_empty() || range.end <= position {
                if !work.try_charge(1) {
                    return Ok(Prepared::Pending);
                }
                self.sweep[kind] += 1;
                return Ok(Prepared::Swept);
            }
            if range.start <= position {
                end = end.min(range.end);
                *slot = Some(at);
            } else {
                end = end.min(range.start);
            }
        }
        if !work.try_charge(1) {
            return Ok(Prepared::Pending);
        }
        let (style, style_origin) = match active[0] {
            Some(index) => (
                Some(semantics.styles.as_slice()[index].style),
                Some(StyleOrigin::Sample(index)),
            ),
            None => (
                semantics
                    .description
                    .as_ref()
                    .map(|d| d.defaults.style.into()),
                semantics
                    .description
                    .as_ref()
                    .map(|_| StyleOrigin::Description),
            ),
        };
        let font_index = style
            .map(|s| found.reference_font(s.font_id))
            .transpose()?
            .flatten();
        Ok(Prepared::Run(ResolvedRun {
            characters: position..end,
            source_bytes: self.source..self.source,
            utf8_bytes: self.utf8..self.utf8,
            style,
            style_origin,
            font_index,
            highlight: active[1],
            hyperlink: active[2],
            blink: active[3],
        }))
    }
}
