//! Separate, pin-bound regular encoded edit admission. Source samples, edits,
//! decoder configuration and clocks remain unchanged. No decoder/AU claim.
//! Admission scans real metadata once before any occurrence is published.
use crate::{
    Access, BoxView, ClockField, CursorStatus, EditEntry, EditList, EncodedEntryKind,
    EncodedStream, PacketView, Sample, SampleCursor, SampleStep, SampleTable,
};
use std::sync::{Arc, atomic::AtomicBool};
use triergon_core::{
    Budget, BudgetVec, Error, InputCursor, InputToken, Reservation, Result, Rounding, SourceId,
    Time, TimeBase, WorkBudget,
};
use triergon_framing::{
    Ac3ConfigKind, Ac3ConfigLimits, Ac3ConfigParser, Ac3Configuration, BuildStep, OPUS_RATE,
};

pub(crate) mod fragment_aac;

// ETSI TS 102 366 Annex F.2: an AC-3 or E-AC-3 ISO media sample covers six blocks.
const DOLBY_SAMPLE_TICKS: u32 = 1536;

#[derive(Clone, Copy, Debug)]
pub struct EncodedEditProfile;
#[derive(Clone, Copy, Debug)]
pub struct EncodedEditLimits {
    pub max_samples: u64,
}
impl Default for EncodedEditLimits {
    fn default() -> Self {
        Self {
            max_samples: 1_000_000,
        }
    }
}
#[derive(Debug)]
pub struct EncodedEditContext<'a> {
    pub table: &'a SampleTable,
    pub edits: &'a EditList,
    pub stream: &'a EncodedStream,
    pub media_base: TimeBase,
    pub movie_base: TimeBase,
}
#[derive(Clone, Copy, Debug)]
enum EditTiming {
    Video,
    FixedAudio(u32),
    Opus,
}

#[derive(Debug)]
struct OpusDurationRun {
    end_sample: u64,
    end_tick: i128,
    ticks: u32,
}

#[derive(Debug)]
struct OpusTiming {
    first: u64,
    first_tick: i128,
    durations: BudgetVec<OpusDurationRun>,
    _charge: Reservation,
}

impl OpusTiming {
    fn duration(&self, ordinal: u64) -> Result<u32> {
        let occurrence = ordinal - self.first;
        let runs = self.durations.as_slice();
        runs.get(runs.partition_point(|run| run.end_sample <= occurrence))
            .map(|run| run.ticks)
            .ok_or(Error::Invalid("missing Opus occurrence duration"))
    }

    fn has_decode_tick(&self, tick: i128) -> bool {
        let runs = self.durations.as_slice();
        let row = runs.partition_point(|run| run.end_tick <= tick);
        let start = if row == 0 { 0 } else { runs[row - 1].end_tick };
        runs.get(row)
            .is_some_and(|run| (tick - start) % i128::from(run.ticks) == 0)
    }
}

#[derive(Clone, Debug)]
pub struct EncodedEditPlan {
    source_table: BoxView,
    source_edit: BoxView,
    source_config: BoxView,
    entry: EditEntry,
    edit_ordinal: u32,
    media_base: TimeBase,
    movie_base: TimeBase,
    anchor: i128,
    end: i128,
    source_end: i128,
    presentation_end: i128,
    count: u64,
    timing: EditTiming,
    opus_timing: Option<Arc<OpusTiming>>,
    dts_scale: Option<u32>,
    dependency_table: bool,
    dependency_recovery: bool,
    recovery_facts: Option<crate::RecoveryFacts>,
    recovery_observation: Option<crate::RecoveryObservation>,
}
impl EncodedEditPlan {
    pub fn recovery_observation(&self) -> Option<&crate::RecoveryObservation> {
        self.recovery_observation.as_ref()
    }

    pub fn recovery_facts(&self) -> Option<&crate::RecoveryFacts> {
        self.recovery_facts.as_ref()
    }
    pub(crate) fn has_dependency_view(&self) -> bool {
        self.dependency_table
    }
    pub fn source_table(&self) -> &BoxView {
        &self.source_table
    }
    pub fn source_edit(&self) -> &BoxView {
        &self.source_edit
    }
    pub fn source_configuration(&self) -> &BoxView {
        &self.source_config
    }
    pub fn source_entry(&self) -> EditEntry {
        self.entry
    }
    pub(crate) fn edit_ordinal(&self) -> u32 {
        self.edit_ordinal
    }
    pub fn anchor_ticks(&self) -> i128 {
        self.anchor
    }
    pub fn selected_end_ticks(&self) -> i128 {
        self.end
    }
    pub(crate) fn align_seek_timestamp(&self, timestamp: i64, base: TimeBase) -> Result<i64> {
        let Some(opus) = &self.opus_timing else {
            return Ok(timestamp);
        };
        // MOV keeps an exact secondary seek hit. Between samples it
        // searches 10 ms earlier. Source roll recovery is a separate policy.
        let source_tick = i128::from(timestamp)
            .checked_add(self.anchor)
            .ok_or(Error::Overflow)?;
        if source_tick >= opus.first_tick
            && source_tick < self.end.min(self.source_end)
            && opus.has_decode_tick(source_tick)
        {
            return Ok(timestamp);
        }
        let lead = Time::new(1, 100)?.to_i64_ticks(base, Rounding::NearestAway)?;
        timestamp.checked_sub(lead).ok_or(Error::Overflow)
    }
    pub(crate) fn matches(
        &self,
        table: &SampleTable,
        edits: &EditList,
        media_base: TimeBase,
        movie_base: TimeBase,
    ) -> bool {
        self.media_base == media_base
            && self.movie_base == movie_base
            && self.source_table.header() == table.raw().header()
            && self.source_edit.header() == edits.raw().header()
            && self.count == table.sample_count()
            && self.source_table.bytes().as_ref().as_ptr() == table.raw().bytes().as_ref().as_ptr()
            && self.source_table.bytes().len() == table.raw().bytes().len()
            && self.source_edit.bytes().as_ref().as_ptr() == edits.raw().bytes().as_ref().as_ptr()
            && self.source_edit.bytes().len() == edits.raw().bytes().len()
    }
    pub(crate) fn project(&self, source: &Sample) -> Result<Option<PacketView>> {
        if source.ordinal == 0 || source.ordinal > self.count {
            return Err(Error::Invalid("encoded edit source ordinal"));
        }
        let dts = source
            .dts_ticks
            .ok_or(Error::Unsupported("encoded edit source DTS unknown"))?;
        let cts = source
            .cts_ticks
            .ok_or(Error::Unsupported("encoded edit source CTS unknown"))?;
        let audio = !matches!(self.timing, EditTiming::Video);
        if audio && dts >= self.end
            || self
                .opus_timing
                .as_ref()
                .is_some_and(|opus| source.ordinal < opus.first)
        {
            return Ok(None);
        }
        let dts_field =
            ClockField::from_exact_ticks(dts.checked_sub(self.anchor).ok_or(Error::Overflow)?)?;
        let pts_field =
            ClockField::from_exact_ticks(cts.checked_sub(self.anchor).ok_or(Error::Overflow)?)?;
        let source_end = dts
            .checked_add(i128::from(source.duration_ticks))
            .ok_or(Error::Overflow)?;
        let discard = if audio {
            source_end <= i128::from(self.entry.media_time)
        } else {
            cts < self.anchor || cts >= self.presentation_end
        };
        let duration = match &self.opus_timing {
            Some(opus) if opus.first > 1 => opus.duration(source.ordinal)?,
            _ => source.duration_ticks,
        };
        Ok(Some(PacketView {
            source_ordinal: source.ordinal,
            source_description: source.description_index,
            source_range: source.range.clone(),
            pts: pts_field.api_ticks(),
            dts: dts_field.api_ticks(),
            pts_field,
            dts_field,
            duration: i64::from(duration),
            flags: u32::from(source.is_container_sync())
                | if discard { 4 } else { 0 }
                | if self.dependency_table
                    && source.dependency.is_some_and(|raw| {
                        if self.dependency_recovery {
                            crate::DependencyObservation::from_source(
                                raw,
                                crate::EncodedRecoveryProfile,
                            )
                            .demux_disposable_hint()
                        } else {
                            crate::DependencyView::from_source(raw).demux_disposable_hint()
                        }
                    })
                {
                    16
                } else {
                    0
                },
        }))
    }
    /// Demux skip-samples side data for a sample from this plan's source track.
    /// The first two little-endian words give leading and trailing sample counts.
    /// DTS counts use the sample-entry clock. The MOV Opus projection reports
    /// leading trim in media ticks and trailing trim in 48 kHz samples.
    pub fn skip_samples(&self, source: &Sample) -> Result<Option<[u8; 10]>> {
        let frame = match self.timing {
            EditTiming::Video => return Ok(None),
            EditTiming::FixedAudio(frame) => frame,
            EditTiming::Opus => return self.opus_skip_samples(source),
        };
        let dts = source
            .dts_ticks
            .ok_or(Error::Unsupported("encoded edit source DTS unknown"))?;
        if dts >= self.end {
            return Ok(None);
        }
        let start = if source.ordinal == 1 {
            u32::try_from(self.entry.media_time).map_err(|_| Error::Overflow)?
        } else {
            0
        };
        let next = dts
            .checked_add(i128::from(source.duration_ticks))
            .ok_or(Error::Overflow)?;
        let terminal = source.ordinal == self.count || next >= self.end;
        let end = if terminal {
            let desired = self.end.min(self.source_end);
            // stts can shorten the last DTS packet without changing the
            // number of samples it decodes. Measure trim from the coded size.
            let nominal = dts.checked_add(i128::from(frame)).ok_or(Error::Overflow)?;
            u32::try_from(nominal.checked_sub(desired).ok_or(Error::Overflow)?.max(0))
                .map_err(|_| Error::Overflow)?
        } else {
            0
        };
        let scale = self.dts_scale.unwrap_or(1);
        let start = start.div_ceil(scale);
        let end = end / scale;
        Ok(skip_side_data(start, end))
    }

    fn opus_skip_samples(&self, source: &Sample) -> Result<Option<[u8; 10]>> {
        let first = self.opus_timing.as_ref().map_or(1, |opus| opus.first);
        let dts = source
            .dts_ticks
            .ok_or(Error::Unsupported("encoded edit source DTS unknown"))?;
        if source.ordinal < first || dts >= self.end {
            return Ok(None);
        }
        let start = if source.ordinal == first {
            u32::try_from(self.anchor.checked_sub(dts).ok_or(Error::Overflow)?)
                .map_err(|_| Error::Overflow)?
        } else {
            0
        };
        let next = dts
            .checked_add(i128::from(source.duration_ticks))
            .ok_or(Error::Overflow)?;
        let end = if next > self.end {
            let padding = next.checked_sub(self.end).ok_or(Error::Overflow)?;
            let pcm_base = TimeBase::new(1, u64::from(OPUS_RATE))?;
            let ticks = Time::from_ticks(padding, self.media_base)?
                .to_ticks(pcm_base, Rounding::NearestAway)?;
            u32::try_from(ticks).map_err(|_| Error::Overflow)?
        } else {
            0
        };
        Ok(skip_side_data(start, end))
    }
}

fn skip_side_data(start: u32, end: u32) -> Option<[u8; 10]> {
    if start == 0 && end == 0 {
        return None;
    }
    let mut bytes = [0; 10];
    bytes[..4].copy_from_slice(&start.to_le_bytes());
    bytes[4..8].copy_from_slice(&end.to_le_bytes());
    Some(bytes)
}

#[allow(missing_debug_implementations)] // Holds a framing parser that has no `Debug`.
pub struct EncodedEditJob<'a> {
    context: EncodedEditContext<'a>,
    cursor: SampleCursor<'a>,
    pending: Option<Sample>,
    pool: Budget,
    source: SourceId,
    token: InputToken,
    limits: EncodedEditLimits,
    initialized: bool,
    scan_done: bool,
    finished: bool,
    failed: Option<Error>,
    result: Option<EncodedEditPlan>,
    entry: EditEntry,
    edit_ordinal: u32,
    end: i128,
    anchor: Option<i128>,
    maximum_cts: Option<i128>,
    source_end: i128,
    count: u64,
    frame: u32,
    opus_preroll_boundary: i128,
    opus_first: u64,
    opus_first_tick: i128,
    opus_durations: BudgetVec<OpusDurationRun>,
    preserve_aac_media_durations: bool,
    dts_scale: Option<u32>,
    dolby_config: Option<Ac3ConfigParser>,
    descriptions: u32,
    dependency_table: bool,
    empty_recovery: Option<crate::encoded_dependency::EmptyRecoveryWalk>,
    dependency_recovery: bool,
    recovery_walk: Option<crate::encoded_recovery::RecoveryWalk>,
    recovery_contexts: Option<crate::encoded_recovery_context::RecoveryContextReceiver>,
    recovery_facts: Option<crate::RecoveryFacts>,
    recovery_observation: Option<crate::RecoveryObservation>,
}
impl<'a> EncodedEditJob<'a> {
    pub fn new(
        context: EncodedEditContext<'a>,
        input: &InputCursor,
        limits: EncodedEditLimits,
        _profile: EncodedEditProfile,
    ) -> Result<Self> {
        context.stream.kind().require_regular_codec()?;
        Self::new_description_bound(context, input, limits, 1, false)
    }
    pub fn new_with_configs(
        context: EncodedEditContext<'a>,
        configs: &[EncodedStream],
        input: &InputCursor,
        limits: EncodedEditLimits,
        _profile: crate::EncodedConfigProfile,
    ) -> Result<Self> {
        crate::encoded_configs::validate_configs(context.table, configs, input.budget())?;
        let count = u32::try_from(configs.len()).map_err(|_| Error::Overflow)?;
        Self::new_description_bound(context, input, limits, count, false)
    }
    /// Separate source-preserving SDTP/public-disposable observation. Effective
    /// recovery memberships and unproven group namespaces remain unsupported.
    pub fn new_with_dependency_table(
        context: EncodedEditContext<'a>,
        configs: &[EncodedStream],
        input: &InputCursor,
        limits: EncodedEditLimits,
        _profile: crate::EncodedMetadataProfile,
    ) -> Result<Self> {
        crate::encoded_configs::validate_configs(context.table, configs, input.budget())?;
        let count = u32::try_from(configs.len()).map_err(|_| Error::Overflow)?;
        // This extension is SDTP admission. A table without SDTP keeps the
        // already established edit/roll observation, with its raw groups
        // retained unchanged; no new recovery capability is inferred here.
        let dependency_table = context.table.has_dependency_table();
        Self::new_description_bound(context, input, limits, count, dependency_table)
    }
    /// Bounded regular open/index receiver observation. All actual roll/prol
    /// source records are validated and retained; this is not decoder preroll.
    pub fn new_with_recovery_groups(
        context: EncodedEditContext<'a>,
        configs: &[EncodedStream],
        input: &InputCursor,
        limits: EncodedEditLimits,
        _profile: crate::EncodedRecoveryProfile,
    ) -> Result<Self> {
        crate::encoded_configs::validate_configs(context.table, configs, input.budget())?;
        let count = u32::try_from(configs.len()).map_err(|_| Error::Overflow)?;
        let dependency = context.table.has_dependency_table();
        let recovery =
            crate::encoded_recovery::RecoveryWalk::new(context.table, limits.max_samples)?;
        let mut job = Self::new_description_bound(context, input, limits, count, dependency)?;
        job.empty_recovery = None;
        job.dependency_recovery = true;
        job.recovery_walk = Some(recovery);
        Ok(job)
    }
    pub fn new_with_recovery_contexts(
        context: EncodedEditContext<'a>,
        configs: &[EncodedStream],
        input: &InputCursor,
        limits: EncodedEditLimits,
        _profile: crate::EncodedContextProfile,
    ) -> Result<Self> {
        crate::encoded_configs::validate_configs(context.table, configs, input.budget())?;
        Self::new_recovery_bound(context, input, limits, configs.len())
    }
    fn new_recovery_bound(
        context: EncodedEditContext<'a>,
        input: &InputCursor,
        limits: EncodedEditLimits,
        descriptions: usize,
    ) -> Result<Self> {
        let count = u32::try_from(descriptions).map_err(|_| Error::Overflow)?;
        let dependency = context.table.has_dependency_table();
        let recovery = crate::encoded_recovery_context::RecoveryContextReceiver::new(
            context.table,
            limits.max_samples,
        )?;
        let mut out = Self::new_description_bound(context, input, limits, count, dependency)?;
        out.empty_recovery = None;
        out.recovery_walk = None;
        out.dependency_recovery = true;
        out.recovery_contexts = Some(recovery);
        Ok(out)
    }
    /// Range playback admits leading encoder-delay edits for AC-3, E-AC-3
    /// and DTS. DTS also supports a leading empty edit before the media edit.
    /// AAC uses the sample durations in `stts` and the coded frame length for trim.
    /// Opus uses the edit list for priming and keeps each source duration.
    pub fn new_range_playback(
        context: EncodedEditContext<'a>,
        configs: &[EncodedStream],
        input: &InputCursor,
        limits: EncodedEditLimits,
    ) -> Result<Self> {
        crate::encoded_configs::validate_range_configs(context.table, configs, input.budget())?;
        let mut job = Self::new_recovery_bound(context, input, limits, configs.len())?;
        job.preserve_aac_media_durations = true;
        let stream = job.context.stream;
        let kind = match stream.kind() {
            EncodedEntryKind::Ac3 => Ac3ConfigKind::Dac3,
            EncodedEntryKind::Eac3 => Ac3ConfigKind::Dec3,
            _ => return Ok(job),
        };
        let raw = stream.configuration_box();
        let payload = raw
            .bytes()
            .slice(usize::from(raw.header().header_size)..raw.bytes().len())?;
        job.dolby_config = Some(Ac3ConfigParser::new(
            input.budget(),
            input.source(),
            payload,
            kind,
            Ac3ConfigLimits {
                // Stream admission already bounded this owned box.
                maximum_record_bytes: raw.bytes().len(),
                maximum_independent_substreams: 8,
                maximum_input_window: 4,
            },
        )?);
        Ok(job)
    }
    fn new_description_bound(
        context: EncodedEditContext<'a>,
        input: &InputCursor,
        limits: EncodedEditLimits,
        descriptions: u32,
        dependency_table: bool,
    ) -> Result<Self> {
        if !context.table.raw().bytes().belongs_to(input.budget())
            || !context.edits.raw().bytes().belongs_to(input.budget())
            || !context
                .stream
                .configuration_box()
                .bytes()
                .belongs_to(input.budget())
        {
            return Err(Error::ForeignPool);
        }
        let dts = context.stream.dts;
        let (edit_ordinal, leading_ticks) = match context.edits.len() {
            1 => (0, 0),
            2 if dts.is_some() => {
                let leading = context.edits.entry(0)?;
                if !leading.is_empty()
                    || leading.segment_duration == 0
                    || leading.rate_integer != 1
                    || leading.rate_fraction != 0
                {
                    return Err(Error::Unsupported(
                        "DTS leading empty rate-one edit required",
                    ));
                }
                (
                    1,
                    i128::from(
                        leading
                            .duration(context.movie_base)?
                            .to_i64_ticks(context.media_base, Rounding::NearestAway)?,
                    ),
                )
            }
            _ => {
                return Err(Error::Unsupported(
                    "encoded single nonempty rate-one edit required",
                ));
            }
        };
        let entry = context.edits.entry(edit_ordinal)?;
        if entry.media_time < 0
            || entry.segment_duration == 0
            || entry.rate_integer != 1
            || entry.rate_fraction != 0
        {
            return Err(Error::Unsupported(
                "encoded single nonempty rate-one edit required",
            ));
        }
        if context.table.descriptions().len() != descriptions as usize
            || context.table.sample_count() == 0
            || context.media_base != context.stream.demux().time_base
        {
            return Err(Error::Unsupported("encoded edit description/base profile"));
        }
        let declared = &context.table.descriptions()[0];
        if declared.raw().header() != context.stream.source_description().raw().header()
            || declared.raw().bytes().len()
                != context.stream.source_description().raw().bytes().len()
            || declared.raw().bytes().as_ref().as_ptr()
                != context
                    .stream
                    .source_description()
                    .raw()
                    .bytes()
                    .as_ref()
                    .as_ptr()
        {
            return Err(Error::Invalid("encoded edit configuration association"));
        }
        let (dts_scale, frame, anchor) = if let Some(dts) = dts {
            let (scale, frame) = dts.media_clock(context.media_base)?;
            if frame.is_none() && context.table.sample_count() < 2 {
                return Err(Error::Unsupported("DTS edit needs a coded frame clock"));
            }
            let anchor = i128::from(entry.media_time)
                .checked_sub(leading_ticks)
                .ok_or(Error::Overflow)?;
            (Some(scale), frame.unwrap_or(0), Some(anchor))
        } else {
            (None, 0, None)
        };
        let cursor = context.table.cursor();
        let empty_recovery = if dependency_table {
            Some(crate::encoded_dependency::EmptyRecoveryWalk::new(
                context.table,
                limits.max_samples,
            )?)
        } else {
            None
        };
        Ok(Self {
            context,
            cursor,
            pending: None,
            pool: input.budget().clone(),
            source: input.source(),
            token: input.token(),
            limits,
            initialized: false,
            scan_done: false,
            finished: false,
            failed: None,
            result: None,
            entry,
            edit_ordinal,
            end: 0,
            anchor,
            maximum_cts: None,
            source_end: 0,
            count: 0,
            frame,
            opus_preroll_boundary: 0,
            opus_first: 1,
            opus_first_tick: 0,
            opus_durations: BudgetVec::new(input.budget()),
            preserve_aac_media_durations: false,
            dts_scale,
            dolby_config: None,
            descriptions,
            dependency_table,
            empty_recovery,
            dependency_recovery: false,
            recovery_walk: None,
            recovery_contexts: None,
            recovery_observation: None,
            recovery_facts: None,
        })
    }
    fn validate_dolby_clock(&self, config: Ac3Configuration) -> Result<()> {
        let sample_rate =
            crate::fields::u32_at(self.context.stream.source_description().raw().payload(), 24)?;
        let rate = sample_rate >> 16;
        let config_matches_rate = match config {
            Ac3Configuration::Dac3(config) => {
                let fields = config.fields();
                fields.syntax_issues().is_empty() && fields.sample_rate_hz() == Some(rate)
            }
            Ac3Configuration::Dec3(config) => config.substreams().iter().all(|fields| {
                fields.syntax_issues().is_empty() && fields.sample_rate_hz() == Some(rate)
            }),
        };
        if self.descriptions != 1
            || self.context.media_base.numerator() != 1
            || self.context.media_base.denominator() != u64::from(rate)
            || sample_rate & 0xffff != 0
            || !config_matches_rate
            || self.entry.media_time >= i64::from(DOLBY_SAMPLE_TICKS)
        {
            return Err(Error::Unsupported("encoded Dolby audio clock profile"));
        }
        Ok(())
    }
    fn check(
        &self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<()> {
        if !self.pool.same_pool(input.budget()) {
            return Err(Error::ForeignPool);
        }
        if self.source != input.source() {
            return Err(Error::SourceMismatch);
        }
        if self.token.generation() != input.token().generation() {
            return Err(Error::StaleGeneration {
                expected: self.token.generation(),
                received: input.token().generation(),
            });
        }
        input.read_into(&mut [], work, cancel)?;
        Ok(())
    }
    pub fn poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<CursorStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let value = self.inner(input, work, cancel);
        if let Err(error) = value {
            self.failed = Some(error);
            self.result = None;
        }
        value
    }
    fn inner(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<CursorStatus> {
        self.check(input, work, cancel)?;
        if self.result.is_some() {
            return Ok(CursorStatus::Ready);
        }
        if self.finished {
            return Ok(CursorStatus::End);
        }
        if let Some(recovery) = &mut self.recovery_contexts {
            if !recovery.poll(work, cancel)? {
                return Ok(CursorStatus::Yield);
            }
            self.recovery_observation = Some(
                self.recovery_contexts
                    .take()
                    .ok_or(Error::Invalid("V7 recovery result invariant"))?
                    .finish()?,
            );
        }
        if let Some(recovery) = &mut self.recovery_walk {
            if !recovery.poll(work, cancel)? {
                return Ok(CursorStatus::Yield);
            }
            self.recovery_facts = Some(
                self.recovery_walk
                    .take()
                    .ok_or(Error::Invalid("recovery completion invariant"))?
                    .finish()?,
            );
        }
        if let Some(recovery) = &mut self.empty_recovery {
            if !recovery.poll(work)? {
                return Ok(CursorStatus::Yield);
            }
            self.empty_recovery = None;
        }
        if !self.initialized {
            if !work.try_charge(1) {
                return Ok(CursorStatus::Yield);
            }
            let duration = self.entry.duration(self.context.movie_base)?;
            let rounding = if self.dts_scale.is_some() {
                Rounding::NearestAway
            } else {
                Rounding::Floor
            };
            let ticks = duration.to_ticks(self.context.media_base, rounding)?;
            self.end = i128::from(self.entry.media_time)
                .checked_add(ticks)
                .ok_or(Error::Overflow)?;
            if self.end <= i128::from(self.entry.media_time) {
                return Err(Error::Unsupported("encoded edit sub-tick duration"));
            }
            match self.context.stream.kind() {
                kind if kind.is_video() => {}
                EncodedEntryKind::Dts => {}
                EncodedEntryKind::Opus => {
                    // Opus in ISO BMFF, section 4.4. dOps PreSkip does not
                    // determine the presentation window.
                    self.anchor = Some(i128::from(self.entry.media_time));
                    // The MOV view retains the sample at or before one second
                    // of history when an edit crops the front of the track.
                    let second =
                        Time::new(1, 1)?.to_ticks(self.context.media_base, Rounding::Floor)?;
                    self.opus_preroll_boundary = i128::from(self.entry.media_time) - second;
                }
                EncodedEntryKind::Aac => {
                    // Narrow LC/standard-rate/channel-config profile. Body
                    // tools, PCE, SBR signalling and decoder validity stay opaque.
                    let b = self.context.stream.demux().extradata.as_ref();
                    let a = *b.first().ok_or(Error::Truncated)?;
                    let c = *b.get(1).ok_or(Error::Truncated)?;
                    let index = usize::from(((a & 7) << 1) | (c >> 7));
                    const RATES: [u32; 13] = [
                        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000,
                        11025, 8000, 7350,
                    ];
                    if a >> 3 != 2
                        || c & 7 != 0
                        || ((c >> 3) & 15) == 0
                        || ((c >> 3) & 15) > 7
                        || RATES.get(index).map(|x| u64::from(*x))
                            != Some(self.context.media_base.denominator())
                        || self.context.media_base.numerator() != 1
                    {
                        return Err(Error::Unsupported(
                            "encoded AAC LC frame clock not established",
                        ));
                    }
                    self.frame = 1024;
                    self.anchor = Some(i128::from(self.entry.media_time));
                    if self.entry.media_time > i64::from(self.frame) * 3 {
                        return Err(Error::Unsupported(
                            "encoded AAC long preroll not established",
                        ));
                    }
                }
                EncodedEntryKind::Ac3 | EncodedEntryKind::Eac3 if self.dolby_config.is_some() => {
                    if Time::from_ticks(ticks, self.context.media_base)? != duration {
                        return Err(Error::Unsupported("encoded Dolby edit fractional duration"));
                    }
                }
                _ => {
                    return Err(Error::Unsupported(
                        "encoded edited audio codec frame clock not established",
                    ));
                }
            }
            self.initialized = true;
        }
        if let Some(parser) = &mut self.dolby_config {
            let config = match parser.step(work, cancel)? {
                BuildStep::Yield => return Ok(CursorStatus::Yield),
                BuildStep::Ready(config) => config,
                BuildStep::Complete => return Err(Error::Invalid("Dolby configuration invariant")),
            };
            self.validate_dolby_clock(config)?;
            self.dolby_config = None;
            self.frame = DOLBY_SAMPLE_TICKS;
            self.anchor = Some(i128::from(self.entry.media_time));
        }
        loop {
            self.check(input, work, cancel)?;
            if self.scan_done {
                if self.context.stream.kind() == EncodedEntryKind::Opus {
                    // The MOV duration run starts at the first retained occurrence.
                    // Keep the source sample clock intact alongside that projection.
                    let runs = self.context.table.decode_runs();
                    loop {
                        let previous = self.opus_durations.as_slice().last();
                        let samples = previous.map_or(0, |run| run.end_sample);
                        let end_tick = previous.map_or(0, |run| run.end_tick);
                        if samples >= self.count {
                            break;
                        }
                        if !work.try_charge(1) {
                            return Ok(CursorStatus::Yield);
                        }
                        let row = self.opus_durations.len() as u32;
                        let count = u64::from(runs.word(row, 0)?);
                        let ticks = runs.word(row, 4)?;
                        let end_sample = samples.checked_add(count).ok_or(Error::Overflow)?;
                        let end_tick = end_tick
                            .checked_add(i128::from(count) * i128::from(ticks))
                            .ok_or(Error::Overflow)?;
                        self.opus_durations
                            .try_push(OpusDurationRun {
                                end_sample,
                                end_tick,
                                ticks,
                            })
                            .map_err(|error| error.error)?;
                    }
                }
                if !work.try_charge(1) {
                    return Ok(CursorStatus::Yield);
                }
                let presentation_end = self.end;
                let anchor = self.anchor.ok_or(Error::Unsupported(
                    "encoded edit has no presentation anchor",
                ))?;
                if self.dts_scale.is_some() {
                    let demux_end = self
                        .end
                        .min(self.source_end)
                        .checked_sub(anchor)
                        .ok_or(Error::Overflow)?;
                    i64::try_from(demux_end).map_err(|_| Error::Overflow)?;
                }
                if matches!(
                    self.context.stream.kind(),
                    EncodedEntryKind::Ac3 | EncodedEntryKind::Eac3
                ) && self.end != self.source_end
                {
                    return Err(Error::Unsupported(
                        "encoded Dolby edit must end at source end",
                    ));
                }
                let timing = if self.context.stream.kind() == EncodedEntryKind::Opus {
                    if i128::from(self.entry.media_time) >= self.source_end {
                        return Err(Error::Invalid("Opus edit starts after the last sample"));
                    }
                    EditTiming::Opus
                } else if self.frame != 0 {
                    EditTiming::FixedAudio(self.frame)
                } else {
                    let duration = self
                        .end
                        .checked_sub(i128::from(self.entry.media_time))
                        .ok_or(Error::Overflow)?;
                    self.end = anchor.checked_add(duration).ok_or(Error::Overflow)?;
                    if self.maximum_cts.is_some_and(|x| x >= self.end) {
                        return Err(Error::Unsupported(
                            "encoded video tail selection not established",
                        ));
                    }
                    EditTiming::Video
                };
                let opus_timing = if matches!(timing, EditTiming::Opus) {
                    let layout = std::alloc::Layout::new::<[usize; 2]>()
                        .extend(std::alloc::Layout::new::<OpusTiming>())
                        .map_err(|_| Error::Overflow)?
                        .0
                        .pad_to_align();
                    let charge = self.pool.reserve(layout.size())?;
                    Some(Arc::new(OpusTiming {
                        first: self.opus_first,
                        first_tick: self.opus_first_tick,
                        durations: std::mem::replace(
                            &mut self.opus_durations,
                            BudgetVec::new(&self.pool),
                        ),
                        _charge: charge,
                    }))
                } else {
                    None
                };
                self.result = Some(EncodedEditPlan {
                    source_table: self.context.table.raw().clone(),
                    source_edit: self.context.edits.raw().clone(),
                    source_config: self.context.stream.configuration_box().clone(),
                    entry: self.entry,
                    edit_ordinal: self.edit_ordinal,
                    media_base: self.context.media_base,
                    movie_base: self.context.movie_base,
                    anchor,
                    end: self.end,
                    source_end: self.source_end,
                    presentation_end,
                    count: self.count,
                    timing,
                    opus_timing,
                    dts_scale: self.dts_scale,
                    dependency_table: self.dependency_table,
                    dependency_recovery: self.dependency_recovery,
                    recovery_facts: self.recovery_facts.take(),
                    recovery_observation: self.recovery_observation.take(),
                });
                return Ok(CursorStatus::Ready);
            }
            if let Some(s) = self.pending.as_ref() {
                if !work.try_charge(1) {
                    return Ok(CursorStatus::Yield);
                }
                if s.duration_ticks == 0
                    || s.partial_sync
                    || (s.dependency.is_some() && !self.dependency_table)
                    || s.access == Access::Unknown
                    || (s.description_index == 0 || s.description_index > self.descriptions)
                    || s.fragment_flags.is_some()
                    || s.composition_offset
                        .is_some_and(|x| x.version() != 0 || x.raw_bits() > i32::MAX as u32)
                {
                    return Err(Error::Unsupported(
                        "encoded edit recovery/clock/config profile",
                    ));
                }
                if self.dependency_table
                    && !self.dependency_recovery
                    && s.dependency.is_some_and(|raw| raw & 0x0c == 12)
                {
                    return Err(Error::Unsupported("encoded V4 reserved depended-on value"));
                }
                let d = s
                    .dts_ticks
                    .ok_or(Error::Unsupported("encoded edit source DTS unknown"))?;
                let c = s
                    .cts_ticks
                    .ok_or(Error::Unsupported("encoded edit source CTS unknown"))?;
                if self.count == 0 && (d != 0 || !s.is_container_sync()) {
                    return Err(Error::Unsupported("encoded edit initial source/key origin"));
                }
                if d != self.source_end {
                    return Err(Error::Invalid("encoded edit source decode continuity"));
                }
                self.count = self.count.checked_add(1).ok_or(Error::Overflow)?;
                if self.count > self.limits.max_samples {
                    return Err(Error::Limit {
                        requested: usize::try_from(self.count).map_err(|_| Error::Overflow)?,
                        maximum: usize::try_from(self.limits.max_samples)
                            .map_err(|_| Error::Overflow)?,
                    });
                }
                if self.dts_scale.is_some() {
                    if self.frame == 0 {
                        // In-band FFmpeg descriptions rely on the first
                        // source duration. Never apply another codec's clock.
                        self.frame = s.duration_ticks;
                    }
                    if c != d
                        || !s.is_container_sync()
                        || s.duration_ticks > self.frame
                        || (self.count < self.context.table.sample_count()
                            && s.duration_ticks != self.frame)
                        || (self.count == 1 && i64::from(s.duration_ticks) <= self.entry.media_time)
                    {
                        return Err(Error::Unsupported(
                            "encoded DTS source packet clock profile",
                        ));
                    }
                } else if self.frame != 0 {
                    if self.frame == DOLBY_SAMPLE_TICKS {
                        if c != d || s.duration_ticks != self.frame {
                            return Err(Error::Unsupported(
                                "encoded Dolby source packet clock profile",
                            ));
                        }
                    } else if c != d
                        || (s.duration_ticks > self.frame && !self.preserve_aac_media_durations)
                    {
                        return Err(Error::Unsupported(
                            "encoded AAC source packet clock profile",
                        ));
                    }
                } else if self.context.stream.kind() == EncodedEntryKind::Opus {
                    if c != d || !s.is_container_sync() {
                        return Err(Error::Invalid("Opus sample timing or sync flag"));
                    }
                    if d <= self.opus_preroll_boundary {
                        self.opus_first = s.ordinal;
                        self.opus_first_tick = d;
                    }
                } else if c >= i128::from(self.entry.media_time) {
                    self.anchor = Some(self.anchor.map_or(c, |prior| prior.min(c)));
                }
                self.maximum_cts = Some(self.maximum_cts.map_or(c, |prior| prior.max(c)));
                self.source_end = d
                    .checked_add(i128::from(s.duration_ticks))
                    .ok_or(Error::Overflow)?;
                self.pending = None;
                continue;
            }
            match self.cursor.poll(work)? {
                SampleStep::Yield => return Ok(CursorStatus::Yield),
                SampleStep::Sample(s) => self.pending = Some(s),
                SampleStep::End => self.scan_done = true,
            }
        }
    }
    pub fn take(
        &mut self,
        input: &mut InputCursor,
        cancel: &AtomicBool,
    ) -> Result<EncodedEditPlan> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        if let Err(e) = self.check(input, &mut WorkBudget::new(0), cancel) {
            self.failed = Some(e);
            self.result = None;
            return Err(e);
        }
        let result = self
            .result
            .take()
            .ok_or(Error::Invalid("encoded edit plan not ready"))?;
        self.finished = true;
        Ok(result)
    }
}
