//! Atomic, bounded complete-current-PAT assembly; no codec or I/O generation policy.
use super::{PatSection, ProgramAssociation, SectionHeader, TableStep};
use crate::{ParseLimits, SourceRange, limit};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, PushError, Result, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub struct PatSourceRange {
    pub section_number: u8,
    pub range: SourceRange,
}
#[derive(Debug)]
pub struct PatTable {
    pub header: SectionHeader,
    pub programs: BudgetVec<ProgramAssociation>,
    pub source_sections: BudgetVec<Bytes>,
    pub source_ranges: BudgetVec<PatSourceRange>,
}
#[derive(Debug)]
struct Version {
    header: SectionHeader,
    sections: BudgetVec<PatSection>,
    program_count: usize,
}
#[derive(Debug)]
struct Build {
    slot: usize,
    number: u16,
    entry: usize,
    range: usize,
    copied: bool,
    programs: BudgetVec<ProgramAssociation>,
    sources: BudgetVec<Bytes>,
    ranges: BudgetVec<PatSourceRange>,
}
#[derive(Debug)]
pub struct PatCollector {
    budget: Budget,
    limits: ParseLimits,
    versions: [Option<Version>; 2],
    active: Option<usize>,
    input: Option<PatSection>,
    building: Option<Build>,
}
impl PatCollector {
    pub fn new(budget: Budget, limits: ParseLimits) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            budget,
            limits,
            versions: [None, None],
            active: None,
            input: None,
            building: None,
        })
    }
    pub fn discard(&mut self) {
        self.versions = [None, None];
        self.active = None;
        self.input = None;
        self.building = None;
    }
    pub(crate) fn retained_sections(&self) -> [Option<&BudgetVec<PatSection>>; 2] {
        self.versions
            .each_ref()
            .map(|version| version.as_ref().map(|version| &version.sections))
    }

    pub(crate) fn active_single_section(&self) -> Option<&Bytes> {
        let version = self.versions[self.active?].as_ref()?;
        let [section] = version.sections.as_slice() else {
            return None;
        };
        Some(&section.source)
    }
    pub(crate) fn has_pending(&self) -> bool {
        self.input.is_some() || self.building.is_some()
    }
    /// Rejection returns the original charged source and metadata allocations.
    /// Allocating a box on this failure path would need a new budget reservation.
    #[allow(
        clippy::result_large_err,
        reason = "return the original charged PAT and source spans without allocating on rejection"
    )]
    pub fn submit(
        &mut self,
        section: PatSection,
    ) -> std::result::Result<(), PushError<PatSection>> {
        if self.input.is_some() || self.building.is_some() {
            return Err(PushError {
                error: Error::Invalid("PAT collector busy"),
                value: section,
            });
        }
        if !section.source.belongs_to(&self.budget) {
            return Err(PushError {
                error: Error::ForeignPool,
                value: section,
            });
        }
        self.input = Some(section);
        Ok(())
    }
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<TableStep<PatTable>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if let Some(build) = self.building.as_mut() {
                let version = self.versions[build.slot]
                    .as_ref()
                    .ok_or(Error::Invalid("PAT build lost version"))?;
                if build.number > u16::from(version.header.last_section_number) {
                    let build = self
                        .building
                        .take()
                        .ok_or(Error::Invalid("PAT build missing"))?;
                    let header = self.versions[build.slot]
                        .as_ref()
                        .ok_or(Error::Invalid("PAT version missing"))?
                        .header;
                    match self.active {
                        Some(old) if old != build.slot => self.versions[old] = None,
                        _ => {}
                    }
                    self.active = Some(build.slot);
                    return Ok(TableStep::Output(PatTable {
                        header,
                        programs: build.programs,
                        source_sections: build.sources,
                        source_ranges: build.ranges,
                    }));
                }
                if !work.try_charge(1) {
                    return Ok(TableStep::Yield);
                }
                let section = version
                    .sections
                    .as_slice()
                    .iter()
                    .find(|section| u16::from(section.header.section_number) == build.number)
                    .ok_or(Error::Invalid("complete PAT missing section"))?;
                if !build.copied {
                    build
                        .sources
                        .try_push(section.source.clone())
                        .map_err(|error| error.error)?;
                    build.copied = true;
                    continue;
                }
                if let Some(range) = section.source_ranges.as_slice().get(build.range) {
                    build
                        .ranges
                        .try_push(PatSourceRange {
                            section_number: section.header.section_number,
                            range: *range,
                        })
                        .map_err(|error| error.error)?;
                    build.range += 1;
                    continue;
                }
                if let Some(entry) = section.programs.as_slice().get(build.entry) {
                    if build
                        .programs
                        .as_slice()
                        .iter()
                        .any(|old| old.program_number == entry.program_number)
                    {
                        return Err(Error::Invalid("PAT duplicate program across sections"));
                    }
                    build
                        .programs
                        .try_push(*entry)
                        .map_err(|error| error.error)?;
                    build.entry += 1;
                    continue;
                }
                build.number += 1;
                build.entry = 0;
                build.range = 0;
                build.copied = false;
                continue;
            }
            let Some(section) = self.input.as_ref() else {
                return Ok(TableStep::Done);
            };
            if !work.try_charge(1) {
                return Ok(TableStep::Yield);
            }
            if !section.header.current_next {
                self.input = None;
                return Ok(TableStep::Done);
            }
            let identity = section.header;
            let mut slot = None;
            for (index, version) in self.versions.iter().enumerate() {
                match version {
                    Some(version)
                        if version.header.extension == identity.extension
                            && version.header.version == identity.version =>
                    {
                        if version.header.last_section_number != identity.last_section_number {
                            return Err(Error::Invalid("PAT conflicting section count"));
                        }
                        slot = Some(index);
                        break;
                    }
                    _ => {}
                }
            }
            let slot = if let Some(slot) = slot {
                slot
            } else {
                let index = self
                    .versions
                    .iter()
                    .position(Option::is_none)
                    .ok_or(limit(3, 2))?;
                self.versions[index] = Some(Version {
                    header: identity,
                    sections: BudgetVec::new(&self.budget),
                    program_count: 0,
                });
                index
            };
            let version = self.versions[slot]
                .as_mut()
                .ok_or(Error::Invalid("PAT version missing"))?;
            if let Some(old) = version
                .sections
                .as_slice()
                .iter()
                .find(|old| old.header.section_number == identity.section_number)
            {
                if old.source.as_ref() != section.source.as_ref() {
                    return Err(Error::Invalid("PAT conflicting repeat"));
                }
                self.input = None;
                return Ok(TableStep::Done);
            }
            let total = version
                .program_count
                .checked_add(section.programs.len())
                .ok_or(Error::Overflow)?;
            if total > self.limits.programs {
                return Err(limit(total, self.limits.programs));
            }
            version.sections.try_reserve_exact(1)?;
            let section = self
                .input
                .take()
                .ok_or(Error::Invalid("PAT input missing"))?;
            version
                .sections
                .try_push(section)
                .map_err(|error| error.error)?;
            version.program_count = total;
            if version.sections.len() == usize::from(version.header.last_section_number) + 1 {
                self.building = Some(Build {
                    slot,
                    number: 0,
                    entry: 0,
                    range: 0,
                    copied: false,
                    programs: BudgetVec::new(&self.budget),
                    sources: BudgetVec::new(&self.budget),
                    ranges: BudgetVec::new(&self.budget),
                });
            } else {
                return Ok(TableStep::Done);
            }
        }
    }
}
