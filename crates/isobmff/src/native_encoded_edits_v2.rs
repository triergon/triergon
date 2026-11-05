//! Separate, pin-bound regular encoded edit admission. Source samples, edits,
//! decoder configuration and clocks remain unchanged. No decoder/AU claim.
//! Admission scans real metadata once before any occurrence is published.
use crate::{
    Access, BoxView, EditEntry, EditList, EncodedEntryKindV1, Mov9ClockField, Mov9CursorStatus,
    Mov9EncodedStreamV1, Mov9PacketView, Sample, SampleCursor, SampleStep, SampleTable,
};
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Error, InputCursor, InputToken, Result, Rounding, SourceId, Time, TimeBase, WorkBudget,
};
use triergon_framing::{
    Ac3ConfigKind, Ac3ConfigLimits, Ac3ConfigParser, Ac3Configuration, BuildStep,
};

// ETSI TS 102 366 Annex F.2: an AC-3 or E-AC-3 ISO media sample covers six blocks.
const DOLBY_SAMPLE_TICKS: u32 = 1536;

#[derive(Clone, Copy, Debug)]
pub struct PinnedMovD32EncodedEditsV2;
#[derive(Clone, Copy, Debug)]
pub struct Mov9EncodedEditLimitsV2 {
    pub max_samples: u64,
}
impl Default for Mov9EncodedEditLimitsV2 {
    fn default() -> Self {
        Self {
            max_samples: 1_000_000,
        }
    }
}
pub struct Mov9EncodedEditContextV2<'a> {
    pub table: &'a SampleTable,
    pub edits: &'a EditList,
    pub stream: &'a Mov9EncodedStreamV1,
    pub media_base: TimeBase,
    pub movie_base: TimeBase,
}
#[derive(Clone, Debug)]
pub struct Mov9EncodedEditPlanV2 {
    source_table: BoxView,
    source_edit: BoxView,
    source_config: BoxView,
    entry: EditEntry,
    media_base: TimeBase,
    movie_base: TimeBase,
    anchor: i128,
    end: i128,
    source_end: i128,
    presentation_end: i128,
    count: u64,
    frame: u32, // PCM samples per coded audio sample. Zero for video.
    dependency_v4: bool,
    dependency_v5: bool,
    recovery_source_v5: Option<crate::Mov9RecoveryFactsV5>,
    recovery_source_v7: Option<crate::Mov9RecoveryObservationV7>,
}
impl Mov9EncodedEditPlanV2 {
    pub fn recovery_source_v7(&self) -> Option<&crate::Mov9RecoveryObservationV7> {
        self.recovery_source_v7.as_ref()
    }

    pub fn recovery_source_v5(&self) -> Option<&crate::Mov9RecoveryFactsV5> {
        self.recovery_source_v5.as_ref()
    }
    pub(crate) fn has_dependency_view_v4(&self) -> bool {
        self.dependency_v4
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
    pub fn native_anchor_ticks(&self) -> i128 {
        self.anchor
    }
    pub fn selected_end_ticks(&self) -> i128 {
        self.end
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
    pub(crate) fn project(&self, source: &Sample) -> Result<Option<Mov9PacketView>> {
        if source.ordinal == 0 || source.ordinal > self.count {
            return Err(Error::Invalid("encoded edit source ordinal"));
        }
        let dts = source
            .dts_ticks
            .ok_or(Error::Unsupported("encoded edit source DTS unknown"))?;
        let cts = source
            .cts_ticks
            .ok_or(Error::Unsupported("encoded edit source CTS unknown"))?;
        if self.frame != 0 && dts >= self.end {
            return Ok(None);
        }
        let dts_field = Mov9ClockField::from_exact_native_ticks(
            dts.checked_sub(self.anchor).ok_or(Error::Overflow)?,
        )?;
        let pts_field = Mov9ClockField::from_exact_native_ticks(
            cts.checked_sub(self.anchor).ok_or(Error::Overflow)?,
        )?;
        let source_end = dts
            .checked_add(i128::from(source.duration_ticks))
            .ok_or(Error::Overflow)?;
        let discard = if self.frame != 0 {
            source_end <= i128::from(self.entry.media_time)
        } else {
            cts < self.anchor || cts >= self.presentation_end
        };
        Ok(Some(Mov9PacketView {
            source_ordinal: source.ordinal,
            source_description: source.description_index,
            source_range: source.range.clone(),
            pts: pts_field.api_ticks(),
            dts: dts_field.api_ticks(),
            pts_field,
            dts_field,
            duration: i64::from(source.duration_ticks),
            flags: u32::from(source.is_container_sync())
                | if discard { 4 } else { 0 }
                | if self.dependency_v4
                    && source.dependency.is_some_and(|raw| {
                        if self.dependency_v5 {
                            crate::Mov9DependencyObservationV5::from_source(
                                raw,
                                crate::PinnedMovD32EncodedRecoveryV5,
                            )
                            .native_disposable_hint()
                        } else {
                            crate::Mov9DependencyViewV4::from_source(raw).native_disposable_hint()
                        }
                    })
                {
                    16
                } else {
                    0
                },
        }))
    }
    /// Native skip-samples side data for a sample from this plan's source track.
    /// The first two little-endian words give leading and trailing sample counts.
    /// The packet retains the original source sample duration.
    pub fn skip_samples(&self, source: &Sample) -> Result<Option<[u8; 10]>> {
        if self.frame == 0 {
            return Ok(None);
        }
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
            let nominal = dts
                .checked_add(i128::from(self.frame))
                .ok_or(Error::Overflow)?;
            u32::try_from(nominal.checked_sub(desired).ok_or(Error::Overflow)?.max(0))
                .map_err(|_| Error::Overflow)?
        } else {
            0
        };
        if start == 0 && end == 0 {
            return Ok(None);
        }
        let mut bytes = [0; 10];
        bytes[..4].copy_from_slice(&start.to_le_bytes());
        bytes[4..8].copy_from_slice(&end.to_le_bytes());
        Ok(Some(bytes))
    }
}
pub struct Mov9EncodedEditJobV2<'a> {
    context: Mov9EncodedEditContextV2<'a>,
    cursor: SampleCursor<'a>,
    pending: Option<Sample>,
    pool: Budget,
    source: SourceId,
    token: InputToken,
    limits: Mov9EncodedEditLimitsV2,
    initialized: bool,
    scan_done: bool,
    finished: bool,
    failed: Option<Error>,
    result: Option<Mov9EncodedEditPlanV2>,
    entry: EditEntry,
    end: i128,
    anchor: Option<i128>,
    maximum_cts: Option<i128>,
    source_end: i128,
    count: u64,
    frame: u32,
    dolby_config: Option<Ac3ConfigParser>,
    descriptions: u32,
    dependency_v4: bool,
    recovery_v4: Option<crate::native_encoded_dependency_v4::EmptyRecoveryWalkV4>,
    dependency_v5: bool,
    recovery_v5: Option<crate::native_encoded_recovery_v5::RecoveryWalkV5>,
    recovery_v7: Option<crate::native_recovery_context_v7::RecoveryReceiverV7>,
    recovery_source_v5: Option<crate::Mov9RecoveryFactsV5>,
    recovery_source_v7: Option<crate::Mov9RecoveryObservationV7>,
}
impl<'a> Mov9EncodedEditJobV2<'a> {
    pub fn new(
        context: Mov9EncodedEditContextV2<'a>,
        input: &InputCursor,
        limits: Mov9EncodedEditLimitsV2,
        _profile: PinnedMovD32EncodedEditsV2,
    ) -> Result<Self> {
        Self::new_description_bound(context, input, limits, 1, false)
    }
    pub fn new_v3(
        context: Mov9EncodedEditContextV2<'a>,
        configs: &[crate::Mov9EncodedStreamV1],
        input: &InputCursor,
        limits: Mov9EncodedEditLimitsV2,
        _profile: crate::PinnedMovD32EncodedRegularV3,
    ) -> Result<Self> {
        crate::native_encoded_configs_v3::validate_configs(context.table, configs, input.budget())?;
        let count = u32::try_from(configs.len()).map_err(|_| Error::Overflow)?;
        Self::new_description_bound(context, input, limits, count, false)
    }
    /// Separate source-preserving SDTP/public-disposable observation. Effective
    /// recovery memberships and unproven group namespaces remain unsupported.
    pub fn new_v4(
        context: Mov9EncodedEditContextV2<'a>,
        configs: &[crate::Mov9EncodedStreamV1],
        input: &InputCursor,
        limits: Mov9EncodedEditLimitsV2,
        _profile: crate::PinnedMovD32EncodedRegularV4,
    ) -> Result<Self> {
        crate::native_encoded_configs_v3::validate_configs(context.table, configs, input.budget())?;
        let count = u32::try_from(configs.len()).map_err(|_| Error::Overflow)?;
        // The V4 extension is SDTP admission. A table without SDTP keeps the
        // already established V3 edit/roll observation, with its raw groups
        // retained unchanged; no new recovery capability is inferred here.
        let dependency_v4 = context.table.has_dependency_table();
        Self::new_description_bound(context, input, limits, count, dependency_v4)
    }
    /// Bounded regular open/index receiver observation. All actual roll/prol
    /// source records are validated and retained; this is not decoder preroll.
    pub fn new_v5(
        context: Mov9EncodedEditContextV2<'a>,
        configs: &[crate::Mov9EncodedStreamV1],
        input: &InputCursor,
        limits: Mov9EncodedEditLimitsV2,
        _profile: crate::PinnedMovD32EncodedRecoveryV5,
    ) -> Result<Self> {
        crate::native_encoded_configs_v3::validate_configs(context.table, configs, input.budget())?;
        let count = u32::try_from(configs.len()).map_err(|_| Error::Overflow)?;
        let dependency = context.table.has_dependency_table();
        let recovery = crate::native_encoded_recovery_v5::RecoveryWalkV5::new(
            context.table,
            limits.max_samples,
        )?;
        let mut job = Self::new_description_bound(context, input, limits, count, dependency)?;
        job.recovery_v4 = None;
        job.dependency_v5 = true;
        job.recovery_v5 = Some(recovery);
        Ok(job)
    }
    pub fn new_v7(
        context: Mov9EncodedEditContextV2<'a>,
        configs: &[crate::Mov9EncodedStreamV1],
        input: &InputCursor,
        limits: Mov9EncodedEditLimitsV2,
        _profile: crate::PinnedMovD32EncodedContextsV7,
    ) -> Result<Self> {
        crate::native_encoded_configs_v3::validate_configs(context.table, configs, input.budget())?;
        let count = u32::try_from(configs.len()).map_err(|_| Error::Overflow)?;
        let dependency = context.table.has_dependency_table();
        let recovery = crate::native_recovery_context_v7::RecoveryReceiverV7::new(
            context.table,
            limits.max_samples,
        )?;
        let mut out = Self::new_description_bound(context, input, limits, count, dependency)?;
        out.recovery_v4 = None;
        out.recovery_v5 = None;
        out.dependency_v5 = true;
        out.recovery_v7 = Some(recovery);
        Ok(out)
    }
    /// The range playback view additionally admits a leading AC-3 or E-AC-3
    /// encoder-delay edit on a complete regular track with a fixed frame clock.
    pub fn new_range_playback(
        context: Mov9EncodedEditContextV2<'a>,
        configs: &[crate::Mov9EncodedStreamV1],
        input: &InputCursor,
        limits: Mov9EncodedEditLimitsV2,
    ) -> Result<Self> {
        let mut job = Self::new_v7(
            context,
            configs,
            input,
            limits,
            crate::PinnedMovD32EncodedContextsV7,
        )?;
        let stream = job.context.stream;
        let kind = match stream.kind() {
            EncodedEntryKindV1::Ac3 => Ac3ConfigKind::Dac3,
            EncodedEntryKindV1::Eac3 => Ac3ConfigKind::Dec3,
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
        context: Mov9EncodedEditContextV2<'a>,
        input: &InputCursor,
        limits: Mov9EncodedEditLimitsV2,
        descriptions: u32,
        dependency_v4: bool,
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
        if context.edits.len() != 1 {
            return Err(Error::Unsupported(
                "encoded single nonempty rate-one edit required",
            ));
        }
        let entry = context.edits.entry(0)?;
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
            || context.media_base != context.stream.native().time_base
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
        let cursor = context.table.cursor();
        let recovery_v4 = if dependency_v4 {
            Some(
                crate::native_encoded_dependency_v4::EmptyRecoveryWalkV4::new(
                    context.table,
                    limits.max_samples,
                )?,
            )
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
            end: 0,
            anchor: None,
            maximum_cts: None,
            source_end: 0,
            count: 0,
            frame: 0,
            dolby_config: None,
            descriptions,
            dependency_v4,
            recovery_v4,
            dependency_v5: false,
            recovery_v5: None,
            recovery_v7: None,
            recovery_source_v7: None,
            recovery_source_v5: None,
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
    ) -> Result<Mov9CursorStatus> {
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
    ) -> Result<Mov9CursorStatus> {
        self.check(input, work, cancel)?;
        if self.result.is_some() {
            return Ok(Mov9CursorStatus::Ready);
        }
        if self.finished {
            return Ok(Mov9CursorStatus::End);
        }
        if let Some(recovery) = &mut self.recovery_v7 {
            if !recovery.poll(work, cancel)? {
                return Ok(Mov9CursorStatus::Yield);
            }
            self.recovery_source_v7 = Some(
                self.recovery_v7
                    .take()
                    .ok_or(Error::Invalid("V7 recovery result invariant"))?
                    .finish()?,
            );
        }
        if let Some(recovery) = &mut self.recovery_v5 {
            if !recovery.poll(work, cancel)? {
                return Ok(Mov9CursorStatus::Yield);
            }
            self.recovery_source_v5 = Some(
                self.recovery_v5
                    .take()
                    .ok_or(Error::Invalid("recovery completion invariant"))?
                    .finish()?,
            );
        }
        if let Some(recovery) = &mut self.recovery_v4 {
            if !recovery.poll(work)? {
                return Ok(Mov9CursorStatus::Yield);
            }
            self.recovery_v4 = None;
        }
        if !self.initialized {
            if !work.try_charge(1) {
                return Ok(Mov9CursorStatus::Yield);
            }
            let duration = self.entry.duration(self.context.movie_base)?;
            let ticks = duration.to_ticks(self.context.media_base, Rounding::Floor)?;
            self.end = i128::from(self.entry.media_time)
                .checked_add(ticks)
                .ok_or(Error::Overflow)?;
            if self.end <= i128::from(self.entry.media_time) {
                return Err(Error::Unsupported("encoded edit sub-tick duration"));
            }
            match self.context.stream.kind() {
                EncodedEntryKindV1::Avc | EncodedEntryKindV1::Hevc => {}
                EncodedEntryKindV1::Aac => {
                    // Narrow LC/standard-rate/channel-config profile. Body
                    // tools, PCE, SBR signalling and decoder validity stay opaque.
                    let b = self.context.stream.native().extradata.as_ref();
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
                EncodedEntryKindV1::Ac3 | EncodedEntryKindV1::Eac3
                    if self.dolby_config.is_some() =>
                {
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
                BuildStep::Yield => return Ok(Mov9CursorStatus::Yield),
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
                if !work.try_charge(1) {
                    return Ok(Mov9CursorStatus::Yield);
                }
                let presentation_end = self.end;
                let anchor = self.anchor.ok_or(Error::Unsupported(
                    "encoded edit has no presentation anchor",
                ))?;
                if self.frame == DOLBY_SAMPLE_TICKS && self.end != self.source_end {
                    return Err(Error::Unsupported(
                        "encoded Dolby edit must end at source end",
                    ));
                }
                if self.frame == 0 {
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
                }
                self.result = Some(Mov9EncodedEditPlanV2 {
                    source_table: self.context.table.raw().clone(),
                    source_edit: self.context.edits.raw().clone(),
                    source_config: self.context.stream.configuration_box().clone(),
                    entry: self.entry,
                    media_base: self.context.media_base,
                    movie_base: self.context.movie_base,
                    anchor,
                    end: self.end,
                    source_end: self.source_end,
                    presentation_end,
                    count: self.count,
                    frame: self.frame,
                    dependency_v4: self.dependency_v4,
                    dependency_v5: self.dependency_v5,
                    recovery_source_v5: self.recovery_source_v5.take(),
                    recovery_source_v7: self.recovery_source_v7.take(),
                });
                return Ok(Mov9CursorStatus::Ready);
            }
            if let Some(s) = self.pending.as_ref() {
                if !work.try_charge(1) {
                    return Ok(Mov9CursorStatus::Yield);
                }
                if s.duration_ticks == 0
                    || s.partial_sync
                    || (s.dependency.is_some() && !self.dependency_v4)
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
                if self.dependency_v4
                    && !self.dependency_v5
                    && s.dependency.is_some_and(|raw| raw & 12 == 12)
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
                if self.frame != 0 {
                    if self.frame == DOLBY_SAMPLE_TICKS {
                        if c != d || s.duration_ticks != self.frame {
                            return Err(Error::Unsupported(
                                "encoded Dolby source packet clock profile",
                            ));
                        }
                    } else if c != d || s.duration_ticks > self.frame {
                        return Err(Error::Unsupported(
                            "encoded AAC source packet clock profile",
                        ));
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
                SampleStep::Yield => return Ok(Mov9CursorStatus::Yield),
                SampleStep::Sample(s) => self.pending = Some(s),
                SampleStep::End => self.scan_done = true,
            }
        }
    }
    pub fn take(
        &mut self,
        input: &mut InputCursor,
        cancel: &AtomicBool,
    ) -> Result<Mov9EncodedEditPlanV2> {
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
