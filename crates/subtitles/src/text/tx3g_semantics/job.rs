use super::super::{
    Encoding, Event, Extractor, Format, Input, Status,
    charset::{Charset, CharsetJob, CharsetStatus},
    tx3g_config::{ConfigJob, ConfigLayout, ConfigStatus},
    unicode::{Cursor, Poll, cancelled},
};
use super::parse::{Item, Reader};
use super::*;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error, Result, WorkBudget};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Source,
    Config,
    Fonts,
    Count,
    DescriptionBoxes,
    Boxes,
    Conflicts,
    Runs,
    RunText,
    RunCommit,
    Ready,
    Taken,
}

/// Composes the existing source/config parsers internally. Public Cue or
/// Configuration values are never accepted as proof of validated wire facts.
#[derive(Debug)]
pub struct SemanticJob {
    pool: Budget,
    limits: SemanticLimits,
    extractor: Option<Extractor>,
    config_job: Option<ConfigJob>,
    cue: Option<Cue>,
    description: Option<Configuration>,
    fonts: BudgetVec<SemanticFont>,
    font_index: FontIndex,
    font_at: usize,
    font_job: Option<CharsetJob>,
    font_pending: Option<SemanticFont>,
    cursor: Option<Cursor>,
    characters: usize,
    description_box: usize,
    box_at: usize,
    reader: Option<Reader>,
    pending: Option<Item>,
    paid: bool,
    records: usize,
    styles: BudgetVec<StyleSpan>,
    highlights: BudgetVec<SpanEffect>,
    links: BudgetVec<Hyperlink>,
    blink: BudgetVec<SpanEffect>,
    karaoke_header: Option<KaraokeHeader>,
    karaoke: BudgetVec<KaraokeEvent>,
    possible_karaoke_end: Option<usize>,
    next_tick: u32,
    globals: GlobalEffects,
    context: ValidationContext,
    conflict_kind: usize,
    conflict_at: usize,
    conflict_event: usize,
    runs: BudgetVec<ResolvedRun>,
    sweep: [usize; 4],
    position: usize,
    run_pending: Option<ResolvedRun>,
    map_character: usize,
    map_source: usize,
    map_utf8: usize,
    phase: Phase,
    failed: Option<Error>,
}
impl SemanticJob {
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
        let config_job = match layout {
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
        Ok(Self {
            pool: pool.clone(),
            limits,
            extractor: Some(extractor),
            config_job,
            cue: None,
            description: None,
            fonts: BudgetVec::new(pool),
            font_index: FontIndex::new(pool),
            font_at: 0,
            font_job: None,
            font_pending: None,
            cursor: None,
            characters: 0,
            description_box: 0,
            box_at: 0,
            reader: None,
            pending: None,
            paid: false,
            records: 0,
            styles: BudgetVec::new(pool),
            highlights: BudgetVec::new(pool),
            links: BudgetVec::new(pool),
            blink: BudgetVec::new(pool),
            karaoke_header: None,
            karaoke: BudgetVec::new(pool),
            possible_karaoke_end: None,
            next_tick: 0,
            globals: GlobalEffects::default(),
            context: ValidationContext {
                description_parsed: false,
                description_bytes_absent: input.codec_private.is_none(),
                explicit_track_timescale: track_timescale,
                sample_duration_known: input.timing.duration().is_some(),
                duration_bounds_checked: track_timescale.is_some()
                    && input.timing.duration().is_some(),
                conditional_combinations_checked: true,
            },
            conflict_kind: 0,
            conflict_at: 0,
            conflict_event: 0,
            runs: BudgetVec::new(pool),
            sweep: [0; 4],
            position: 0,
            run_pending: None,
            map_character: 0,
            map_source: 0,
            map_utf8: 0,
            phase: Phase::Source,
            failed: None,
        })
    }
    fn cue(&self) -> Result<&Cue> {
        self.cue
            .as_ref()
            .ok_or(Error::Invalid("tx3g semantic source absent"))
    }
    fn text_cursor(&self) -> Result<Cursor> {
        let cue = self.cue()?;
        let mut range = cue.text_range.clone();
        if cue.effective_encoding == Encoding::Utf16Be {
            range.start = range.start.checked_add(2).ok_or(Error::Overflow)?;
        }
        Ok(Cursor::new(range, cue.effective_encoding))
    }
    fn span(&self, start: u16, end: u16, sentinel: bool) -> Result<CharacterSpan> {
        let start = usize::from(start);
        let raw_end = usize::from(end);
        let special =
            sentinel && raw_end == self.characters.checked_add(1).ok_or(Error::Overflow)?;
        if start > raw_end || start > self.characters || (raw_end > self.characters && !special) {
            return Err(Error::Invalid("tx3g character range"));
        }
        Ok(CharacterSpan {
            declared_start: u16::try_from(start).map_err(|_| Error::Overflow)?,
            declared_end: end,
            characters: start..if special { self.characters } else { raw_end },
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
        self.context
            .explicit_track_timescale
            .map(|scale| Time::new(i128::from(ticks), u128::from(scale)))
            .transpose()
    }
    fn time_bound(&self, ticks: u32) -> Result<()> {
        if let (Some(relative), Some(duration)) = (self.relative(ticks)?, self.cue()?.duration)
            && relative > duration
        {
            return Err(Error::Invalid("tx3g modifier time exceeds sample duration"));
        }
        Ok(())
    }
    fn reference_font(&self, id: u16) -> Result<Option<usize>> {
        let index = self.font_index.get(id);
        if self.description.is_some() && index.is_none() {
            return Err(Error::Invalid(
                "tx3g font reference absent from description",
            ));
        }
        Ok(index)
    }
    fn commit(&mut self, item: &Item) -> Result<()> {
        macro_rules! unique {
            ($field:ident,$value:expr,$source:expr) => {{
                if self.globals.$field.is_some() {
                    return Err(Error::Invalid("duplicate global tx3g modifier"));
                }
                self.globals.$field = Some(SampleValue {
                    value: $value,
                    source: $source.clone(),
                });
            }};
        }
        match item {
            Item::Style(source, record) => {
                let span = self.span(record.start, record.end, false)?;
                Self::ordered(self.styles.as_slice().last().map(|x| &x.span), &span)?;
                self.reference_font(record.font_id)?;
                self.styles
                    .try_push(StyleSpan {
                        source: source.clone(),
                        span,
                        style: (*record).into(),
                    })
                    .map_err(|e| e.error)?;
            }
            Item::Highlight(source, start, end) => {
                let span = self.span(*start, *end, true)?;
                Self::ordered(self.highlights.as_slice().last().map(|x| &x.span), &span)?;
                self.highlights
                    .try_push(SpanEffect {
                        source: source.clone(),
                        span,
                    })
                    .map_err(|e| e.error)?;
            }
            Item::Blink(source, start, end) => {
                let span = self.span(*start, *end, false)?;
                Self::ordered(self.blink.as_slice().last().map(|x| &x.span), &span)?;
                self.blink
                    .try_push(SpanEffect {
                        source: source.clone(),
                        span,
                    })
                    .map_err(|e| e.error)?;
            }
            Item::Hyperlink(source, start, end, url, alternate) => {
                let span = self.span(*start, *end, false)?;
                Self::ordered(self.links.as_slice().last().map(|x| &x.span), &span)?;
                self.links
                    .try_push(Hyperlink {
                        source: source.clone(),
                        span,
                        url: url.clone(),
                        alternate: alternate.clone(),
                    })
                    .map_err(|e| e.error)?;
            }
            Item::KaraokeHeader(source, start_tick, entries) => {
                if self.karaoke_header.is_some() {
                    return Err(Error::Invalid("duplicate tx3g karaoke box"));
                }
                self.time_bound(*start_tick)?;
                self.karaoke_header = Some(KaraokeHeader {
                    source: source.clone(),
                    start_tick: *start_tick,
                    entries: *entries,
                });
                self.next_tick = *start_tick;
            }
            Item::Karaoke(source, end_tick, start, end) => {
                let span = self.span(*start, *end, true)?;
                Self::ordered(self.karaoke.as_slice().last().map(|x| &x.span), &span)?;
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
                self.karaoke.try_push(event).map_err(|e| e.error)?;
                if let Some(end) = possible_end {
                    self.possible_karaoke_end =
                        Some(self.possible_karaoke_end.map_or(end, |old| old.max(end)));
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
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<SemanticStatus> {
        cancelled(cancel)?;
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.inner(work, cancel);
        if matches!(result, Err(Error::Backpressure { .. })) {
            return Ok(SemanticStatus::NeedOutput);
        }
        if let Err(error) = result
            && !matches!(error, Error::Cancelled | Error::Backpressure { .. })
        {
            self.extractor = None;
            self.config_job = None;
            self.cue = None;
            self.description = None;
            self.font_job = None;
            self.font_pending = None;
            self.fonts = BudgetVec::new(&self.pool);
            self.font_index = FontIndex::new(&self.pool);
            self.styles = BudgetVec::new(&self.pool);
            self.highlights = BudgetVec::new(&self.pool);
            self.links = BudgetVec::new(&self.pool);
            self.blink = BudgetVec::new(&self.pool);
            self.karaoke = BudgetVec::new(&self.pool);
            self.runs = BudgetVec::new(&self.pool);
            self.pending = None;
            self.run_pending = None;
            self.cursor = None;
            self.reader = None;
            self.failed = Some(error);
        }
        result
    }
    fn inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<SemanticStatus> {
        loop {
            cancelled(cancel)?;
            if self.phase == Phase::Ready {
                return Ok(SemanticStatus::Ready);
            }
            if self.phase == Phase::Taken {
                return Err(Error::Invalid("tx3g semantics already taken"));
            }
            if work.remaining() == 0 {
                return Ok(SemanticStatus::Pending);
            }
            match self.phase {
                Phase::Source => match self
                    .extractor
                    .as_mut()
                    .ok_or(Error::Invalid("tx3g source parser absent"))?
                    .step(work, cancel)?
                {
                    Status::Yield => return Ok(SemanticStatus::Pending),
                    Status::NeedOutput => return Ok(SemanticStatus::NeedOutput),
                    Status::Event(Event::Cue(cue)) => {
                        self.cue = Some(cue);
                        self.extractor = None;
                        self.phase = Phase::Config;
                    }
                    _ => return Err(Error::Invalid("unexpected tx3g source state")),
                },
                Phase::Config => {
                    if let Some(job) = self.config_job.as_mut() {
                        if job.poll(work, cancel)? == ConfigStatus::Pending {
                            return Ok(SemanticStatus::Pending);
                        }
                        self.description = Some(job.take_output(cancel)?);
                        self.context.description_parsed = true;
                        self.config_job = None;
                    }
                    if let Some(description) = self.description.as_ref() {
                        self.globals.display =
                            Some(DisplayIntent::new(description.defaults.display_flags));
                    }
                    self.phase = Phase::Fonts;
                }
                Phase::Fonts => {
                    let count = self.description.as_ref().map_or(0, |c| c.fonts.len());
                    if self.font_at == count {
                        if let Some(d) = self.description.as_ref() {
                            self.reference_font(d.defaults.style.font_id)?;
                        }
                        self.cursor = Some(self.text_cursor()?);
                        self.phase = Phase::Count;
                        continue;
                    }
                    if self.font_pending.is_some() {
                        self.commit_font(work)?;
                        continue;
                    }
                    if self.font_job.is_none() {
                        if !work.try_charge(1) {
                            return Ok(SemanticStatus::Pending);
                        }
                        let d = self
                            .description
                            .as_ref()
                            .ok_or(Error::Invalid("tx3g description absent"))?;
                        let record = &d.fonts.as_slice()[self.font_at];
                        let bytes = d.raw.slice(record.name_range.clone())?;
                        let encoding = if bytes.as_ref().starts_with(&[0xfe, 0xff]) {
                            Charset::Utf16Bom
                        } else {
                            Charset::Utf8
                        };
                        self.font_job = Some(CharsetJob::new(
                            &bytes,
                            None,
                            encoding,
                            &self.pool,
                            self.limits.text,
                        )?);
                    }
                    let job = self
                        .font_job
                        .as_mut()
                        .ok_or(Error::Invalid("tx3g font conversion absent"))?;
                    if job.poll(work, cancel)? == CharsetStatus::Pending {
                        return Ok(SemanticStatus::Pending);
                    }
                    let name = job.take_output(cancel)?;
                    let d = self
                        .description
                        .as_ref()
                        .ok_or(Error::Invalid("tx3g description absent"))?;
                    let record = &d.fonts.as_slice()[self.font_at];
                    self.font_pending = Some(SemanticFont {
                        id: record.id,
                        raw_name: record.name_range.clone(),
                        name_utf8: name.utf8,
                        utf16_bom: name.charset == Charset::Utf16Bom,
                    });
                    self.font_job = None;
                    self.paid = false;
                }
                Phase::Count => {
                    let bytes = self
                        .cue
                        .as_ref()
                        .ok_or(Error::Invalid("tx3g cue absent"))?
                        .input
                        .payload
                        .as_ref();
                    match self
                        .cursor
                        .as_mut()
                        .ok_or(Error::Invalid("tx3g text cursor absent"))?
                        .poll(bytes, work, cancel)?
                    {
                        Poll::Yield => return Ok(SemanticStatus::Pending),
                        Poll::Ready(_) => {
                            self.characters =
                                self.characters.checked_add(1).ok_or(Error::Overflow)?;
                        }
                        Poll::End => {
                            self.cursor = None;
                            self.phase = Phase::DescriptionBoxes;
                        }
                    }
                }
                Phase::DescriptionBoxes => {
                    let count = self.description.as_ref().map_or(0, |c| c.boxes.len());
                    if self.description_box == count {
                        self.phase = Phase::Boxes;
                        continue;
                    }
                    let d = self
                        .description
                        .as_ref()
                        .ok_or(Error::Invalid("tx3g description absent"))?;
                    if self.reader.is_none() {
                        if !work.try_charge(1) {
                            return Ok(SemanticStatus::Pending);
                        }
                        let modifier = &d.boxes.as_slice()[self.description_box];
                        if modifier.kind != *b"disp" {
                            self.description_box += 1;
                            continue;
                        }
                        self.reader =
                            Some(Reader::new(d.raw.as_ref(), modifier, self.description_box)?);
                    }
                    match self
                        .reader
                        .as_mut()
                        .ok_or(Error::Invalid("tx3g description modifier reader"))?
                        .poll(d.raw.as_ref(), work, cancel)?
                    {
                        Poll::Yield => return Ok(SemanticStatus::Pending),
                        Poll::Ready(Item::Disparity(_, value)) => {
                            if self.globals.description_disparity.is_some() {
                                return Err(Error::Invalid("duplicate tx3g description disparity"));
                            }
                            self.globals.description_disparity = Some(value);
                        }
                        Poll::Ready(_) => {
                            return Err(Error::Invalid("unexpected description semantic field"));
                        }
                        Poll::End => {
                            self.reader = None;
                            self.description_box += 1;
                        }
                    }
                }
                Phase::Boxes => {
                    if let Some(item) = self.pending.as_ref() {
                        if !self.paid {
                            if !work.try_charge(1) {
                                return Ok(SemanticStatus::Pending);
                            }
                            self.paid = true;
                        }
                        if self.records >= self.limits.maximum_records {
                            return Err(Error::Limit {
                                requested: self.records.checked_add(1).ok_or(Error::Overflow)?,
                                maximum: self.limits.maximum_records,
                            });
                        }
                        let item = item.clone();
                        self.commit(&item)?;
                        self.pending = None;
                        self.paid = false;
                        continue;
                    }
                    if self.box_at == self.cue()?.modifiers.len() {
                        self.finish_globals();
                        self.phase = Phase::Conflicts;
                        continue;
                    }
                    if self.reader.is_none() {
                        if !work.try_charge(1) {
                            return Ok(SemanticStatus::Pending);
                        }
                        let cue = self.cue()?;
                        self.reader = Some(Reader::new(
                            cue.input.payload.as_ref(),
                            &cue.modifiers.as_slice()[self.box_at],
                            self.box_at,
                        )?);
                    }
                    let bytes = self
                        .cue
                        .as_ref()
                        .ok_or(Error::Invalid("tx3g cue absent"))?
                        .input
                        .payload
                        .as_ref();
                    match self
                        .reader
                        .as_mut()
                        .ok_or(Error::Invalid("tx3g modifier reader absent"))?
                        .poll(bytes, work, cancel)?
                    {
                        Poll::Yield => return Ok(SemanticStatus::Pending),
                        Poll::Ready(item) => {
                            self.pending = Some(item);
                            self.paid = false;
                        }
                        Poll::End => {
                            self.reader = None;
                            self.box_at += 1;
                        }
                    }
                }
                Phase::Conflicts => {
                    if !self.check_conflict(work)? {
                        return Ok(SemanticStatus::Pending);
                    }
                    if self.conflict_kind >= 2 {
                        self.cursor = Some(self.text_cursor()?);
                        let mut start = self.cue()?.text_range.start;
                        if self.cue()?.effective_encoding == Encoding::Utf16Be {
                            start += 2;
                        }
                        self.map_source = start;
                        self.phase = Phase::Runs;
                    }
                }
                Phase::Runs => {
                    if self.position == self.characters {
                        self.phase = Phase::Ready;
                        continue;
                    }
                    if !self.prepare_run(work)? {
                        return Ok(SemanticStatus::Pending);
                    }
                    self.phase = Phase::RunText;
                }
                Phase::RunText => {
                    let end = self
                        .run_pending
                        .as_ref()
                        .ok_or(Error::Invalid("tx3g run absent"))?
                        .characters
                        .end;
                    if self.map_character == end {
                        let run = self
                            .run_pending
                            .as_mut()
                            .ok_or(Error::Invalid("tx3g run absent"))?;
                        run.source_bytes.end = self.map_source;
                        run.utf8_bytes.end = self.map_utf8;
                        self.phase = Phase::RunCommit;
                        self.paid = false;
                        continue;
                    }
                    let bytes = self
                        .cue
                        .as_ref()
                        .ok_or(Error::Invalid("tx3g cue absent"))?
                        .input
                        .payload
                        .as_ref();
                    match self
                        .cursor
                        .as_mut()
                        .ok_or(Error::Invalid("tx3g run cursor absent"))?
                        .poll(bytes, work, cancel)?
                    {
                        Poll::Yield => return Ok(SemanticStatus::Pending),
                        Poll::End => return Err(Error::Truncated),
                        Poll::Ready(scalar) => {
                            self.map_character += 1;
                            self.map_source = scalar.end;
                            self.map_utf8 = scalar.decoded_end;
                        }
                    }
                }
                Phase::RunCommit => {
                    if !self.paid {
                        if !work.try_charge(1) {
                            return Ok(SemanticStatus::Pending);
                        }
                        self.paid = true;
                    }
                    if self.runs.len() >= self.limits.maximum_runs {
                        return Err(Error::Limit {
                            requested: self.runs.len() + 1,
                            maximum: self.limits.maximum_runs,
                        });
                    }
                    let run = self
                        .run_pending
                        .as_ref()
                        .ok_or(Error::Invalid("tx3g run absent"))?
                        .clone();
                    let end = run.characters.end;
                    self.runs.try_push(run).map_err(|e| e.error)?;
                    self.run_pending = None;
                    self.position = end;
                    self.phase = Phase::Runs;
                    self.paid = false;
                }
                Phase::Ready | Phase::Taken => unreachable!(),
            }
        }
    }
    fn commit_font(&mut self, work: &mut WorkBudget) -> Result<()> {
        if !self.paid {
            if !work.try_charge(1) {
                return Ok(());
            }
            self.paid = true;
        }
        let font = self
            .font_pending
            .as_ref()
            .ok_or(Error::Invalid("tx3g font pending absent"))?;
        if self.font_index.get(font.id).is_some() {
            return Err(Error::Invalid("duplicate tx3g font identifier"));
        }
        let high = usize::from(font.id >> 8);
        let page = if let Some(page) = self.font_index.mapping[high] {
            usize::from(page)
        } else {
            let index = self.font_index.pages.len();
            let value = u8::try_from(index).map_err(|_| Error::Overflow)?;
            self.font_index
                .pages
                .try_push(FontPage { entries: [0; 256] })
                .map_err(|e| e.error)?;
            self.font_index.mapping[high] = Some(value);
            index
        };
        let encoded = u16::try_from(self.fonts.len().checked_add(1).ok_or(Error::Overflow)?)
            .map_err(|_| Error::Overflow)?;
        // Bounded checked growth from actual demand, preserving the pending
        // owned font and its decoded storage on pressure.
        let font = self
            .font_pending
            .take()
            .ok_or(Error::Invalid("tx3g font pending absent"))?;
        let low = usize::from(font.id & 255);
        if let Err(error) = self.fonts.try_push(font) {
            self.font_pending = Some(error.value);
            return Err(error.error);
        }
        self.font_index.pages.as_mut_slice()[page].entries[low] = encoded;
        self.font_at += 1;
        self.paid = false;
        Ok(())
    }
    fn finish_globals(&mut self) {
        self.globals.resolved_text_box = self
            .globals
            .text_box
            .as_ref()
            .map(|x| DerivedValue {
                value: x.value,
                origin: ValueOrigin::Sample,
            })
            .or_else(|| {
                self.description.as_ref().map(|x| DerivedValue {
                    value: x.defaults.text_box,
                    origin: ValueOrigin::Description,
                })
            });
        self.globals.resolved_disparity = self
            .globals
            .disparity
            .as_ref()
            .map(|x| DerivedValue {
                value: x.value,
                origin: ValueOrigin::Sample,
            })
            .or(self.globals.description_disparity.map(|x| DerivedValue {
                value: x,
                origin: ValueOrigin::Description,
            }))
            .or_else(|| {
                self.context.description_parsed.then_some(DerivedValue {
                    value: 0,
                    origin: ValueOrigin::SpecifiedAbsentDefault,
                })
            });
    }
    fn check_conflict(&mut self, work: &mut WorkBudget) -> Result<bool> {
        if self.conflict_kind >= 2 {
            return Ok(true);
        }
        let range = if self.conflict_kind == 0 {
            self.highlights
                .as_slice()
                .get(self.conflict_at)
                .map(|x| x.span.characters.clone())
        } else {
            self.links
                .as_slice()
                .get(self.conflict_at)
                .map(|x| x.span.characters.clone())
        };
        let Some(range) = range else {
            self.conflict_kind += 1;
            self.conflict_at = 0;
            self.conflict_event = 0;
            return Ok(true);
        };
        if !work.try_charge(1) {
            return Ok(false);
        }
        if range.is_empty() {
            self.conflict_at += 1;
            return Ok(true);
        }
        let continuous = self.globals.display.map(|x| x.continuous_karaoke);
        if continuous != Some(false) {
            let end = self.possible_karaoke_end.unwrap_or(0);
            if range.start < end {
                if continuous == Some(true) {
                    return Err(Error::Invalid(
                        "tx3g dynamic highlight conflicts with static highlight or link",
                    ));
                }
                self.context.conditional_combinations_checked = false;
            }
        }
        let Some(event) = self.karaoke.as_slice().get(self.conflict_event) else {
            self.conflict_at += 1;
            return Ok(true);
        };
        // Every record has already passed source range/time ordering checks.
        // An empty time interval never applies a cross-kind effect; retain the
        // record and advance the charged scan without inventing a clock.
        if event.start_tick == event.end_tick {
            self.conflict_event += 1;
            return Ok(true);
        }
        let other = &event.span.characters;
        if !other.is_empty() && range.start < other.end && other.start < range.end {
            return Err(Error::Invalid(
                "tx3g dynamic highlight conflicts with static highlight or link",
            ));
        }
        if other.end <= range.start || other.is_empty() {
            self.conflict_event += 1;
        } else {
            self.conflict_at += 1;
        }
        Ok(true)
    }
    fn prepare_run(&mut self, work: &mut WorkBudget) -> Result<bool> {
        let position = self.position;
        let mut end = self.characters;
        let mut active = [None; 4];
        for (kind, slot) in active.iter_mut().enumerate() {
            let range = match kind {
                0 => self
                    .styles
                    .as_slice()
                    .get(self.sweep[kind])
                    .map(|x| &x.span.characters),
                1 => self
                    .highlights
                    .as_slice()
                    .get(self.sweep[kind])
                    .map(|x| &x.span.characters),
                2 => self
                    .links
                    .as_slice()
                    .get(self.sweep[kind])
                    .map(|x| &x.span.characters),
                _ => self
                    .blink
                    .as_slice()
                    .get(self.sweep[kind])
                    .map(|x| &x.span.characters),
            };
            if let Some(range) = range {
                if range.is_empty() || range.end <= position {
                    if !work.try_charge(1) {
                        return Ok(false);
                    }
                    self.sweep[kind] += 1;
                    return Ok(false);
                }
                if range.start <= position {
                    end = end.min(range.end);
                    *slot = Some(self.sweep[kind]);
                } else {
                    end = end.min(range.start);
                }
            }
        }
        if !work.try_charge(1) {
            return Ok(false);
        }
        let (style, origin) = if let Some(index) = active[0] {
            (
                Some(self.styles.as_slice()[index].style),
                Some(StyleOrigin::Sample(index)),
            )
        } else {
            (
                self.description.as_ref().map(|d| d.defaults.style.into()),
                self.description.as_ref().map(|_| StyleOrigin::Description),
            )
        };
        let font = style
            .map(|s| self.reference_font(s.font_id))
            .transpose()?
            .flatten();
        self.run_pending = Some(ResolvedRun {
            characters: position..end,
            source_bytes: self.map_source..self.map_source,
            utf8_bytes: self.map_utf8..self.map_utf8,
            style,
            style_origin: origin,
            font_index: font,
            highlight: active[1],
            hyperlink: active[2],
            blink: active[3],
        });
        Ok(true)
    }
    pub fn take_output(&mut self, cancel: &AtomicBool) -> Result<Semantics> {
        cancelled(cancel)?;
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.phase != Phase::Ready {
            return Err(Error::Invalid("tx3g semantics not ready"));
        }
        let out = Semantics {
            source: self
                .cue
                .take()
                .ok_or(Error::Invalid("tx3g source absent"))?,
            description: self.description.take(),
            fonts: std::mem::replace(&mut self.fonts, BudgetVec::new(&self.pool)),
            font_index: std::mem::replace(&mut self.font_index, FontIndex::new(&self.pool)),
            styles: std::mem::replace(&mut self.styles, BudgetVec::new(&self.pool)),
            highlights: std::mem::replace(&mut self.highlights, BudgetVec::new(&self.pool)),
            hyperlinks: std::mem::replace(&mut self.links, BudgetVec::new(&self.pool)),
            blink: std::mem::replace(&mut self.blink, BudgetVec::new(&self.pool)),
            karaoke_header: self.karaoke_header.take(),
            karaoke: std::mem::replace(&mut self.karaoke, BudgetVec::new(&self.pool)),
            possible_karaoke_end: self.possible_karaoke_end,
            globals: std::mem::take(&mut self.globals),
            runs: std::mem::replace(&mut self.runs, BudgetVec::new(&self.pool)),
            characters: self.characters,
            context: self.context,
        };
        self.cursor = None;
        self.phase = Phase::Taken;
        Ok(out)
    }
}
