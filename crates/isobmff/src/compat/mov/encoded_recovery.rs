//! The `roll` and `prol` sample groups and the `sdtp` bytes of an encoded track as the
//! recovery view reads them. FFmpeg's MOV demuxer does not apply these groups when it
//! opens and indexes a track, so they are checked and kept but not applied. A group is
//! a source fact, not a promise of decoder recovery; no roll distance, mapping, clock or
//! key flag is changed.
use crate::{
    BuildStatus, Limits, Next,
    boxes::BoxChildren,
    fields,
    sample_groups::{
        RecoveryDescriptions, RecoveryKind, RecoveryLimits, RecoveryMappings, RecoveryScope,
        RecoverySyntax,
    },
    table::SampleTable,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};

/// One grouping type's `sgpd` descriptions and `sbgp` mappings in a sample table;
/// either can be absent.
#[derive(Clone, Debug, Default)]
pub struct RecoveryGroup {
    descriptions: Option<RecoveryDescriptions>,
    mappings: Option<RecoveryMappings>,
}
impl RecoveryGroup {
    /// The parsed `sgpd` box; `None` when the table has none of this type.
    pub fn descriptions(&self) -> Option<&RecoveryDescriptions> {
        self.descriptions.as_ref()
    }
    pub(crate) fn mappings(&self) -> Option<&RecoveryMappings> {
        self.mappings.as_ref()
    }
}
/// Why FFmpeg's MOV demuxer does not use a track's recovery groups, for trace
/// comparison.
#[cfg(feature = "reference-trace")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryReceiverCause {
    /// The demuxer's open and index apply no `roll` or `prol` group.
    RollProlNotAppliedByOpenIndex,
}
/// The checked `roll` and `prol` groups of a sample table, which FFmpeg's MOV demuxer
/// does not apply when it opens and indexes the track.
#[derive(Clone, Debug)]
pub struct RecoveryFacts {
    groups: [RecoveryGroup; 2],
    samples: u64,
}
impl RecoveryFacts {
    /// The group of `kind`; empty when the table has no box of that type.
    pub fn group(&self, kind: RecoveryKind) -> &RecoveryGroup {
        &self.groups[match kind {
            RecoveryKind::Roll => 0,
            RecoveryKind::Prol => 1,
        }]
    }
    /// The group of `kind` as a [`RecoveryScope`] over every sample of the table, from
    /// ordinal 1.
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
    /// Why the demuxer does not apply the groups, for trace comparison.
    #[cfg(feature = "reference-trace")]
    pub fn cause(&self) -> RecoveryReceiverCause {
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
    raw: &crate::boxes::BoxView,
    fault: &mut Option<crate::boxes::BoxView>,
) -> Result<T> {
    if value.is_err() {
        *fault = Some(raw.clone());
    }
    value
}
#[derive(Debug)]
pub(crate) struct RecoveryWalk {
    children: BoxChildren,
    pending: Option<Pending>,
    seen: u8,
    facts: RecoveryFacts,
    maximum_indices: [u32; 2],
    limits: RecoveryLimits,
    error_backing: Option<crate::boxes::BoxView>,
}
impl RecoveryWalk {
    pub(crate) fn new(table: &SampleTable, maximum: u64) -> Result<Self> {
        Ok(Self {
            children: table.raw().children(0, Limits::default())?,
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
            error_backing: None,
        })
    }
    pub(crate) fn error_backing(&self) -> Option<&crate::boxes::BoxView> {
        self.error_backing.as_ref()
    }
    pub(crate) fn take_facts(&mut self) -> RecoveryFacts {
        RecoveryFacts {
            groups: std::mem::take(&mut self.facts.groups),
            samples: self.facts.samples,
        }
    }
    pub(crate) fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<RecoveryFacts>> {
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
                            let next = source.validation_at(index, *offset);
                            let mut lookup =
                                source_result(next, source.raw(), &mut self.error_backing)?;
                            if source_result(
                                lookup.poll(work, cancel),
                                source.raw(),
                                &mut self.error_backing,
                            )? == BuildStatus::Yield
                            {
                                return Ok(None);
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
                                return Ok(None);
                            }
                            let (count, index) = source_result(
                                source.run(*row),
                                source.raw(),
                                &mut self.error_backing,
                            )?;
                            if count == 0 {
                                *row += 1;
                                continue;
                            }
                            if index >= 65536 {
                                return source_result(
                                    Err(Error::Unsupported("encoded fragment recovery namespace")),
                                    source.raw(),
                                    &mut self.error_backing,
                                );
                            }
                            *covered = covered
                                .checked_add(u64::from(count))
                                .ok_or(Error::Overflow)?;
                            if *covered > self.facts.samples {
                                return source_result(
                                    Err(Error::Invalid("encoded recovery exceeds samples")),
                                    source.raw(),
                                    &mut self.error_backing,
                                );
                            }
                            *maximum = (*maximum).max(index);
                            *row += 1;
                            continue;
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
            match self.children.poll(work)? {
                Next::Yield => return Ok(None),
                Next::End => {
                    for (slot, group) in self.facts.groups.iter().enumerate() {
                        let count = group
                            .descriptions
                            .as_ref()
                            .map_or(0, RecoveryDescriptions::entry_count);
                        if self.maximum_indices[slot] > count {
                            self.error_backing = group.mappings.as_ref().map(|s| s.raw().clone());
                            return Err(Error::Invalid("encoded unresolved recovery description"));
                        }
                    }
                    return Ok(Some(self.take_facts()));
                }
                Next::Item(raw) => {
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
                        x if x == *b"sync" || (!description && x == *b"rap ") => {
                            return source_result(
                                Err(Error::Unsupported(
                                    "encoded sample group that changes key frames",
                                )),
                                &raw,
                                &mut self.error_backing,
                            );
                        }
                        _ => continue,
                    };
                    let bit = 1u8 << (slot + if description { 0 } else { 2 });
                    if self.seen & bit != 0 {
                        return source_result(
                            Err(Error::Invalid("encoded duplicate recovery context")),
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
                            && !matches!(source.fixed_entry_length(), 0 | 2)
                        {
                            return source_result(
                                Err(Error::Unsupported(
                                    "encoded variable or extended recovery entries",
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
}
