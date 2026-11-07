//! Retained source roll/prol facts and a separately selected reference open/index view.
//! Source prerequisites are not decoder claims. reference does not apply these group
//! kinds in this operation; no source distance, mapping, clock or key is erased.
use crate::{
    BoxChildren, BoxStep, BuildStatus, DependencyView, Limits, RecoveryDescriptions, RecoveryKind,
    RecoveryLimits, RecoveryMappings, RecoveryScope, RecoverySyntax, SampleTable, fields,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub struct EncodedRecoveryProfile;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DependencyCause {
    SourceUnspecified,
    SourceReferenced,
    SourceUnreferenced,
    SourceReservedNotAppliedByReceiver,
}
#[derive(Clone, Copy, Debug)]
pub struct DependencyObservation {
    pub source: DependencyView,
    pub cause: DependencyCause,
}
impl DependencyObservation {
    pub fn from_source(raw: u8, _profile: EncodedRecoveryProfile) -> Self {
        let cause = match raw & 0x0c {
            0 => DependencyCause::SourceUnspecified,
            4 => DependencyCause::SourceReferenced,
            8 => DependencyCause::SourceUnreferenced,
            _ => DependencyCause::SourceReservedNotAppliedByReceiver,
        };
        Self {
            source: DependencyView::from_source(raw),
            cause,
        }
    }
    pub fn demux_disposable_hint(self) -> bool {
        self.cause == DependencyCause::SourceUnreferenced
    }
}
#[derive(Clone, Debug, Default)]
pub struct RecoveryGroup {
    descriptions: Option<RecoveryDescriptions>,
    mappings: Option<RecoveryMappings>,
}
impl RecoveryGroup {
    pub fn descriptions(&self) -> Option<&RecoveryDescriptions> {
        self.descriptions.as_ref()
    }
    pub fn mappings(&self) -> Option<&RecoveryMappings> {
        self.mappings.as_ref()
    }
}
#[cfg(feature = "reference-trace")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryReceiverCause {
    RollProlNotAppliedByOpenIndex,
}
#[derive(Clone, Debug)]
pub struct RecoveryFacts {
    groups: [RecoveryGroup; 2],
    samples: u64,
}
impl RecoveryFacts {
    pub fn group(&self, kind: RecoveryKind) -> &RecoveryGroup {
        &self.groups[match kind {
            RecoveryKind::Roll => 0,
            RecoveryKind::Prol => 1,
        }]
    }
    pub fn source_scope(&self, kind: RecoveryKind) -> RecoveryScope<'_> {
        let group = self.group(kind);
        RecoveryScope {
            kind,
            first_ordinal: 1,
            sample_count: self.samples,
            fragment_ordinal: None,
            track_descriptions: group.descriptions(),
            local_descriptions: None,
            mappings: group.mappings(),
        }
    }
    #[cfg(feature = "reference-trace")]
    pub fn receiver_cause(&self) -> RecoveryReceiverCause {
        RecoveryReceiverCause::RollProlNotAppliedByOpenIndex
    }
}
#[derive(Debug)]
enum Pending {
    Descriptions {
        source: RecoveryDescriptions,
        row: u32,
        offset: usize,
        slot: usize,
    },
    Mappings {
        source: RecoveryMappings,
        row: u32,
        covered: u64,
        maximum: u32,
        slot: usize,
    },
}
fn source_result<T>(
    value: Result<T>,
    raw: &crate::BoxView,
    fault: &mut Option<crate::BoxView>,
) -> Result<T> {
    if value.is_err() {
        *fault = Some(raw.clone());
    }
    value
}
#[derive(Debug)]
pub(crate) struct RecoveryWalk {
    children: Option<BoxChildren>,
    pending: Option<Pending>,
    seen: u8,
    facts: RecoveryFacts,
    maximum_indices: [u32; 2],
    limits: RecoveryLimits,
    finished: bool,
    extended: bool,
    error_backing: Option<crate::BoxView>,
}
impl RecoveryWalk {
    pub(crate) fn new(table: &SampleTable, maximum: u64) -> Result<Self> {
        Ok(Self {
            children: Some(table.raw().children(0, Limits::default())?),
            pending: None,
            seen: 0,
            facts: RecoveryFacts {
                groups: Default::default(),
                samples: table.sample_count(),
            },
            maximum_indices: [0; 2],
            limits: RecoveryLimits {
                max_descriptions: 64,
                max_runs: u32::try_from(maximum).unwrap_or(u32::MAX),
            },
            finished: false,
            extended: false,
            error_backing: None,
        })
    }
    pub(crate) fn new_with_recovery_contexts(table: &SampleTable, maximum: u64) -> Result<Self> {
        let mut out = Self::new(table, maximum)?;
        out.extended = true;
        Ok(out)
    }
    pub(crate) fn error_backing(&self) -> Option<&crate::BoxView> {
        self.error_backing.as_ref()
    }
    pub(crate) fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if let Some(pending) = &mut self.pending {
                match pending {
                    Pending::Descriptions {
                        source,
                        row,
                        offset,
                        ..
                    } => {
                        if *row != source.entry_count() {
                            let index = row.checked_add(1).ok_or(Error::Overflow)?;
                            let next = if self.extended {
                                source.validation_at(index, *offset)
                            } else {
                                source.locate(index)
                            };
                            let mut lookup =
                                source_result(next, source.raw(), &mut self.error_backing)?;
                            if source_result(
                                lookup.poll(work, cancel),
                                source.raw(),
                                &mut self.error_backing,
                            )? == BuildStatus::Yield
                            {
                                return Ok(false);
                            }
                            let entry = source_result(
                                lookup.finish(),
                                source.raw(),
                                &mut self.error_backing,
                            )?;
                            *offset = usize::try_from(
                                entry
                                    .source_range
                                    .end
                                    .checked_sub(source.raw().header().payload_range()?.start)
                                    .ok_or(Error::Overflow)?,
                            )
                            .map_err(|_| Error::Overflow)?;
                            *row = index;
                            continue;
                        }
                    }
                    Pending::Mappings {
                        source,
                        row,
                        covered,
                        maximum,
                        ..
                    } => {
                        if *row != source.run_count() {
                            if !work.try_charge(1) {
                                return Ok(false);
                            }
                            let (count, index) = source_result(
                                source.run(*row),
                                source.raw(),
                                &mut self.error_backing,
                            )?;
                            if count == 0 {
                                return source_result(
                                    Err(Error::Invalid("encoded V5 empty recovery run")),
                                    source.raw(),
                                    &mut self.error_backing,
                                );
                            }
                            if index >= 65536 {
                                return source_result(
                                    Err(Error::Unsupported(
                                        "encoded V5 fragment recovery namespace",
                                    )),
                                    source.raw(),
                                    &mut self.error_backing,
                                );
                            }
                            *covered = covered
                                .checked_add(u64::from(count))
                                .ok_or(Error::Overflow)?;
                            if *covered > self.facts.samples {
                                return source_result(
                                    Err(Error::Invalid("encoded V5 recovery exceeds samples")),
                                    source.raw(),
                                    &mut self.error_backing,
                                );
                            }
                            *maximum = (*maximum).max(index);
                            *row += 1;
                            continue;
                        }
                        if *covered != self.facts.samples {
                            return source_result(
                                Err(Error::Unsupported(
                                    "encoded V5 incomplete recovery coverage",
                                )),
                                source.raw(),
                                &mut self.error_backing,
                            );
                        }
                    }
                }
                match self
                    .pending
                    .take()
                    .ok_or(Error::Invalid("recovery pending invariant"))?
                {
                    Pending::Descriptions { source, slot, .. } => {
                        self.facts.groups[slot].descriptions = Some(source);
                    }
                    Pending::Mappings {
                        source,
                        maximum,
                        slot,
                        ..
                    } => {
                        self.facts.groups[slot].mappings = Some(source);
                        self.maximum_indices[slot] = maximum;
                    }
                }
                continue;
            }
            let Some(children) = &mut self.children else {
                return Ok(self.finished);
            };
            match children.poll(work)? {
                BoxStep::Yield => return Ok(false),
                BoxStep::End => {
                    for (slot, group) in self.facts.groups.iter().enumerate() {
                        let count = group
                            .descriptions
                            .as_ref()
                            .map_or(0, RecoveryDescriptions::entry_count);
                        if self.maximum_indices[slot] > count {
                            self.error_backing = group.mappings.as_ref().map(|s| s.raw().clone());
                            return Err(Error::Invalid(
                                "encoded V5 unresolved recovery description",
                            ));
                        }
                    }
                    self.children = None;
                    self.finished = true;
                    return Ok(true);
                }
                BoxStep::Box(raw) => {
                    let description = match raw.header().kind {
                        x if x == *b"sgpd" => true,
                        x if x == *b"sbgp" => false,
                        _ => continue,
                    };
                    let kind = source_result(
                        fields::fourcc(raw.payload(), 4),
                        &raw,
                        &mut self.error_backing,
                    )?;
                    let slot = match kind {
                        x if x == *b"roll" => 0,
                        x if x == *b"prol" => 1,
                        _ => {
                            return source_result(
                                Err(Error::Unsupported(
                                    "encoded V5 other sample-group semantics",
                                )),
                                &raw,
                                &mut self.error_backing,
                            );
                        }
                    };
                    let bit = 1u8 << (slot + if description { 0 } else { 2 });
                    if self.seen & bit != 0 {
                        return source_result(
                            Err(Error::Invalid("encoded V5 duplicate recovery context")),
                            &raw,
                            &mut self.error_backing,
                        );
                    }
                    self.seen |= bit;
                    self.pending = Some(if description {
                        let source = source_result(
                            RecoveryDescriptions::parse(
                                raw.clone(),
                                RecoverySyntax::Versions0And1,
                                self.limits,
                            ),
                            &raw,
                            &mut self.error_backing,
                        )?;
                        if source.entry_count() != 0
                            && source.fixed_entry_length() != 2
                            && !(self.extended && source.fixed_entry_length() == 0)
                        {
                            return source_result(
                                Err(Error::Unsupported(
                                    "encoded V5 variable or extended recovery entries",
                                )),
                                &raw,
                                &mut self.error_backing,
                            );
                        }
                        let offset = source.validation_start();
                        Pending::Descriptions {
                            source,
                            row: 0,
                            offset,
                            slot,
                        }
                    } else {
                        let source = source_result(
                            RecoveryMappings::parse(raw.clone(), self.limits),
                            &raw,
                            &mut self.error_backing,
                        )?;
                        if !self.extended && source.grouping_parameter().is_some_and(|p| p != 0) {
                            return source_result(
                                Err(Error::Unsupported("encoded V5 recovery grouping parameter")),
                                &raw,
                                &mut self.error_backing,
                            );
                        }
                        Pending::Mappings {
                            source,
                            row: 0,
                            covered: 0,
                            maximum: 0,
                            slot,
                        }
                    });
                }
            }
        }
    }
    pub(crate) fn finish(self) -> Result<RecoveryFacts> {
        if !self.finished {
            return Err(Error::Invalid("encoded V5 recovery not ready"));
        }
        Ok(self.facts)
    }
}
